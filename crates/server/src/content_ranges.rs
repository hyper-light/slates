//! A shard's slice of the anchor content object, range by range (§4.2, §4.14; A-744, main's ruling of 2026-11-20:
//! split the content object's resident bytes by range before deciding where a green's bytes live).
//!
//! **What it reports.** For each range of the shard's slice (`DaemonConfig::shard_content_layout`): the write log, the
//! two image slots, the delta log, the DAX range table, and the chunk arena summed over the regions the shard holds,
//! the range's bytes, the bytes of its pages in RAM, and the bytes written where the OS says (macOS), read from the
//! kernel's page table (`slates_mem::resident`); for the arena also the bytes allocated in it. Beside them, the
//! register (the partition's log since its last snapshot, and that snapshot) by what its bytes carry: repository records, green chain entries, checkpoints and the rest (A-745,
//! `slates_db::replay::RegisterSplit`). `slates status --json` prints them per shard under `content_ranges`.
//!
//! **Bounded.** The kernel answers a page at a time, about 3 ns a page on the Mac Studio (1 GiB in 0.23 ms, 64 GiB in
//! 11.9 ms, measured 2026-11-20), so a slice of tens of GiB of address space is past a turn. Each status request
//! advances the shard's sweep for at most half its live step quantum (`DaemonConfig::step_quantum_ns`, the bound every
//! cooperative slice keeps), a vector page of pages at a time, and reports the last sweep that completed, with how many
//! have. A daemon sized for its machine sweeps its slice within a request or two.

use slates_ipc::protocol::RefusalCount;
use slates_mem::resident::Residency;

use crate::state::ShardState;

/// Format: the prefix of a shard's content-range figures among its counters: `content_range.<range>.<figure>`.
pub(crate) const PREFIX: &str = "content_range.";

/// Format: the ranges, in the slice's order, then the shard's partition's regions of the anchor segment (the
/// metadata object beside the content object): its log ring and its two snapshot slots; then the daemon's own memory
/// report's ranges (§4.14, A-232's gotcha): on the control shard the anchor segment whole and the anchor content object
/// whole, and on every shard its clients' ring regions.
const RANGES: [&str; 10] = [
  "write_log",
  "images",
  "delta_log",
  "dax_ranges",
  "arena",
  "segment_log",
  "segment_snapshots",
  "segment",
  "content",
  "client_regions",
];

/// Format: the arena's place in [`RANGES`].
const ARENA: usize = 4;

/// Format: the segment log ring's place in [`RANGES`].
const SEGMENT_LOG: usize = 5;

/// Format: the segment snapshot slots' place in [`RANGES`].
const SEGMENT_SNAPSHOTS: usize = 6;

/// Format: the whole anchor segment's place in [`RANGES`] (the control shard's).
const SEGMENT_WHOLE: usize = 7;

/// Format: the whole anchor content object's place in [`RANGES`] (the control shard's).
const CONTENT_WHOLE: usize = 8;

/// Format: the shard's clients' ring regions' place in [`RANGES`].
const CLIENT_REGIONS: usize = 9;

