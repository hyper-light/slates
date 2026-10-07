//! The shared operation layer's corrected behaviors (§4.6 "POSIX and transparency acceptance"),
//! driven directly against an in-memory scratch volume on every host, no mount. These are the
//! A-9 audit fixes folded into the seam: `setattr` honors every field it is given instead of
//! acknowledging an ignored one (BUG-8), `rename` honors or refuses its `renameat2` flags instead
//! of dropping them (BUG-10), and open handles are reused from a bounded generational arena
//! instead of growing without end (BUG-4).
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_core::{
  Attachments, Bridge, ObjectId, OpContext, RenameFlags, Rights, SetAttr, View, VolumeBridge,
};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::error::VfsError;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::{BudgetGrowth, Quota};
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Shape: the page and a small arena for the test volume.
const PAGE: usize = 4096;
const REGION_PAGES: usize = 4096;

/// A context minted through the attachment registry (the only way to build one).
fn context(view: View, rights: Rights) -> OpContext {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      VolumeId { bytes: [0; 16] },
      view,
      Principal::Uid { uid: 0 },
      rights,
    )
    .unwrap();
  attachments.context(id).unwrap()
}

/// A read-write current-view context.
fn rw_cx() -> OpContext {
  context(
    View::Current,
    Rights {
      read: true,
      write: true,
    },
  )
}

/// A read-write current-view context whose enrolled subject is the Unix user `uid` — the mounting
/// user a request runs as (§4.13), so an object it creates is owned by that user rather than root.
fn rw_cx_as(uid: u32) -> OpContext {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      VolumeId { bytes: [0; 16] },
      View::Current,
      Principal::Uid { uid },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  attachments.context(id).unwrap()
}

/// Two read-write current-view contexts for two distinct attachments of the same volume, minted
/// from one registry so they hold different attachment keys (two mounts of one volume).
fn two_rw_contexts() -> (OpContext, OpContext) {
  let mut attachments = Attachments::new();
  let rights = Rights {
    read: true,
    write: true,
  };
  let a = attachments
    .attach(
      VolumeId { bytes: [0; 16] },
      View::Current,
      Principal::Uid { uid: 0 },
      rights,
    )
    .unwrap();
  let b = attachments
    .attach(
      VolumeId { bytes: [0; 16] },
      View::Current,
      Principal::Uid { uid: 0 },
      rights,
    )
    .unwrap();
  (
    attachments.context(a).unwrap(),
    attachments.context(b).unwrap(),
  )
}

/// The object at inode `ino` (generation zero — the volume core does not yet track generations, so
/// every live object's generation is zero and identity is the never-reused inode number, D-4).
fn oid(ino: u64) -> ObjectId {
  ObjectId::new(ino, 0)
}

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
      journal_bytes: 1 << 16,
      clock: Box::new(HostClock::default()),
    },
  )
  .unwrap()
}

/// A `setattr` of ownership and times takes effect and is reported, and a later single-field
/// `setattr` leaves the other fields alone — no field is ignored while success is returned (BUG-8).
#[test]
fn setattr_honors_ownership_and_times() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();
  let (attr, _fh) = bridge.create(oid(root), &cx, "f", 0o644, 0).unwrap();
  let ino = attr.ino;

  let changed = bridge
    .setattr(
      oid(ino),
      &cx,
      SetAttr {
        uid: Some(501),
        gid: Some(20),
        atime: Some(111),
        mtime: Some(222),
        ..SetAttr::default()
      },
    )
    .unwrap();
  assert_eq!(
    (changed.uid, changed.gid, changed.atime, changed.mtime),
    (501, 20, 111, 222),
    "the reply describes the applied ownership and times"
  );
  // A fresh getattr shows the change stuck.
  let stat = bridge.getattr(oid(ino), &cx).unwrap();
  assert_eq!(
    (stat.uid, stat.gid, stat.atime, stat.mtime),
    (501, 20, 111, 222)
  );

  // A mode-only setattr changes the mode and nothing else.
  bridge
    .setattr(
      oid(ino),
      &cx,
      SetAttr {
        mode: Some(0o600),
        ..SetAttr::default()
      },
    )
    .unwrap();
  let after = bridge.getattr(oid(ino), &cx).unwrap();
  assert_eq!(after.mode, 0o600);
  assert_eq!(after.uid, 501, "a mode-only setattr leaves the uid alone");
  assert_eq!(
    after.mtime, 222,
    "a mode-only setattr leaves the mtime alone"
  );
}

/// An object created through the bridge is owned by the mounting user — the request subject's uid —
/// not root, and takes its parent directory's group (the BSD/macOS create rule a mount must present
/// transparently). Before the fix a created inode kept the volume core's born default (uid 0, gid
/// 0), so a file an ordinary user made through the mount listed as `root wheel` regardless of who
/// made it (§4.13 "each request runs as the mounting user"; the root:wheel mount bug). This drives
/// all three creating verbs — `create`, `mkdir`, `symlink` — since they shared the gap.
#[test]
fn a_created_object_is_owned_by_the_mounting_user_and_its_parent_group() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx_as(501);
  let root = bridge.root(&cx).unwrap();

  // A directory the user makes is owned by the user; give it a distinct group so a child proves the
  // group is inherited from the parent (the subject carries no group of its own).
  let dir = bridge.mkdir(oid(root), &cx, "d", 0o755).unwrap();
  assert_eq!(
    dir.uid, 501,
    "a created directory is owned by the mounting user, not root"
  );
  bridge
    .setattr(
      oid(dir.ino),
      &cx,
      SetAttr {
        gid: Some(20),
        ..SetAttr::default()
      },
    )
    .unwrap();

  let (file, _fh) = bridge.create(oid(dir.ino), &cx, "f", 0o644, 0).unwrap();
  assert_eq!(
    file.uid, 501,
    "a created file is owned by the mounting user, not root"
  );
  assert_eq!(
    file.gid, 20,
    "and takes its parent directory's group (BSD/macOS create semantics)"
  );

  let link = bridge.symlink(oid(dir.ino), &cx, "l", "f").unwrap();
  assert_eq!(
    (link.uid, link.gid),
    (501, 20),
    "a created symlink is owned by the user and grouped by its parent too"
  );
}

