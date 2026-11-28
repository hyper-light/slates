//! The large frees made on each thread of the server's test binary, so a test counts its own (A-920, main's ruling of
//! 2026-11-28 01:38: a landing's prepare frees no large buffer on its shard). Test-only: the binary's one global
//! allocator, passing every call on to the system's unchanged.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// Shape: the size from which a free counts as large, 64 KiB: the smallest of the frees a landing's increment buffer
/// made as it doubled (64 KiB through 1 MiB, then its 2 MiB buffer; the wide move import, debug, 2026-11-28).
pub(crate) const LARGE_BYTES: usize = 64 * 1024;

struct Counting;

std::thread_local! {
  static LARGE: Cell<u64> = const { Cell::new(0) };
}

/// The frees of [`LARGE_BYTES`] or more the calling thread has made so far.
pub(crate) fn large_frees() -> u64 {
  LARGE.try_with(Cell::get).unwrap_or(0)
}

// SAFETY: every call is passed on to the system allocator unchanged; only a thread-local count is kept beside it.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    // SAFETY: the caller's contract, passed on.
    unsafe { System.alloc(layout) }
  }
  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    if layout.size() >= LARGE_BYTES {
      let _ = LARGE.try_with(|large| large.set(large.get() + 1));
    }
    // SAFETY: the caller's contract, passed on.
    unsafe { System.dealloc(ptr, layout) }
  }
}

#[global_allocator]
static COUNTING: Counting = Counting;
