//! A lagging holder's answer costs its owner the same whatever the holder still lacks (live shipping, A-200): folding one
//! reply must allocate the same behind a small backlog as behind a large one. The allocator calls are counted per
//! thread, so the verdict is exact and independent of the host's load. Measured before this test (2026-11-09,
//! diag-live-durability-73): every reply of a holder resumed under a write stream cloned each retained entry it still
//! lacked, for the owner to drop (1.4 ms a reply 1,000 entries behind, 7.1 ms 5,000 behind, 8 KiB entries, release, an
//! M-series Mac). The owner shard spent itself on that holder's replies, its writes fell to about 210 a second, and the
//! holder never came within 50 entries of the head in 6 of 24 x86 runs.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use slates_db::live::{Bounds, Shipper};
use slates_db::register::{HostEpoch, HostId, ObjectId, Quorum};

struct Counting;
thread_local! {
  // Const initialization and no destructor: observing an allocation never allocates recursively.
  static CALLS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards the allocator contract unchanged; the counter observes calls only.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    CALLS.set(CALLS.get() + 1);
    // SAFETY: forwarded unchanged.
    unsafe { System.alloc(layout) }
  }
  unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
    // SAFETY: forwarded unchanged.
    unsafe { System.dealloc(pointer, layout) }
  }
  unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
    CALLS.set(CALLS.get() + 1);
    // SAFETY: forwarded unchanged.
    unsafe { System.realloc(pointer, layout, size) }
  }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The allocator calls `work` makes on this thread, and its result.
fn counted<T>(work: impl FnOnce() -> T) -> (usize, T) {
  let before = CALLS.get();
  let out = work();
  (CALLS.get() - before, out)
}

/// Shape: the small backlog, in entries.
const SMALL: usize = 16;
/// Shape: the large backlog: sixty-four times the small one, so a cost that follows the backlog is a thousand
/// allocations apart, not one.
const LARGE: usize = 1024;
/// Shape: one entry's payload; a count does not depend on its size.
const PAYLOAD: usize = 64;
/// Shape: the bytes each payload holds.
const FILL: u8 = 0;
/// Derived: the bytes the owner retains for laggards, and its credit: every entry the large backlog appends (`LARGE`, the
/// two the lagging holder acknowledges, and the one that measures an entry), each at its wire size, so the whole backlog
/// lies inside the retained tail and is shipped as entries, never an install (`Bounds::of_whole`, the daemon's
/// derivation, with the install as large as the backlog).
fn retained_bytes(entry_wire_bytes: usize) -> usize {
  (LARGE + 3) * entry_wire_bytes
}

/// Shape: the volume's number on its owner.
const VOLUME: u64 = 7;

/// The allocator calls the owner makes folding one reply of a holder that still lacks `backlog` entries after it, and
/// asking whether its retained tail reaches that holder (`Shipper::retains_after`, which each write's sends ask).
fn lagging_reply_allocations(backlog: usize) -> (usize, usize) {
  let (owner, healthy, lagging) = (HostId(1), HostId(2), HostId(3));
  // One delta is admitted with no credit while none is in flight: it measures an entry's wire size for the bounds.
  let mut shipper = Shipper::new(
    (owner, ObjectId::new(owner, VOLUME), HostEpoch(1)),
    Quorum { f: 1 },
    &[healthy, lagging],
    None,
    Bounds::of_whole(0),
  );
  let first = shipper.append(vec![FILL; PAYLOAD]).unwrap();
  shipper.set_bounds(Bounds::of_whole(retained_bytes(first.wire_bytes())));
  assert_eq!(shipper.fold_reply(healthy, Ok(Some(first.head()))), None);
  let mut heads = Vec::new();
  for _ in 0..backlog + 2 {
    let entry = shipper.append(vec![FILL; PAYLOAD]).unwrap();
    heads.push(entry.head());
    assert_eq!(shipper.fold_reply(healthy, Ok(Some(entry.head()))), None);
  }
  // The lagging holder holds the first entry, then acknowledges the second: `backlog` entries are still after it.
  assert_eq!(shipper.fold_reply(lagging, Ok(Some(heads[0]))), None);
  let (folded, fenced) = counted(|| shipper.fold_reply(lagging, Ok(Some(heads[1]))));
  assert_eq!(fenced, None, "nothing fenced this owner");
  assert_eq!(
    shipper.held_by(lagging),
    Some(heads[1]),
    "the reply was folded"
  );
  let (asked, retained) = counted(|| shipper.retains_after(heads[1].position));
  assert!(retained, "the backlog lies inside the retained tail");
  (folded, asked)
}

/// A-200: do fold one reply of a holder 16 entries behind and one 1,024 behind, and ask each time whether the retained
/// tail reaches it. Expect the same allocator calls for both at either backlog. Before, each reply cloned every entry the
/// holder lacked (21 calls at 16 behind, 1,035 at 1,024), and `retains_after` cloned them again on every write's sends.
#[test]
fn a_lagging_holders_reply_costs_its_owner_the_same_whatever_it_lacks() {
  let (small_fold, small_ask) = lagging_reply_allocations(SMALL);
  let (large_fold, large_ask) = lagging_reply_allocations(LARGE);
  assert_eq!(
    small_fold, large_fold,
    "folding one reply allocated {small_fold} times {SMALL} entries behind and {large_fold} times {LARGE} behind"
  );
  assert_eq!(
    small_ask, large_ask,
    "asking whether the tail reaches the holder allocated {small_ask} times {SMALL} behind and {large_ask} times {LARGE} behind"
  );
}