/// A-107 (condition 4, escapes adversarially tested): an agent (uid 1000) plants links in its volume, one to a host
/// path, one climbing above the volume root, one that stays inside. Do follow each as another user (uid 0, the
/// privileged operator a planted link would steer) and as the agent. Expect the two that leave refused
/// `LinkProtected` to the other user and answered to the agent, the inside link answered to both, and a read of the
/// text with no Unix caller (the SDK, which follows nothing) answered for every link.
#[test]
fn a_link_out_of_the_volume_resolves_only_for_its_owner() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  // The transport names each request's Unix caller (the FUSE header, NFS AUTH_SYS), as `owner_uid`.
  let mut agent = rw_cx_as(1000);
  agent.owner_uid = Some(1000);
  let mut operator = rw_cx_as(0);
  operator.owner_uid = Some(0);
  let root = bridge.root(&agent).unwrap();
  let dir = bridge.mkdir(oid(root), &agent, "d", 0o755).unwrap();
  let host = bridge
    .symlink(oid(dir.ino), &agent, "cron", "/etc/cron.d/x")
    .unwrap();
  let above = bridge
    .symlink(oid(dir.ino), &agent, "up", "../../escape")
    .unwrap();
  let inside = bridge
    .symlink(oid(dir.ino), &agent, "sib", "../d/f")
    .unwrap();
  for link in [host.ino, above.ino] {
    assert_eq!(
      bridge.readlink(oid(link), &operator),
      Err(VfsError::LinkProtected)
    );
    assert!(
      bridge.readlink(oid(link), &agent).is_ok(),
      "the owner follows its own link"
    );
  }
  assert_eq!(
    bridge.readlink(oid(inside.ino), &operator).unwrap(),
    "../d/f"
  );
  let mut sdk = rw_cx_as(0);
  sdk.owner_uid = None;
  assert_eq!(
    bridge.readlink(oid(host.ino), &sdk).unwrap(),
    "/etc/cron.d/x"
  );
}

/// When the request's credential names a group (`OpContext::owner_gid`, an NFS `AUTH_SYS` gid), a
/// created object takes *that* group, not its parent directory's — matching what a native NFS server
/// stamps, so a file the mounting user makes lists as their own group rather than the volume root's
/// `wheel`. This is the credential-group half of the root:wheel fix; the parent-inherited half is
/// covered above (the `None` case).
#[test]
fn a_created_object_takes_the_request_group_when_the_credential_names_one() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  // The mounting user is uid 501 with group 20 (the credential's gid), the export edge overlaying the
  // group onto the context (as the daemon's NFS path does per request).
  let mut cx = rw_cx_as(501);
  cx.owner_gid = Some(20);
  let root = bridge.root(&cx).unwrap();

  // The parent directory (the volume root) has group 0, but the credential names 20: the created file
  // takes 20, proving the request's group wins over the parent's when the credential supplies it.
  let (file, _fh) = bridge.create(oid(root), &cx, "f", 0o644, 0).unwrap();
  assert_eq!(
    (file.uid, file.gid),
    (501, 20),
    "a created file takes the mounting user's uid and their credential group, not the parent's"
  );
  // A directory and a symlink take it too.
  let dir = bridge.mkdir(oid(root), &cx, "d", 0o755).unwrap();
  let link = bridge.symlink(oid(root), &cx, "l", "f").unwrap();
  assert_eq!((dir.uid, dir.gid), (501, 20));
  assert_eq!((link.uid, link.gid), (501, 20));
}

/// AC-3.10 / §4.6: one admitted mount can serve multiple Unix callers. All creation kinds
/// take each request's owner and group while the attachment's enrolled principal stays fixed.
#[test]
fn every_created_kind_takes_the_current_requests_unix_owner() {
  use slates_vfs::inode::Kind;
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let mut cx = rw_cx_as(0);
  let root = oid(bridge.root(&cx).unwrap());
  for uid in [1001, 1002] {
    cx.owner_uid = Some(uid);
    cx.owner_gid = Some(uid);
    let dir = bridge.mkdir(root, &cx, &format!("{uid}"), 0o700).unwrap();
    let parent = oid(dir.ino);
    let (file, handle) = bridge.create(parent, &cx, "file", 0o600, 0).unwrap();
    let link = bridge.symlink(parent, &cx, "link", "file").unwrap();
    let fifo = bridge
      .mknod(parent, &cx, "fifo", 0o600, Kind::Fifo)
      .unwrap();
    let socket = bridge
      .mknod(parent, &cx, "socket", 0o600, Kind::Socket)
      .unwrap();
    for node in [dir, file, link, fifo, socket] {
      let attr = bridge.getattr(oid(node.ino), &cx).unwrap();
      assert_eq!((attr.uid, attr.gid), (uid, uid), "{:?}", node.kind);
    }
    bridge.release(oid(file.ino), &cx, handle).unwrap();
  }
}

/// `RENAME_NOREPLACE` fails onto an existing name and succeeds onto a free one — the flag is
/// honored, not dropped and turned into an ordinary replacing rename (BUG-10).
#[test]
fn rename_noreplace_refuses_an_existing_destination() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();
  bridge.create(oid(root), &cx, "a", 0o644, 0).unwrap();
  bridge.create(oid(root), &cx, "b", 0o644, 0).unwrap();

  let noreplace = RenameFlags {
    no_replace: true,
    exchange: false,
  };
  assert!(
    bridge
      .rename(oid(root), oid(root), &cx, "a", "b", noreplace)
      .is_err(),
    "NOREPLACE onto an existing name is refused"
  );
  assert!(
    bridge
      .rename(oid(root), oid(root), &cx, "a", "c", noreplace)
      .is_ok(),
    "NOREPLACE onto a free name succeeds"
  );
  assert!(
    bridge.lookup(oid(root), &cx, "b").is_ok(),
    "b was not replaced"
  );
  assert!(bridge.lookup(oid(root), &cx, "a").is_err(), "a moved away");
  assert!(
    bridge.lookup(oid(root), &cx, "c").is_ok(),
    "c is the moved file"
  );
}

/// `RENAME_EXCHANGE` is refused, not silently downgraded to a plain rename that would replace the
/// destination and lose it (BUG-10).
#[test]
fn rename_exchange_is_refused_not_downgraded() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();
  bridge.create(oid(root), &cx, "x", 0o644, 0).unwrap();
  bridge.create(oid(root), &cx, "y", 0o644, 0).unwrap();

  let exchange = RenameFlags {
    no_replace: false,
    exchange: true,
  };
  assert!(
    bridge
      .rename(oid(root), oid(root), &cx, "x", "y", exchange)
      .is_err(),
    "EXCHANGE is refused, not performed as a plain rename"
  );
  assert!(bridge.lookup(oid(root), &cx, "x").is_ok(), "x is untouched");
  assert!(bridge.lookup(oid(root), &cx, "y").is_ok(), "y is untouched");
}

/// A released handle is stale and its slot is reused, so repeated open/close does not grow the
/// handle table (BUG-4).
#[test]
fn open_handles_are_reused_so_memory_stays_bounded() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();
  let (attr, create_fh) = bridge.create(oid(root), &cx, "h", 0o644, 0).unwrap();
  let ino = attr.ino;
  bridge.release(oid(ino), &cx, create_fh).unwrap();

  let obj = oid(ino);
  let fh1 = bridge.open(oid(ino), &cx, 0).unwrap();
  bridge.release(oid(ino), &cx, fh1).unwrap();
  let fh2 = bridge.open(oid(ino), &cx, 0).unwrap();
  let mut out = Vec::new();
  // A read is addressed by inode now, not the handle; the handle table only holds per-open state.
  assert!(
    bridge.read(obj, &cx, 0, 16, &mut out).is_ok(),
    "a read is addressed by inode"
  );
  bridge.release(oid(ino), &cx, fh2).unwrap();

  // A thousand open/release cycles leak nothing: the slab reuses one slot.
  let before = format!("{bridge:?}");
  for _ in 0..1000 {
    let fh = bridge.open(oid(ino), &cx, 0).unwrap();
    bridge.release(oid(ino), &cx, fh).unwrap();
  }
  assert_eq!(
    before,
    format!("{bridge:?}"),
    "open/release cycles reuse slots and leak no handles"
  );
}

