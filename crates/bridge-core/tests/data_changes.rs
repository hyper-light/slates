//! The data-change feed a transport consumes to know what its kernel holds stale, by use through a real volume bridge
//! (§4.6 "Cache posture"; `docs/bugs/2026-10-21-a-dirty-shared-map-page-overwrites-a-later-write-through-another-path.md`):
//! every change to a file's bytes after a point, with its sequence, its file and its bytes, and an honest `false` once
//! the journal has let go of records since the point.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_core::{
  Attachments, Bridge, DataChange, ObjectId, OpContext, Rights, SetAttr, TO_THE_END, View,
  VolumeBridge,
};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::journal::OpRecord;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Shape: the store's page size.
const PAGE: usize = 4096;
/// Shape: the pages the store's region maps.
const REGION_PAGES: usize = 1024;
/// Shape: a journal that holds every record these tests make.
const ROOMY_JOURNAL: usize = 1 << 16;
/// Shape: a journal that holds four records' fixed parts, so a handful of writes evicts the oldest.
const SMALL_JOURNAL: usize = 4 * std::mem::size_of::<OpRecord>();

fn store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 128,
      max_dirs: 64,
      max_inodes: 256,
      max_chunks: REGION_PAGES,
      max_dir_blocks: 64,
      dir_cutover: 16,
    },
    arena,
    0,
  )
}

fn volume(store: &mut Store, journal_bytes: usize) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes,
      clock: Box::new(HostClock::default()),
    },
  )
  .unwrap()
}

fn rw_cx() -> OpContext {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      VolumeId { bytes: [0; 16] },
      View::Current,
      Principal::Uid { uid: 0 },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  attachments.context(id).unwrap()
}

/// The feed by use. Do: create and write a file, take the point; overwrite bytes 100–200, create and write a second
/// file, truncate the first to 50; read the feed from the point. Expect, oldest first, each change with its sequence
/// ascending: the first file's 100–200, the second file's write, then the first file from 50 to the end — and the
/// creation itself (no bytes) absent; the feed complete.
#[test]
fn every_data_change_after_a_point_comes_with_its_sequence_file_and_bytes() {
  let mut store = store();
  let mut vol = volume(&mut store, ROOMY_JOURNAL);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();
  let (first, _fh) = bridge
    .create(ObjectId::new(root, 0), &cx, "f", 0o644, 0)
    .unwrap();
  let first_object = ObjectId::new(first.ino, 0);
  bridge.write(first_object, &cx, 0, &[b'A'; PAGE]).unwrap();
  let point = bridge.seen(&cx).journal_seq;

  bridge.write(first_object, &cx, 100, &[b'R'; 100]).unwrap();
  let (second, _fh) = bridge
    .create(ObjectId::new(root, 0), &cx, "g", 0o644, 0)
    .unwrap();
  bridge
    .write(ObjectId::new(second.ino, 0), &cx, 0, b"second")
    .unwrap();
  bridge
    .setattr(
      first_object,
      &cx,
      SetAttr {
        size: Some(50),
        ..SetAttr::default()
      },
    )
    .unwrap();

  let mut changes: Vec<DataChange> = Vec::new();
  assert!(
    bridge.data_changes(&cx, point, &mut changes).unwrap(),
    "complete"
  );
  let shape: Vec<(u64, std::ops::Range<u64>)> = changes
    .iter()
    .map(|change| (change.ino, change.range.clone()))
    .collect();
  assert_eq!(
    shape,
    vec![
      (first.ino, 100..200),
      (second.ino, 0..6),
      (first.ino, 50..TO_THE_END)
    ],
    "the overwrite, the other file's write, the truncation's tail; the creation changed no bytes"
  );
  assert!(
    changes.windows(2).all(|pair| pair[0].seq < pair[1].seq),
    "oldest first, by sequence"
  );
  assert!(
    changes.iter().all(|change| change.seq > point),
    "nothing at or before the point"
  );
}

/// The feed's refusal to pretend. Do: with a journal that retains four records, write a file, take the point, then write
/// it eight more times; read the feed from the point and from a point still retained. Expect `false` (incomplete) from
/// the evicted point and `true` from the retained one, with its one change; and the bridge's journal capacity is the
/// journal's own bound.
#[test]
fn past_the_journals_retention_the_feed_says_it_is_incomplete() {
  let mut store = store();
  let mut vol = volume(&mut store, SMALL_JOURNAL);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();
  let (file, _fh) = bridge
    .create(ObjectId::new(root, 0), &cx, "f", 0o644, 0)
    .unwrap();
  let object = ObjectId::new(file.ino, 0);
  bridge.write(object, &cx, 0, &[b'A'; 64]).unwrap();
  let early = bridge.seen(&cx).journal_seq;
  for at in 0..8u64 {
    bridge.write(object, &cx, at, b"x").unwrap();
  }
  let mut changes = Vec::new();
  assert!(
    !bridge.data_changes(&cx, early, &mut changes).unwrap(),
    "records since the point were evicted"
  );
  let late = bridge.seen(&cx).journal_seq;
  bridge.write(object, &cx, 10, b"y").unwrap();
  let mut changes = Vec::new();
  assert!(
    bridge.data_changes(&cx, late, &mut changes).unwrap(),
    "a point still retained"
  );
  assert_eq!(
    changes
      .iter()
      .map(|change| change.range.clone())
      .collect::<Vec<_>>(),
    vec![10..11]
  );
  assert_eq!(
    bridge.journal_capacity(&cx),
    4,
    "the journal's own bound: its budget over a record's fixed size"
  );
}
