//! The words a synchronization primitive's ends share (A-767 piece C2, ported from hyper-raft's `hyper-rt`
//! `sync/cell.rs` at `5e40eb9`, as mantle vendors it): a process-wide table of cells, each a few atomics with a
//! generation, claimed by the primitive that needs one and freed by the last of its handles. A cell holds no value
//! of the primitive's type, only words: the waiting task's word, a state word and a second word, so sharing one is
//! a generational arena, never shared ownership of a value (R2: no `Arc`).
//!
//! **Bounded.** At most [`MAX_CELLS`] cells live at once; past it a claim is refused [`SyncRefusal::Capacity`].
//! Cells are allocated a segment of [`SEGMENT`] at a time, when first needed, and a segment is never freed (its
//! cells are reused), so the table's memory is the most cells ever live at once, rounded up to a segment.
//!
//! **Claiming.** A two-level bitmap of free cells: a word per segment (a bit per cell) and a word per 64 segments
//! (a bit per segment that may have a free cell). A claim finds a marked segment and clears one free bit with a
//! compare-and-swap; a free sets the bits again. No lock, and a claim reads at most every summary word once.
//!
//! **Stale handles.** A cell's generation is bumped when it is freed, and every access through a [`CellRef`]
//! checks it first: a handle that outlived its cell finds the cell closed, never another primitive's state.
//!
//! **Only tasks wait in a cell.** Upstream also parks plain threads here, naming a thread by the address of its
//! `Thread` handle on its own stack, which takes `unsafe` to read back. slates' primitives block a plain thread
//! inside the standard library's own channel instead (`super`), so a waiter word names a task alone and the module
//! holds no `unsafe` code.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering, fence};

use slates_mem::Encoded;

use super::SyncRefusal;

/// Format: cells per segment, one bit each in the segment's `u64` free word.
pub const SEGMENT: usize = 64;

/// Format: `log2(SEGMENT)`, the shift that turns a cell's index into its segment's.
const SEGMENT_SHIFT: u32 = SEGMENT.trailing_zeros();

/// Format: the mask that turns a cell's index into its bit within its segment.
const SEGMENT_MASK: usize = SEGMENT - 1;

/// Derived: the most cells at once, as many as a task word's slot field names (`Encoded::MAX_SLOT + 1`, 2^24), so a
/// cell's index packs as a task's slot does. A bound, not an allocation: segments are allocated as cells are first
/// needed, and the table of segment pointers is zero until then.
pub const MAX_CELLS: usize = 1 << 24;

/// Derived: the segments [`MAX_CELLS`] needs.
const SEGMENTS: usize = MAX_CELLS >> SEGMENT_SHIFT;

/// Derived: the summary words, one bit per segment.
const SUMMARY_WORDS: usize = SEGMENTS >> SEGMENT_SHIFT;

/// Format: "no task waits" in a cell's waiter word. No task word is all ones: its shard field would be `u16::MAX`,
/// which the registry never issues (a runtime's shards are numbered from zero, far below it).
const NO_WAITER: u64 = u64::MAX;

/// One cell: its generation (bumped at each free), the handles that keep it, the waiting task's word, and two words
/// the primitive uses as it chooses.
#[derive(Debug)]
pub(crate) struct Cell {
  generation: AtomicU32,
  handles: AtomicU32,
  waiter: AtomicU64,
  /// The primitive's state word (flags).
  pub(crate) state: AtomicU64,
  /// The primitive's second word (a count, an index).
  pub(crate) aux: AtomicU64,
}

impl Cell {
  const fn new() -> Self {
    Self {
      generation: AtomicU32::new(0),
      handles: AtomicU32::new(0),
      waiter: AtomicU64::new(NO_WAITER),
      state: AtomicU64::new(0),
      aux: AtomicU64::new(0),
    }
  }

  /// Records the task `word` as this cell's waiter, replacing any earlier one, then fences (`SeqCst`), so the
  /// caller's check of the state that follows is ordered after the registration: a publisher changes the state,
  /// fences, then reads the waiter, the store-buffering shape, so either the waiter's check sees the change or the
  /// publisher's read sees the waiter (upstream's `handoff` argument).
  pub(crate) fn register(&self, word: Encoded) {
    self.waiter.swap(word.word(), Ordering::AcqRel);
    fence(Ordering::SeqCst);
  }