/// A scratch volume with one closed file and the lent-in handle map (`attached`), whose occupancy
/// and allocated slots the AC-3.12 tests observe from outside: `(store, volume, handles, root,
/// the file's inode)`.
fn lent_handles_fixture() -> (Store, Volume, slates_mem::Slab<u64>, u64, u64) {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut handles = slates_bridge_core::new_handle_store();
  let cx = rw_cx();
  let (root, ino) = {
    let mut bridge = VolumeBridge::attached(
      VolumeId { bytes: [0; 16] },
      &mut vol,
      &mut store,
      &mut handles,
      None,
    );
    let root = bridge.root(&cx).unwrap();
    let (attr, fh) = bridge.create(oid(root), &cx, "h", 0o644, 0).unwrap();
    bridge.release(oid(attr.ino), &cx, fh).unwrap();
    (root, attr.ino)
  };
  (store, vol, handles, root, ino)
}

/// AC-3.12/T-3.15: open/close beyond the handle arena's capacity in *total operations* reuses
/// generational slots with bounded memory — more cycles than the arena has slots leave one
/// segment allocated (well under the capacity) and no handle open.
#[test]
fn open_close_beyond_the_arena_capacity_reuses_slots_with_bounded_memory() {
  let (mut store, mut vol, mut handles, _root, ino) = lent_handles_fixture();
  let capacity = handles.max_slots();
  let cx = rw_cx();
  {
    let mut bridge = VolumeBridge::attached(
      VolumeId { bytes: [0; 16] },
      &mut vol,
      &mut store,
      &mut handles,
      None,
    );
    for _ in 0..=capacity {
      let fh = bridge.open(oid(ino), &cx, 0).unwrap();
      bridge.release(oid(ino), &cx, fh).unwrap();
    }
  }
  assert_eq!(handles.len(), 0, "nothing left open");
  assert!(
    handles.slots() < capacity,
    "{} slots allocated for one concurrent open across {} operations: bounded memory",
    handles.slots(),
    capacity + 1
  );
}

/// AC-3.12/T-3.15: a stale handle from a reused slot never acts on the slot's new holder — open
/// A, release it, open B (the same slot, a new generation); releasing A's handle again must not
/// drop B's open reference, which still keeps the inode alive across an unlink, and B's own
/// release reclaims it.
#[test]
fn a_stale_handle_from_a_reused_slot_never_acts_on_the_slots_new_holder() {
  let (mut store, mut vol, mut handles, root, ino) = lent_handles_fixture();
  let cx = rw_cx();
  let mut bridge = VolumeBridge::attached(
    VolumeId { bytes: [0; 16] },
    &mut vol,
    &mut store,
    &mut handles,
    None,
  );
  let stale = bridge.open(oid(ino), &cx, 0).unwrap();
  bridge.release(oid(ino), &cx, stale).unwrap();
  let live = bridge.open(oid(ino), &cx, 0).unwrap();
  assert_ne!(stale, live, "the reused slot carries a new generation");
  bridge.release(oid(ino), &cx, stale).unwrap();
  bridge.unlink(oid(root), &cx, "h").unwrap();
  assert!(
    bridge.getattr(oid(ino), &cx).is_ok(),
    "the stale release touched nothing: the live open still pins the unlinked inode"
  );
  bridge.release(oid(ino), &cx, live).unwrap();
  assert!(
    bridge.getattr(oid(ino), &cx).is_err(),
    "reclaimed by the live release"
  );
}

/// AC-3.12/T-3.15: at the arena's bound the next open is a typed refusal (the FUSE edge's
/// `EMFILE`), one release makes room for exactly one more, and releasing everything empties the
/// map.
#[test]
fn an_open_past_the_handle_bound_is_a_typed_refusal_lifted_by_one_release() {
  let (mut store, mut vol, mut handles, _root, ino) = lent_handles_fixture();
  let capacity = handles.max_slots();
  let cx = rw_cx();
  {
    let mut bridge = VolumeBridge::attached(
      VolumeId { bytes: [0; 16] },
      &mut vol,
      &mut store,
      &mut handles,
      None,
    );
    let mut open = Vec::with_capacity(capacity);
    while open.len() < capacity {
      open.push(bridge.open(oid(ino), &cx, 0).unwrap());
    }
    assert!(
      matches!(
        bridge.open(oid(ino), &cx, 0),
        Err(VfsError::Memory(slates_mem::MemError::SlabFull { .. }))
      ),
      "the open past the bound is a typed refusal"
    );
    let freed = open.pop().unwrap();
    bridge.release(oid(ino), &cx, freed).unwrap();
    open.push(bridge.open(oid(ino), &cx, 0).unwrap());
    for fh in open.drain(..) {
      bridge.release(oid(ino), &cx, fh).unwrap();
    }
  }
  assert_eq!(handles.len(), 0);
}

/// A write under a read-only attachment is refused before any effect, though a read is allowed
/// (Ada review: authorization of an actual write, not only ACCESS reporting).
#[test]
fn a_write_is_refused_on_a_read_only_attachment() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();
  let (attr, _fh) = bridge.create(oid(root), &cx, "f", 0o644, 0).unwrap();
  let obj = oid(attr.ino);
  let read_only = context(
    View::Current,
    Rights {
      read: true,
      write: false,
    },
  );
  assert!(
    bridge.write(obj, &read_only, 0, b"x").is_err(),
    "a read-only attachment cannot write"
  );
  let mut out = Vec::new();
  assert!(
    bridge.read(obj, &read_only, 0, 16, &mut out).is_ok(),
    "but it can read"
  );
}

/// A write against a pinned immutable view is refused (a green-volume or snapshot attachment is
/// read-only, §4.16).
#[test]
fn a_write_is_refused_on_a_pinned_view() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();
  let (attr, _fh) = bridge.create(oid(root), &cx, "f", 0o644, 0).unwrap();
  let obj = oid(attr.ino);
  let pinned = context(
    View::Version(1),
    Rights {
      read: true,
      write: true,
    },
  );
  assert!(
    bridge.write(obj, &pinned, 0, b"x").is_err(),
    "a pinned view cannot write"
  );
}

