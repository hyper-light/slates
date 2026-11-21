//! A-827: a reader's hold on a volume's head. A reader that reads the head over turns (a live log's whole capture)
//! holds it: the head's roots frozen, the epoch advanced as a snapshot advances it, and what the head lets go of
//! meanwhile that the held head reaches kept until the reader lets go. Before, a capture had to take the whole image in
//! one turn: 26 to 36 us for 311 inodes and 10.5 to 17 us to encode it in release (M4 Max), at or past a live write's
//! 50 us median.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

mod common;

use std::collections::BTreeMap;

use common::{store, volume};
use slates_vfs::ids::InodeNo;
use slates_vfs::recover::{BodyImage, VolumeImage};
use slates_vfs::volume::{Store, Volume};

/// Shape: the directories and files a directory the scene makes, each file a few windows long.
const DIRS: usize = 4;
/// Shape: see [`DIRS`].
const FILES: usize = 6;

/// A file's bytes for `seed`, `len` long.
fn body(seed: usize, len: usize) -> Vec<u8> {
  (0..len)
    .map(|at| u8::try_from((at.wrapping_mul(31) ^ seed.wrapping_mul(131)) % 251).unwrap())
    .collect()
}

/// Every file's bytes through the head, by inode number.
fn contents(vol: &Volume, store: &Store, files: &[InodeNo]) -> BTreeMap<u64, Vec<u8>> {
  files
    .iter()
    .map(|no| {
      let mut bytes = vec![0u8; 1 << 20];
      let read = vol.read(store, *no, 0, &mut bytes).unwrap();
      bytes.truncate(read);
      (no.0, bytes)
    })
    .collect()
}

/// The scene: `DIRS` directories of `FILES` files, of sizes about a chunk window and past it, some left open (written
/// last, not sealed), and one snapshot taken in the middle. The volume, its files and the snapshot.
fn scene(store: &mut Store) -> (Volume, Vec<InodeNo>, slates_vfs::ids::SnapshotId) {
  let mut vol = volume(store, 1 << 26);
  let root = vol.root_inode(store).unwrap();
  let chunk = store.content.chunk_bytes();
  let mut files = Vec::new();
  let mut dirs = Vec::new();
  for d in 0..DIRS {
    dirs.push(vol.mkdir_no(store, root, &format!("d{d}"), 0o755).unwrap());
  }
  for (d, dir) in dirs.iter().enumerate() {
    for f in 0..FILES {
      let no = vol
        .create_file_no(store, *dir, &format!("f{f}"), 0o644)
        .unwrap();
      let len = chunk / 2 + (d * FILES + f) * chunk / 3;
      vol.write(store, no, 0, &body(d * FILES + f, len)).unwrap();
      files.push(no);
    }
  }
  vol.seal_idle(store, u64::MAX);
  vol.seal_idle(store, u64::MAX);
  let snapshot = vol.snapshot(store).unwrap();
  // Written after the snapshot and not sealed: open extents in the held head.
  for (at, no) in files.iter().enumerate().step_by(3) {
    vol.write(store, *no, 7, &body(1000 + at, 300)).unwrap();
  }
  (vol, files, snapshot)
}

