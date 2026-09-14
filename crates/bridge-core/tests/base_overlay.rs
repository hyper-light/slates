//! T-1.21 (AC-1.17): base metadata mutations through the shared operation layer obey the same
//! witness rules as content writes, and base lookups need no prior listing. The bridge is driven
//! over the base plane's simulated host — the host oracle of `crates/vfs/tests/base.rs`, with
//! outsider edits, a controllable clock and a watcher that can be told to overflow — on every host,
//! with no mount: lookup-before-readdir, chmod, truncate, chown/utimes, rename (plain and
//! `NOREPLACE`), links, unlink-while-open, an unlink after a watcher hint, and a create over a name
//! the base holds. Each case asserts the observable effect (the bytes read back, the listing, the
//! volume's diverged set the landing plans from) and that the disk beneath is untouched (R1).
//!
//! Before the sweep (2026-09-14) the bridge's mutating verbs called the plain `Volume` forms, so
//! every mutating case here failed (8 of 10 at `4f5deae`; the lookup and the unlink-while-open
//! passed): a chmod/truncate/chown/utimes of an untouched base file recorded no witness and the
//! next live-disk stat undid it, a rename left the base name listed beside the new one, a
//! `NOREPLACE` rename replaced a base name silently, a link over a base name succeeded, an unlink
//! after a watcher hint lost its whiteout, and a create over a base name made a second file
//! (`docs/bugs/2026-09-14-base-mutations-bypass-overlay.md`).
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use slates_bridge_core::{
  Attachments, Bridge, ObjectId, OpContext, RenameFlags, Rights, SetAttr, View, VolumeBridge,
  new_handle_store,
};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::Slab;
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::base::{BaseConfig, Divergence};
use slates_vfs::clock::StepClock;
use slates_vfs::error::VfsError;
use slates_vfs::host::HostFs;
use slates_vfs::host::sim::SimHost;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Shape: the page and a small arena for the test volume.
const PAGE: usize = 4096;
const REGION_PAGES: usize = 4096;
/// Shape: the large-file class boundary, one chunk window (as the base oracle uses), so every file
/// here is small class and a content copy-up reads it whole.
const LARGE: u64 = 65_536;
/// Format: the `st_mode` the simulated host reports for a file (`S_IFREG` over `rw-r--r--`).
const SIM_FILE_MODE: u32 = 0o100_644;

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

/// An overlay volume over the simulated host's root, with the base oracle's stepping clock.
fn overlay(host: &mut SimHost, store: &mut Store) -> Volume {
  let root = host.root();
  let facts = host.facts(root).unwrap();
  Volume::create_overlay(
    store,
    VolumeConfig {
      prefix: 7,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 20,
      clock: Box::new(StepClock::new(1_000_000, 1_000)),
    },
    BaseConfig {
      root,
      facts,
      large_class_bytes: LARGE,
    },
  )
  .unwrap()
}

/// A read-write current-view context minted through the attachment registry (the only way).
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

/// The whole fixture: the host (edited by outsiders between bridge calls), the store, the volume
/// and the persistent open-handle map — the daemon's serve shape, where the bridge is rebuilt per
/// request over the shard's state and lends the slot's host.
struct Fixture {
  host: SimHost,
  store: Store,
  vol: Volume,
  handles: Slab<u64>,
  cx: OpContext,
}

impl Fixture {
  /// A base of `src/lib.rs` and `src/main.rs` (the design's overlay worked example, §4.4).
  fn worked_example() -> Fixture {
    let mut host = SimHost::new();
    host.mkdir("/src");
    host.replace_file("/src/lib.rs", b"pub fn lib() {}");
    host.replace_file("/src/main.rs", b"fn main() {}");
    Fixture::over(host)
  }

  fn over(mut host: SimHost) -> Fixture {
    let mut store = store();
    let vol = overlay(&mut host, &mut store);
    Fixture {
      host,
      store,
      vol,
      handles: new_handle_store(),
      cx: rw_cx(),
    }
  }

  /// Runs `f` with a transient bridge over the fixture (the daemon's per-request shape).
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

  /// The inode of `name` under `parent`, looked up through the bridge.
  fn lookup(&mut self, parent: u64, name: &str) -> Result<u64, VfsError> {
    self.bridge(|b, cx| b.lookup(oid(parent), cx, name).map(|a| a.ino))
  }

  fn root(&mut self) -> u64 {
    self.bridge(|b, cx| b.root(cx).unwrap())
  }

