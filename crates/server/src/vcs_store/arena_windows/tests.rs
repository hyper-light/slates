//! A-727 slice a2's tests: the repository content store's windows kept as chunks of the shard's chunk store, lent for a
//! step: read back lent in place and freed with their last holder inside a lend; refused typed outside one; a lend inside a
//! lend taking nothing; and the shard's chunk store handed back holding the windows' chunks.

use std::borrow::Cow;

use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vcs::{ContentRefused, ContentStore};
use slates_vfs::content::ChunkStore;

use super::ChunkWindows;

/// Shape: the tests' page, and the arena's granule.
const PAGE: usize = 16 * 1024;
/// Shape: the chunk records the tests' chunk store may hold, far past what they keep.
const CHUNKS: usize = 1 << 12;
/// Shape: the content store's window, one page: an object of five pages is five windows.
const WINDOW: usize = PAGE;
/// Shape: a content store's budget, far past what the tests keep.
const BUDGET: u64 = 1 << 30;

/// A shard chunk store over one region of `pages` pages.
fn shard_chunks(pages: usize) -> ChunkStore {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * pages, PAGE, false).unwrap())
    .unwrap();
  ChunkStore::new(arena, PAGE, CHUNKS)
}

/// A content store over windows kept as chunks of a lent chunk store.
fn content() -> ContentStore {
  ContentStore::with_storage(BUDGET, WINDOW, Box::new(ChunkWindows::new(PAGE))).unwrap()
}

/// A-727 slice a2 (main's ruling (ii)). Do: lend a shard chunk store of 64 pages to a content store, put an object of
/// five windows and an empty one, read both back whole and window by window, hand the chunk store back, then lend it
/// again and release the long object. Expect: every window read lent in place from the chunks (`Cow::Borrowed`: a
/// chunk in the clear, nothing copied or allocated; Ada's ruling of 2026-11-20) and equal to
/// the bytes put; the chunk store handed back holding five chunks, and none once the long object is released in a
/// second lend; the empty object read back empty with no chunk throughout.
#[test]
fn windows_lent_the_shards_chunk_store_read_back_in_place_and_free_their_chunks() {
  let mut chunks = shard_chunks(64);
  let mut store = content();
  assert!(store.lend_in(&mut chunks));
  let long: Vec<u8> = (0..5 * WINDOW)
    .map(|at| u8::try_from(at % 251).unwrap())
    .collect();
  let root = store.put_object(&long).unwrap();
  let empty = store.put_object(&[]).unwrap();
  let len = u64::try_from(long.len()).unwrap();
  assert_eq!(store.object_bytes(&root, len), Some(long.clone()));
  assert!(
    store
      .windows(&root)
      .all(|window| matches!(window, Cow::Borrowed(_)))
  );
  assert!(store.hand_back(&mut chunks));
  assert_eq!(
    chunks.chunks(),
    5,
    "the shard's chunk store holds the windows' chunks"
  );
  assert_eq!(store.object_bytes(&empty, 0), Some(Vec::new()));
  assert!(store.lend_in(&mut chunks));
  store.release(&root).unwrap();
  assert!(store.hand_back(&mut chunks));
  assert_eq!(chunks.chunks(), 0, "the released windows' chunks are freed");
}

/// A-727 slice a2 (the lend's rules). Do: put and read on a content store no chunk store is lent to; lend one, then lend
/// a second inside the first; hand back twice. Expect: outside a lend a put refused `ContentRefused::Storage` and a
/// stored window read as none; inside, the window read; the second lend taking nothing and the first hand-back alone
/// giving the store back, so the second finds nothing to hand back.
#[test]
fn a_window_storage_refuses_typed_outside_a_lend_and_a_lend_inside_a_lend_takes_nothing() {
  let mut chunks = shard_chunks(8);
  let mut other = shard_chunks(8);
  let mut store = content();
  assert_eq!(store.put(b"too early"), Err(ContentRefused::Storage));
  assert!(store.lend_in(&mut chunks));
  let kept = store.put(b"kept").unwrap();
  assert!(
    !store.lend_in(&mut other),
    "a lend inside a lend takes nothing"
  );
  assert!(store.hand_back(&mut chunks));
  assert_eq!(store.bytes_of(&kept), None, "no read outside a lend");
  assert!(!store.hand_back(&mut chunks), "nothing more to hand back");
  assert!(store.lend_in(&mut chunks));
  assert_eq!(store.bytes_of(&kept).as_deref(), Some(&b"kept"[..]));
  assert!(store.hand_back(&mut chunks));
}

/// Ada's ruling of 2026-11-20 (a read lends a window, it allocates nothing; A-645 P3's referenced files read through
/// `ContentStore::run_at`). Do: lend a shard chunk store to a content store over these windows, put an object of five
/// windows and a one-window object, then read each from every offset a prime number of bytes apart through `run_at`,
/// and ask for the five-window object under a length of one window. Expect: every run lent in place
/// (`Cow::Borrowed`), the next bytes of the object from its offset to its window's end, so the runs make each object
/// whole; the end and a wrong length answered none.
#[test]
fn a_run_of_an_object_kept_as_chunks_is_lent_in_place_and_its_length_checked() {
  let mut chunks = shard_chunks(64);
  let mut store = content();
  assert!(store.lend_in(&mut chunks));
  let long: Vec<u8> = (0..5 * WINDOW)
    .map(|at| u8::try_from(at * 7 % 251).unwrap())
    .collect();
  let short: Vec<u8> = (0..WINDOW / 3)
    .map(|at| u8::try_from(at % 13).unwrap())
    .collect();
  for bytes in [&long, &short] {
    let identity = store.put_object(bytes).unwrap();
    let len = u64::try_from(bytes.len()).unwrap();
    let mut at = 0usize;
    while at < bytes.len() {
      let run = store
        .run_at(&identity, len, u64::try_from(at).unwrap())
        .unwrap();
      assert!(matches!(run, Cow::Borrowed(_)), "lent in place at {at}");
      assert_eq!(&run[..], &bytes[at..at + run.len()]);
      at += run.len().min(509);
    }
    assert_eq!(store.run_at(&identity, len, len), None);
  }
  let identity = store.object_id(&long);
  let one = u64::try_from(WINDOW).unwrap();
  assert_eq!(
    store.run_at(&identity, one, 0),
    None,
    "a wrong length is refused"
  );
  assert!(store.hand_back(&mut chunks));
}
