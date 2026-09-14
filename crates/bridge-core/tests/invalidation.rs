//! Kernel coherence at the seam (§4.6 "Cache posture", "A drift report or a watcher hint on a
//! base path invalidates the kernel's entry and attributes for it"; GAP-A9-3 "real
//! invalidations"). A transport that caches names and attributes in its kernel asks the seam,
//! before each request, for every invalidation owed since it last looked; these tests are the
//! transport's side of that contract, driven on every host with no mount: a change through the
//! SDK path (a direct volume mutation outside the bridge) or a watcher hint on a base directory
//! yields the entry and inode invalidations the kernel needs, the transport's own changes yield
//! none, and after a hint the next lookup re-lists the directory (the host-call counter moves) and
//! shows the outsider's change. Before the sweep (2026-09-14) nothing produced invalidations at
//! all: the notification encoders had no writer (`grep -rn "inval_" crates` found only the
//! encoder module), so a FUSE kernel that cached forever was never told of a change.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_core::{
  Attachments, Bridge, CacheLifetime, Invalidation, InvalidationCursor, ObjectId, OpContext,
  Rights, View, VolumeBridge, new_handle_store,
};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::Slab;
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::base::BaseConfig;
use slates_vfs::clock::StepClock;
use slates_vfs::host::HostFs;
use slates_vfs::host::sim::SimHost;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Shape: the page and a small arena for the test volume.
const PAGE: usize = 4096;
const REGION_PAGES: usize = 4096;
/// Shape: the large-file class boundary (one chunk window), as the base oracle uses.
const LARGE: u64 = 65_536;
/// Shape: the simulated base filesystem's timestamp granularity — HFS+'s one second, so the
/// bounded lifetime is a value no default could produce by accident.
const GRANULARITY_NS: u64 = 1_000_000_000;

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

