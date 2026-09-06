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
  let mut bridge = VolumeBridge::new(&mut vol, &mut store);
  let root = bridge.root().unwrap();
  let (attr, _fh) = bridge.create(root, "f", 0o644, 0).unwrap();
  let ino = attr.ino;

  let changed = bridge
    .setattr(
      ino,
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
  let stat = bridge.getattr(ino).unwrap();
  assert_eq!(
    (stat.uid, stat.gid, stat.atime, stat.mtime),
    (501, 20, 111, 222)
  );

  // A mode-only setattr changes the mode and nothing else.
  bridge
    .setattr(
      ino,
      SetAttr {
        mode: Some(0o600),
        ..SetAttr::default()
      },
    )
    .unwrap();
  let after = bridge.getattr(ino).unwrap();
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
  let mut bridge = VolumeBridge::new(&mut vol, &mut store);
  let root = bridge.root().unwrap();
  bridge.create(root, "a", 0o644, 0).unwrap();
  bridge.create(root, "b", 0o644, 0).unwrap();

  let noreplace = RenameFlags {
    no_replace: true,
    exchange: false,
  };
  assert!(
    bridge.rename(root, "a", root, "b", noreplace).is_err(),
    "NOREPLACE onto an existing name is refused"
  );
  assert!(
    bridge.rename(root, "a", root, "c", noreplace).is_ok(),
    "NOREPLACE onto a free name succeeds"
  );
  assert!(bridge.lookup(root, "b").is_ok(), "b was not replaced");
  assert!(bridge.lookup(root, "a").is_err(), "a moved away");
  assert!(bridge.lookup(root, "c").is_ok(), "c is the moved file");
}

/// `RENAME_EXCHANGE` is refused, not silently downgraded to a plain rename that would replace the
/// destination and lose it (BUG-10).
#[test]
fn rename_exchange_is_refused_not_downgraded() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(&mut vol, &mut store);
  let root = bridge.root().unwrap();
  bridge.create(root, "x", 0o644, 0).unwrap();
  bridge.create(root, "y", 0o644, 0).unwrap();

  let exchange = RenameFlags {
    no_replace: false,
    exchange: true,
  };
  assert!(
    bridge.rename(root, "x", root, "y", exchange).is_err(),
    "EXCHANGE is refused, not performed as a plain rename"
  );
  assert!(bridge.lookup(root, "x").is_ok(), "x is untouched");
  assert!(bridge.lookup(root, "y").is_ok(), "y is untouched");
}

/// A released handle is stale and its slot is reused, so repeated open/close does not grow the
/// handle table (BUG-4).
#[test]
fn open_handles_are_reused_so_memory_stays_bounded() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(&mut vol, &mut store);
  let root = bridge.root().unwrap();
  let (attr, create_fh) = bridge.create(root, "h", 0o644, 0).unwrap();
  let ino = attr.ino;
  bridge.release(ino, create_fh).unwrap();

  let obj = ObjectId {
    inode: ino,
    generation: 0,
  };
  let cx = rw_cx();
  let fh1 = bridge.open(ino, 0).unwrap();
  bridge.release(ino, fh1).unwrap();
  let fh2 = bridge.open(ino, 0).unwrap();
  let mut out = Vec::new();
  // A read is addressed by inode now, not the handle; the handle table only holds per-open state.
  assert!(
    bridge.read(obj, &cx, 0, 16, &mut out).is_ok(),
    "a read is addressed by inode"
  );
  bridge.release(ino, fh2).unwrap();

  // A thousand open/release cycles leak nothing: the slab reuses one slot.
  let before = format!("{bridge:?}");
  for _ in 0..1000 {
    let fh = bridge.open(ino, 0).unwrap();
    bridge.release(ino, fh).unwrap();
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
  let mut bridge = VolumeBridge::new(&mut vol, &mut store);
  let root = bridge.root().unwrap();
  let (attr, _fh) = bridge.create(root, "f", 0o644, 0).unwrap();
  let obj = ObjectId {
    inode: attr.ino,
    generation: 0,
  };
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
  let mut bridge = VolumeBridge::new(&mut vol, &mut store);
  let root = bridge.root().unwrap();
  let (attr, _fh) = bridge.create(root, "f", 0o644, 0).unwrap();
  let obj = ObjectId {
    inode: attr.ino,
    generation: 0,
  };
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