  /// The bytes of inode `ino` read through the bridge.
  fn read(&mut self, ino: u64) -> Result<Vec<u8>, VfsError> {
    self.bridge(|b, cx| {
      let mut out = Vec::new();
      b.read(oid(ino), cx, 0, 1 << 16, &mut out)?;
      Ok(out)
    })
  }

  /// The names a readdir of `dir` lists through the bridge, without `.` and `..`.
  fn names(&mut self, dir: u64) -> BTreeSet<String> {
    self.bridge(|b, cx| {
      b.readdir(oid(dir), cx, 0, 0)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .filter(|n| n != "." && n != "..")
        .collect()
    })
  }

  /// The diverged set as `(path, kind)`, sorted by path — what the landing plans from (AC-1.10).
  fn diverged(&self) -> Vec<(String, Divergence)> {
    self
      .vol
      .diverged(&self.store)
      .into_iter()
      .map(|d| (d.path, d.kind))
      .collect()
  }
}

fn set(names: &[&str]) -> BTreeSet<String> {
  names.iter().map(|n| (*n).to_owned()).collect()
}

/// A base file resolves by a direct lookup with no readdir before it, its bytes read back exactly,
/// and the listing follows the design's rule (§4.5 "Lookup"; audit BUG-5): loaded on first use,
/// validated by the directory's fingerprint on every later use, and re-read only when the
/// fingerprint says an outsider changed the directory. The host-call counter states the rule
/// without pinning an implementation's count: two lookups in an unchanged directory cost the same
/// (the validated path, no re-list), and a lookup after an outsider change costs strictly more (the
/// re-list). Passed before the sweep too: the lookup path was already host-aware.
#[test]
fn a_base_file_resolves_by_lookup_before_any_readdir_and_the_listing_follows_the_rule() {
  let mut f = Fixture::worked_example();
  f.host.replace_file("/src/util.rs", b"pub fn util() {}");
  let root = f.root();
  let src = f.lookup(root, "src").unwrap();

  let lib = f.lookup(src, "lib.rs").unwrap();
  assert_eq!(f.read(lib).unwrap(), b"pub fn lib() {}");

  let before = f.host.calls();
  let main = f.lookup(src, "main.rs").unwrap();
  let steady_first = f.host.calls() - before;
  let before = f.host.calls();
  let util = f.lookup(src, "util.rs").unwrap();
  let steady_second = f.host.calls() - before;
  assert_eq!(f.read(main).unwrap(), b"fn main() {}");
  assert_eq!(f.read(util).unwrap(), b"pub fn util() {}");
  assert_eq!(
    steady_first, steady_second,
    "an unchanged directory is validated by its fingerprint, never re-listed"
  );

  // An outsider adds a file: the directory's fingerprint moves, so the next lookup re-lists and
  // finds it — and a name the disk never had is still ENOENT.
  f.host.advance_ns(1);
  f.host.replace_file("/src/new.rs", b"// new");
  let before = f.host.calls();
  let new = f.lookup(src, "new.rs").unwrap();
  let relisted = f.host.calls() - before;
  assert!(
    relisted > steady_second,
    "a changed directory is re-listed ({relisted} host calls, against {steady_second} validated)"
  );
  assert_eq!(f.read(new).unwrap(), b"// new");
  assert_eq!(f.lookup(src, "absent.rs"), Err(VfsError::NotFound));
  assert!(f.diverged().is_empty(), "lookups and reads diverge nothing");
}

/// A `chmod` of an untouched base file through the bridge copies its witness up (metadata-only:
/// nothing pinned), the new mode is what a later stat reports, the entry is in the diverged set, and
/// the disk's mode is unchanged (§4.5 "Metadata-only changes copy up the witness and pin nothing").
/// Failed before the sweep: no witness, so the diverged set was empty.
#[test]
fn chmod_of_an_untouched_base_file_through_the_bridge_witnesses_it() {
  let mut f = Fixture::worked_example();
  let root = f.root();
  let src = f.lookup(root, "src").unwrap();
  let lib = f.lookup(src, "lib.rs").unwrap();

  let changed = f.bridge(|b, cx| {
    b.setattr(
      oid(lib),
      cx,
      SetAttr {
        mode: Some(0o600),
        ..SetAttr::default()
      },
    )
    .unwrap()
  });
  assert_eq!(changed.mode, 0o600, "the reply describes the applied mode");
  let stat = f.bridge(|b, cx| b.getattr(oid(lib), cx).unwrap());
  assert_eq!(stat.mode, 0o600, "a later stat reports the new mode");
  assert_eq!(
    f.diverged(),
    vec![("/src/lib.rs".to_owned(), Divergence::Witnessed)],
    "a metadata change witnesses the base entry"
  );
  assert_eq!(
    f.host.fingerprint("/src/lib.rs").unwrap().mode,
    SIM_FILE_MODE,
    "the disk's mode is untouched (R1)"
  );
  assert_eq!(
    f.read(lib).unwrap(),
    b"pub fn lib() {}",
    "the bytes still come from the disk"
  );
}

