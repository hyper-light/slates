//! Releases of handles a dead daemon issued, served by its successor (goal item 4 of 2026-10-22; A-61, A-148; AC-3.4).
//! A FUSE mount the anchor holds across a daemon's death keeps the kernel's open handles; the successor recovers the
//! volume from its image and serves those handles' releases from a fresh handle store, which never issued them. A
//! release of one must give back the open reference it held, so an unlinked file is reclaimed once the kernel lets go of
//! it, and must never give back a reference something else still needs. Each test runs the mount before the takeover,
//! publishes (the image), rebuilds the volume and settles its references as recovery does, and serves the rest through
//! the successor's bridge.

// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_core::{Attachments, Bridge, ObjectId, OpContext, Rights, View, VolumeBridge};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::recover::{Claims, VolumeImage};
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Shape: the page and a small arena for the test volume.
const PAGE: usize = 4096;
/// Shape: the arena's pages, room for every file the tests write.
const REGION_PAGES: usize = 1024;
/// Shape: the journal a volume keeps, in bytes.
const JOURNAL_BYTES: usize = 1 << 16;
/// Shape: the durable id of the attachment record the mount serves, which survives the takeover (A-61).
const RECORD: u64 = 41;
/// Shape: the bytes the tests' file holds.
const BYTES: &[u8] = b"opened before the takeover";

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

fn volume(store: &mut Store) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: JOURNAL_BYTES,
      clock: Box::new(HostClock::default()),
    },
  )
  .unwrap()
}

/// A read-write context for the mount, whose references and opens belong to the durable record [`RECORD`], as a FUSE
/// mount's do (`Attachments::set_owner`): so the image carries them and a recovery gives them back. Each daemon mints
/// its own from its own registry, as each serves the record anew.
fn recorded_cx() -> OpContext {
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
  attachments.set_owner(id, RECORD);
  attachments.context(id).unwrap()
}

fn oid(ino: u64) -> ObjectId {
  ObjectId::new(ino, 0)
}

/// The daemon dies and its successor recovers `image`: a fresh store over the arena's surviving bytes (the anchor's RAM
/// outlives the daemon), the volume rebuilt, and the references settled with the mount's record surviving.
fn taken_over(old: &Store, image: &VolumeImage) -> (Store, Volume) {
  let mut fresh = store();
  let regions = old.content.arena().regions();
  for index in 0..regions {
    let index = u16::try_from(index).unwrap();
    let bytes = old
      .content
      .arena()
      .region(index)
      .map(|region| region.bytes().to_vec())
      .unwrap();
    fresh
      .content
      .arena_mut()
      .region_mut(index)
      .unwrap()
      .bytes_mut()
      .copy_from_slice(&bytes);
  }
  let claims = Claims::prepare(&mut fresh, [image]).unwrap();
  let mut recovered = Volume::from_image(
    &mut fresh,
    image,
    &claims,
    Box::new(HostClock::default()),
    JOURNAL_BYTES,
    None,
  )
  .unwrap();
  recovered
    .settle_recovered_references(&mut fresh, |id| id == RECORD)
    .unwrap();
  (fresh, recovered)
}

/// Before the takeover: creates `f` through the dead daemon's bridge (its open handle, and the lookup reference the FUSE
/// edge takes for the entry it hands the kernel), writes it, opens it `more` times besides, and unlinks it while open.
/// Returns its inode and every handle the kernel holds on it.
fn an_unlinked_open_file(vol: &mut Volume, store: &mut Store, more: usize) -> (u64, Vec<u64>) {
  let cx = recorded_cx();
  let mut dead = slates_bridge_core::new_handle_store();
  let mut bridge = VolumeBridge::attached(VolumeId { bytes: [0; 16] }, vol, store, &mut dead, None);
  let root = bridge.root(&cx).unwrap();
  let (attr, fh) = bridge.create(oid(root), &cx, "f", 0o644, 0).unwrap();
  bridge.reference(oid(attr.ino), &cx).unwrap();
  bridge.write(oid(attr.ino), &cx, 0, BYTES).unwrap();
  let mut handles = vec![fh];
  for _ in 0..more {
    handles.push(bridge.open(oid(attr.ino), &cx, 0).unwrap());
  }
  bridge.unlink(oid(root), &cx, "f").unwrap();
  (attr.ino, handles)
}

/// Whether file `ino` still reads its bytes through the successor's bridge.
fn readable(bridge: &mut VolumeBridge<'_>, cx: &OpContext, ino: u64) -> bool {
  let mut out = Vec::new();
  bridge.read(oid(ino), cx, 0, 64, &mut out).is_ok() && out == BYTES
}

/// Goal item 4 (A-61, AC-3.4). Do: open a file, unlink it while open, publish, take over; release the handle opened
/// before the takeover through the successor, then forget the kernel's lookup of it. Expect the file still readable
/// after the release (the kernel's lookup still holds it), and reclaimed at the forget: the release gave back its open
/// reference. Before the fix the successor released nothing for a handle it never issued, so the file stayed until the
/// mount ended.
#[test]
fn a_release_of_a_handle_opened_before_a_takeover_gives_back_its_reference() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let (f, handles) = an_unlinked_open_file(&mut vol, &mut store, 0);
  let image = vol.to_image(&store, None).unwrap();
  let (mut after, mut recovered) = taken_over(&store, &image);
  let cx = recorded_cx();
  let mut successor = slates_bridge_core::new_handle_store();
  let mut bridge = VolumeBridge::attached(
    VolumeId { bytes: [0; 16] },
    &mut recovered,
    &mut after,
    &mut successor,
    None,
  );
  for fh in &handles {
    bridge.release(oid(f), &cx, *fh).unwrap();
  }
  assert!(
    readable(&mut bridge, &cx, f),
    "the kernel's lookup still holds the file after its handle's release"
  );
  bridge.forget(oid(f), &cx, 1);
  assert!(
    bridge.getattr(oid(f), &cx).is_err(),
    "reclaimed once the kernel lets go: the release gave back the open reference it held"
  );
}

