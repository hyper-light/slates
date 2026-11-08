//! Releases of handles a dead daemon issued, served by its successor (goal item 4 of 2026-10-22; A-61, A-148; AC-3.4).
//! A FUSE mount the anchor holds across a daemon's death keeps the kernel's open handles; the successor recovers the
//! volume from its image and serves those handles' releases from a fresh handle store, which never issued them. A
//! release of one must give back the open reference it held, and must never give back a reference something else still
//! needs. An unlinked file made before the takeover is kept until the surviving mount ends, since its kernel may hold a
//! handle no publication counted (U10): each test asserts every reference given back, then the keep, then the reclaim
//! at the mount's end. Each test runs the mount before the takeover, publishes (the image), rebuilds the volume and
//! settles its references as recovery does, naming the mount as held, and serves the rest through the successor's
//! bridge.

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
      Principal::Uid {
        host: slates_db::HostId(0),
        uid: 0,
      },
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
    .settle_recovered_references(&mut fresh, |id| id == RECORD, &[RECORD])
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

/// After the kernel has let go of `f` on a mount that survived the takeover (U10, the supervisor's option 2): every
/// reference was given back (`what`), so the file is kept only for that mount, counted; and it is reclaimed when the
/// mount ends.
fn kept_until_the_mount_ends(vol: &mut Volume, store: &mut Store, f: u64, what: &str) {
  assert_eq!(
    vol.takeover_kept(),
    1,
    "{what}: no reference left but the mount's keep"
  );
  vol
    .sweep_attachment(store, slates_vfs::ids::RefOwner::Attachment(RECORD))
    .unwrap();
  assert_eq!(vol.takeover_kept(), 0, "nothing kept once the mount ended");
  let cx = recorded_cx();
  let mut handles = slates_bridge_core::new_handle_store();
  let mut bridge =
    VolumeBridge::attached(VolumeId { bytes: [0; 16] }, vol, store, &mut handles, None);
  assert!(
    bridge.getattr(oid(f), &cx).is_err(),
    "reclaimed when the mount ends"
  );
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
    readable(&mut bridge, &cx, f),
    "kept after the kernel lets go: the mount that survived may hold a handle no publication counted (U10)"
  );
  drop(bridge);
  kept_until_the_mount_ends(
    &mut recovered,
    &mut after,
    f,
    "reclaimed once the kernel lets go: the release gave back the open reference it held",
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
    readable(&mut bridge, &cx, f),
    "kept after the kernel lets go: the mount that survived may hold a handle no publication counted (U10)"
  );
  drop(bridge);
  kept_until_the_mount_ends(&mut recovered, &mut after, f, "reclaimed at the forget");
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
    readable(&mut bridge, &cx, f),
    "kept after the kernel lets go: the mount that survived may hold a handle no publication counted (U10)"
  );
  drop(bridge);
  kept_until_the_mount_ends(
    &mut recovered,
    &mut after,
    f,
    "reclaimed at the forget: nothing leaked, nothing given back twice",
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
    readable(&mut bridge, &cx, f),
    "kept after the kernel lets go: the mount that survived may hold a handle no publication counted (U10)"
  );
  drop(bridge);
  kept_until_the_mount_ends(
    &mut recovered,
    &mut after,
    f,
    "reclaimed at the forget: both opens given back",
  );
}

/// Goal item 4, the window's other edge (A-154): a handle released after the last publication. The kernel never resends a
/// release its daemon answered, so the release must be published before its reply, or the image would keep its open and
/// a successor restore it with nothing ever to give it back. Do: through the dead daemon, create a file and open it
/// twice more, unlink it, and publish (committed); release the first handle through the same daemon. Expect the volume
/// to await a publication (the reply waits for the barrier). Then take the publication that wait runs, take over from
/// it, release the other two through the successor and forget. Expect the file reclaimed at the forget.
#[test]
fn a_handle_released_after_the_last_publication_leaves_no_open_behind() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let cx = recorded_cx();
  let mut dead = slates_bridge_core::new_handle_store();
  let (f, handles) = {
    let mut bridge = VolumeBridge::attached(
      VolumeId { bytes: [0; 16] },
      &mut vol,
      &mut store,
      &mut dead,
      None,
    );
    let root = bridge.root(&cx).unwrap();
    let (attr, created) = bridge.create(oid(root), &cx, "f", 0o644, 0).unwrap();
    bridge.reference(oid(attr.ino), &cx).unwrap();
    bridge.write(oid(attr.ino), &cx, 0, BYTES).unwrap();
    let first = bridge.open(oid(attr.ino), &cx, 0).unwrap();
    let second = bridge.open(oid(attr.ino), &cx, 0).unwrap();
    bridge.unlink(oid(root), &cx, "f").unwrap();
    (attr.ino, [created, first, second])
  };
  vol.to_image(&store, None).unwrap();
  vol.mark_published(&store);
  {
    let mut bridge = VolumeBridge::attached(
      VolumeId { bytes: [0; 16] },
      &mut vol,
      &mut store,
      &mut dead,
      None,
    );
    bridge.release(oid(f), &cx, handles[0]).unwrap();
    assert!(
      bridge.awaits_publication(),
      "the release of an open the committed publication counts waits for the next publication"
    );
  }
  let image = vol.to_image(&store, None).unwrap();
  vol.mark_published(&store);
  let (mut after, mut recovered) = taken_over(&store, &image);
  let mut successor = slates_bridge_core::new_handle_store();
  let mut bridge = VolumeBridge::attached(
    VolumeId { bytes: [0; 16] },
    &mut recovered,
    &mut after,
    &mut successor,
    None,
  );
  for fh in &handles[1..] {
    bridge.release(oid(f), &cx, *fh).unwrap();
  }
  bridge.forget(oid(f), &cx, 1);
  assert!(
    readable(&mut bridge, &cx, f),
    "kept after the kernel lets go: the mount that survived may hold a handle no publication counted (U10)"
  );
  drop(bridge);
  kept_until_the_mount_ends(
    &mut recovered,
    &mut after,
    f,
    "reclaimed at the forget: the release the dead daemon answered left no open behind",
  );
}