/// A truncate (`setattr` of the size) of an untouched base file copies the content up first, so the
/// kept prefix reads back and the disk is untouched. Failed before the sweep: the plain truncate
/// recorded no witness, and the next live-disk stat restored the disk's size.
#[test]
fn truncate_of_an_untouched_base_file_through_the_bridge_witnesses_and_keeps_the_prefix() {
  let mut f = Fixture::worked_example();
  let root = f.root();
  let src = f.lookup(root, "src").unwrap();
  let lib = f.lookup(src, "lib.rs").unwrap();

  let changed = f.bridge(|b, cx| {
    b.setattr(
      oid(lib),
      cx,
      SetAttr {
        size: Some(3),
        ..SetAttr::default()
      },
    )
    .unwrap()
  });
  assert_eq!(changed.size, 3);
  assert_eq!(
    f.bridge(|b, cx| b.getattr(oid(lib), cx).unwrap()).size,
    3,
    "the size sticks across a later stat"
  );
  assert_eq!(f.read(lib).unwrap(), b"pub", "the kept prefix");
  assert_eq!(
    f.diverged(),
    vec![("/src/lib.rs".to_owned(), Divergence::Witnessed)]
  );
  assert_eq!(
    f.host.bytes("/src/lib.rs").unwrap(),
    b"pub fn lib() {}",
    "the disk is untouched (R1)"
  );
}

/// A `chown` and a `utimes` of an untouched base file copy its witness up and stick; the disk's
/// times are untouched. Failed before the sweep: the plain verbs recorded no witness, and the next
/// live-disk stat restored the disk's mtime over the requested one.
#[test]
fn chown_and_utimes_of_an_untouched_base_file_through_the_bridge_witness_it() {
  let mut f = Fixture::worked_example();
  let root = f.root();
  let src = f.lookup(root, "src").unwrap();
  let lib = f.lookup(src, "lib.rs").unwrap();
  let disk_mtime = f.host.fingerprint("/src/lib.rs").unwrap().mtime_ns;

  let changed = f.bridge(|b, cx| {
    b.setattr(
      oid(lib),
      cx,
      SetAttr {
        uid: Some(501),
        gid: Some(20),
        atime: Some(111),
        mtime: Some(222),
        ..SetAttr::default()
      },
    )
    .unwrap()
  });
  assert_eq!(
    (changed.uid, changed.gid, changed.atime, changed.mtime),
    (501, 20, 111, 222)
  );
  let stat = f.bridge(|b, cx| b.getattr(oid(lib), cx).unwrap());
  assert_eq!(
    (stat.uid, stat.gid, stat.atime, stat.mtime),
    (501, 20, 111, 222),
    "the ownership and times stick across a later stat"
  );
  assert_eq!(
    f.diverged(),
    vec![("/src/lib.rs".to_owned(), Divergence::Witnessed)]
  );
  assert_eq!(
    f.host.fingerprint("/src/lib.rs").unwrap().mtime_ns,
    disk_mtime,
    "the disk's mtime is untouched (R1)"
  );
}

