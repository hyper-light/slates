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
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
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

  let entries = bridge.readdir(oid(sub.ino), &cx, 0, 0).unwrap();
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
  let root_entries = bridge.readdir(oid(root), &cx, 0, 0).unwrap();
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
