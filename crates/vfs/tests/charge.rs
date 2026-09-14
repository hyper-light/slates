//! §4.2 all-cost charging, the allocator-rounding dimension (GAP-A9-1): a window is charged the
//! arena block it takes — the smallest power-of-two number of pages holding its materialized
//! length, at most a chunk — so the bytes a volume is charged are the bytes it holds in the arena.
//! Before this, the charge was the materialized length itself (a one-byte write at a window start
//! charged one byte and took a page), so a sparse writer could hold `page ×` its quota of arena:
//! the "uncharged ... bytes can defeat the cap" finding in its allocator form. These tests drive
//! the hostile shape and assert the observable outcome — what the arena holds against what the
//! quota admits.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{PAGE, store, volume};
use slates_vfs::error::VfsError;

/// T-1.3's hostile sibling: one byte at every window start. Do: a bounded volume of sixteen pages
/// writes one byte at successive chunk-window starts. Expect: sixteen windows are admitted (one page
/// each), the seventeenth is refused `NoSpace`, and the arena holds exactly the quota — never more.
/// Non-vacuous: charged by the materialized byte, the seventeenth write (and thousands after it)
/// succeeds and the arena holds a page per byte.
#[test]
fn a_sparse_writer_cannot_hold_more_arena_than_its_quota() {
  let mut store = store();
  let page = u64::try_from(PAGE).unwrap();
  let windows: u64 = 16;
  let quota = windows * page;
  let mut vol = volume(&mut store, quota);
  let root = vol.root_inode(&store).unwrap();
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let f = vol
    .create_file_no(&mut store, root, "sparse", 0o644)
    .unwrap();
  for window in 0..windows {
    vol
      .write(&mut store, f, window * chunk, b"x")
      .unwrap_or_else(|e| panic!("window {window} is within the quota: {e:?}"));
  }
  assert_eq!(
    vol.accounting().referenced_bytes,
    quota,
    "sixteen one-byte windows are charged a page each"
  );
  assert_eq!(
    vol.write(&mut store, f, windows * chunk, b"x"),
    Err(VfsError::NoSpace),
    "the seventeenth window would exceed the quota"
  );
  assert_eq!(
    u64::try_from(store.content.allocated_bytes()).unwrap(),
    quota,
    "the arena holds exactly what the quota admitted, never a page per byte beyond it"
  );
}

/// A truncate that cuts a window to a smaller block returns the difference: the charge tracks the
/// block in both directions. Do: write a whole window, truncate it to one page plus one byte, then
/// to one byte. Expect: the charge falls to two pages, then one, and the arena agrees each time.
#[test]
fn a_truncate_returns_the_pages_a_window_no_longer_takes() {
  let mut store = store();
  let page = u64::try_from(PAGE).unwrap();
  let mut vol = volume(&mut store, 1 << 24);
  let root = vol.root_inode(&store).unwrap();
  let chunk = store.content.chunk_bytes();
  let f = vol.create_file_no(&mut store, root, "cut", 0o644).unwrap();
  vol.write(&mut store, f, 0, &vec![b'c'; chunk]).unwrap();
  let held = |store: &slates_vfs::volume::Store, vol: &slates_vfs::volume::Volume| {
    (
      vol.accounting().referenced_bytes,
      u64::try_from(store.content.allocated_bytes()).unwrap(),
    )
  };
  assert_eq!(
    held(&store, &vol),
    (u64::try_from(chunk).unwrap(), u64::try_from(chunk).unwrap())
  );
  vol.truncate(&mut store, f, page + 1).unwrap();
  assert_eq!(
    held(&store, &vol),
    (2 * page, 2 * page),
    "a page and a byte take two pages"
  );
  vol.truncate(&mut store, f, 1).unwrap();
  assert_eq!(held(&store, &vol), (page, page), "one byte takes one page");
  let mut back = [0u8; 2];
  assert_eq!(vol.read(&store, f, 0, &mut back).unwrap(), 1);
  assert_eq!(back[0], b'c', "the kept byte survived the rebuilds");
}
