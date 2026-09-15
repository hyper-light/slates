//! Chunk ownership across copy-ups (D-5, D-6; §4.5 "CoW at chunk granularity"): a copy-up clones the
//! inode version's body, so the retired version and its successor share every chunk until the head
//! rewrites a window. A chunk is therefore released exactly once, by the head, through the epoch
//! rule (freed now, or onto the newest snapshot's deadlist as its own `Dead::Chunk`) — never by a
//! retired version's release. Before this was enforced, releasing a retained version freed its
//! chunks while the head still reached them: a partially overwritten file lost its untouched
//! windows on `destroy_snapshot` (read back as zeros), and a copy-up after the last snapshot was
//! gone freed the whole file at once (docs/bugs/2026-09-13-snapshot-destroy-frees-head-shared-chunks.md).
//! These tests drive the volume through those histories and assert the head's bytes and the
//! arena's accounting: no premature free, no double free, no leak.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{store, volume};
use slates_vfs::clock::StepClock;
use slates_vfs::ids::InodeNo;
use slates_vfs::volume::{DestroyProgress, Store, Volume};

/// Writes `windows` whole chunk windows of `fill` into `no`, window by window.
fn write_windows(vol: &mut Volume, store: &mut Store, no: InodeNo, windows: u64, fill: u8) {
  let chunk = u64::try_from(store.content.chunk_bytes()).unwrap();
  let block = vec![fill; usize::try_from(chunk).unwrap()];
  for window in 0..windows {
    vol.write(store, no, window * chunk, &block).unwrap();
  }
}

/// Reads window `window` of `no` whole and asserts every byte is `fill`.
fn assert_window(vol: &Volume, store: &Store, no: InodeNo, window: u64, fill: u8, what: &str) {
  let chunk = store.content.chunk_bytes();
  let mut back = vec![0u8; chunk];
  let read = vol
    .read(store, no, window * u64::try_from(chunk).unwrap(), &mut back)
    .unwrap();
  assert_eq!(read, chunk, "{what}: window {window} is whole");
  assert!(
    back.iter().all(|b| *b == fill),
    "{what}: window {window} must hold {fill:?}; first bytes {:?}",
    &back[..8]
  );
}

/// A snapshot keeps a chunk alive that the head shares with the retained inode version; destroying
/// the snapshot must free only what the head no longer reaches. Do: write two windows, snapshot,
/// overwrite window 0 only, destroy the snapshot. Expect: window 1 (never rewritten, still the
/// head's) reads back intact, and the arena holds exactly the head's two windows.
#[test]
fn destroying_a_snapshot_keeps_the_windows_the_head_still_reaches() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 24);
  let root = vol.root_inode(&store).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  write_windows(&mut vol, &mut store, f, 2, b'a');
  let chunk = store.content.chunk_bytes();
  let snap = vol.snapshot(&mut store).unwrap();
  // Rewrite window 0 only: window 1's chunk stays shared between the head and the retained version.
  let block = vec![b'c'; chunk];
  vol.write(&mut store, f, 0, &block).unwrap();
  vol.destroy_snapshot(&mut store, snap).unwrap();
  assert_window(&vol, &store, f, 1, b'a', "after the snapshot's destroy");
  assert_window(&vol, &store, f, 0, b'c', "after the snapshot's destroy");
  assert_eq!(
    store.content.allocated_bytes(),
    2 * chunk,
    "the arena holds the head's two windows and nothing else (the retained window 0 was freed)"
  );
}

/// A metadata copy-up (`chmod`) clones the body, so the retired version shares every window with
/// the head. Do: write two windows, snapshot, chmod, destroy the snapshot. Expect: both windows
/// intact and the arena unchanged (the drop freed the version's slot, no chunk).
#[test]
fn a_metadata_copy_up_shares_every_window_and_the_snapshot_drop_frees_none() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 24);
  let root = vol.root_inode(&store).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  write_windows(&mut vol, &mut store, f, 2, b'm');
  let chunk = store.content.chunk_bytes();
  let snap = vol.snapshot(&mut store).unwrap();
  vol.chmod(&mut store, f, 0o600).unwrap();
  let versions_before = store.inodes.len();
  vol.destroy_snapshot(&mut store, snap).unwrap();
  assert_window(&vol, &store, f, 0, b'm', "after a metadata-only copy-up");
  assert_window(&vol, &store, f, 1, b'm', "after a metadata-only copy-up");
  assert_eq!(
    store.content.allocated_bytes(),
    2 * chunk,
    "no chunk was freed: every window is still the head's"
  );
  assert_eq!(
    store.inodes.len(),
    versions_before - 1,
    "the retained version's slot was freed"
  );
}

/// A copy-up after the last snapshot is gone retires the old version at once (nothing pins it);
/// that release must not take the chunks the new version shares. Do: write two windows, snapshot,
/// destroy the snapshot, chmod. Expect: the file reads back whole.
#[test]
fn a_copy_up_after_the_last_snapshot_is_gone_keeps_the_content() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 24);
  let root = vol.root_inode(&store).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  write_windows(&mut vol, &mut store, f, 2, b'k');
  let chunk = store.content.chunk_bytes();
  let snap = vol.snapshot(&mut store).unwrap();
  vol.destroy_snapshot(&mut store, snap).unwrap();
  // The head's epoch stayed advanced, so this copies the version up and retires the old one now.
  vol.chmod(&mut store, f, 0o600).unwrap();
  assert_window(&vol, &store, f, 0, b'k', "after a copy-up with no snapshot");
  assert_window(&vol, &store, f, 1, b'k', "after a copy-up with no snapshot");
  assert_eq!(store.content.allocated_bytes(), 2 * chunk);
}