  /// Wakes the waiting task, if one waits, after a fence that orders the caller's state change before the read of
  /// the waiter. Of several publishers, the one whose compare-and-swap takes the word wakes it; the rest find it
  /// empty.
  pub(crate) fn wake(&self) {
    fence(Ordering::SeqCst);
    let mut word = self.waiter.load(Ordering::Acquire);
    while word != NO_WAITER {
      match self
        .waiter
        .compare_exchange(word, NO_WAITER, Ordering::AcqRel, Ordering::Acquire)
      {
        Ok(_) => {
          crate::registry::wake(Encoded::from_word(word));
          return;
        }
        Err(seen) => word = seen,
      }
    }
  }
}

/// A segment of cells and its free bits.
#[derive(Debug)]
struct Segment {
  cells: [Cell; SEGMENT],
  free: AtomicU64,
}

/// The segments, each allocated when first needed and kept for the process.
static TABLE: [OnceLock<Box<Segment>>; SEGMENTS] = [const { OnceLock::new() }; SEGMENTS];

/// A bit per segment that may have a free cell: every segment starts free.
static NONFULL: [AtomicU64; SUMMARY_WORDS] = [const { AtomicU64::new(u64::MAX) }; SUMMARY_WORDS];

/// Cells live now.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// Cells live now (a test's and a census's reading).
pub fn live() -> usize {
  LIVE.load(Ordering::Acquire)
}

/// A handle to a cell: its index and the generation it was claimed at. `Copy`; every access checks the generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CellRef {
  index: u32,
  generation: u32,
}

/// Segment `index`, allocated on first use.
fn segment(index: usize) -> Option<&'static Segment> {
  TABLE.get(index).map(|slot| {
    &**slot.get_or_init(|| {
      Box::new(Segment {
        cells: [const { Cell::new() }; SEGMENT],
        free: AtomicU64::new(u64::MAX),
      })
    })
  })
}

/// The bit of `low` in a `u64` word (`low` below 64 by construction; any other reads as no bit).
fn bit(low: usize) -> u64 {
  u32::try_from(low)
    .ok()
    .and_then(|low| 1u64.checked_shl(low))
    .unwrap_or(0)
}

/// Claims a cell for a primitive with `handles` handles, its words zeroed and no waiter. Refused `Capacity` at
/// [`MAX_CELLS`].
pub(crate) fn claim(handles: u32) -> Result<CellRef, SyncRefusal> {
  let refused = SyncRefusal::Capacity {
    what: "synchronization cells",
    bound: MAX_CELLS,
  };
  if LIVE.fetch_add(1, Ordering::AcqRel) >= MAX_CELLS {
    LIVE.fetch_sub(1, Ordering::AcqRel);
    return Err(refused);
  }
  for (high, summary) in NONFULL.iter().enumerate() {
    let mut marked = summary.load(Ordering::Acquire);
    while marked != 0 {
      let low = usize::try_from(marked.trailing_zeros()).unwrap_or(0);
      marked &= marked.wrapping_sub(1);
      let index = high.saturating_mul(SEGMENT).saturating_add(low);
      if let Some(cell) = claim_in(index, high, low) {
        return Ok(start(cell, handles));
      }
    }
  }
  LIVE.fetch_sub(1, Ordering::AcqRel);
  Err(refused)
}

/// Claims a free cell of segment `index` (the `low` bit of summary word `high`), clearing the segment's summary bit
/// when it is full.
fn claim_in(index: usize, high: usize, low: usize) -> Option<(usize, &'static Cell)> {
  let segment = segment(index)?;
  loop {
    let free = segment.free.load(Ordering::Acquire);
    if free == 0 {
      // Full: say so, then look again, so a cell freed meanwhile is advertised again rather than lost.
      let summary = NONFULL.get(high)?;
      summary.fetch_and(!bit(low), Ordering::AcqRel);
      if segment.free.load(Ordering::Acquire) != 0 {
        summary.fetch_or(bit(low), Ordering::AcqRel);
        continue;
      }
      return None;
    }
    let taken_bit = usize::try_from(free.trailing_zeros()).unwrap_or(0);
    let taken = free & !bit(taken_bit);
    if segment
      .free
      .compare_exchange(free, taken, Ordering::AcqRel, Ordering::Acquire)
      .is_ok()
    {
      let cell = segment.cells.get(taken_bit)?;
      return Some((
        index.saturating_mul(SEGMENT).saturating_add(taken_bit),
        cell,
      ));
    }
  }
}

