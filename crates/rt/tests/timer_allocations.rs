//! AC-0.4, §4.3: reserving a bounded wheel must not issue one allocation per wheel bucket.
//! Count the actual allocator calls, then fill, renew and expire timers without allocating.
//! Each thread counts its own allocator calls; libtest can allocate on its reporting thread
//! even when only one test runs. A controlled foreign allocation checks the attribution.

#![allow(clippy::unwrap_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use slates_rt::timer::Wheel;

struct Counting;
thread_local! {
  // Const initialization and no destructor: observing an allocation never allocates recursively
  // or accesses a counter that a thread-local destructor has already destroyed.
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

/// AC-0.4: an allocation measurement belongs to the thread exercising the wheel. Drive
/// an allocation on another thread during the interval; expect it to be excluded while
/// that thread's own measurement still observes it. Libtest's reporting thread can do this
/// even when the test binary contains only one test.
#[test]
fn an_allocation_measurement_excludes_another_threads_work() {
  let (start, started) = std::sync::mpsc::sync_channel(0);
  let (done, completed) = std::sync::mpsc::sync_channel(0);
  let worker = std::thread::spawn(move || {
    // The first handshake warms the channel's per-thread waiting state; the second is measured.
    for _ in 0..2 {
      started.recv().unwrap();
      let before = CALLS.get();
      drop(std::hint::black_box(Box::new(std::hint::black_box(42u64))));
      done.send(CALLS.get() - before).unwrap();
    }
  });
  start.send(()).unwrap();
  completed.recv().unwrap();
  let before = CALLS.get();
  start.send(()).unwrap();
  let foreign_calls = completed.recv().unwrap();
  let local_calls = CALLS.get() - before;
  worker.join().unwrap();
  assert!(foreign_calls > 0, "the worker really allocated");
  assert_eq!(
    local_calls, 0,
    "another thread cannot change this measurement"
  );
}

/// AC-0.4: do reserve CI's reported capacity, fill a smaller non-power-of-two wheel,
/// renew every timer, refuse stale cancellation and capacity overflow, then expire it.
/// Expect bounded boot allocations and no allocator calls while using the reserved slots.
#[test]
fn reservation_cost_is_independent_of_capacity_and_renewals_allocate_nothing() {
  let before = CALLS.get();
  let small = Wheel::new(1, 65, 0);
  let small_calls = CALLS.get() - before;
  let before = CALLS.get();
  let large = Wheel::new(1, 1_617_130, 0);
  let large_calls = CALLS.get() - before;
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
  let before = CALLS.get();
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
  let calls = CALLS.get() - before;
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