/// Two snapshots with a partial overwrite between them: each retained window is freed exactly once,
/// when the last snapshot reaching it goes, in either destroy order. Do: write two windows; S1;
/// overwrite window 0; S2; overwrite window 1; destroy the snapshots. Expect: the head reads its
/// final bytes and the arena holds exactly the head's two windows afterwards.
#[test]
fn a_chain_of_snapshots_frees_each_retained_window_exactly_once() {
  for newest_first in [true, false] {
    let mut store = store();
    let mut vol = volume(&mut store, 1 << 24);
    let root = vol.root_inode(&store).unwrap();
    let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
    write_windows(&mut vol, &mut store, f, 2, b'1');
    let chunk = store.content.chunk_bytes();
    let block = |fill: u8| vec![fill; chunk];
    let s1 = vol.snapshot(&mut store).unwrap();
    vol.write(&mut store, f, 0, &block(b'2')).unwrap();
    let s2 = vol.snapshot(&mut store).unwrap();
    vol
      .write(&mut store, f, u64::try_from(chunk).unwrap(), &block(b'3'))
      .unwrap();
    assert_eq!(
      store.content.allocated_bytes(),
      4 * chunk,
      "the head's two windows plus the two retained ones"
    );
    let order = if newest_first { [s2, s1] } else { [s1, s2] };
    for snap in order {
      vol.destroy_snapshot(&mut store, snap).unwrap();
      assert_window(&vol, &store, f, 0, b'2', "during the chain's destroys");
      assert_window(&vol, &store, f, 1, b'3', "during the chain's destroys");
    }
    assert_eq!(
      store.content.allocated_bytes(),
      2 * chunk,
      "newest_first={newest_first}: every retained window freed once, none leaked"
    );
  }
}

/// Destroying a whole volume returns every chunk to the arena: the head's own windows (shared with
/// retained versions or not), the retained ones on the deadlists, and the ones a retained version
/// alone reaches. Do: a history of writes, snapshots, a metadata copy-up and an unlink; destroy to
/// completion. Expect: zero bytes and zero chunks allocated.
#[test]
fn destroying_a_volume_with_snapshots_returns_the_whole_arena() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 24);
  let root = vol.root_inode(&store).unwrap();
  let f = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  let g = vol.create_file_no(&mut store, root, "g", 0o644).unwrap();
  write_windows(&mut vol, &mut store, f, 2, b'f');
  write_windows(&mut vol, &mut store, g, 1, b'g');
  let chunk = store.content.chunk_bytes();
  let _s1 = vol.snapshot(&mut store).unwrap();
  vol.write(&mut store, f, 0, &vec![b'F'; chunk]).unwrap();
  vol.chmod(&mut store, g, 0o600).unwrap();
  let _s2 = vol.snapshot(&mut store).unwrap();
  vol.unlink_no(&mut store, root, "g").unwrap();
  assert!(store.content.allocated_bytes() > 0);
  vol.destroy(&mut store).unwrap();
  while !matches!(
    vol.destroy_step(&mut store, u64::MAX).unwrap(),
    DestroyProgress::Done
  ) {}
  assert_eq!(store.content.chunks(), 0, "every chunk record is gone");
  assert_eq!(
    store.content.allocated_bytes(),
    0,
    "every arena block is back"
  );
}

/// A recovered snapshot's diverged file is a private copy, so dropping the snapshot frees exactly
/// that copy's chunks. Do: write two windows, snapshot, overwrite window 0, capture and rebuild,
/// drop the recovered snapshot. Expect: the head reads both windows and the arena holds only the
/// head's two.
#[test]
fn dropping_a_recovered_snapshot_frees_its_private_chunks_and_keeps_the_heads() {
  let mut source = store();
  let mut vol = volume(&mut source, 1 << 24);
  let root = vol.root_inode(&source).unwrap();
  let f = vol.create_file_no(&mut source, root, "f", 0o644).unwrap();
  write_windows(&mut vol, &mut source, f, 2, b'r');
  let chunk = source.content.chunk_bytes();
  let snap = vol.snapshot(&mut source).unwrap();
  vol.write(&mut source, f, 0, &vec![b'R'; chunk]).unwrap();
  let image = vol.to_image(&source, None).unwrap();

  let mut fresh = store();
  let mut recovered = Volume::from_image(
    &mut fresh,
    &image,
    Box::new(StepClock::new(0, 1)),
    1 << 16,
    None,
  )
  .unwrap();
  let with_snapshot = fresh.content.allocated_bytes();
  assert!(
    with_snapshot >= 4 * chunk,
    "the rebuilt store holds the head's two windows and the snapshot's private copy"
  );
  recovered.destroy_snapshot(&mut fresh, snap).unwrap();
  assert_window(
    &recovered,
    &fresh,
    f,
    0,
    b'R',
    "after the recovered snapshot's drop",
  );
  assert_window(
    &recovered,
    &fresh,
    f,
    1,
    b'r',
    "after the recovered snapshot's drop",
  );
  assert_eq!(
    fresh.content.allocated_bytes(),
    2 * chunk,
    "the snapshot's private copy was freed and the head's windows kept"
  );
}