/// A sweep of the shard's slice under way, and the last one that completed.
#[derive(Debug, Default)]
pub(crate) struct Sweep {
  /// The target the sweep is at ([`targets`]), and the offset within it.
  at: (usize, usize),
  /// What the sweep under way has found, range by range.
  partial: [Residency; RANGES.len()],
  /// The last completed sweep's figures.
  done: Option<[Residency; RANGES.len()]>,
  /// Sweeps completed.
  sweeps: u64,
  /// What a page cost the last step past its fixed part, nanoseconds (zero until a step is measured).
  ns_per_page: u64,
  /// A step's fixed cost, nanoseconds: the cheapest step seen; zero until measured.
  ns_fixed: u64,
  /// Advances that crossed their budget on a step of more than one page: the sizing's failure, zero in every correct
  /// use (a one-page step that alone outlasts the budget is the least step, not a failure).
  past_budget: u64,
  /// The longest advance, nanoseconds.
  longest_ns: u64,
  /// The longest single step, nanoseconds: what an advance may pass its budget by at most.
  longest_step_ns: u64,
  /// Whether a status request armed a sweep the shard's idle time is advancing ([`arm`]): one at a time.
  running: bool,
  /// Slices of the sweep run on the shard's idle time ([`idle_slice`]): the background path's non-vacuity count.
  idle_slices: u64,
  /// When the armed sweep was armed, shard clock nanoseconds, and the sweeps completed then ([`arm`]): the armed sweep
  /// is done once one more has completed, by an idle slice or a request's own advance.
  armed: (u64, u64),
  /// When the last armed sweep completed, and how long it took from its arming: the spacing [`arm`] keeps.
  last: Option<(u64, u64)>,
  /// Status requests that found the last sweep too recent to arm another ([`arm`]).
  spaced: u64,
  /// Sweeps armed ([`arm`]): only a request that asks for the memory report arms one.
  armed_count: u64,
  /// Steps the sweep has taken ([`advance`]), each a kernel call over a vector page of pages: none for a plain status.
  steps: u64,
  /// The anchor segment and content object whole, from the kernel's per-mapping figures (on the control shard), as the
  /// last memory request read them ([`read_whole`]); by their place in [`RANGES`].
  whole: [Option<Residency>; RANGES.len()],
}

/// One mapped range the sweep walks: a range of the content object, or a region of the arena.
enum Target {
  /// `len` bytes of the content object at `start`, for range `range`.
  Object {
    range: usize,
    start: usize,
    len: usize,
  },
  /// Arena region `id`, `len` bytes.
  Region { id: u16, len: usize },
  /// A region of the anchor segment, `len` bytes, for range `range`.
  Segment {
    range: usize,
    kind: slates_anchor::layout::RegionKind,
    len: usize,
  },
  /// The ring region of client `client`, `len` bytes. A client gone between two advances is passed over.
  Client {
    client: slates_mem::Handle<crate::state::ClientSlot>,
    len: usize,
  },
}

impl Target {
  /// Its length.
  fn len(&self) -> usize {
    match self {
      Target::Object { len, .. }
      | Target::Region { len, .. }
      | Target::Segment { len, .. }
      | Target::Client { len, .. } => *len,
    }
  }

  /// Its range's place in [`RANGES`].
  fn range(&self) -> usize {
    match self {
      Target::Object { range, .. } | Target::Segment { range, .. } => *range,
      Target::Region { .. } => ARENA,
      Target::Client { .. } => CLIENT_REGIONS,
    }
  }
}

/// The mapped ranges of the shard's slice, in order: the object's four, then the arena's regions.
fn targets(state: &ShardState) -> Vec<Target> {
  let mut targets = Vec::new();
  if state.content.is_some() {
    let layout = state.config.shard_content_layout();
    let start = crate::daemon::slice_start(&state.config, state.partition);
    for (range, (from, to)) in [
      layout.write_log,
      layout.images,
      layout.delta_log,
      layout.dax_ranges,
    ]
    .into_iter()
    .enumerate()
    {
      targets.push(Target::Object {
        range,
        start: start.saturating_add(from),
        len: to.saturating_sub(from),
      });
    }
  }
  let arena = state.store.content.arena();
  for id in arena.region_ids() {
    if let Some(region) = arena.region(*id) {
      targets.push(Target::Region {
        id: *id,
        len: region.len(),
      });
    }
  }
  let partition = state.partition;
  for (range, kind) in [
    (
      SEGMENT_LOG,
      slates_anchor::layout::RegionKind::Log(partition),
    ),
    (
      SEGMENT_SNAPSHOTS,
      slates_anchor::layout::RegionKind::Snapshot(partition, 0),
    ),
    (
      SEGMENT_SNAPSHOTS,
      slates_anchor::layout::RegionKind::Snapshot(partition, 1),
    ),
  ] {
    if let Ok(len) = state.segment.region_len(kind) {
      targets.push(Target::Segment { range, kind, len });
    }
  }
  // The daemon's own memory report (§4.14): each shard's clients' ring regions on their shard. The two anchor objects
  // whole are read from the kernel's per-mapping figures, never walked ([`read_whole`]).
  for (client, slot) in state.clients.iter() {
    targets.push(Target::Client {
      client,
      len: slot.end.region().object().len(),
    });
  }
  targets
}

