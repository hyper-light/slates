//! AC-0.4, §4.3: reserving a bounded wheel must not issue one allocation per wheel bucket.
//! Count the actual allocator calls, then fill, renew and expire timers without allocating.
//! One test in this binary keeps other tests out of the measurement.

#![allow(clippy::unwrap_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use slates_rt::timer::Wheel;

struct Counting;
static CALLS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards the allocator contract unchanged; the counter observes calls only.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    CALLS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: forwarded unchanged.
    unsafe { System.alloc(layout) }
  }
  unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
    // SAFETY: forwarded unchanged.
    unsafe { System.dealloc(pointer, layout) }
  }
  unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
    CALLS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: forwarded unchanged.
    unsafe { System.realloc(pointer, layout, size) }
  }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// AC-0.4: do reserve CI's reported capacity, fill a smaller non-power-of-two wheel,
/// renew every timer, refuse stale cancellation and capacity overflow, then expire it.
/// Expect bounded boot allocations and no allocator calls while using the reserved slots.
#[test]
fn reservation_cost_is_independent_of_capacity_and_renewals_allocate_nothing() {
  let before = CALLS.load(Ordering::Relaxed);
  let small = Wheel::new(1, 65, 0);
  let small_calls = CALLS.load(Ordering::Relaxed) - before;
  let before = CALLS.load(Ordering::Relaxed);
  let large = Wheel::new(1, 1_617_130, 0);
  let large_calls = CALLS.load(Ordering::Relaxed) - before;
  eprintln!("wheel allocations: 65 timers = {small_calls}; 1617130 timers = {large_calls}");
  assert_eq!(
    large_calls, small_calls,
    "reservation must not scale with possible leases"
  );
  drop((small, large));

  let capacity = 129;
  let mut wheel = Wheel::new(1, capacity, 0);
  let mut handles = Vec::with_capacity(capacity);
  let mut fired = Vec::with_capacity(capacity);
  let before = CALLS.load(Ordering::Relaxed);
  for word in 0..capacity {
    handles.push(wheel.insert(10, u64::try_from(word).unwrap()).unwrap());
  }
  assert!(wheel.insert(10, u64::MAX).is_err());
  for (word, handle) in handles.iter_mut().enumerate() {
    let old = *handle;
    wheel.cancel(old).unwrap();
    *handle = wheel.insert(20, u64::try_from(word).unwrap()).unwrap();
    assert!(wheel.cancel(old).is_err());
  }
  wheel.advance(10, &mut fired);
  assert!(fired.is_empty());
  wheel.advance(20, &mut fired);
  let calls = CALLS.load(Ordering::Relaxed) - before;
  assert_eq!(
    calls, 0,
    "insertion, renewal and expiry stay in reserved storage"
  );
  fired.sort_unstable();
  assert_eq!(
    fired,
    (0..u64::try_from(capacity).unwrap()).collect::<Vec<_>>()
  );
}