/// A-154, the common case costs nothing: an open and its release both after the last publication were never published,
/// so the release waits for no barrier. Do: create a file and publish (committed); open it and release that handle.
/// Expect the volume not to await a publication.
#[test]
fn a_release_of_an_open_no_publication_counted_waits_for_nothing() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let cx = recorded_cx();
  let mut handles = slates_bridge_core::new_handle_store();
  let f = {
    let mut bridge = VolumeBridge::attached(
      VolumeId { bytes: [0; 16] },
      &mut vol,
      &mut store,
      &mut handles,
      None,
    );
    let root = bridge.root(&cx).unwrap();
    let (attr, created) = bridge.create(oid(root), &cx, "f", 0o644, 0).unwrap();
    bridge.release(oid(attr.ino), &cx, created).unwrap();
    attr.ino
  };
  vol.to_image(&store, None).unwrap();
  vol.mark_published(&store);
  let mut bridge = VolumeBridge::attached(
    VolumeId { bytes: [0; 16] },
    &mut vol,
    &mut store,
    &mut handles,
    None,
  );
  let opened = bridge.open(oid(f), &cx, 0).unwrap();
  bridge.release(oid(f), &cx, opened).unwrap();
  assert!(
    !bridge.awaits_publication(),
    "an open and release since the last publication wait for nothing"
  );
}

/// Shape: a quota small enough that a restart loop reaches it in a few rounds.
const SMALL_QUOTA: u64 = 1 << 16;
/// Shape: the bytes each round's file holds: a sixteenth of the quota.
const ROUND_BYTES: usize = 1 << 12;

/// A volume of [`SMALL_QUOTA`].
fn small_volume(store: &mut Store) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: SMALL_QUOTA },
      journal_bytes: JOURNAL_BYTES,
      clock: Box::new(HostClock::default()),
    },
  )
  .unwrap()
}

/// U10 (the supervisor's option 2): a handle a dead daemon issued after its last publication is unknown to its
/// successor, which used to reclaim the file when it left the namespace, and the handle's next read was refused. Do:
/// make `g`, close and forget it, publish; then, unpublished, look it up and open it again (the kernel's handle no image
/// counts); take over; unlink `g` through the successor; make, close, forget and unlink `h` there too; then end the
/// mount. Expect: `g` still readable after its unlink and counted kept; `h`, made after the takeover, reclaimed at once;
/// and `g` reclaimed when the mount ends.
#[test]
fn a_file_a_handle_no_publication_counted_may_hold_is_kept_until_its_mount_ends() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let cx = recorded_cx();
  let g = {
    let mut dead = slates_bridge_core::new_handle_store();
    let mut bridge = VolumeBridge::attached(
      VolumeId { bytes: [0; 16] },
      &mut vol,
      &mut store,
      &mut dead,
      None,
    );
    let root = bridge.root(&cx).unwrap();
    let (attr, fh) = bridge.create(oid(root), &cx, "g", 0o644, 0).unwrap();
    bridge.write(oid(attr.ino), &cx, 0, BYTES).unwrap();
    bridge.release(oid(attr.ino), &cx, fh).unwrap();
    attr.ino
  };
  let image = vol.to_image(&store, None).unwrap();
  {
    let mut dead = slates_bridge_core::new_handle_store();
    let mut bridge = VolumeBridge::attached(
      VolumeId { bytes: [0; 16] },
      &mut vol,
      &mut store,
      &mut dead,
      None,
    );
    let root = bridge.root(&cx).unwrap();
    let found = bridge.lookup(oid(root), &cx, "g").unwrap();
    bridge.reference(oid(found.ino), &cx).unwrap();
    let _uncounted = bridge.open(oid(found.ino), &cx, 0).unwrap();
  }
  let (mut after, mut recovered) = taken_over(&store, &image);
  let mut successor = slates_bridge_core::new_handle_store();
  let h = {
    let mut bridge = VolumeBridge::attached(
      VolumeId { bytes: [0; 16] },
      &mut recovered,
      &mut after,
      &mut successor,
      None,
    );
    let root = bridge.root(&cx).unwrap();
    bridge.unlink(oid(root), &cx, "g").unwrap();
    assert!(
      readable(&mut bridge, &cx, g),
      "the file a handle no publication counted may hold is kept"
    );
    let (attr, fh) = bridge.create(oid(root), &cx, "h", 0o644, 0).unwrap();
    bridge.release(oid(attr.ino), &cx, fh).unwrap();
    bridge.forget(oid(attr.ino), &cx, 1);
    bridge.unlink(oid(root), &cx, "h").unwrap();
    assert!(
      bridge.getattr(oid(attr.ino), &cx).is_err(),
      "a file made after the takeover is reclaimed at once: its handles are all counted"
    );
    attr.ino
  };
  assert_ne!(g, h);
  assert_eq!(recovered.takeover_kept(), 1, "the kept file is counted");
  kept_until_the_mount_ends(&mut recovered, &mut after, g, "the unlinked file");
}