/// What `len` bytes at `offset` within `target` hold in RAM; `None` where the OS refuses to say.
fn residency(
  state: &ShardState,
  target: &Target,
  (offset, len): (usize, usize),
) -> Option<Residency> {
  match target {
    Target::Object { start, .. } => state
      .content
      .as_ref()?
      .residency(start.saturating_add(offset), len)
      .ok(),
    Target::Region { id, .. } => state
      .store
      .content
      .arena()
      .region(*id)?
      .residency_of(offset, len)
      .ok(),
    Target::Segment { kind, .. } => state.segment.region_residency_of(*kind, offset, len).ok(),
    Target::Client { client, .. } => state
      .clients
      .get(*client)
      .ok()?
      .end
      .region()
      .object()
      .residency(offset, len, state.config.page)
      .ok(),
  }
}

/// Arms one sweep to completion in the background (§4.14, the daemon's memory report): a request's own advance walks
/// half a step quantum, and the anchor segment and content object whole are tens of GiB, millions of pages, which a
/// status request alone would take hundreds of requests to sweep (84 GiB unswept after 400 requests, a debug daemon on the
/// Mac Studio, 2026-11-20). So a status request also hands the rest to the shard's idle time, unless a sweep is already
/// armed: the shard's idle work ([`idle_slice`], `ShardContext::set_idle_work`) advances it a bounded step whenever the
/// shard has nothing ready and its idle spin has run out, until one sweep completes; the next request reads it.
///
/// Never as a task: one that advanced the sweep a step a turn, yielding between, ran half a step quantum of `mincore`
/// ahead of every turn's ready work for as long as a sweep lasted, seconds over tens of GiB, on every shard a status
/// request reached. A stream of live writes begun after a status request paid it at every hop: on the 32-core Studio
/// (load 4-75, 2026-11-20, BENCHMARKS) what live shipping added went from 543-823 µs to 1,312-4,111 µs at the median, its
/// p999 from 5.6-11.4 ms to 25-198 ms, and every daemon burned 1.5-9.3 s of CPU over a 2,000-write stream against
/// 0.26-0.93 s, `mincore` the heaviest frame in each (g228). On idle time a sweep takes nothing from ready work, and a
/// busy shard's sweep waits for its idle moments while each request still moves it (`report`'s own advance).
///
/// Spaced by its own length: a sweep is armed no sooner after the last one completed than that one took. So the sweeps
/// take at most half of a shard's time over any span however often status is asked (a poller asking every few
/// milliseconds kept every shard sweeping), and what a request reads is at most two sweeps old. A request that finds the
/// last sweep too recent is counted (`content_range.sweep.spaced`) and reads it. The shard has one idle-work slot and
/// this is the daemon's only use of it since A-777 removed the arena reserve's refill.
fn arm(state: &mut ShardState) {
  if state.content_sweep.running {
    return;
  }
  let now = slates_vfs::clock::Clock::monotonic_ns(&mut state.clock);
  if state
    .content_sweep
    .last
    .is_some_and(|(completed_ns, took_ns)| now < completed_ns.saturating_add(took_ns))
  {
    state.content_sweep.spaced = state.content_sweep.spaced.saturating_add(1);
    return;
  }
  let registered =
    slates_rt::registry::with_current(|context| context.set_idle_work(Box::new(idle_slice)));
  if matches!(registered, Some(Ok(()))) {
    state.content_sweep.running = true;
    state.content_sweep.armed = (now, state.content_sweep.sweeps);
    state.content_sweep.armed_count = state.content_sweep.armed_count.saturating_add(1);
  }
}