/// A rename of an untouched base file leaves a whiteout at the old name and a witness on the moved
/// entry: the listing shows the new name only, the old name is ENOENT, the bytes follow the file,
/// and the disk still holds the old name and never the new one (§4.5 "Rename"). Failed before the
/// sweep: the plain rename left no whiteout, so the base name came back beside the new one.
#[test]
fn rename_of_an_untouched_base_file_through_the_bridge_leaves_a_whiteout_and_a_witness() {
  let mut f = Fixture::worked_example();
  let root = f.root();
  let src = f.lookup(root, "src").unwrap();
  f.lookup(src, "lib.rs").unwrap();

  f.bridge(|b, cx| {
    b.rename(
      oid(src),
      oid(src),
      cx,
      "lib.rs",
      "lib2.rs",
      RenameFlags::default(),
    )
    .unwrap();
  });
  assert_eq!(
    f.names(src),
    set(&["lib2.rs", "main.rs"]),
    "the old base name is hidden by its whiteout"
  );
  assert_eq!(f.lookup(src, "lib.rs"), Err(VfsError::NotFound));
  let moved = f.lookup(src, "lib2.rs").unwrap();
  assert_eq!(f.read(moved).unwrap(), b"pub fn lib() {}");
  assert_eq!(
    f.diverged(),
    vec![
      ("/src/lib.rs".to_owned(), Divergence::Whiteout),
      ("/src/lib2.rs".to_owned(), Divergence::Witnessed),
    ]
  );
  let on_disk: Vec<String> = f.host.paths().into_iter().map(|(p, _)| p).collect();
  assert!(on_disk.contains(&"/src/lib.rs".to_owned()));
  assert!(!on_disk.contains(&"/src/lib2.rs".to_owned()));
}

/// `RENAME_NOREPLACE` onto a base name the mount has not looked up yet is refused: the name exists
/// on the disk beneath, and the flag means "never replace". Failed before the sweep: the existence
/// check used the plain lookup, which knows only materialized entries, so the base file was replaced.
#[test]
fn rename_noreplace_onto_a_base_name_not_yet_looked_up_is_refused() {
  let mut host = SimHost::new();
  host.replace_file("/a", b"A");
  host.replace_file("/b", b"B");
  let mut f = Fixture::over(host);
  let root = f.root();
  f.lookup(root, "a").unwrap();

  let refused = f.bridge(|b, cx| {
    b.rename(
      oid(root),
      oid(root),
      cx,
      "a",
      "b",
      RenameFlags {
        no_replace: true,
        exchange: false,
      },
    )
  });
  assert_eq!(refused, Err(VfsError::AlreadyExists));
  let b = f.lookup(root, "b").unwrap();
  assert_eq!(f.read(b).unwrap(), b"B", "b was not replaced");
  assert!(f.diverged().is_empty(), "a refused rename changes nothing");
}

/// A hard link to an untouched base file copies its witness up (the link count is a metadata
/// change) and both names resolve to it; a link at a name the base holds is `EEXIST`. Failed before
/// the sweep: no witness, and the base name was taken.
#[test]
fn link_to_an_untouched_base_file_witnesses_it_and_a_base_name_cannot_be_taken() {
  let mut f = Fixture::worked_example();
  let root = f.root();
  let src = f.lookup(root, "src").unwrap();
  let lib = f.lookup(src, "lib.rs").unwrap();

  let linked = f.bridge(|b, cx| b.link(oid(lib), oid(src), cx, "lib_link").unwrap());
  assert_eq!(linked.ino, lib);
  assert_eq!(linked.nlink, 2);
  assert_eq!(f.lookup(src, "lib_link").unwrap(), lib);
  assert_eq!(
    f.diverged(),
    vec![
      ("/src/lib.rs".to_owned(), Divergence::Witnessed),
      ("/src/lib_link".to_owned(), Divergence::Witnessed),
    ]
  );

  let taken = f.bridge(|b, cx| b.link(oid(lib), oid(src), cx, "main.rs"));
  assert_eq!(
    taken,
    Err(VfsError::AlreadyExists),
    "a name the base holds cannot be taken by a link"
  );
  let main = f.lookup(src, "main.rs").unwrap();
  assert_eq!(
    f.read(main).unwrap(),
    b"fn main() {}",
    "main.rs is the base's"
  );
}

