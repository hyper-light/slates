//! The dirty-page write-back rule's journal walk, by use through a real volume bridge (§4.6 "Cache posture";
//! `docs/bugs/2026-10-21-a-dirty-shared-map-page-overwrites-a-later-write-through-another-path.md`): what another path
//! changed in a file after a point, the transport's own writes since excepted, and what cannot be told once the journal
//! has let go of the records since the point.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// The answers are lists of changed byte ranges; a list holding one range is not the numbers inside it.
#![allow(clippy::single_range_in_vec_init)]

use slates_bridge_core::{
  Attachments, Bridge, Changed, ObjectId, OpContext, Rights, SetAttr, TO_THE_END, View,
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

/// The sequence span of the records `write` makes through `bridge`, as a transport notes its own.
fn own_span(
  bridge: &mut VolumeBridge<'_>,
  cx: &OpContext,
  write: impl FnOnce(&mut VolumeBridge<'_>),
) -> std::ops::Range<u64> {
  let before = bridge.seen(cx).journal_seq;
  write(bridge);
  let after = bridge.seen(cx).journal_seq;
  before + 1..after + 1
}

/// The rule's walk by use. Do: write a file; take the point; write bytes 100–200 as another path; ask. Then write bytes
/// 150–250 as the transport itself and ask with its span; then truncate to 50 as another path and ask. Expect: first
/// 100–200 stale; then only 100–150, since the transport's own write refreshed 150–200 in its kernel; then everything
/// from 50, since a truncation cuts off whatever the kernel held past it.
#[test]
fn another_paths_change_is_stale_and_the_transports_own_write_is_not() {
  let mut store = store();
  let mut vol = volume(&mut store, ROOMY_JOURNAL);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();
  let (file, _fh) = bridge
    .create(ObjectId::new(root, 0), &cx, "f", 0o644, 0)
    .unwrap();
  let object = ObjectId::new(file.ino, 0);
  bridge.write(object, &cx, 0, &[b'A'; PAGE]).unwrap();
  let fetched_at = bridge.seen(&cx).journal_seq;

  bridge.write(object, &cx, 100, &[b'R'; 100]).unwrap();
  assert_eq!(
    bridge
      .changed_since(&cx, fetched_at, file.ino, &[])
      .unwrap(),
    Changed::Known(vec![100..200]),
    "another path's write after the fetch is stale"
  );

  let own = own_span(&mut bridge, &cx, |bridge| {
    bridge.write(object, &cx, 150, &[b'O'; 100]).unwrap();
  });
  assert_eq!(
    bridge
      .changed_since(&cx, fetched_at, file.ino, std::slice::from_ref(&own))
      .unwrap(),
    Changed::Known(vec![100..150]),
    "the transport's own later write refreshed what it wrote"
  );

  bridge
    .setattr(
      object,
      &cx,
      SetAttr {
        size: Some(50),
        ..SetAttr::default()
      },
    )
    .unwrap();
  assert_eq!(
    bridge
      .changed_since(&cx, fetched_at, file.ino, std::slice::from_ref(&own))
      .unwrap(),
    Changed::Known(vec![50..TO_THE_END]),
    "a truncation by another path leaves everything past the new end stale"
  );
  assert_eq!(
    bridge
      .changed_since(&cx, fetched_at, file.ino + 1, &[])
      .unwrap(),
    Changed::Known(Vec::new()),
    "another file changed in nothing"
  );
}

/// The rule's refusal to guess. Do: with a journal that retains four records, write a file, take the point, then write
/// it eight more times; ask from the point, and from a point still retained. Expect `Unknown` from the evicted point —
/// a transport must refuse a write-back then rather than apply or drop it — and a known answer from the retained one;
/// and the bridge's journal capacity is the journal's own bound.
#[test]
fn past_the_journals_retention_what_changed_is_unknown() {
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
  assert_eq!(
    bridge.changed_since(&cx, early, file.ino, &[]).unwrap(),
    Changed::Unknown,
    "records since the point were evicted: what changed cannot be told"
  );
  let late = bridge.seen(&cx).journal_seq;
  bridge.write(object, &cx, 10, b"y").unwrap();
  assert_eq!(
    bridge.changed_since(&cx, late, file.ino, &[]).unwrap(),
    Changed::Known(vec![10..11]),
    "a point still retained is answered"
  );
  assert_eq!(
    bridge.journal_capacity(&cx),
    4,
    "the journal's own bound: its budget over a record's fixed size"
  );
}