/// Unlink-while-open works end to end through the bridge via the OPEN reference (the handle a
/// create holds): a created file survives an unlink and still serves reads, and is reclaimed once
/// its open handle is released. The open reference is transport-neutral — both FUSE and NFS hold an
/// open handle across a create — so this needs no lookup reference.
#[test]
fn an_open_file_survives_unlink_through_the_bridge() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();
  // create takes an open reference (the handle); it takes no lookup reference (§3).
  let (attr, fh) = bridge.create(oid(root), &cx, "f", 0o644, 0).unwrap();
  let ino = attr.ino;
  let obj = oid(ino);
  bridge.write(obj, &cx, 0, b"hello").unwrap();

  bridge.unlink(oid(root), &cx, "f").unwrap();
  assert!(
    bridge.lookup(oid(root), &cx, "f").is_err(),
    "the name is unlinked"
  );
  let mut out = Vec::new();
  bridge.read(obj, &cx, 0, 16, &mut out).unwrap();
  assert_eq!(
    &out, b"hello",
    "the open file keeps its content after unlink (the open reference pins it)"
  );

  // Releasing the open handle drops the last reference, so the unlinked inode is reclaimed.
  bridge.release(oid(ino), &cx, fh).unwrap();
  assert!(
    bridge.getattr(oid(ino), &cx).is_err(),
    "reclaimed once the open reference is released"
  );
  // A FORGET of an inode no reference was held on (or one already reclaimed) is a safe no-op.
  bridge.forget(oid(ino), &cx, 1);
  assert!(bridge.getattr(oid(ino), &cx).is_err(), "still gone");
}

/// The FUSE model: a lookup reference the edge takes explicitly (`bridge.reference`) pins an object
/// across the release of its open handle, until `forget` drops it — this is why the kernel's node
/// id stays valid after a file is closed. The reference is taken by the transport, not implicitly by
/// the shared operation (§3).
#[test]
fn a_lookup_reference_pins_across_release_until_forget() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();
  let (attr, fh) = bridge.create(oid(root), &cx, "f", 0o644, 0).unwrap();
  let ino = attr.ino;
  // The edge takes a lookup reference (what FUSE does after CREATE/LOOKUP).
  bridge.reference(oid(ino), &cx).unwrap();
  // Releasing the open handle leaves the lookup reference holding the object.
  bridge.release(oid(ino), &cx, fh).unwrap();
  bridge.unlink(oid(root), &cx, "f").unwrap();
  assert!(
    bridge.getattr(oid(ino), &cx).is_ok(),
    "the lookup reference keeps the unlinked inode alive after release"
  );
  // FORGET drops the lookup reference; now nlink == 0 and references == 0, so it is reclaimed.
  bridge.forget(oid(ino), &cx, 1);
  assert!(
    bridge.getattr(oid(ino), &cx).is_err(),
    "reclaimed once the lookup reference is forgotten"
  );
}

/// The NFS model, and the fix for the lookup-reference leak (docs/bugs/2026-09-05-...): a transport
/// that takes NO lookup reference does not pin an object past its open handle. After the handle is
/// released, an unlink reclaims immediately — nothing lingers. Before the fix the shared
/// lookup/create referenced implicitly, so this inode would stay alive forever with no way to
/// forget it (NFS has no FORGET); this test would then have failed at the final assertion.
#[test]
fn a_transport_without_a_reference_does_not_pin_after_release() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();
  // Create then release, taking no lookup reference (the NFS path resolves objects but references
  // none). The open reference the create took is the only one, and release drops it.
  let (attr, fh) = bridge.create(oid(root), &cx, "f", 0o644, 0).unwrap();
  let ino = attr.ino;
  bridge.release(oid(ino), &cx, fh).unwrap();
  // A later lookup (as NFS does per request) also takes no reference.
  bridge.lookup(oid(root), &cx, "f").unwrap();
  // Unlink: nlink == 0 and references == 0, so the inode is reclaimed at once — not leaked.
  bridge.unlink(oid(root), &cx, "f").unwrap();
  assert!(
    bridge.getattr(oid(ino), &cx).is_err(),
    "an unreferenced inode is reclaimed on unlink, not pinned forever"
  );
}

/// The shared readdir synthesizes "." (the directory) and ".." (its parent) before the children,
/// so every transport lists them identically (R8, one code path): a subdirectory's readdir names
/// itself as ".", the root as "..", then its own entries; the root's ".." is the root itself.
#[test]
fn readdir_synthesizes_dot_and_dotdot() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();
  let sub = bridge.mkdir(oid(root), &cx, "sub", 0o755).unwrap();
  bridge.create(oid(sub.ino), &cx, "f", 0o644, 0).unwrap();

  let entries = bridge.readdir(oid(sub.ino), &cx, 0, 0, usize::MAX).unwrap();
  let by_name: std::collections::BTreeMap<&str, u64> =
    entries.iter().map(|e| (e.name.as_str(), e.ino)).collect();
  assert_eq!(
    by_name.get("."),
    Some(&sub.ino),
    "'.' names the directory itself"
  );
  assert_eq!(by_name.get(".."), Some(&root), "'..' names the parent");
  assert!(by_name.contains_key("f"), "the children follow . and ..");

  // The root has no parent, so its ".." is the root itself (POSIX).
  let root_entries = bridge.readdir(oid(root), &cx, 0, 0, usize::MAX).unwrap();
  let root_dotdot = root_entries.iter().find(|e| e.name == "..").unwrap();
  assert_eq!(root_dotdot.ino, root, "the root's '..' is the root itself");
}

/// The teardown sweep through the bridge (the FUSE unmount path — no per-inode FORGET): files a
/// mount holds open or referenced are released when its attachment is swept, and an
/// unlinked-but-referenced inode is reclaimed. A second attachment's references are untouched, so
/// the sweep never reclaims an object another mount is serving.
#[test]
fn a_teardown_sweep_releases_one_attachments_references_through_the_bridge() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let (a, b) = two_rw_contexts();
  let root = bridge.root(&a).unwrap();

  // Attachment `a` creates the file (an open reference attributed to `a`); attachment `b` also
  // references it (a lookup reference attributed to `b`).
  let (attr, _fh) = bridge.create(oid(root), &a, "f", 0o644, 0).unwrap();
  let ino = attr.ino;
  bridge.reference(oid(ino), &b).unwrap();
  bridge.unlink(oid(root), &a, "f").unwrap();

  // Tear down attachment `a`: its open reference is swept, but `b` still holds the inode alive.
  bridge.sweep_attachment(&a).unwrap();
  assert!(
    bridge.getattr(oid(ino), &b).is_ok(),
    "attachment b still holds the inode after a's teardown"
  );

  // Tear down attachment `b`: no reference remains, so the unlinked inode is reclaimed.
  bridge.sweep_attachment(&b).unwrap();
  assert!(
    bridge.getattr(oid(ino), &a).is_err(),
    "reclaimed once the last attachment tears down"
  );
  // A second sweep of a drained attachment releases nothing (idempotent).
  bridge.sweep_attachment(&a).unwrap();
}

