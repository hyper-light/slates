//! The heap bytes a repository window's record keeps (goal 93, main 01a20182's (3); A-877). A shard keeps each window a
//! repository's content store holds as a record in its window storage (`vcs_store::arena_windows`), beside the window's
//! bytes in the arena; w-perf's push of Mercurial's history (57,035 changesets, 2026-11-23) left 22,107,816 bytes of
//! those records live for 114,200 windows, 194 a window. This binary's global allocator counts each thread's live heap
//! bytes, so the test holds the records to what they keep by the bytes its own thread allocated and kept, whatever any
//! other thread does. The count lives in this binary alone: an allocator installed under the library's tests changed
//! what they measure of their own timing (w-perf, 2026-11-23: `memory_gauges`' status test then carried a submit's
//! increment in its entry and held none).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::print_stdout
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::content::ChunkStore;

/// Shape: the test's page, and the arena's granule.
const PAGE: usize = 16 * 1024;
/// Shape: the chunk records the test's chunk store may hold, far past what it keeps.
const CHUNKS: usize = 1 << 12;
/// Shape: the pages of the test's one region, room for every window it puts.
const PAGES: usize = 512;
/// Shape: arena-15's mean window (CI 45224166925: 131,425,429 bytes over 114,200 windows).
const MEAN_WINDOW: usize = 1_151;
/// Shape: the windows the test counts.
const WINDOWS: u64 = 4_000;

/// Derived: the bytes a kept window's record may take, its share of the storage's maps included. An entry is its key,
/// the window's handle (8 bytes), and its record, what a window alone names: its identity (32), its length and its
/// slot's offset (4 each) and its slab's key, a region and an offset (16 with alignment), 64 bytes in all. A B-tree
/// keeps every leaf but the root at least 5 of its 11 entries full (Rust's `MIN_LEN`), and handles are put in order,
/// which leaves its leaves about that full: 11/5 of an entry a window. Its inner nodes add about a sixth more; the
/// slabs' records are shared among each slab's windows, and the windows changed since the last publication (a set of
/// handles the next publication clears) take a handle's share of a set. So at most three entries a window.
const RECORD_BYTES_A_WINDOW: i64 = 3 * (8 + 32 + 4 + 4 + 16);

thread_local! {
  /// The heap bytes this thread allocated less those it freed.
  static LIVE: Cell<i64> = const { Cell::new(0) };
}

/// Adds `bytes` to the calling thread's count; a thread being torn down counts nothing.
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

/// The bytes of a window numbered `n` of `len` bytes, distinct for every `n`.
fn window_bytes(n: u64, len: usize) -> Vec<u8> {
  let mut bytes: Vec<u8> = (0..len)
    .map(|at| u8::try_from((u64::try_from(at).unwrap() * 31 + n * 7) % 251).unwrap())
    .collect();
  let tag = n.to_le_bytes();
  let tagged = tag.len().min(len);
  bytes[..tagged].copy_from_slice(&tag[..tagged]);
  bytes
}

/// Goal 93 (the 57k push's 194 bytes a window's record). Do: lend a shard chunk store of one region to a repository's
/// window storage, put and name a first window (the chunk store makes its table of chunk records at its first chunk,
/// 558,784 bytes for 4,096 records, which a shard's store holds from its start), then put and name [`WINDOWS`] windows of
/// arena-15's mean length and count the heap bytes the putting thread kept for those. Expect: at most
/// [`RECORD_BYTES_A_WINDOW`] a window. Failing first: each record repeated its window's whole placement (its extent,
/// region, offset, block length, bytes used and birth epoch), which the slab it lies in holds once: 250 bytes a window
/// (1,002,952 for 4,000; the compact record 150, 602,816 for 4,000; w-perf, 2026-11-23).
#[test]
fn a_kept_windows_record_keeps_what_the_window_alone_names() {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * PAGES, PAGE, false).unwrap())
    .unwrap();
  let mut chunks = ChunkStore::new(arena, PAGE, CHUNKS);
  let mut windows = slates_server::chunk_window_storage(PAGE);
  assert!(windows.lend_in(&mut chunks));
  let first = windows.put(&window_bytes(0, MEAN_WINDOW)).unwrap();
  windows.named(first, &slates_vcs::ContentId([0; 32]));
  let bytes: Vec<Vec<u8>> = (1..=WINDOWS)
    .map(|n| window_bytes(n, MEAN_WINDOW))
    .collect();
  let before = live_bytes();
  for (n, window) in bytes.iter().enumerate() {
    let stored = windows.put(window).unwrap();
    windows.named(
      stored,
      &slates_vcs::ContentId([u8::try_from(n % 251).unwrap(); 32]),
    );
  }
  let kept = live_bytes() - before;
  let a_window = kept / i64::try_from(WINDOWS).unwrap();
  println!("{WINDOWS} windows' records: {kept} bytes, {a_window} a window");
  assert!(
    a_window <= RECORD_BYTES_A_WINDOW,
    "{a_window} bytes a window's record, past {RECORD_BYTES_A_WINDOW}"
  );
  for (n, window) in bytes.iter().enumerate().step_by(997) {
    let handle = slates_vcs::StoredWindow(u64::try_from(n).unwrap() + 1);
    assert_eq!(
      windows.read(handle).as_deref(),
      Some(&window[..]),
      "window {n} reads back"
    );
  }
  assert!(windows.hand_back(&mut chunks));
}
