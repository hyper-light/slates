//! A file's chunks pinned for a reader that reads them over turns (A-768: a live log's whole capture): what the head
//! lets go of while they are pinned stays readable to the reader, and is freed when its last pin goes; a pin taken and
//! let go frees nothing the head still holds. Before, a chunk the head replaced was freed at once, and a reader still
//! holding its extent met `StaleHandle`, so the capture had to read every byte of the volume in its own turn.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

mod common;

use common::{store, volume};
use slates_vfs::content::Extent;
use slates_vfs::ids::InodeNo;
use slates_vfs::volume::{Store, Volume};

/// Shape: the windows the file holds, each a chunk once sealed.
const WINDOWS: u64 = 3;

/// Writes `WINDOWS` whole chunk windows of `fill` into `no`, window by window.
fn write_windows(vol: &mut Volume, store: &mut Store, no: InodeNo, fill: u8) {
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let block = vec![fill; usize::try_from(chunk).unwrap()];
  for window in 0..WINDOWS {
    vol.write(store, no, window * chunk, &block).unwrap();
  }
}

/// Every one of `pinned` reads back whole, every byte `fill`: what it was pinned at, whatever the head did since.
fn pinned_extents_read(store: &Store, pinned: &[Extent], fill: u8) {
  for extent in pinned {
    let mut back = vec![0u8; usize::try_from(extent.len).unwrap()];
    store
      .content
      .read_extent_into(extent, extent.off, &mut back)
      .expect("a pinned extent stays readable after the head let it go");
    assert!(
      back.iter().all(|b| *b == fill),
      "a pinned extent at {} reads the bytes it was pinned at",
      extent.off
    );
  }
}

/// Do: write three windows of `a`, pin the file's extents, overwrite every window with `b` (the head replaces each
/// chunk), read the pinned extents, then unpin. Expect: the head reads `b`; the pinned extents read `a`, every byte; a
/// replaced chunk was freed while pinned; once unpinned, no pin is held and those chunks are gone.
#[test]
fn a_pinned_extent_reads_the_bytes_it_was_pinned_at_after_the_head_replaced_them() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 24);
  let root = vol.root_inode(&store).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  write_windows(&mut vol, &mut store, f, b'a');
  let mut pinned = Vec::new();
  let file = vol
    .pin_extents(&mut store, f, &mut pinned)
    .unwrap()
    .expect("a written file has chunked content");
  assert_eq!(file.extents, pinned.len());
  assert!(!pinned.is_empty(), "the file's sealed windows were pinned");
  assert_eq!(store.content.pinned(), u64::try_from(pinned.len()).unwrap());
  write_windows(&mut vol, &mut store, f, b'b');
  let mut head = vec![0u8; store.content.chunk_bytes()];
  vol.read(&store, f, 0, &mut head).unwrap();
  assert!(
    head.iter().all(|b| *b == b'b'),
    "the head reads its new bytes"
  );
  assert!(
    store.content.freed_while_pinned() >= 1,
    "the overwrite freed a pinned chunk: {}",
    store.content.freed_while_pinned()
  );
  pinned_extents_read(&store, &pinned, b'a');
  let held = store.content.chunks();
  store.content.unpin_extents(&pinned).unwrap();
  assert_eq!(store.content.pinned(), 0, "every pin was let go");
  assert_eq!(
    held - store.content.chunks(),
    usize::try_from(store.content.freed_while_pinned()).unwrap(),
    "each chunk freed while pinned went with its last pin, and nothing else did"
  );
  vol.read(&store, f, 0, &mut head).unwrap();
  assert!(
    head.iter().all(|b| *b == b'b'),
    "the head still reads its bytes"
  );
}

/// Do: pin a file's extents and unpin them with the head unchanged. Expect: no pin held, nothing freed, the head's
/// bytes as written.
#[test]
fn a_pin_let_go_frees_nothing_the_head_holds() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 24);
  let root = vol.root_inode(&store).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  write_windows(&mut vol, &mut store, f, b'a');
  let chunks = store.content.chunks();
  let mut pinned = Vec::new();
  vol.pin_extents(&mut store, f, &mut pinned).unwrap();
  store.content.unpin_extents(&pinned).unwrap();
  assert_eq!(store.content.pinned(), 0);
  assert_eq!(store.content.chunks(), chunks, "nothing was freed");
  let mut head = vec![0u8; store.content.chunk_bytes()];
  vol.read(&store, f, 0, &mut head).unwrap();
  assert!(head.iter().all(|b| *b == b'a'));
}
