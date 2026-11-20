//! A-727 slice a2's tests: the repository content store's windows kept as sealed chunks of a repository arena, read
//! back by copy, freed with their last holder, the arena grown from its source when full, and a full arena with nothing
//! to grow from refused typed.

use std::borrow::Cow;

use slates_mem::arena::{ChunkArena, ExtentSource};
use slates_mem::region::Region;
use slates_vcs::{ContentRefused, ContentStore, StoredWindow, WindowStorage};

use super::ArenaWindows;

/// Shape: the tests' page, and the arena's granule.
const PAGE: usize = 16 * 1024;
/// Shape: the chunk records the tests' stores may hold, far past what they keep.
const CHUNKS: usize = 1 << 12;
/// Shape: the content store's window, one page: an object of five pages is five windows.
const WINDOW: usize = PAGE;
/// Shape: a content store's budget, far past what the tests keep.
const BUDGET: u64 = 1 << 30;

/// An arena over one region of `pages` pages and no source.
fn arena_of(pages: usize) -> ChunkArena {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * pages, PAGE, false).unwrap())
    .unwrap();
  arena
}

/// A source handing out `left` more regions of one page each, then none: a stand-in for the content pool.
#[derive(Debug)]
struct Pages {
  left: u16,
  next: u16,
}

impl ExtentSource for Pages {
  fn claim(&mut self) -> Option<(u16, Region)> {
    self.left = self.left.checked_sub(1)?;
    self.next += 1;
    Some((self.next, Region::map(PAGE, PAGE, false).ok()?))
  }

  fn release(&mut self, _id: u16, _region: Region) {}

  fn refused(&self) -> u64 {
    0
  }

  fn available(&self) -> usize {
    usize::from(self.left) * PAGE
  }
}

/// A-727 slice a2. Do: make a content store over windows kept in a repository arena of 64 pages, put an object of five
/// windows and an empty one, read them back whole and window by window, then release the long one. Expect: every
/// window read copied out of the arena (`Cow::Owned`) and equal to the bytes put; the empty object read back empty with
/// no chunk; and the long object gone with its hold while the empty one stays.
#[test]
fn a_content_store_over_the_repository_arena_reads_back_by_copy_and_frees_with_its_last_hold() {
  let windows = ArenaWindows::new(arena_of(64), PAGE, CHUNKS);
  let mut store = ContentStore::with_storage(BUDGET, WINDOW, Box::new(windows)).unwrap();
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
      .all(|window| matches!(window, Cow::Owned(_)))
  );
  assert_eq!(store.object_bytes(&empty, 0), Some(Vec::new()));
  store.release(&root).unwrap();
  assert_eq!(store.object_bytes(&root, len), None);
  assert_eq!(store.object_bytes(&empty, 0), Some(Vec::new()));
}

/// A-727 slice a2 (growth). Do: keep windows of a page each in an arena of one page whose source hands out four more,
/// six times. Expect: the first five kept and read back (the arena grown from its source, counted), and the sixth
/// refused `ContentRefused::Storage`, the source spent, nothing kept for it.
#[test]
fn a_full_repository_arena_grows_from_its_source_and_refuses_typed_once_it_is_spent() {
  let mut arena = arena_of(1);
  arena.set_source(Box::new(Pages { left: 4, next: 1 }));
  let mut windows = ArenaWindows::new(arena, PAGE, CHUNKS);
  let page = |tag: u8| vec![tag; PAGE];
  let mut kept: Vec<StoredWindow> = Vec::new();
  for tag in 1..=5 {
    kept.push(windows.put(&page(tag)).unwrap());
  }
  assert!(windows.grown() >= 1, "the arena grew from its source");
  for (tag, window) in (1..=5).zip(&kept) {
    assert_eq!(windows.read(*window).as_deref(), Some(&page(tag)[..]));
  }
  assert_eq!(windows.put(&page(6)), Err(ContentRefused::Storage));
}