/// statfs reports the volume's real capacity and free space, not an invented multiple of the used
/// amount (audit BUG-9): the total is the quota ceiling, the used reflects the written bytes, and
/// the free is the remaining quota. The previous `blocks = 2 * used` / `free = used` would fail
/// this — its total tracked usage instead of the fixed 1 GiB capacity.
#[test]
fn statfs_reports_the_real_capacity_not_an_invented_figure() {
  const QUOTA_BYTES: u64 = 1 << 30; // the test volume()'s Quota::Bounded limit
  const BLOCK: u64 = 4096;
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();

  // An empty volume: the total is the real quota, and (nearly) all of it is free.
  let empty = bridge.statfs(oid(root), &cx).unwrap();
  assert_eq!(
    u64::from(empty.bsize),
    BLOCK,
    "the block size is the volume's page unit"
  );
  assert_eq!(
    empty.blocks,
    QUOTA_BYTES / BLOCK,
    "the total is the real quota ceiling, not a multiple of the used amount"
  );
  assert!(
    empty.bfree > (QUOTA_BYTES / BLOCK) - 16,
    "an empty volume reports nearly all of its quota free, got {}",
    empty.bfree
  );

  // After writing, used rises and free falls by the same amount — both against the fixed total.
  let (attr, _fh) = bridge.create(oid(root), &cx, "f", 0o644, 0).unwrap();
  let payload = vec![0u8; 200 * 1024]; // 200 KiB, several blocks
  bridge.write(oid(attr.ino), &cx, 0, &payload).unwrap();
  let after = bridge.statfs(oid(root), &cx).unwrap();
  assert_eq!(
    after.blocks, empty.blocks,
    "the total capacity does not change with usage"
  );
  assert!(
    after.bfree < empty.bfree,
    "free space fell after the write ({} -> {})",
    empty.bfree,
    after.bfree
  );
  // The drop in free blocks reflects the written bytes (at least the payload's worth).
  assert!(
    empty.bfree - after.bfree >= (200 * 1024) / BLOCK,
    "free fell by at least the written blocks"
  );
}

/// statfs of a dynamic volume reports only the capacity the shard can honour (§4.6 "logical
/// capacity and remaining space that the physical claim can honor"), not the volume's bare
/// ceiling: a volume allowed to grow to 1 GiB on a shard whose budget is the 16 MiB test arena
/// reports a total within that budget, its free space falls with its own writes, and it falls
/// again when another dynamic volume takes capacity from the same shard budget — the space it
/// could no longer honour. Failed at `9a960bb`: the total was the 1 GiB ceiling (a `df` the
/// shard could not back).
#[test]
fn statfs_of_a_dynamic_volume_reports_only_what_the_shard_can_honour() {
  const BLOCK: u64 = 4096;
  const CEILING: u64 = 1 << 30;
  let mut store = store();
  let budget_capacity = store.budget.capacity();
  assert!(
    budget_capacity < CEILING,
    "the fixture's arena is far below the ceiling"
  );
  let dynamic = |prefix: u16| VolumeConfig {
    prefix,
    names: NameEquivalence::Exact,
    quota: Quota::Dynamic {
      max: CEILING,
      source: Box::new(BudgetGrowth),
      granted: 0,
      denied: 0,
    },
    journal_bytes: 1 << 16,
    clock: Box::new(HostClock::default()),
  };
  let mut first = Volume::create(&mut store, dynamic(1)).unwrap();
  let mut second = Volume::create(&mut store, dynamic(2)).unwrap();
  let cx = rw_cx();

  let (empty, root) = {
    let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut first, &mut store);
    let root = bridge.root(&cx).unwrap();
    (bridge.statfs(oid(root), &cx).unwrap(), root)
  };
  assert!(
    empty.blocks * BLOCK <= budget_capacity,
    "the total is within what the shard can honour ({} blocks, budget {budget_capacity})",
    empty.blocks
  );
  assert!(
    empty.bavail * BLOCK <= budget_capacity,
    "and so is the free space"
  );

  // The volume's own write takes from its free space.
  let after_write = {
    let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut first, &mut store);
    let (attr, _fh) = bridge.create(oid(root), &cx, "f", 0o644, 0).unwrap();
    bridge
      .write(oid(attr.ino), &cx, 0, &vec![0u8; 200 * 1024])
      .unwrap();
    bridge.statfs(oid(root), &cx).unwrap()
  };
  assert!(
    empty.bavail - after_write.bavail >= (200 * 1024) / BLOCK,
    "free fell by at least the written blocks"
  );

  // Another dynamic volume takes 4 MiB of the same shard budget: the first can no longer honour it.
  {
    let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut second, &mut store);
    let other_root = bridge.root(&cx).unwrap();
    let (attr, _fh) = bridge.create(oid(other_root), &cx, "g", 0o644, 0).unwrap();
    bridge
      .write(oid(attr.ino), &cx, 0, &vec![0u8; 4 << 20])
      .unwrap();
  }
  let after_other = {
    let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut first, &mut store);
    bridge.statfs(oid(root), &cx).unwrap()
  };
  assert!(
    after_write.bavail - after_other.bavail >= (4 << 20) / BLOCK,
    "another volume's growth reduced what this one can honour ({} -> {} blocks)",
    after_write.bavail,
    after_other.bavail
  );
  assert!(
    after_other.blocks < after_write.blocks,
    "the reported total shrank with it, never a fixed ceiling"
  );
}

/// statfs reports backed inode availability (§4.2: "statfs includes backed inode availability"), not
/// zero: `files` is the volume's inode allowance and `ffree` is the allowance less its live inodes —
/// the same pair `next_no` admits creates against — so a create rises `used` and falls `ffree`, and
/// an unlink returns the credit. The previous `files = ffree = 0` would fail this.
#[test]
fn statfs_reports_backed_inode_availability() {
  let mut store = store();
  let mut vol = volume(&mut store);
  vol.set_inode_allowance(5).unwrap(); // the root and four more
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();

  let empty = bridge.statfs(oid(root), &cx).unwrap();
  assert_eq!(empty.files, 5, "the total inodes is the volume's allowance");
  assert_eq!(
    empty.ffree, 4,
    "four inodes free: the allowance less the one live root"
  );

  let (a, fh_a) = bridge.create(oid(root), &cx, "a", 0o644, 0).unwrap();
  bridge.create(oid(root), &cx, "b", 0o644, 0).unwrap();
  let two = bridge.statfs(oid(root), &cx).unwrap();
  assert_eq!(two.files, 5, "the allowance does not change with usage");
  assert_eq!(two.ffree, 2, "two creates took two inodes");

  // Closing the handle and unlinking with no other reference reclaims the inode and returns its
  // credit (an unlinked-but-open file would stay a live orphan, correctly, until its handle closes).
  bridge.release(oid(a.ino), &cx, fh_a).unwrap();
  bridge.unlink(oid(root), &cx, "a").unwrap();
  let freed = bridge.statfs(oid(root), &cx).unwrap();
  assert_eq!(
    freed.ffree, 3,
    "an unlink of a closed file returns the inode credit"
  );
}