/// One slice of an armed sweep on the shard's idle time ([`arm`]): one [`advance`], bounded by half the step quantum as
/// the shard's idle work must be. Whether it did anything: `false` once the armed sweep has completed (or none is armed,
/// or the state is out of reach), so the shard parks, and an idle shard pays one flag read before each park after that.
fn idle_slice() -> bool {
  crate::state::with_state(|state| {
    if !state.content_sweep.running {
      return false;
    }
    let advanced = !armed_sweep_done(state);
    if advanced {
      advance(state);
      state.content_sweep.idle_slices = state.content_sweep.idle_slices.saturating_add(1);
    }
    if armed_sweep_done(state) {
      let now = slates_vfs::clock::Clock::monotonic_ns(&mut state.clock);
      let sweep = &mut state.content_sweep;
      sweep.running = false;
      sweep.last = Some((now, now.saturating_sub(sweep.armed.0)));
    }
    advanced
  })
  .unwrap_or(false)
}

/// Whether the armed sweep has completed: one more sweep since it was armed, by an idle slice or a request's own advance
/// ([`report`]), so a request that finishes it leaves the idle work nothing to begin again.
fn armed_sweep_done(state: &ShardState) -> bool {
  state.content_sweep.sweeps > state.content_sweep.armed.1
}

/// Advances the sweep for at most half the shard's step quantum (module docs, "Bounded"); at most one sweep completes.
fn advance(state: &mut ShardState) {
  let budget = state.config.step_quantum_ns() / 2;
  let page = state.config.page.max(1);
  // Derived: at most a vector page of pages a step ([`slates_mem::resident::of_range`]'s own batch), and fewer: a step
  // costs a fixed part (the cheapest step seen: a kernel call and two clock reads) and a part a page, so it walks as many
  // pages as the budget left holds past the fixed part (one while nothing is measured).
  let most = page;
  // The targets first: a handful, built in constant time; the budget's clock starts at the first step.
  let targets = targets(state);
  let began = slates_vfs::clock::Clock::monotonic_ns(&mut state.clock);
  // Each step is timed from where the last ended (the first from `began`), so the steps cover the advance whole: time
  // the thread spent preempted between two steps is the later step's, and the advance passes its budget by at most the
  // step that crossed it.
  let mut mark = began;
  loop {
    let (index, offset) = state.content_sweep.at;
    let Some(target) = targets.get(index) else {
      let sweep = &mut state.content_sweep;
      sweep.done = Some(std::mem::take(&mut sweep.partial));
      sweep.sweeps = sweep.sweeps.saturating_add(1);
      sweep.at = (0, 0);
      return;
    };
    let now = mark;
    let left = budget.saturating_sub(now.saturating_sub(began));
    let fixed = state.content_sweep.ns_fixed;
    let pages = match state.content_sweep.ns_per_page {
      0 => 1,
      cost => usize::try_from(left.saturating_sub(fixed).checked_div(cost).unwrap_or(0))
        .unwrap_or(usize::MAX)
        .clamp(1, most),
    };
    let len = target
      .len()
      .saturating_sub(offset)
      .min(pages.saturating_mul(page));
    let found = residency(state, target, (offset, len));
    state.content_sweep.steps = state.content_sweep.steps.saturating_add(1);
    let after = slates_vfs::clock::Clock::monotonic_ns(&mut state.clock);
    mark = after;
    let sweep = &mut state.content_sweep;
    let walked = u64::try_from(len.div_ceil(page).max(1)).unwrap_or(u64::MAX);
    let took = after.saturating_sub(now);
    sweep.longest_step_ns = sweep.longest_step_ns.max(took);
    sweep.ns_fixed = match sweep.ns_fixed {
      0 => took,
      seen => seen.min(took),
    };
    sweep.ns_per_page = took
      .saturating_sub(sweep.ns_fixed)
      .checked_div(walked)
      .unwrap_or(0)
      .max(1);
    if let (Some(found), Some(sum)) = (found, sweep.partial.get_mut(target.range())) {
      *sum = if sum.bytes == 0 && sum.modified.is_none() {
        found
      } else {
        sum.plus(found)
      };
    }
    let next = offset.saturating_add(len);
    sweep.at = if next >= target.len() {
      (index.saturating_add(1), 0)
    } else {
      (index, next)
    };
    let elapsed = after.saturating_sub(began);
    sweep.longest_ns = sweep.longest_ns.max(elapsed);
    if elapsed >= budget {
      // The budget is checked after every step, so an advance passes it by at most the step that crossed it, and a step
      // is at most one kernel batch: past by more than a whole batch's cost at the measured rates is the bound broken.
      let batch = sweep.ns_fixed.saturating_add(
        sweep
          .ns_per_page
          .saturating_mul(u64::try_from(most).unwrap_or(u64::MAX)),
      );
      // The host's scheduling noise allowed for, as the runtime measures it (its timer overrun): a thread preempted
      // between two steps is the host's time, not the sweep's.
      let noise =
        slates_rt::registry::with_current(|context| context.counters().scheduler_overrun_ns)
          .unwrap_or(0);
      if elapsed.saturating_sub(budget) > batch.max(sweep.longest_step_ns).saturating_add(noise) {
        sweep.past_budget = sweep.past_budget.saturating_add(1);
      }
      return;
    }
  }
}

