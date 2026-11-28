//! The frees made on each thread of the crate's test binary, so a test counts its own (A-920 unit 0; the engine's
//! retired-member tests since unit 1). Test-only: the binary's one global allocator, passing every call on to the
//! system's unchanged.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;

std::thread_local! {
  static FREES: Cell<u64> = const { Cell::new(0) };
}

/// The frees the calling thread has made so far.
pub(crate) fn frees() -> u64 {
  FREES.try_with(Cell::get).unwrap_or(0)
}

// SAFETY: every call is passed on to the system allocator unchanged; only a thread-local count is kept beside it.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    // SAFETY: the caller's contract, passed on.
    unsafe { System.alloc(layout) }
  }
  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    let _ = FREES.try_with(|frees| frees.set(frees.get() + 1));
    // SAFETY: the caller's contract, passed on.
    unsafe { System.dealloc(ptr, layout) }
  }
}

#[global_allocator]
static COUNTING: Counting = Counting;