/// A hard link creates a second name for one inode: both names resolve to it and its link count
/// rises; a directory cannot be hard-linked (audit BUG-7, the volume core's link over the seam).
#[test]
fn link_creates_a_second_name_for_the_same_inode() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();
  let (attr, _fh) = bridge.create(oid(root), &cx, "original", 0o644, 0).unwrap();
  let ino = attr.ino;

  let linked = bridge.link(oid(ino), oid(root), &cx, "alias").unwrap();
  assert_eq!(linked.ino, ino, "the link resolves to the same inode");
  assert_eq!(linked.nlink, 2, "the link count is now two");
  assert_eq!(bridge.lookup(oid(root), &cx, "original").unwrap().ino, ino);
  assert_eq!(bridge.lookup(oid(root), &cx, "alias").unwrap().ino, ino);

  // A directory cannot be hard-linked.
  let sub = bridge.mkdir(oid(root), &cx, "d", 0o755).unwrap();
  assert!(
    bridge.link(oid(sub.ino), oid(root), &cx, "dlink").is_err(),
    "a directory cannot be hard-linked"
  );
}

/// Shape: the files of the paged directory — past the small form's cutover (16), so it is the indexed tree,
/// and within the fixture's inode table (256).
const PAGED_FILES: usize = 200;
/// Shape: the entries a page asks for — small, so the directory takes dozens of pages.
const PAGE_LIMIT: usize = 7;

/// Lists `dir` page by page from cookie 0, each page at most `limit` entries (plus a group sharing the last
/// cookie), running `between` after each page; the names in the order returned.
fn paged(
  bridge: &mut VolumeBridge<'_>,
  cx: &OpContext,
  dir: u64,
  limit: usize,
  mut between: impl FnMut(&mut VolumeBridge<'_>, &[slates_bridge_core::DirEntry]),
) -> Vec<String> {
  let mut cookie = 0;
  let mut names = Vec::new();
  loop {
    let page = bridge.readdir(oid(dir), cx, 0, cookie, limit).unwrap();
    let Some(last) = page.last() else {
      return names;
    };
    cookie = last.cookie;
    let shared = page.iter().filter(|e| e.cookie == last.cookie).count();
    assert!(
      page.len() <= limit || page.len() == limit - 1 + shared,
      "a page holds at most its limit, then only the last cookie's group: {} entries",
      page.len()
    );
    between(bridge, &page);
    names.extend(page.into_iter().map(|e| e.name));
  }
}

/// AUD-29-86. Do: page a 200-file directory seven entries at a time. Expect: every page within its bound,
/// and the pages together are the whole listing — `.`, `..` and every file — each exactly once.
#[test]
fn a_directory_paged_by_cookie_lists_every_entry_exactly_once() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();
  for n in 0..PAGED_FILES {
    bridge
      .create(oid(root), &cx, &format!("file-{n:03}"), 0o644, 0)
      .unwrap();
  }
  let names = paged(&mut bridge, &cx, root, PAGE_LIMIT, |_, _| {});
  let unique: std::collections::BTreeSet<&String> = names.iter().collect();
  assert_eq!(names.len(), PAGED_FILES + 2, "no entry repeated");
  assert_eq!(unique.len(), PAGED_FILES + 2, "every entry listed");
  assert_eq!(names.first().map(String::as_str), Some("."));
  assert_eq!(names.get(1).map(String::as_str), Some(".."));
}

/// AUD-29-86 (POSIX `readdir`: an entry not removed while a directory is read is returned exactly once).
/// Do: page the 200-file directory and, after each page, unlink one file the listing has returned and one
/// it has not reached. Expect: every file never unlinked is listed exactly once and no name twice — the rule a
/// position cookie cannot keep, since each unlink of a returned entry moves every later entry down one.
#[test]
fn unlinks_between_pages_skip_and_repeat_no_survivor() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();
  let all: Vec<String> = (0..PAGED_FILES).map(|n| format!("file-{n:03}")).collect();
  for name in &all {
    bridge.create(oid(root), &cx, name, 0o644, 0).unwrap();
  }
  let mut unlinked = std::collections::BTreeSet::new();
  let mut returned = std::collections::BTreeSet::new();
  let names = paged(&mut bridge, &cx, root, PAGE_LIMIT, |bridge, page| {
    returned.extend(page.iter().map(|e| e.name.clone()));
    let behind = page
      .iter()
      .map(|e| e.name.clone())
      .find(|n| n.starts_with("file-"));
    let ahead = all
      .iter()
      .find(|n| !returned.contains(*n) && !unlinked.contains(*n))
      .cloned();
    for name in [behind, ahead].into_iter().flatten() {
      if unlinked.insert(name.clone()) {
        bridge.unlink(oid(root), &cx, &name).unwrap();
      }
    }
  });
  let mut seen = std::collections::BTreeMap::<&str, usize>::new();
  for name in &names {
    *seen.entry(name.as_str()).or_insert(0) += 1;
  }
  assert!(seen.values().all(|&n| n == 1), "no name listed twice");
  for name in all.iter().filter(|n| !unlinked.contains(*n)) {
    assert_eq!(
      seen.get(name.as_str()),
      Some(&1),
      "{name} survived and was listed once"
    );
  }
  assert!(
    unlinked.len() > PAGED_FILES / PAGE_LIMIT,
    "the test unlinked across the listing"
  );
}

/// Two names whose cookies are equal (their hashes share the cookie's bits), found by search over the
/// volume's real name hash.
fn names_sharing_a_cookie(policy: NameEquivalence) -> (String, String) {
  let mut by_cookie = std::collections::HashMap::new();
  (0u64..)
    .find_map(|n| {
      let name = format!("c{n}");
      let cookie = slates_vfs::dir_cookie(policy.hash(&name));
      by_cookie
        .insert(cookie, name.clone())
        .map(|first| (first, name))
    })
    .expect("two names share a cookie within the birthday bound of its bits")
}

/// Shape: files besides the pair, enough to put the directory past the small form's cutover (16) into the
/// indexed tree.
const FILLERS: usize = 20;

