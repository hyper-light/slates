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
    // A segment region is measured whole in one step: the segment answers by region, and its regions are the
    // partition's log ring and snapshot slots, each a fraction of the content slice.
    Target::Segment { kind, .. } if offset == 0 => state.segment.region_residency(*kind).ok(),
    Target::Segment { .. } => None,
  }
}

/// Advances the sweep for at most half the shard's step quantum (module docs, "Bounded"); at most one sweep completes.
fn advance(state: &mut ShardState) {
  let budget = state.config.step_quantum_ns() / 2;
  let began = slates_vfs::clock::Clock::monotonic_ns(&mut state.clock);
  let page = state.config.page.max(1);
  // Derived: a vector page of pages a call to the kernel ([`slates_mem::resident::of_range`]'s own batch).
  let step = page.saturating_mul(page);
  let targets = targets(state);
  loop {
    let (index, offset) = state.content_sweep.at;
    let Some(target) = targets.get(index) else {
      let sweep = &mut state.content_sweep;
      sweep.done = Some(std::mem::take(&mut sweep.partial));
      sweep.sweeps = sweep.sweeps.saturating_add(1);
      sweep.at = (0, 0);
      return;
    };
    // A segment region is one step whole (the segment answers by region); the object's ranges and the arena's regions
    // a vector page of pages at a time.
    let len = match target {
      Target::Segment { .. } => target.len().saturating_sub(offset),
      _ => target.len().saturating_sub(offset).min(step),
    };
    let found = residency(state, target, (offset, len));
    let sweep = &mut state.content_sweep;
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
    let now = slates_vfs::clock::Clock::monotonic_ns(&mut state.clock);
    if now.saturating_sub(began) >= budget {
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
  rows
    .into_iter()
    .map(|(kind, count)| RefusalCount { kind, count })
    .collect()
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used)]

  use super::*;

  /// A-744. Do: on a shard, ask the report again and again until a sweep completes (a bounded number of times: the
  /// slice's bytes over one step each, plus one). Expect: a sweep completes; the arena's measured bytes are its regions'
  /// lengths; every range's resident and written bytes are within its own bytes; the counter of sweeps moved.
  #[test]
  fn a_sweep_completes_within_its_bound_and_holds_every_range_to_its_bytes() {
    let found = crate::daemon::audit_on_shard(|state| {
      let page = state.config.page.max(1);
      let step = page.saturating_mul(page);
      let total: usize = targets(state).iter().map(Target::len).sum();
      let bound = total / step + targets(state).len() + 1;
      let mut asked = 0;
      while state.content_sweep.sweeps == 0 && asked < bound {
        report(state);
        asked += 1;
      }
      let arena_len: usize = targets(state)
        .iter()
        .filter(|target| target.range() == ARENA)
        .map(Target::len)
        .sum();
      (
        state.content_sweep.sweeps,
        state.content_sweep.done,
        arena_len,
      )
    });
    let (sweeps, done, arena_len) = found;
    assert!(sweeps > 0, "no sweep completed within its bound");
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
  }
}
