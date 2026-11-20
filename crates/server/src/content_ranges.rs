//! A shard's slice of the anchor content object, range by range (§4.2, §4.14; A-744, main's ruling of 2026-11-20:
//! split the content object's resident bytes by range before deciding where a green's bytes live).
//!
//! **What it reports.** For each range of the shard's slice (`DaemonConfig::shard_content_layout`): the write log, the
//! two image slots, the delta log, the DAX range table, and the chunk arena summed over the regions the shard holds,
//! the range's bytes, the bytes of its pages in RAM, and the bytes written where the OS says (macOS), read from the
//! kernel's page table (`slates_mem::resident`); for the arena also the bytes allocated in it and the reserve an idle
//! shard keeps pre-faulted. Beside them, the register (the partition's log since its last snapshot, and that snapshot)
//! by what its bytes carry: repository records, green chain entries, checkpoints and the rest (A-745,
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
/// metadata object beside the content object): its log ring and its two snapshot slots.
const RANGES: [&str; 7] = [
  "write_log",
  "images",
  "delta_log",
  "dax_ranges",
  "arena",
  "segment_log",
  "segment_snapshots",
];

/// Format: the arena's place in [`RANGES`].
const ARENA: usize = 4;

/// Format: the segment log ring's place in [`RANGES`].
const SEGMENT_LOG: usize = 5;

/// Format: the segment snapshot slots' place in [`RANGES`].
const SEGMENT_SNAPSHOTS: usize = 6;

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
}

impl Target {
  /// Its length.
  fn len(&self) -> usize {
    match self {
      Target::Object { len, .. } | Target::Region { len, .. } | Target::Segment { len, .. } => *len,
    }
  }

  /// Its range's place in [`RANGES`].
  fn range(&self) -> usize {
    match self {
      Target::Object { range, .. } | Target::Segment { range, .. } => *range,
      Target::Region { .. } => ARENA,
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
  }
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
  loop {
    let (index, offset) = state.content_sweep.at;
    let Some(target) = targets.get(index) else {
      let sweep = &mut state.content_sweep;
      sweep.done = Some(std::mem::take(&mut sweep.partial));
      sweep.sweeps = sweep.sweeps.saturating_add(1);
      sweep.at = (0, 0);
      return;
    };
    let now = slates_vfs::clock::Clock::monotonic_ns(&mut state.clock);
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
    let after = slates_vfs::clock::Clock::monotonic_ns(&mut state.clock);
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

/// The shard's content-range figures for the status report (module docs): the sweep advanced, then the last completed
/// sweep's figures, every one reported (zero too, so an absent `.modified` means the OS does not say), the arena's
/// allocated bytes now, and the sweeps completed.
pub(crate) fn report(state: &mut ShardState) -> Vec<RefusalCount> {
  advance(state);
  let mut rows: Vec<(String, u64)> = Vec::new();
  if let Some(done) = &state.content_sweep.done {
    for (name, found) in RANGES.iter().zip(done) {
      rows.push((format!("{PREFIX}{name}.bytes"), found.bytes));
      rows.push((format!("{PREFIX}{name}.resident"), found.resident));
      if let Some(modified) = found.modified {
        rows.push((format!("{PREFIX}{name}.modified"), modified));
      }
    }
  }
  rows.push((
    format!("{PREFIX}arena.allocated"),
    u64::try_from(state.store.content.allocated_bytes()).unwrap_or(u64::MAX),
  ));
  rows.push((
    format!("{PREFIX}arena.prefault_reserve"),
    u64::try_from(crate::daemon::prefault_reserve_bytes(state)).unwrap_or(u64::MAX),
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
  rows
    .into_iter()
    .map(|(kind, count)| RefusalCount { kind, count })
    .collect()
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
        report(state);
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