/// AUD-29-86. Do: create two names whose cookies are equal among other files, then ask for a one-entry page
/// starting just before them, and a page from their shared cookie. Expect: the one-entry page holds both —
/// a page never ends inside a group a cookie cannot tell apart — and the page after them holds neither.
#[test]
fn a_page_never_ends_between_names_sharing_a_cookie() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();
  let (first, second) = names_sharing_a_cookie(NameEquivalence::Exact);
  for name in [first.clone(), second.clone()]
    .into_iter()
    .chain((0..FILLERS).map(|n| format!("fill-{n}")))
  {
    bridge.create(oid(root), &cx, &name, 0o644, 0).unwrap();
  }
  let whole = bridge.readdir(oid(root), &cx, 0, 0, usize::MAX).unwrap();
  let at = whole
    .iter()
    .position(|e| e.name == first || e.name == second)
    .unwrap();
  let before = at.checked_sub(1).map_or(0, |i| whole[i].cookie);
  let page = bridge.readdir(oid(root), &cx, 0, before, 1).unwrap();
  let names: Vec<&str> = page.iter().map(|e| e.name.as_str()).collect();
  assert!(
    names.contains(&first.as_str()) && names.contains(&second.as_str()),
    "both names sharing the cookie in one page: {names:?}"
  );
  let shared = page.last().unwrap().cookie;
  let after = bridge
    .readdir(oid(root), &cx, 0, shared, usize::MAX)
    .unwrap();
  assert!(
    after.iter().all(|e| e.name != first && e.name != second),
    "neither repeats after their cookie"
  );
}

/// The hard-link pattern git finalizes every object with (a temporary name, a link to the final name, then the
/// temporary removed; 2026-10-01, the OCI lane's git workload). Do: create `tmp` and write it, link `obj` to it,
/// remove `tmp`, then look `obj` up, open it and read it, then release it. Expect: `obj` names the same object,
/// its link count back at one, and serves the bytes — the second name never depends on the first. Every
/// transport (NFS, FUSE, virtio-fs, WinFsp) serves through this bridge, and the test runs on every lane.
#[test]
fn a_hard_link_outlives_the_removal_of_its_first_name() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let root = bridge.root(&cx).unwrap();
  let (attr, fh) = bridge.create(oid(root), &cx, "tmp", 0o444, 0).unwrap();
  bridge.write(oid(attr.ino), &cx, 0, b"payload").unwrap();
  bridge.release(oid(attr.ino), &cx, fh).unwrap();
  let linked = bridge.link(oid(attr.ino), oid(root), &cx, "obj").unwrap();
  assert_eq!(linked.nlink, 2);
  bridge.unlink(oid(root), &cx, "tmp").unwrap();
  assert!(
    bridge.lookup(oid(root), &cx, "tmp").is_err(),
    "the first name is gone"
  );
  let found = bridge.lookup(oid(root), &cx, "obj").unwrap();
  assert_eq!(found.ino, attr.ino, "the second name is the same object");
  assert_eq!(found.nlink, 1);
  let fh = bridge.open(oid(found.ino), &cx, 0).unwrap();
  let mut out = Vec::new();
  bridge.read(oid(found.ino), &cx, 0, 16, &mut out).unwrap();
  assert_eq!(out, b"payload");
  bridge.release(oid(found.ino), &cx, fh).unwrap();
}

/// AUD-29-76 (a subtree export). Do: in a volume holding `shared/` (with `g`) and `private/` (with `secret`,
/// linked into `shared` as `alias` before the scope existed), serve `shared` through a [`ScopedBridge`]; ask
/// its root, list it, and name `private` and `secret` by handle — to read, to create in, to rename into, to link
/// from. Expect: the root is `shared`, whose listing names `shared` as its own `..`; `g` is found and read; every
/// request naming `private` or `secret` is `NotFound` and changes nothing; `alias`, homed outside, is neither
/// listed nor found. The scope holds for every transport, all of which serve through this seam.
/// The scoped-bridge fixture: `shared/` holding `g` ("shared bytes"), `private/` holding `secret`, linked into
/// `shared` as `alias` before any scope existed. The directories' and `secret`'s inode numbers.
fn shared_and_private(inner: &mut VolumeBridge<'_>, cx: &OpContext) -> (u64, u64, u64) {
  let root = inner.root(cx).unwrap();
  let shared = inner.mkdir(oid(root), cx, "shared", 0o755).unwrap().ino;
  let private = inner.mkdir(oid(root), cx, "private", 0o700).unwrap().ino;
  let (g, fh) = inner.create(oid(shared), cx, "g", 0o644, 0).unwrap();
  inner.write(oid(g.ino), cx, 0, b"shared bytes").unwrap();
  inner.release(oid(g.ino), cx, fh).unwrap();
  let (secret, fh) = inner.create(oid(private), cx, "secret", 0o600, 0).unwrap();
  inner.release(oid(secret.ino), cx, fh).unwrap();
  inner
    .link(oid(secret.ino), oid(shared), cx, "alias")
    .unwrap();
  (shared, private, secret.ino)
}

/// Every request naming `private` or `secret` through `scoped` is `NotFound`.
fn assert_nothing_outside_is_reached(
  scoped: &mut slates_bridge_core::scoped::ScopedBridge<'_>,
  cx: &OpContext,
  (shared, private, secret): (u64, u64, u64),
) {
  assert_eq!(
    scoped.lookup(oid(shared), cx, "alias"),
    Err(VfsError::NotFound)
  );
  assert_eq!(scoped.getattr(oid(private), cx), Err(VfsError::NotFound));
  assert_eq!(scoped.getattr(oid(secret), cx), Err(VfsError::NotFound));
  assert!(matches!(
    scoped.create(oid(private), cx, "planted", 0o644, 0),
    Err(VfsError::NotFound)
  ));
  assert_eq!(
    scoped.rename(
      oid(shared),
      oid(private),
      cx,
      "g",
      "g",
      RenameFlags::default()
    ),
    Err(VfsError::NotFound)
  );
  assert_eq!(
    scoped.link(oid(secret), oid(shared), cx, "again"),
    Err(VfsError::NotFound)
  );
}

#[test]
fn a_scoped_bridge_reaches_nothing_outside_its_directory() {
  use slates_bridge_core::scoped::ScopedBridge;
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut inner = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let (shared, private, secret) = shared_and_private(&mut inner, &cx);
  {
    let mut scoped = ScopedBridge::new(&mut inner, shared);
    assert_eq!(scoped.root(&cx).unwrap(), shared);
    let listed = scoped.readdir(oid(shared), &cx, 0, 0, 64).unwrap();
    let dotdot = listed.iter().find(|e| e.name == "..").unwrap();
    assert_eq!(dotdot.ino, shared, "the scope is its own parent");
    let names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
    assert!(
      names.contains(&"g") && !names.contains(&"alias"),
      "{names:?}"
    );
    let found = scoped.lookup(oid(shared), &cx, "g").unwrap();
    let mut out = Vec::new();
    scoped.read(oid(found.ino), &cx, 0, 64, &mut out).unwrap();
    assert_eq!(out, b"shared bytes");
    assert_nothing_outside_is_reached(&mut scoped, &cx, (shared, private, secret));
  }
  assert!(
    inner.lookup(oid(private), &cx, "planted").is_err(),
    "nothing was made outside"
  );
  assert!(inner.lookup(oid(shared), &cx, "g").is_ok(), "g stayed in");
}