fn config(prefix: u16) -> VolumeConfig {
  VolumeConfig {
    prefix,
    names: NameEquivalence::Exact,
    quota: Quota::Bounded { limit: 1 << 30 },
    journal_bytes: 1 << 20,
    clock: Box::new(StepClock::new(1_000_000, 1_000)),
  }
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

fn oid(ino: u64) -> ObjectId {
  ObjectId::new(ino, 0)
}

/// An overlay fixture: the simulated host, the store, the volume, the persistent handle map and
/// the transport's cursor — the transport is rebuilt per request, as the daemon's serve path is.
struct Overlay {
  host: SimHost,
  store: Store,
  vol: Volume,
  handles: Slab<u64>,
  cx: OpContext,
  cursor: InvalidationCursor,
}

impl Overlay {
  fn over(mut host: SimHost) -> Overlay {
    host.set_granularity_ns(GRANULARITY_NS);
    let mut store = store();
    let root = host.root();
    let facts = host.facts(root).unwrap();
    let vol = Volume::create_overlay(
      &mut store,
      config(7),
      BaseConfig {
        root,
        facts,
        large_class_bytes: LARGE,
      },
    )
    .unwrap();
    let mut fixture = Overlay {
      host,
      store,
      vol,
      handles: new_handle_store(),
      cx: rw_cx(),
      cursor: InvalidationCursor::default(),
    };
    // At mount time the kernel holds nothing: the transport starts from the seam's position.
    fixture.cursor = fixture.bridge(|b, cx| b.seen(cx));
    fixture
  }

  fn bridge<R>(&mut self, f: impl FnOnce(&mut VolumeBridge<'_>, &OpContext) -> R) -> R {
    let mut bridge = VolumeBridge::attached(
      VolumeId { bytes: [0; 16] },
      &mut self.vol,
      &mut self.store,
      &mut self.handles,
      Some(&mut self.host as &mut dyn HostFs),
    );
    f(&mut bridge, &self.cx)
  }

  /// One served request, as the transport does it: the invalidations owed since the cursor are
  /// taken first, the request runs, and the cursor moves past the request's own records.
  fn serve<R>(
    &mut self,
    f: impl FnOnce(&mut VolumeBridge<'_>, &OpContext) -> R,
  ) -> (Vec<Invalidation>, R) {
    let cursor = self.cursor;
    let (owed, next, result) = self.bridge(|b, cx| {
      let mut owed = Vec::new();
      let next = b.invalidations(cx, cursor, &mut owed).unwrap();
      let result = f(b, cx);
      (owed, next, result)
    });
    let _ = next;
    self.cursor = self.bridge(|b, cx| b.seen(cx));
    (owed, result)
  }

  fn lookup(&mut self, parent: u64, name: &str) -> (Vec<Invalidation>, u64) {
    self.serve(|b, cx| b.lookup(oid(parent), cx, name).unwrap().ino)
  }

  fn size_of(&mut self, ino: u64) -> u64 {
    self.serve(|b, cx| b.getattr(oid(ino), cx).unwrap().size).1
  }
}

fn entry(parent: u64, name: &str, expire: bool) -> Invalidation {
  Invalidation::Entry {
    parent,
    name: name.to_owned(),
    expire,
  }
}

fn inode(ino: u64, data: bool) -> Invalidation {
  Invalidation::Inode { ino, data }
}

/// A change through the SDK path — a write, a truncate, an unlink and a rename applied to the
/// volume directly, outside the bridge — owes the attached transport an invalidation for each
/// affected kernel entry, drawn from the journal since the transport's cursor; the transport's
/// own request owes nothing, and a cursor taken after it sees only later changes.
#[test]
fn a_change_outside_the_transport_owes_its_kernel_entries_and_the_transports_own_owes_none() {
  fn bridge<'v>(
    vol: &'v mut Volume,
    store: &'v mut Store,
    handles: &'v mut Slab<u64>,
  ) -> VolumeBridge<'v> {
    VolumeBridge::attached(VolumeId { bytes: [0; 16] }, vol, store, handles, None)
  }
  let mut store = store();
  let mut vol = Volume::create(&mut store, config(1)).unwrap();
  let mut handles = new_handle_store();
  let cx = rw_cx();

  // The transport creates "f" (its own request) and takes its cursor after.
  let (root, f, cursor) = {
    let mut b = bridge(&mut vol, &mut store, &mut handles);
    let root = b.root(&cx).unwrap();
    let (attr, fh) = b.create(oid(root), &cx, "f", 0o644, 0).unwrap();
    b.release(oid(attr.ino), &cx, fh).unwrap();
    (root, attr.ino, b.seen(&cx))
  };
  let mut owed = Vec::new();
  let cursor = bridge(&mut vol, &mut store, &mut handles)
    .invalidations(&cx, cursor, &mut owed)
    .unwrap();
  assert!(
    owed.is_empty(),
    "the transport's own create owes nothing: {owed:?}"
  );

  // The SDK writes and truncates "f": its attributes and data are stale in the kernel.
  let f_no = slates_vfs::ids::InodeNo(f);
  vol.write(&mut store, f_no, 0, b"hello").unwrap();
  vol.truncate(&mut store, f_no, 2).unwrap();
  let cursor = bridge(&mut vol, &mut store, &mut handles)
    .invalidations(&cx, cursor, &mut owed)
    .unwrap();
  assert_eq!(owed, vec![inode(f, true), inode(f, true)]);
  owed.clear();

  // The SDK renames "f" to "g" and creates "h": both names and the directory are stale.
  let root_h = vol.root();
  vol.rename(&mut store, root_h, "f", root_h, "g").unwrap();
  vol.create_file(&mut store, root_h, "h", 0o644).unwrap();
  let cursor = bridge(&mut vol, &mut store, &mut handles)
    .invalidations(&cx, cursor, &mut owed)
    .unwrap();
  assert_eq!(
    owed,
    vec![
      entry(root, "f", false),
      inode(root, true),
      entry(root, "g", false),
      inode(root, true),
      entry(root, "h", false),
      inode(root, true),
    ]
  );
  owed.clear();

  // A chmod through the SDK is attributes only: the pages stay.
  vol.chmod(&mut store, f_no, 0o600).unwrap();
  let _ = bridge(&mut vol, &mut store, &mut handles)
    .invalidations(&cx, cursor, &mut owed)
    .unwrap();
  assert_eq!(owed, vec![inode(f, false)]);
}

/// A watcher hint on a base directory (an outsider replaced a file beneath it) owes the transport
/// an expire-only invalidation for every live entry the kernel may hold under that directory and
/// an inode invalidation for each, plus the directory's own; the next lookup then re-lists (the
/// host-call counter moves against a steady lookup) and reports the outsider's new size. A
/// second request with nothing new owes nothing.
#[test]
fn a_watcher_hint_expires_the_directorys_live_entries_and_the_next_lookup_relists() {
  let mut host = SimHost::new();
  host.mkdir("/src");
  host.replace_file("/src/lib.rs", b"pub fn lib() {}");
  host.replace_file("/src/main.rs", b"fn main() {}");
  let mut f = Overlay::over(host);
  let root = f.bridge(|b, cx| b.root(cx).unwrap());
  let (_, src) = f.lookup(root, "src");
  let (_, lib) = f.lookup(src, "lib.rs");
  let (owed, main) = f.lookup(src, "main.rs");
  assert!(owed.is_empty(), "nothing changed yet: {owed:?}");
  assert_eq!(f.size_of(lib), 15);

  // A steady lookup's host-call cost, for the re-list comparison below.
  let before = f.host.calls();
  let (owed, _) = f.lookup(src, "main.rs");
  let steady = f.host.calls() - before;
  assert!(owed.is_empty());

  // An outsider replaces lib.rs (a new inode, a bigger file): the watcher hints.
  f.host.advance_ns(i64::try_from(GRANULARITY_NS).unwrap());
  f.host
    .replace_file("/src/lib.rs", b"pub fn lib() { /* replaced */ }");
  let before = f.host.calls();
  let (owed, relisted_lib) = f.lookup(src, "lib.rs");
  let relist = f.host.calls() - before;
  // The directory itself, an expire for each live name beneath it, and each name's attributes.
  for expected in [
    inode(src, true),
    entry(src, "lib.rs", true),
    entry(src, "main.rs", true),
    inode(lib, true),
    inode(main, true),
  ] {
    assert!(
      owed.contains(&expected),
      "{expected:?} missing from {owed:?}"
    );
  }
  assert!(
    relist > steady,
    "the hinted directory was re-listed ({relist} calls against {steady})"
  );
  assert_eq!(relisted_lib, lib, "the same inode number (D-4), refreshed");
  assert_eq!(f.size_of(lib), 31, "the outsider's size shows");

  // Nothing new: the next request owes nothing.
  let (owed, _) = f.lookup(src, "main.rs");
  assert!(owed.is_empty(), "delivered once: {owed:?}");
}

/// A watcher overflow (lost events) marks every loaded directory stale: the transport owes an
/// expire for every live entry it may hold anywhere, not only under one directory.
#[test]
fn a_watcher_overflow_expires_every_loaded_directorys_live_entries() {
  let mut host = SimHost::new();
  host.mkdir("/a");
  host.mkdir("/b");
  host.replace_file("/a/x", b"x");
  host.replace_file("/b/y", b"y");
  let mut f = Overlay::over(host);
  let root = f.bridge(|b, cx| b.root(cx).unwrap());
  let (_, a) = f.lookup(root, "a");
  let (_, b) = f.lookup(root, "b");
  let (_, x) = f.lookup(a, "x");
  let (_, y) = f.lookup(b, "y");

  f.host.watcher_overflow();
  let (owed, _) = f.lookup(root, "a");
  for expected in [
    entry(root, "a", true),
    entry(root, "b", true),
    entry(a, "x", true),
    entry(b, "y", true),
    inode(x, true),
    inode(y, true),
  ] {
    assert!(
      owed.contains(&expected),
      "{expected:?} missing from {owed:?}"
    );
  }
}

/// The seam's cache posture per object (§4.6): a live base entry and a merged directory may be
/// cached only for the base filesystem's timestamp granularity; the volume's own objects — a file
/// it created, a base file it copied up — until an explicit invalidation.
#[test]
fn live_base_entries_get_a_bounded_lifetime_and_the_volumes_own_objects_forever() {
  let mut host = SimHost::new();
  host.mkdir("/src");
  host.replace_file("/src/lib.rs", b"pub fn lib() {}");
  let mut f = Overlay::over(host);
  let root = f.bridge(|b, cx| b.root(cx).unwrap());
  let (_, src) = f.lookup(root, "src");
  let (_, lib) = f.lookup(src, "lib.rs");
  let bounded = CacheLifetime::Bounded { ns: GRANULARITY_NS };
  assert_eq!(f.bridge(|b, cx| b.cache_lifetime(oid(lib), cx)), bounded);
  assert_eq!(f.bridge(|b, cx| b.cache_lifetime(oid(src), cx)), bounded);
  assert_eq!(f.bridge(|b, cx| b.cache_lifetime(oid(root), cx)), bounded);

  let created = f.bridge(|b, cx| b.create(oid(src), cx, "new.rs", 0o644, 0).unwrap().0.ino);
  assert_eq!(
    f.bridge(|b, cx| b.cache_lifetime(oid(created), cx)),
    CacheLifetime::Forever
  );
  f.bridge(|b, cx| b.write(oid(lib), cx, 0, b"pub").unwrap());
  assert_eq!(
    f.bridge(|b, cx| b.cache_lifetime(oid(lib), cx)),
    CacheLifetime::Forever,
    "a copied-up base file is the volume's own"
  );
}
