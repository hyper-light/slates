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

use slates_db::live::Shipper;
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
/// Shape: the owner's credit and retained-entry count, the daemon's own (`live_ship`'s `CREDIT` and `RETAIN`); the
/// backlog lies past them, kept by bytes as an install's size keeps it in the daemon (`Shipper::retain_up_to`).
const CREDIT: u64 = 2;
/// See [`CREDIT`].
const RETAIN: usize = 4;
/// Shape: the volume's number on its owner.
const VOLUME: u64 = 7;

/// The allocator calls the owner makes folding one reply of a holder that still lacks `backlog` entries after it.
fn lagging_reply_allocations(backlog: usize) -> usize {
  let (owner, healthy, lagging) = (HostId(1), HostId(2), HostId(3));
  let mut shipper = Shipper::new(
    (owner, ObjectId::new(owner, VOLUME), HostEpoch(1)),
    Quorum { f: 1 },
    &[healthy, lagging],
    None,
    (CREDIT, RETAIN),
  );
  shipper.retain_up_to(usize::MAX);
  let mut heads = Vec::new();
  for _ in 0..backlog + 2 {
    let entry = shipper.append(vec![FILL; PAYLOAD]).unwrap();
    heads.push(entry.head());
    assert_eq!(shipper.fold_reply(healthy, Ok(Some(entry.head()))), None);
  }
  // The lagging holder holds the first entry, then acknowledges the second: `backlog` entries are still after it.
  assert_eq!(shipper.fold_reply(lagging, Ok(Some(heads[0]))), None);
  let (calls, fenced) = counted(|| shipper.fold_reply(lagging, Ok(Some(heads[1]))));
  assert_eq!(fenced, None, "nothing fenced this owner");
  assert_eq!(
    shipper.held_by(lagging),
    Some(heads[1]),
    "the reply was folded"
  );
  calls
}

#[test]
fn a_lagging_holders_reply_costs_its_owner_the_same_whatever_it_lacks() {
  let small = lagging_reply_allocations(SMALL);
  let large = lagging_reply_allocations(LARGE);
  assert_eq!(
    small, large,
    "folding one reply allocated {small} times {SMALL} entries behind and {large} times {LARGE} behind"
  );
}