/// AUD-29-76 (the scoped listing checks its entries against the directory it admitted, then the scope). Do: in
/// `shared/`, make `a/` holding `f` and `b/`, and link `a/f` into `b` as `alias`, so `alias` is homed in another
/// directory inside the scope; serve `shared` scoped; list `b` and look up `alias` there. Expect: `alias` is listed
/// and found — an entry whose home is not the listed directory falls back to the climb to the scope, and is shown
/// when that home is inside.
#[test]
fn an_alias_homed_in_another_directory_inside_the_scope_is_listed_and_found() {
  use slates_bridge_core::scoped::ScopedBridge;
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut inner = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let (shared, _, _) = shared_and_private(&mut inner, &cx);
  let a = inner.mkdir(oid(shared), &cx, "a", 0o755).unwrap().ino;
  let b = inner.mkdir(oid(shared), &cx, "b", 0o755).unwrap().ino;
  let (f, fh) = inner.create(oid(a), &cx, "f", 0o644, 0).unwrap();
  inner.release(oid(f.ino), &cx, fh).unwrap();
  inner.link(oid(f.ino), oid(b), &cx, "alias").unwrap();
  let mut scoped = ScopedBridge::new(&mut inner, shared);
  let listed = scoped.readdir(oid(b), &cx, 0, 0, 64).unwrap();
  assert!(
    listed.iter().any(|e| e.name == "alias" && e.ino == f.ino),
    "{:?}",
    listed.iter().map(|e| e.name.as_str()).collect::<Vec<_>>()
  );
  assert_eq!(scoped.lookup(oid(b), &cx, "alias").unwrap().ino, f.ino);
}

/// A-61 (a mount taken over after its daemon died). Do: serve a volume holding `lost` and `kept` through a bridge
/// told `lost` lost writes; flush `lost` twice through a handle the dead daemon issued (one this bridge never
/// did), then fsync it twice and flush it; flush `kept` through another; flush `lost` through a handle opened
/// after the takeover. Expect: both flushes answer `RecoveryIncomplete` (the kernel's `EIO`) without consuming
/// it, the first fsync answers it and consumes it, and the next fsync and flush succeed — the writer is told once,
/// as a Linux writeback error is; `kept` and the new handle see no error. With every file lost (the record
/// overflowed), any old handle's first fsync answers it.
#[test]
fn a_lost_write_is_reported_once_to_each_handle_that_predates_the_takeover() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let cx = rw_cx();
  let (lost, kept) = {
    let mut bridge = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
    let root = bridge.root(&cx).unwrap();
    let lost = bridge
      .create(oid(root), &cx, "lost", 0o644, 0)
      .unwrap()
      .0
      .ino;
    let kept = bridge
      .create(oid(root), &cx, "kept", 0o644, 0)
      .unwrap()
      .0
      .ino;
    (lost, kept)
  };
  /// Format: handles the dead daemon issued, which this bridge never did.
  const OLD_HANDLES: [u64; 2] = [0xDEAD_0000_0001, 0xDEAD_0000_0002];
  let mut handles = slates_bridge_core::new_handle_store();
  let mut record =
    slates_bridge_core::LostWrites::new(std::collections::BTreeSet::from([lost]), false);
  let mut bridge = VolumeBridge::attached(
    VolumeId { bytes: [0; 16] },
    &mut vol,
    &mut store,
    &mut handles,
    None,
  )
  .with_lost_writes(&mut record);
  for close in ["a child's exec-time close", "another close"] {
    assert_eq!(
      bridge.flush(oid(lost), &cx, OLD_HANDLES[0]),
      Err(VfsError::RecoveryIncomplete),
      "{close} reports without consuming"
    );
  }
  assert_eq!(
    bridge.fsync(oid(lost), &cx, OLD_HANDLES[0]),
    Err(VfsError::RecoveryIncomplete),
    "the writer's fsync"
  );
  assert_eq!(
    bridge.fsync(oid(lost), &cx, OLD_HANDLES[0]),
    Ok(()),
    "told once"
  );
  assert_eq!(
    bridge.flush(oid(lost), &cx, OLD_HANDLES[0]),
    Ok(()),
    "and the close after it succeeds"
  );
  assert_eq!(
    bridge.flush(oid(kept), &cx, OLD_HANDLES[1]),
    Ok(()),
    "a file that lost nothing"
  );
  let fresh = bridge.open(oid(lost), &cx, 0).unwrap();
  assert_eq!(
    bridge.flush(oid(lost), &cx, fresh),
    Ok(()),
    "a handle opened after the takeover"
  );
  drop(bridge);
  let mut everything = slates_bridge_core::LostWrites::new(std::collections::BTreeSet::new(), true);
  let mut handles = slates_bridge_core::new_handle_store();
  let mut bridge = VolumeBridge::attached(
    VolumeId { bytes: [0; 16] },
    &mut vol,
    &mut store,
    &mut handles,
    None,
  )
  .with_lost_writes(&mut everything);
  assert_eq!(
    bridge.fsync(oid(kept), &cx, OLD_HANDLES[1]),
    Err(VfsError::RecoveryIncomplete)
  );
  assert_eq!(bridge.fsync(oid(kept), &cx, OLD_HANDLES[1]), Ok(()));
}

/// `.` and `..` through the bridge, whole and scoped (POSIX path resolution; NFSv3 LOOKUP, NFSv4 LOOKUPP). Do: make
/// `shared/a`; look up `.` and `..` in `a`, in `shared` and at the root through the whole bridge, then `..` in `a` and
/// in `shared` through a bridge scoped to `shared`, and `.` in a file. Expect: `.` is the directory itself; `..` is its
/// parent and the root's is the root; scoped, `..` of `a` is `shared` and `..` of `shared` is `shared` itself, never
/// the directory above the scope; `.` in a file is `ENOTDIR`. Both were `ENOENT` before 2026-10-06.
#[test]
fn dot_and_dotdot_resolve_and_never_climb_out_of_a_root_or_a_scope() {
  use slates_bridge_core::scoped::ScopedBridge;
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut inner = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
  let cx = rw_cx();
  let (shared, _, secret) = shared_and_private(&mut inner, &cx);
  let a = inner.mkdir(oid(shared), &cx, "a", 0o755).unwrap().ino;
  let root = inner.root(&cx).unwrap();
  let ino = |bridge: &mut dyn Bridge, dir: u64, name: &str| {
    bridge.lookup(oid(dir), &cx, name).map(|n| n.ino)
  };
  assert_eq!(ino(&mut inner, a, "."), Ok(a));
  assert_eq!(ino(&mut inner, a, ".."), Ok(shared));
  assert_eq!(ino(&mut inner, shared, ".."), Ok(root));
  assert_eq!(ino(&mut inner, root, ".."), Ok(root));
  assert_eq!(ino(&mut inner, secret, "."), Err(VfsError::NotDirectory));
  let mut scoped = ScopedBridge::new(&mut inner, shared);
  assert_eq!(ino(&mut scoped, a, ".."), Ok(shared));
  assert_eq!(
    ino(&mut scoped, shared, ".."),
    Ok(shared),
    "the scope's `..` is the scope"
  );
}
