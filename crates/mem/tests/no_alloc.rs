//! AC-0.4: slab and buddy allocation never call the system allocator after start. A counting
//! global allocator proves zero calls across a hot loop of inserts, removes, chunk allocations and
//! frees once the slab's segments and the arena's regions exist. Counts belong to the
//! allocating thread, so libtest's concurrent reporting cannot contaminate the measurement.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_mem::slab::Slab;

struct Counting;

thread_local! {
  // Const initialization and no destructor keep the allocator counter free of allocations
  // and available during other thread-local destructors.
  static CALLS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: every method forwards to the system allocator unchanged; only a counter is added.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    CALLS.set(CALLS.get() + 1);
    // SAFETY: forwarded with the same layout.
    unsafe { System.alloc(layout) }
  }
  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    // SAFETY: forwarded with the pointer and layout the caller received from `alloc`.
    unsafe { System.dealloc(ptr, layout) }
  }
  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    CALLS.set(CALLS.get() + 1);
    // SAFETY: forwarded unchanged.
    unsafe { System.realloc(ptr, layout, new_size) }
  }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn the_hot_path_makes_zero_system_allocations() {
  let page = usize::try_from(slates_machine::facts::Facts::query().page.base).unwrap();
  let mut slab: Slab<[u8; 64]> = Slab::new(256, 1024);
  slab.reserve_segments(4);
  let mut arena = ChunkArena::new(page);
  arena
    .add_region(Region::map(page * 64, page, false).unwrap())
    .unwrap();
  let mut handles = Vec::with_capacity(1024);
  let mut extents = Vec::with_capacity(64);

  let before = CALLS.get();
  for round in 0..8 {
    for i in 0..1024usize {
      handles.push(slab.insert([u8::try_from(i % 256).unwrap(); 64]).unwrap());
    }
    for h in handles.drain(..) {
      slab.remove(h).unwrap();
    }
    for i in 0..12usize {
      extents.push(arena.alloc(page * (1 + (i + round) % 3)).unwrap());
    }
    for e in extents.drain(..) {
      arena.free(e).unwrap();
    }
  }
  let after = CALLS.get();
  assert_eq!(
    after - before,
    0,
    "the hot path reached the system allocator"
  );
}

/// AC-0.4: an allocation measurement belongs to the thread exercising the allocator. Drive
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