/// Every kind of change a held head must survive, the `round`-th of several: overwrites (copy-ups of sealed and open
/// files), a truncate, an unlink, new files and a directory, a rename, a chmod, a seal of every open extent, and a
/// snapshot taken and the older one destroyed.
fn change(
  vol: &mut Volume,
  store: &mut Store,
  files: &[InodeNo],
  round: usize,
  snapshot: &mut Option<slates_vfs::ids::SnapshotId>,
) {
  let root = vol.root_inode(store).unwrap();
  let chunk = store.content.chunk_bytes();
  for (at, no) in files.iter().enumerate().skip(round % 3).step_by(3) {
    // A file an earlier round unlinked is gone: skipped.
    let _ = vol.write(store, *no, 0, &body(5000 + round * 100 + at, chunk));
  }
  let _ = vol.truncate(store, files[round % files.len()], 11);
  let d0 = vol.lookup_no(store, root, "d0").unwrap().inode;
  let _ = vol.unlink_no(store, d0, &format!("f{}", round % FILES));
  let made = vol
    .create_file_no(store, root, &format!("new{round}"), 0o644)
    .unwrap();
  vol
    .write(store, made, 0, &body(9000 + round, chunk + 5))
    .unwrap();
  vol
    .mkdir_no(store, root, &format!("nd{round}"), 0o700)
    .unwrap();
  let _ = vol.rename_no(
    store,
    root,
    &format!("new{round}"),
    root,
    &format!("moved{round}"),
  );
  let _ = vol.chmod(store, files[(round + 1) % files.len()], 0o600);
  vol.age_writes();
  vol.seal_idle(store, u64::MAX);
  let taken = vol.snapshot(store).unwrap();
  if let Some(old) = snapshot.replace(taken) {
    vol.destroy_snapshot(store, old).unwrap();
  }
}

/// A file as a held image step handed it: its size, its sealed extents, and its open extent's place and bytes.
type HeldFile = (
  u64,
  Vec<slates_vfs::content::Extent>,
  Option<(u64, Vec<u8>)>,
);

/// Asserts `image` is `reference` but for bodies sealed in place between steps, and counts those.
fn imaged_as_held(image: &VolumeImage, reference: &VolumeImage) -> usize {
  // Every field but the inodes' bodies is the reference's; a body differs only where a copy-up or the idle sweep sealed
  // the held version's open extent in place (A-170 rewrites the old record so every view sees the seal), and then it
  // names the sealed chunks where the reference named the open extent: the same bytes, checked below.
  let mut sealed_in_place = 0;
  for (held_inode, then) in image.inodes.iter().zip(&reference.inodes) {
    let (mut a, mut b) = (held_inode.clone(), then.clone());
    if a.body != b.body {
      let opened_then = matches!(b.body, BodyImage::Chunked { open: Some(_), .. });
      let sealed_now = matches!(a.body, BodyImage::Chunked { open: None, .. });
      assert!(
        opened_then && sealed_now,
        "inode {}'s body differs other than by a seal in place: held {:?}, then {:?}",
        a.no,
        a.body,
        b.body
      );
      sealed_in_place += 1;
      a.body = BodyImage::Empty;
      b.body = BodyImage::Empty;
    }
    assert!(a == b, "inode {} is imaged as it was held", a.no);
  }
  let (mut header, mut header_then) = (image.clone(), reference.clone());
  header.inodes.clear();
  header_then.inodes.clear();
  assert!(
    header == header_then,
    "the held image's fields are the head's when it was held"
  );
  sealed_in_place
}

/// A held file's bytes: its held extents read from `store`, its open extent's bytes laid over them.
fn held_bytes(store: &Store, (size, extents, open): &HeldFile) -> Vec<u8> {
  let mut bytes = vec![0u8; usize::try_from(*size).unwrap()];
  for extent in extents {
    store
      .content
      .read_extent_into(extent, 0, &mut bytes)
      .expect("a held extent stays readable through every change");
  }
  if let Some((at, open)) = open {
    let at = usize::try_from(*at).unwrap();
    let end = (at + open.len()).min(bytes.len());
    bytes[at..end].copy_from_slice(&open[..end - at]);
  }
  bytes
}