/// U10's bound (the supervisor's condition). Do: round after round, make a file of [`ROUND_BYTES`] on a volume of
/// [`SMALL_QUOTA`] through the surviving mount, close and forget it, publish, take over and unlink it, so each takeover
/// keeps one more file; stop at the first refusal. Expect: the loop refused `NoSpace` before the kept files' bytes
/// passed the quota, the volume's bytes never past it; and once the mount ends, every kept file reclaimed and a write
/// of the same size admitted again.
#[test]
fn a_restart_loop_cannot_grow_the_kept_files_past_the_quota() {
  let mut store = store();
  let mut vol = small_volume(&mut store);
  let cx = recorded_cx();
  let payload = vec![0x5Au8; ROUND_BYTES];
  let mut refused = None;
  let rounds = usize::try_from(SMALL_QUOTA).unwrap() / ROUND_BYTES * 2;
  for round in 0..rounds {
    let name = format!("round-{round}");
    let made = {
      let mut handles = slates_bridge_core::new_handle_store();
      let mut bridge = VolumeBridge::attached(
        VolumeId { bytes: [0; 16] },
        &mut vol,
        &mut store,
        &mut handles,
        None,
      );
      let root = bridge.root(&cx).unwrap();
      bridge
        .create(oid(root), &cx, &name, 0o644, 0)
        .and_then(|(attr, fh)| {
          let wrote = bridge.write(oid(attr.ino), &cx, 0, &payload);
          bridge.release(oid(attr.ino), &cx, fh)?;
          bridge.forget(oid(attr.ino), &cx, 1);
          wrote.map(|_| ())
        })
    };
    if let Err(error) = made {
      refused = Some((round, error));
      break;
    }
    assert!(
      vol.accounting().referenced_bytes <= vol.capacity_bytes(),
      "round {round}: the volume's bytes are within its quota"
    );
    let image = vol.to_image(&store, None).unwrap();
    let (mut next_store, mut next) = taken_over(&store, &image);
    {
      let mut handles = slates_bridge_core::new_handle_store();
      let mut bridge = VolumeBridge::attached(
        VolumeId { bytes: [0; 16] },
        &mut next,
        &mut next_store,
        &mut handles,
        None,
      );
      let root = bridge.root(&cx).unwrap();
      bridge.unlink(oid(root), &cx, &name).unwrap();
    }
    store = next_store;
    vol = next;
  }
  let (round, error) = refused.expect("the restart loop met the quota");
  eprintln!(
    "refused at round {round} with {error:?}; {} files kept, {} of {} bytes",
    vol.takeover_kept(),
    vol.accounting().referenced_bytes,
    vol.capacity_bytes()
  );
  assert_eq!(
    error,
    slates_vfs::VfsError::NoSpace,
    "refused by the quota, typed"
  );
  assert!(
    vol.takeover_kept() > 0,
    "the files kept across the restarts were held"
  );
  vol
    .sweep_attachment(&mut store, slates_vfs::ids::RefOwner::Attachment(RECORD))
    .unwrap();
  assert_eq!(
    vol.takeover_kept(),
    0,
    "every kept file reclaimed when the mount ends"
  );
  let mut handles = slates_bridge_core::new_handle_store();
  let mut bridge = VolumeBridge::attached(
    VolumeId { bytes: [0; 16] },
    &mut vol,
    &mut store,
    &mut handles,
    None,
  );
  let root = bridge.root(&cx).unwrap();
  let (attr, _) = bridge.create(oid(root), &cx, "after", 0o644, 0).unwrap();
  assert!(
    bridge.write(oid(attr.ino), &cx, 0, &payload).is_ok(),
    "a write of the same size is admitted once the kept files are gone"
  );
}