/// The shard's content-range figures for the status report (module docs). A request that asks for the memory report
/// (`memory`) advances the sweep, arms it to completion on idle time ([`arm`]) and reads the anchor's objects whole
/// ([`read_whole`]); a plain one does no residency work at all and reports the last figures read (main's ruling on
/// g228's finding: every plain status armed a sweep of tens of GiB on every shard, about a core a daemon for a second or
/// more after a burst). Then every figure is reported (zero too, so an absent `.modified` means the OS does not say),
/// the arena's allocated bytes now, and the sweeps completed.
pub(crate) fn report(state: &mut ShardState, memory: bool) -> Vec<RefusalCount> {
  if memory {
    advance(state);
    arm(state);
    read_whole(state);
  }
  let mut rows: Vec<(String, u64)> = Vec::new();
  range_rows(&state.content_sweep, &mut rows);
  rows.push((
    format!("{PREFIX}arena.allocated"),
    u64::try_from(state.store.content.allocated_bytes()).unwrap_or(u64::MAX),
  ));
  // The register (the partition's log and snapshot in the anchor segment) by what its bytes carry (A-745): the log
  // since the last snapshot, and the last snapshot.
  let stats = state.db.stats();
  for (range, split) in [
    ("register_log", stats.tail_split),
    ("register_snapshot", stats.snapshot_split),
  ] {
    for (figure, bytes) in [
      ("repository_records", split.repository_records),
      ("chain_entries", split.chain_entries),
      ("checkpoints", split.checkpoints),
      ("rest", split.rest),
    ] {
      rows.push((format!("{PREFIX}{range}.{figure}"), bytes));
    }
  }
  rows.push((
    format!("{PREFIX}sweep.completed"),
    state.content_sweep.sweeps,
  ));
  rows.push((
    format!("{PREFIX}sweep.idle_slices"),
    state.content_sweep.idle_slices,
  ));
  rows.push((format!("{PREFIX}sweep.spaced"), state.content_sweep.spaced));
  rows.push((
    format!("{PREFIX}sweep.armed"),
    state.content_sweep.armed_count,
  ));
  rows.push((format!("{PREFIX}sweep.steps"), state.content_sweep.steps));
  rows.push((
    format!("{PREFIX}sweep.past_budget"),
    state.content_sweep.past_budget,
  ));
  rows.push((
    format!("{PREFIX}sweep.longest_ns"),
    state.content_sweep.longest_ns,
  ));
  // The pushes' work builds' turns (A-752): beside the sweep's, the shard's other paced work this report shows.
  for (name, count) in state.push_turns.rows() {
    rows.push((name.to_owned(), count));
  }
  // The runtime's own counters, the shard's since its start: steps, polls, polls past their allowance, the longest
  // poll, and polls by length (A-783; `crates/cli/bench/pacing_ab.py` reads them before and after its pushes).
  runtime_rows(&mut rows);
  rows
    .into_iter()
    .map(|(kind, count)| RefusalCount { kind, count })
    .collect()
}