/// A-827. Do: make the scene, take the head's standalone image and every file's bytes as the reference, hold the head,
/// and image it one inode a step with a round of changes between steps (every kind: overwrites of sealed and open files,
/// a truncate, an unlink, new files, a directory, a rename, a chmod, a seal of every open extent, a snapshot taken and
/// the older destroyed). Expect: the held image the reference in every field and inode, a file's body differing only
/// where the held version's open extent was sealed in place (A-170: the old record is rewritten to name the seal, the
/// same bytes); each file's held extents, read after every
/// change, and its open extent's bytes as the step handed them, laid together, the file's bytes then. Then let the hold
/// go and make the same changes on a twin volume never held. Expect the stores to hold the same counts of chunks, inode
/// versions, table nodes and directory nodes: nothing the hold kept outlives it, and nothing the head reaches was freed.
#[test]
fn a_held_heads_image_built_over_changes_is_the_image_it_was_held_at() {
  let mut held_store = store();
  let (mut vol, files, first) = scene(&mut held_store);
  let reference = vol.to_standalone_image(&held_store, None).unwrap();
  let bytes_then = contents(&vol, &held_store, &files);
  let epoch = vol.hold_head().unwrap();
  assert_eq!(epoch.0, reference.epoch, "the held epoch is the head's");
  let mut imaging = vol.begin_held_image(&held_store, None).unwrap();
  let mut held: BTreeMap<u64, HeldFile> = BTreeMap::new();
  let mut snapshot = Some(first);
  let mut round = 0;
  loop {
    let done = vol
      .held_image_step(&held_store, &mut imaging, 1, |file| {
        held.insert(
          file.no.0,
          (
            file.size,
            file.extents.to_vec(),
            file.open.map(|(at, bytes)| (at, bytes.to_vec())),
          ),
        );
      })
      .unwrap();
    if done {
      break;
    }
    change(&mut vol, &mut held_store, &files, round, &mut snapshot);
    round += 1;
  }
  assert!(
    round > files.len(),
    "changes were made between {round} steps"
  );
  let record = imaging.finish();
  let Ok(slates_vfs::delta::VolumeRecord::Full { image }) =
    <slates_vfs::delta::VolumeRecord as slates_wire::Wire>::decode(&mut &record[..])
  else {
    panic!("the held record is a whole image");
  };
  assert_eq!(
    image.inodes.len(),
    reference.inodes.len(),
    "every inode the head held is imaged"
  );
  let sealed_in_place = imaged_as_held(&image, &reference);
  println!("{sealed_in_place} held versions' open extents were sealed in place between steps");
  for no in &files {
    let file = held
      .get(&no.0)
      .expect("every file was handed with its image");
    let bytes = held_bytes(&held_store, file);
    assert_eq!(
      &bytes,
      bytes_then.get(&no.0).unwrap(),
      "file {} reads as it was held",
      no.0
    );
  }
  let freed = vol.release_hold(&mut held_store).unwrap();
  assert!(freed > 0, "the hold kept what the changes let go");
  // The twin: the same scene and changes, never held.
  let mut twin_store = store();
  let (mut twin, twin_files, twin_first) = scene(&mut twin_store);
  let mut twin_snapshot = Some(twin_first);
  for at in 0..round {
    change(
      &mut twin,
      &mut twin_store,
      &twin_files,
      at,
      &mut twin_snapshot,
    );
  }
  let counts = |store: &Store| {
    (
      store.content.chunks(),
      store.inodes.len(),
      store.tries.len(),
      store.dirs.len(),
    )
  };
  println!(
    "held over {round} rounds of changes: {freed} objects freed at the release; held store {:?}, twin {:?} (chunks, \
     inode versions, table nodes, directory nodes)",
    counts(&held_store),
    counts(&twin_store)
  );
  assert_eq!(
    counts(&held_store),
    counts(&twin_store),
    "after the release the held store holds what the twin holds"
  );
}

/// A-827. Do: hold a head, try to hold it again, let it go, and hold it again. Expect the second hold refused while the
/// first is held, and taken once it is let go.
#[test]
fn a_head_is_held_by_one_reader_at_a_time() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 20);
  vol.hold_head().unwrap();
  assert!(vol.hold_head().is_err(), "a second hold is refused");
  vol.release_hold(&mut store).unwrap();
  vol.hold_head().expect("held again once let go");
}