/// A create, a mkdir and a symlink over a name the base holds are `EEXIST` through the bridge, and a
/// directory made in an overlay is opaque and in the diverged set (so the landing creates it before
/// its children). Failed before the sweep: the plain verbs checked only materialized entries, so a
/// second file shadowed the disk's, and the directory was not in the diverged set.
#[test]
fn create_mkdir_and_symlink_over_a_name_the_base_holds_are_refused() {
  let mut host = SimHost::new();
  host.mkdir("/src");
  host.mkdir("/src/sub");
  host.replace_file("/src/lib.rs", b"pub fn lib() {}");
  let mut f = Fixture::over(host);
  let root = f.root();
  let src = f.lookup(root, "src").unwrap();

  assert_eq!(
    f.bridge(|b, cx| b.create(oid(src), cx, "lib.rs", 0o644, 0).map(|_| ())),
    Err(VfsError::AlreadyExists)
  );
  assert_eq!(
    f.bridge(|b, cx| b.mkdir(oid(src), cx, "sub", 0o755).map(|_| ())),
    Err(VfsError::AlreadyExists)
  );
  assert_eq!(
    f.bridge(|b, cx| b.symlink(oid(src), cx, "lib.rs", "main.rs").map(|_| ())),
    Err(VfsError::AlreadyExists)
  );
  let lib = f.lookup(src, "lib.rs").unwrap();
  assert_eq!(
    f.read(lib).unwrap(),
    b"pub fn lib() {}",
    "the base file is still the one served"
  );

  let fresh = f.bridge(|b, cx| b.mkdir(oid(src), cx, "fresh", 0o755).unwrap());
  f.bridge(|b, cx| b.create(oid(fresh.ino), cx, "f", 0o644, 0).unwrap());
  assert_eq!(
    f.diverged(),
    vec![
      ("/src/fresh".to_owned(), Divergence::Created),
      ("/src/fresh/f".to_owned(), Divergence::Created),
    ],
    "a directory made through the mount is in the diverged set with its children"
  );
}

/// Unlink-while-open on an untouched base file: the open answers from the daemon's descriptor on the
/// backing file (§4.6 "Base files"), so after the unlink the open handle still reads the bytes the
/// opener saw, the name is gone (a whiteout hides the base name), and the inode is reclaimed only
/// when the handle is released; the disk still holds the file. Passed before the sweep as well —
/// the stat at lookup had already opened the descriptor as a side effect; the open now takes it by
/// the design's rule, so it holds even after a listing refresh has dropped the stat's descriptor.
#[test]
fn unlink_of_an_open_base_file_keeps_its_bytes_until_release_and_hides_the_base_name() {
  let mut host = SimHost::new();
  host.replace_file("/f", b"hello");
  let mut f = Fixture::over(host);
  let root = f.root();
  let ino = f.lookup(root, "f").unwrap();

  let fh = f.bridge(|b, cx| b.open(oid(ino), cx, 0).unwrap());
  f.bridge(|b, cx| b.unlink(oid(root), cx, "f").unwrap());
  assert_eq!(f.lookup(root, "f"), Err(VfsError::NotFound));
  assert!(f.names(root).is_empty(), "the base name is hidden");
  assert_eq!(
    f.read(ino).unwrap(),
    b"hello",
    "the open file still serves the bytes the opener saw"
  );
  assert_eq!(f.diverged(), vec![("/f".to_owned(), Divergence::Whiteout)]);

  f.bridge(|b, cx| b.release(oid(ino), cx, fh).unwrap());
  assert!(
    f.bridge(|b, cx| b.getattr(oid(ino), cx)).is_err(),
    "reclaimed once the open handle is released"
  );
  assert_eq!(
    f.host.bytes("/f").unwrap(),
    b"hello",
    "the disk is untouched"
  );
}

/// An unlink of a base name after a watcher hint invalidated the directory's cached listing still
/// leaves a whiteout: the listing is reloaded before the removal, so the base name stays hidden
/// (§4.5 "Unlinking a base-backed entry writes a whiteout"). Failed before the sweep: the plain
/// unlink consulted only the cached listing, found it empty, wrote no whiteout, and the base name
/// came back at the next listing.
#[test]
fn unlink_of_a_base_name_after_a_hint_invalidated_the_listing_still_leaves_a_whiteout() {
  let mut host = SimHost::new();
  host.replace_file("/f", b"f");
  let mut f = Fixture::over(host);
  let root = f.root();
  f.lookup(root, "f").unwrap();

  // An outsider adds a sibling: the watcher hints, and the daemon's status drains the hint, which
  // invalidates the root's cached listing.
  f.host.advance_ns(1);
  f.host.replace_file("/g", b"g");
  let status = f.vol.with_host(&mut f.host).status(&mut f.store).unwrap();
  assert!(status.drift.is_empty());

  f.bridge(|b, cx| b.unlink(oid(root), cx, "f").unwrap());
  assert_eq!(f.names(root), set(&["g"]), "f stays hidden, g shows");
  assert_eq!(f.diverged(), vec![("/f".to_owned(), Divergence::Whiteout)]);
  assert_eq!(f.host.bytes("/f").unwrap(), b"f", "the disk is untouched");
}
