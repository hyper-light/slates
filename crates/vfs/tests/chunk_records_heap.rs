//! The heap a chunk store's table of chunk records takes as it grows (goal 93, main 01a20182's order of 2026-11-23 on
//! w-perf's finding). The table is a segmented slab that grows a segment at a time to its derived bound (`max_chunks`,
//! a shard's reserve over its granule), so a store pays for the records it holds, a segment at a time. This binary's
//! global allocator counts each thread's live heap bytes, so the test holds the table to what its first chunk makes by
//! the bytes its own thread allocated and kept.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::print_stdout
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use slates_mem::arena::{ChunkArena, Owner};
use slates_mem::region::Region;
use slates_vfs::content::ChunkStore;
use slates_vfs::ids::Epoch;

/// Shape: the test's page and the arena's granule, macOS arm64's: the page a store's segment is sized by.
const PAGE: usize = 16 * 1024;
/// Shape: the pages of the test's one region, room for its chunks.
const PAGES: usize = 64;
/// Shape: a shard's bound on its chunk records, past a page's worth, as every shard's is (`max_chunks`: a 1 GiB reserve
/// over a 16 KiB granule is 65,536).
const MAX_CHUNKS: usize = 1 << 16;
/// Shape: the bytes of the test's one small file.
const FILE: usize = 100;

/// Derived: the heap a chunk store's first chunk may make. Its table grows a segment at a time, and a segment is the
/// records a page holds, so the first chunk makes one page of records; the segments' own table and the slab's free list
/// take a few pointers more. So at most two pages.
const FIRST_CHUNK_BYTES: i64 = 2 * PAGE as i64;

thread_local! {
  /// The heap bytes this thread allocated less those it freed.
  static LIVE: Cell<i64> = const { Cell::new(0) };
}

/// Adds `bytes` with `sign` to the calling thread's count; a thread being torn down counts nothing.
fn count(bytes: usize, sign: i64) {
  let bytes = i64::try_from(bytes).unwrap_or(i64::MAX);
  let _ = LIVE.try_with(|live| live.set(live.get().wrapping_add(sign.wrapping_mul(bytes))));
}

/// The calling thread's live heap bytes so far.
fn live_bytes() -> i64 {
  LIVE.with(Cell::get)
}

/// The system's allocator, counting each thread's live bytes.
struct Counting;

// SAFETY: every call is forwarded unchanged to the system allocator; counting touches only a thread-local cell, which
// allocates nothing (a const initializer and no destructor).
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    count(layout.size(), 1);
    // SAFETY: forwarded unchanged.
    unsafe { System.alloc(layout) }
  }
  unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    count(layout.size(), 1);
    // SAFETY: forwarded unchanged.
    unsafe { System.alloc_zeroed(layout) }
  }
  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    count(layout.size(), -1);
    // SAFETY: forwarded unchanged.
    unsafe { System.dealloc(ptr, layout) }
  }
  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    count(layout.size(), -1);
    count(new_size, 1);
    // SAFETY: forwarded unchanged.
    unsafe { System.realloc(ptr, layout, new_size) }
  }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Goal 93 (w-perf's finding of 2026-11-23: a chunk store's first chunk made 558,784 bytes of records at a 4 KiB page).
/// Do: make a chunk store over one region at a 16 KiB page with a shard's bound on its records, then open, write and
/// seal one file of 100 bytes, counting the heap the thread kept for it. Expect: at most [`FIRST_CHUNK_BYTES`]. Failing
/// first: the table's segment was `min(max_chunks, page)` records, the page's byte count taken as a record count, so the
/// first chunk made 16,384 records at once.
#[test]
fn a_chunk_stores_first_chunk_makes_a_page_of_records_not_a_page_count_of_them() {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * PAGES, PAGE, false).unwrap())
    .unwrap();
  let mut store = ChunkStore::new(arena, PAGE, MAX_CHUNKS);
  let before = live_bytes();
  let mut open = store
    .open(0, FILE, Epoch(0), (false, Owner::Volume))
    .unwrap();
  store.write_open(&mut open, 0, &[7; FILE]).unwrap();
  let sealed = store.seal(open, None).unwrap();
  let kept = live_bytes() - before;
  println!("the first chunk kept {kept} bytes of heap");
  assert!(sealed.is_some(), "the file is sealed into a chunk");
  assert!(
    kept <= FIRST_CHUNK_BYTES,
    "the first chunk kept {kept} bytes, past {FIRST_CHUNK_BYTES}"
  );
}