/// Each range's last figures, as rows: its bytes, resident bytes and, where the OS says, modified bytes.
fn range_rows(sweep: &Sweep, rows: &mut Vec<(String, u64)>) {
  for (range, name) in RANGES.iter().enumerate() {
    if let Some(found) = figure(sweep, range) {
      rows.push((format!("{PREFIX}{name}.bytes"), found.bytes));
      rows.push((format!("{PREFIX}{name}.resident"), found.resident));
      if let Some(modified) = found.modified {
        rows.push((format!("{PREFIX}{name}.modified"), modified));
      }
    }
  }
}

/// The runtime's own counters, the shard's since its start, as rows: steps, polls, polls past their allowance, the
/// longest poll, and polls by length (A-783).
fn runtime_rows(rows: &mut Vec<(String, u64)>) {
  if let Some(counters) = slates_rt::registry::with_current(|context| context.counters()) {
    for (name, count) in [
      ("rt.steps", counters.steps),
      ("rt.polls", counters.polls),
      ("rt.long_steps", counters.long_steps),
      ("rt.preempted_steps", counters.preempted_steps),
      ("rt.unattributed_steps", counters.unattributed_steps),
      ("rt.longest_step_ns", counters.longest_step_ns),
      ("rt.waiting_asked", counters.waiting_asked),
      ("rt.waiting_found", counters.waiting_found),
    ] {
      rows.push((name.to_owned(), count));
    }
    for (bucket, count) in counters.poll_log2.iter().enumerate() {
      if *count > 0 {
        rows.push((format!("rt.poll_log2.{bucket}"), *count));
      }
    }
  }
}

/// A range's last figures: the anchor's objects whole from their per-mapping read ([`read_whole`]), every other range
/// from the last completed sweep.
fn figure(sweep: &Sweep, range: usize) -> Option<Residency> {
  if range == SEGMENT_WHOLE || range == CONTENT_WHOLE {
    sweep.whole.get(range).copied().flatten()
  } else {
    sweep
      .done
      .as_ref()
      .and_then(|done| done.get(range))
      .copied()
  }
}

/// Reads the anchor segment and content object whole from the kernel's per-mapping figures, on the control shard
/// (`slates_mem::resident::of_mapping`; main's ruling): the cost of reading their mappings, where a page walk of tens of
/// GiB took tens of milliseconds a sweep. A read the OS refuses leaves the last figures.
fn read_whole(state: &mut ShardState) {
  if !crate::verbs::is_control_shard(state) {
    return;
  }
  let segment = state.segment.mapped_residency().ok();
  let content = state
    .content
    .as_ref()
    .and_then(|content| content.mapped_residency().ok());
  let whole = &mut state.content_sweep.whole;
  for (range, found) in [(SEGMENT_WHOLE, segment), (CONTENT_WHOLE, content)] {
    if let (Some(found), Some(slot)) = (found, whole.get_mut(range)) {
      *slot = Some(found);
    }
  }
}

