//! AC-0.4: slab and buddy allocation never call the system allocator after start. A counting
//! global allocator proves zero calls across a hot loop of inserts, removes, chunk allocations and
//! frees once the slab's segments and the arena's regions exist.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_mem::slab::Slab;

struct Counting;

static CALLS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every method forwards to the system allocator unchanged; only a counter is added.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    CALLS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: forwarded with the same layout.
    unsafe { System.alloc(layout) }
  }
  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    // SAFETY: forwarded with the pointer and layout the caller received from `alloc`.
    unsafe { System.dealloc(ptr, layout) }
  }
  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    CALLS.fetch_add(1, Ordering::Relaxed);
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

  let before = CALLS.load(Ordering::Relaxed);
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
  let after = CALLS.load(Ordering::Relaxed);
  assert_eq!(
    after - before,
    0,
    "the hot path reached the system allocator"
  );
}