/// Readies a claimed cell and hands back its handle.
fn start((index, cell): (usize, &'static Cell), handles: u32) -> CellRef {
  cell.waiter.store(NO_WAITER, Ordering::Release);
  cell.state.store(0, Ordering::Release);
  cell.aux.store(0, Ordering::Release);
  cell.handles.store(handles, Ordering::Release);
  CellRef {
    index: u32::try_from(index).unwrap_or(u32::MAX),
    generation: cell.generation.load(Ordering::Acquire),
  }
}

impl CellRef {
  /// The cell, while this handle's generation is its own.
  pub(crate) fn cell(self) -> Option<&'static Cell> {
    let index = usize::try_from(self.index).ok()?;
    let segment = TABLE.get(index >> SEGMENT_SHIFT)?.get()?;
    let cell = segment.cells.get(index & SEGMENT_MASK)?;
    (cell.generation.load(Ordering::Acquire) == self.generation).then_some(cell)
  }

  /// Adds a handle (a cloned end).
  pub(crate) fn retain(self) {
    if let Some(cell) = self.cell() {
      cell.handles.fetch_add(1, Ordering::AcqRel);
    }
  }

  /// Drops a handle; the last frees the cell.
  pub(crate) fn release(self) {
    let Some(cell) = self.cell() else {
      return;
    };
    if cell.handles.fetch_sub(1, Ordering::AcqRel) != 1 {
      return;
    }
    cell.generation.fetch_add(1, Ordering::AcqRel);
    cell.waiter.store(NO_WAITER, Ordering::Release);
    let index = usize::try_from(self.index).unwrap_or(usize::MAX);
    let (segment_index, cell_bit) = (index >> SEGMENT_SHIFT, index & SEGMENT_MASK);
    if let Some(segment) = TABLE.get(segment_index).and_then(OnceLock::get) {
      segment.free.fetch_or(bit(cell_bit), Ordering::AcqRel);
    }
    if let Some(summary) = NONFULL.get(segment_index >> SEGMENT_SHIFT) {
      summary.fetch_or(bit(segment_index & SEGMENT_MASK), Ordering::AcqRel);
    }
    LIVE.fetch_sub(1, Ordering::AcqRel);
  }

  /// Records `word` as the task to wake ([`Cell::register`]).
  pub(crate) fn register(self, word: Encoded) {
    if let Some(cell) = self.cell() {
      cell.register(word);
    }
  }

  /// Wakes the waiting task, if one waits ([`Cell::wake`]).
  pub(crate) fn wake(self) {
    if let Some(cell) = self.cell() {
      cell.wake();
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Do: claim a cell with two handles, release them one by one, claim again. Expect: the cell lives until its last
  /// handle goes, and the stale handle never reads the reused slot.
  #[test]
  fn a_cell_is_freed_by_its_last_handle_and_a_stale_handle_finds_it_closed() {
    let cell = claim(2).unwrap();
    assert!(cell.cell().is_some());
    cell.release();
    assert!(cell.cell().is_some(), "one handle remains");
    cell.release();
    assert!(
      cell.cell().is_none(),
      "freed: the stale handle finds it closed"
    );
    let again = claim(1).unwrap();
    assert!(
      cell.cell().is_none(),
      "a reuse of the slot is not the stale handle's"
    );
    again.release();
  }

  /// Do: claim 2,000 cells from four threads at once. Expect: every claim took its own cell.
  #[test]
  fn claims_never_share_a_cell_across_threads() {
    let mut claimed: Vec<CellRef> = std::thread::scope(|scope| {
      let handles: Vec<_> = (0..4)
        .map(|_| scope.spawn(|| (0..500).map(|_| claim(1).unwrap()).collect::<Vec<_>>()))
        .collect();
      handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect()
    });
    claimed.sort_by_key(|cell| cell.index);
    claimed.dedup_by_key(|cell| cell.index);
    assert_eq!(claimed.len(), 2_000, "every claim took its own cell");
    for cell in claimed {
      cell.release();
    }
  }
}