/// This shard's part of the daemon's memory by kind (§4.14; A-232's gotcha): on the control shard the anchor segment and
/// content object whole, on every shard its arena's extents and its clients' ring regions. Each kind's reserved bytes
/// and mappings are its live lengths now; its resident and written bytes are the last completed sweep's ([`advance`],
/// which each status request moves), `measured` once one has completed. Called after [`report`], which advances it.
pub(crate) fn memory(state: &ShardState) -> Vec<slates_ipc::protocol::MemoryKindReport> {
  let kind = |name: &str, range: usize, (mappings, reserved): (u64, u64)| {
    let found = figure(&state.content_sweep, range);
    slates_ipc::protocol::MemoryKindReport {
      kind: name.to_owned(),
      mappings,
      reserved_bytes: reserved,
      resident_bytes: found.map_or(0, |found| found.resident.min(reserved)),
      modified_bytes: found.and_then(|found| found.modified),
      measured: found.is_some(),
    }
  };
  let bytes = |len: usize| u64::try_from(len).unwrap_or(u64::MAX);
  let mut kinds = Vec::new();
  if crate::verbs::is_control_shard(state) {
    kinds.push(kind(
      "segment",
      SEGMENT_WHOLE,
      (1, bytes(state.segment.len())),
    ));
    if let Some(content) = state.content.as_ref() {
      kinds.push(kind("content", CONTENT_WHOLE, (1, bytes(content.len()))));
    }
  }
  let arena = state.store.content.arena();
  let (extents, extent_bytes) = arena
    .region_ids()
    .iter()
    .filter_map(|id| arena.region(*id))
    .fold((0u64, 0u64), |(count, sum), region| {
      (
        count.saturating_add(1),
        sum.saturating_add(bytes(region.len())),
      )
    });
  kinds.push(kind("arenas", ARENA, (extents, extent_bytes)));
  let (clients, region_bytes) =
    state
      .clients
      .iter()
      .fold((0u64, 0u64), |(count, sum), (_, slot)| {
        (
          count.saturating_add(1),
          sum.saturating_add(bytes(slot.end.region().object().len())),
        )
      });
  kinds.push(kind("regions", CLIENT_REGIONS, (clients, region_bytes)));
  kinds
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used)]

  use super::*;

  /// A-744 (main's ruling: bounded a step at a time). Do: on a running daemon, ask the shard's report once a poll, as
  /// status requests do, until a sweep completes (at most one request a page of the slice, plus one per target).
  /// Expect: a sweep completes within that bound; the arena's measured bytes are its regions' lengths; every range's
  /// resident and written bytes are within its own bytes; no advance crossed its budget on a step larger than one page
  /// (the sweep's `past_budget` at 0: each step sized to the budget left); and the longest advance far inside the
  /// watchdog's window. Before, the test asked every report inside one poll, a step was a fixed batch of 16,384 pages and
  /// a segment region one step whole: a debug shard spun 3.2 s in one poll (w-perf); with one report a poll but a fixed
  /// batch, every advance ran past its budget. (The runtime's `long_steps` is no measure here: the debug audit daemon's
  /// live quantum is 3.2 us, which no status report's own rows fit; 200 no-op observes counted 6, 200 reports 200.)
  /// A-744, with g228's finding (a status request's sweep ran ahead of every turn's ready work for seconds). Do: on a
  /// running daemon, ask the shard's report once, as a status request does, then only watch, with observations that
  /// report nothing, until a sweep completes; then ask the report again at once. Expect: the sweep completes with no
  /// further request, by slices of the shard's idle time (`idle_slices` moved, the background path's non-vacuity count),
  /// and then stops (nothing left armed); and the request right after it arms no new sweep (`spaced` moved), since the
  /// last completed less than its own length ago.
  #[test]
  // The watch leaves the shard idle between its looks, which is the condition under test: a look is a task on the shard.
  #[allow(clippy::disallowed_methods)]
  fn a_status_requests_sweep_runs_on_the_shards_idle_time_and_is_spaced_by_its_length() {
    /// Shape: the watch's bound: a debug audit daemon's whole slice, swept on idle time, within ten liveness budgets.
    const WATCH: std::time::Duration =
      std::time::Duration::from_nanos(10 * crate::daemon::LIVENESS_BUDGET_NS);
    /// Shape: the pause between two looks: a hundredth of a liveness budget, long against a shard's idle spin, so the
    /// shard reaches its idle work between looks, as a daemon nobody asks does.
    const LOOK: std::time::Duration =
      std::time::Duration::from_nanos(crate::daemon::LIVENESS_BUDGET_NS / 100);
    let daemon = crate::daemon::audit_daemon();
    crate::daemon::observe_first(&daemon, |state| {
      report(state, true);
    });
    let started = std::time::Instant::now();
    let mut seen = (0, 0, true);
    while started.elapsed() < WATCH {
      seen = crate::daemon::observe_first(&daemon, |state| {
        let sweep = &state.content_sweep;
        (sweep.sweeps, sweep.idle_slices, sweep.running)
      });
      if seen.0 > 0 && !seen.2 {
        break;
      }
      std::thread::sleep(LOOK);
    }
    let spaced = crate::daemon::observe_first(&daemon, |state| {
      report(state, true);
      (state.content_sweep.spaced, state.content_sweep.running)
    });
    daemon.stop();
    let (sweeps, idle_slices, running) = seen;
    assert!(
      sweeps > 0,
      "a sweep completed on the shard's idle time within {WATCH:?}: (sweeps, idle slices, armed) {seen:?}"
    );
    assert!(
      idle_slices > 0,
      "the sweep advanced in idle slices: {seen:?}"
    );
    assert!(!running, "the completed sweep left nothing armed: {seen:?}");
    assert_eq!(
      spaced,
      (1, false),
      "a request right after a sweep completed armed none: (spaced, running) {spaced:?}"
    );
  }

  #[test]
  fn a_sweep_completes_within_its_bound_and_holds_every_range_to_its_bytes() {
    let daemon = crate::daemon::audit_daemon();
    let (bound, arena_len) = crate::daemon::observe_first(&daemon, |state| {
      let page = state.config.page.max(1);
      let targets = targets(state);
      // The worst case: a step of one page a request (each step is sized to the budget left).
      let total: usize = targets
        .iter()
        .map(|target| target.len().div_ceil(page))
        .sum();
      let arena_len: usize = targets
        .iter()
        .filter(|target| target.range() == ARENA)
        .map(Target::len)
        .sum();
      (total + targets.len() + 1, arena_len)
    });
    let mut asked = 0;
    let mut sweeps = 0;
    while sweeps == 0 && asked < bound {
      sweeps = crate::daemon::observe_first(&daemon, |state| {
        report(state, true);
        state.content_sweep.sweeps
      });
      asked += 1;
    }
    let (done, past_budget, longest_ns) = crate::daemon::observe_first(&daemon, |state| {
      let sweep = &state.content_sweep;
      (sweep.done, sweep.past_budget, sweep.longest_ns)
    });
    daemon.stop();
    assert!(sweeps > 0, "no sweep completed within {bound} requests");
    let done = done.unwrap();
    assert_eq!(done[ARENA].bytes, u64::try_from(arena_len).unwrap());
    for found in done {
      assert!(found.resident <= found.bytes, "{found:?}");
      assert!(
        found
          .modified
          .is_none_or(|modified| modified <= found.bytes),
        "{found:?}"
      );
    }
    assert_eq!(
      past_budget, 0,
      "an advance's step was sized past its budget"
    );
    assert!(
      longest_ns < crate::daemon::LIVENESS_BUDGET_NS,
      "an advance held the shard {longest_ns} ns, past the watchdog's window"
    );
  }
}
