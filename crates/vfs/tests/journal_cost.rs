//! The journal's append is a bounded unit (§4.3, §4.5; GAPS "the journal's ring grows by copying", 2026-10-24): no
//! append allocates more, or allocates larger, when a lagging reader has grown the ring long than when it is short. A
//! ring that doubles copies every retained record in the one append that needs room: measured with a journal started
//! at 16 MiB, one append copied 2.75 MiB (16,384 records of 176 bytes), a 302 µs unit in `image_bench` against a
//! daemon's slice of 23.7 µs. The allocator calls are counted per thread, so the verdict is exact and independent of
//! the host's load (the pattern of `image_cost.rs`).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use slates_mem::budget::MetadataBudget;
use slates_vfs::ids::{Epoch, InodeNo};
use slates_vfs::journal::{Op, OpLog};

struct Counting;
thread_local! {
  // Const initialization and no destructor: observing an allocation never allocates recursively.
  static CALLS: Cell<usize> = const { Cell::new(0) };
  static LARGEST: Cell<usize> = const { Cell::new(0) };
}

/// Notes one allocation or reallocation of `size` bytes.
fn note_allocation(size: usize) {
  CALLS.set(CALLS.get() + 1);
  LARGEST.set(LARGEST.get().max(size));
}

// SAFETY: forwards the allocator contract unchanged; the counters observe calls only.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    note_allocation(layout.size());
    // SAFETY: forwarded unchanged.
    unsafe { System.alloc(layout) }
  }
  unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
    // SAFETY: forwarded unchanged.
    unsafe { System.dealloc(pointer, layout) }
  }
  unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
    note_allocation(size);
    // SAFETY: forwarded unchanged.
    unsafe { System.realloc(pointer, layout, size) }
  }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// Shape: the journal's starting budget, the daemon's floor of one page (`journal_floor_bytes`, §4.5, A-157).
const JOURNAL_BYTES: usize = 4096;
/// Shape: the records a short history appends, a few pages' worth.
const SHORT: usize = 256;
/// Shape: the records a long history appends, 64 times the short one: a reader lagging this far grows a doubling
/// ring to about 2.75 MiB.
const LONG: usize = 64 * SHORT;
/// Shape: the reader that never reads, so every record stays owed and the ring grows with the history.
const LAGGING_READER: u64 = 1;
/// Shape: a lagging reader's patience, past any history here, so no record outlasts it.
const PATIENCE_NS: u64 = u64::MAX;

/// The largest single allocation any one append of a `records`-long history made, its path included.
fn largest_append_allocation(records: usize) -> usize {
  let mut ledger = MetadataBudget::new(u64::MAX);
  let mut log = OpLog::new(JOURNAL_BYTES);
  log.open_reader(LAGGING_READER, 0, PATIENCE_NS);
  let mut largest = 0;
  for record in 0..records {
    let path = format!("dir/file-{record}");
    let at = u64::try_from(record).unwrap();
    LARGEST.set(0);
    CALLS.set(0);
    log.append(
      &mut ledger,
      Op::Create,
      path,
      Some(InodeNo(at)),
      Epoch(0),
      at,
      0,
    );
    largest = largest.max(LARGEST.get());
  }
  assert_eq!(log.len(), records, "the lagging reader kept every record");
  assert_eq!(log.dropped(), 0, "nothing left while it was owed");
  largest
}

/// §4.3 bounded work over §4.5's journal. Do: append a short history and one 64 times longer, each held whole by a
/// reader that never reads. Expect: the largest allocation one append makes in the long history is no larger than in
/// the short one, so no append's work follows the ring's length.
#[test]
fn no_append_allocates_larger_in_a_long_journal_than_in_a_short_one() {
  let short = largest_append_allocation(SHORT);
  let long = largest_append_allocation(LONG);
  eprintln!(
    "largest allocation of one append: {short} bytes over {SHORT} records, {long} over {LONG}"
  );
  assert!(
    long <= short,
    "an append in a {LONG}-record journal allocated {long} bytes at once, against {short} in a {SHORT}-record one"
  );
}