/// Goal item 4: a release never gives back a reference another handle still needs. Do: open a file three times,
/// unlink it, publish, take over; release the handles one by one through the successor, reading after each, then one
/// release more of a handle already released, then forget the lookup. Expect the file readable after every release and
/// the extra one, and reclaimed only at the forget.
#[test]
fn a_release_after_a_takeover_never_gives_back_a_reference_another_handle_still_needs() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let (f, handles) = an_unlinked_open_file(&mut vol, &mut store, 2);
  let image = vol.to_image(&store, None).unwrap();
  let (mut after, mut recovered) = taken_over(&store, &image);
  let cx = recorded_cx();
  let mut successor = slates_bridge_core::new_handle_store();
  let mut bridge = VolumeBridge::attached(
    VolumeId { bytes: [0; 16] },
    &mut recovered,
    &mut after,
    &mut successor,
    None,
  );
  for fh in &handles {
    bridge.release(oid(f), &cx, *fh).unwrap();
    assert!(
      readable(&mut bridge, &cx, f),
      "still readable after releasing {fh}"
    );
  }
  bridge.release(oid(f), &cx, handles[0]).unwrap();
  assert!(
    readable(&mut bridge, &cx, f),
    "a release repeated gives back nothing more"
  );
  bridge.forget(oid(f), &cx, 1);
  assert!(
    bridge.getattr(oid(f), &cx).is_err(),
    "reclaimed at the forget"
  );
}

/// Goal item 4, the window between a publication and the death: a handle opened after the last publication left no
/// reference in the image, so its release must give back nothing the image holds for another. Do: open a file, unlink
/// it, publish; open it once more (unpublished); take over from the image; release the unpublished handle first, then
/// the published one, then forget. Expect the file readable after each release and reclaimed at the forget: two
/// releases gave back exactly the one open the image restored.
#[test]
fn a_handle_opened_after_the_last_publication_gives_back_no_more_than_the_image_restored() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let (f, mut handles) = an_unlinked_open_file(&mut vol, &mut store, 0);
  let image = vol.to_image(&store, None).unwrap();
  {
    let cx = recorded_cx();
    let mut dead = slates_bridge_core::new_handle_store();
    let mut bridge = VolumeBridge::attached(
      VolumeId { bytes: [0; 16] },
      &mut vol,
      &mut store,
      &mut dead,
      None,
    );
    handles.push(bridge.open(oid(f), &cx, 0).unwrap());
  }
  let (mut after, mut recovered) = taken_over(&store, &image);
  let cx = recorded_cx();
  let mut successor = slates_bridge_core::new_handle_store();
  let mut bridge = VolumeBridge::attached(
    VolumeId { bytes: [0; 16] },
    &mut recovered,
    &mut after,
    &mut successor,
    None,
  );
  for fh in handles.iter().rev() {
    bridge.release(oid(f), &cx, *fh).unwrap();
    assert!(
      readable(&mut bridge, &cx, f),
      "still readable after releasing {fh}"
    );
  }
  bridge.forget(oid(f), &cx, 1);
  assert!(
    bridge.getattr(oid(f), &cx).is_err(),
    "reclaimed at the forget: nothing leaked, nothing given back twice"
  );
}

/// Goal item 4 with A-148: the successor's fresh handle store reissues the dead daemon's values, so a handle it opens
/// on the same file can carry the value of a handle opened before the takeover. Do: open a file, unlink it, publish,
/// take over; open it through the successor (non-vacuous: the same value); release that value twice, once for each
/// handle, then forget. Expect the file readable after the first release and reclaimed at the forget: the two releases
/// give back the two opens, whichever handle each is taken for.
#[test]
fn a_reissued_handle_value_on_the_same_file_balances_across_the_takeover() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let (f, handles) = an_unlinked_open_file(&mut vol, &mut store, 0);
  let image = vol.to_image(&store, None).unwrap();
  let (mut after, mut recovered) = taken_over(&store, &image);
  let cx = recorded_cx();
  let mut successor = slates_bridge_core::new_handle_store();
  let mut bridge = VolumeBridge::attached(
    VolumeId { bytes: [0; 16] },
    &mut recovered,
    &mut after,
    &mut successor,
    None,
  );
  let reissued = bridge.open(oid(f), &cx, 0).unwrap();
  assert_eq!(
    reissued, handles[0],
    "non-vacuous: the successor reissued the dead daemon's handle value"
  );
  bridge.release(oid(f), &cx, reissued).unwrap();
  assert!(
    readable(&mut bridge, &cx, f),
    "one of the two handles is still open"
  );
  bridge.release(oid(f), &cx, reissued).unwrap();
  bridge.forget(oid(f), &cx, 1);
  assert!(
    bridge.getattr(oid(f), &cx).is_err(),
    "reclaimed at the forget: both opens given back"
  );
}
