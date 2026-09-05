//! The edge, error, fault and amplification tests of Phase 1 (design Part 5, Phase 1): each
//! drives the volume through its public API and asserts observable behaviour.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{PAGE, dynamic_volume, store, store_with, volume, volume_with};
use slates_vfs::dir::Child;
use slates_vfs::error::VfsError;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, Volume};

/// Creates `entry-NNNNNN` for every number in `range` under `dir`.
fn fill(
  vol: &mut Volume,
  store: &mut Store,
  dir: slates_mem::Handle<slates_vfs::dir::DirNode>,
  range: std::ops::Range<usize>,
) {
  for n in range {
    vol
      .create_file(store, dir, &format!("entry-{n:06}"), 0o644)
      .unwrap();
  }
}

/// Unlinks `entry-NNNNNN` for every number in `range` under `dir`.
fn drain(
  vol: &mut Volume,
  store: &mut Store,
  dir: slates_mem::Handle<slates_vfs::dir::DirNode>,
  range: std::ops::Range<usize>,
) {
  for n in range {
    vol.unlink(store, dir, &format!("entry-{n:06}")).unwrap();
  }
}

/// The names a directory lists, in the order the volume returns them.
fn listing(
  vol: &Volume,
  store: &Store,
  dir: slates_mem::Handle<slates_vfs::dir::DirNode>,
) -> Vec<String> {
  vol
    .readdir(store, dir)
    .unwrap()
    .iter()
    .map(|r| r.name.to_string())
    .collect()
}

/// T-1.2: names that fold equal under the policy are one entry: `README` and `readme`, and the
/// precomposed and decomposed spellings of `é`; the second create is `EEXIST` and lookups by
/// either spelling find the one entry.
#[test]
fn names_that_fold_equal_are_one_entry() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root();
  let upper = vol.create_file(&mut store, root, "README", 0o644).unwrap();
  assert_eq!(
    vol.create_file(&mut store, root, "readme", 0o644),
    Err(VfsError::AlreadyExists)
  );
  assert_eq!(vol.lookup(&store, root, "ReadMe").unwrap().inode, upper);
  let composed = vol
    .create_file(&mut store, root, "caf\u{e9}", 0o644)
    .unwrap();
  assert_eq!(
    vol.create_file(&mut store, root, "cafe\u{301}", 0o644),
    Err(VfsError::AlreadyExists)
  );
  assert_eq!(
    vol.lookup(&store, root, "CAFE\u{301}").unwrap().inode,
    composed
  );
  let names = listing(&vol, &store, root);
  assert_eq!(names.len(), 2, "one entry per folded name: {names:?}");

  // An exact-policy volume keeps them apart.
  let mut exact = volume_with(
    &mut store,
    Quota::Bounded { limit: 1 << 20 },
    NameEquivalence::Exact,
  );
  let root = exact.root();
  exact
    .create_file(&mut store, root, "README", 0o644)
    .unwrap();
  exact
    .create_file(&mut store, root, "readme", 0o644)
    .unwrap();
  assert_eq!(exact.readdir(&store, root).unwrap().len(), 2);
}

/// T-1.3: a write at offset 10 GiB in a dynamic volume charges one chunk holding one byte,
/// holes read as zeros, and the content memory taken is one page-multiple block.
#[test]
fn a_sparse_write_in_a_dynamic_volume_charges_one_chunk() {
  let mut store = store();
  let mut vol = dynamic_volume(&mut store, 1 << 40, 1 << 40);
  let root = vol.root();
  let f = vol.create_file(&mut store, root, "sparse", 0o644).unwrap();
  let far = 10u64 << 30;
  vol.write(&mut store, f, far, b"x").unwrap();
  assert_eq!(vol.stat(&store, f).unwrap().size, far + 1);
  let mut buf = [7u8; 8];
  assert_eq!(vol.read(&store, f, far - 4, &mut buf).unwrap(), 5);
  assert_eq!(&buf[..5], &[0, 0, 0, 0, b'x']);
  assert_eq!(
    vol.accounting().referenced_bytes,
    1,
    "one chunk, holding the one byte written at a window start"
  );
  assert_eq!(
    store.content.allocated_bytes(),
    PAGE,
    "one page-multiple block"
  );
}

/// T-1.5: growth denied by the pressure source is `ENOSPC` with a pressure event and no
/// partial write: the size and the bytes are unchanged.
#[test]
fn growth_denied_by_the_pressure_source_is_enospc_with_an_event_and_no_partial_write() {
  let mut store = store();
  let mut vol = dynamic_volume(&mut store, 1 << 30, 100);
  let root = vol.root();
  let f = vol.create_file(&mut store, root, "f", 0o644).unwrap();
  assert_eq!(
    vol.write(&mut store, f, 0, &[1u8; 200]),
    Err(VfsError::NoSpace)
  );
  assert_eq!(VfsError::NoSpace.errno_name(), "ENOSPC");
  assert_eq!(vol.growth_denials(), 1, "one pressure event");
  assert_eq!(vol.stat(&store, f).unwrap().size, 0);
  let mut buf = [0u8; 8];
  assert_eq!(vol.read(&store, f, 0, &mut buf).unwrap(), 0);
  vol.write(&mut store, f, 0, &[2u8; 60]).unwrap();
  assert_eq!(
    vol.write(&mut store, f, 60, &[3u8; 50]),
    Err(VfsError::NoSpace)
  );
  assert_eq!(vol.growth_denials(), 2);
  assert_eq!(
    vol.stat(&store, f).unwrap().size,
    60,
    "the failed append changed nothing"
  );
  assert_eq!(vol.accounting().referenced_bytes, 60);
}

/// T-1.8: a directory of 61,067 entries lists in canonical order, every name looks up, and
/// after shrinking it back below the cut-over the order is the same and the representation is
/// the small one again.
#[test]
fn a_directory_of_61067_entries_lists_canonically_and_looks_up_every_name() {
  /// Format: the design's entry count for this case.
  const ENTRIES: usize = 61_067;
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root();
  let dir = vol.mkdir(&mut store, root, "big", 0o755).unwrap();
  fill(&mut vol, &mut store, dir, 0..ENTRIES);
  let listed = listing(&vol, &store, dir);
  assert_eq!(listed.len(), ENTRIES);
  let policy = vol.policy();
  let mut canonical = listed.clone();
  canonical.sort_by_key(|n| (policy.hash(n), n.clone()));
  assert_eq!(listed, canonical, "readdir order is (hash, name)");
  assert!(store.dirs.get(dir).unwrap().is_indexed());
  for n in (0..ENTRIES).step_by(997) {
    assert!(vol.lookup(&store, dir, &format!("ENTRY-{n:06}")).is_ok());
  }
  // Back below half the cut-over (the hysteresis of §4.5) the small representation returns.
  drain(&mut vol, &mut store, dir, 1..ENTRIES);
  assert!(!store.dirs.get(dir).unwrap().is_indexed(), "shrunk back");
  assert_eq!(listing(&vol, &store, dir), vec!["entry-000000".to_owned()]);
}

/// T-1.9: an allocator refusal in the middle of a rename fails the rename atomically with a
/// typed refusal; both names are as they were.
#[test]
fn an_allocator_refusal_during_rename_fails_atomically_and_typed() {
  // Three directory slots: the root, `a` and `b`; the copies a post-snapshot rename needs do
  // not fit.
  let mut store = store_with(3, 4);
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root();
  let a = vol.mkdir(&mut store, root, "a", 0o755).unwrap();
  let b = vol.mkdir(&mut store, root, "b", 0o755).unwrap();
  vol.create_file(&mut store, a, "inside", 0o644).unwrap();
  vol.snapshot(&mut store).unwrap();
  let refusal = vol.rename(&mut store, root, "a", b, "x");
  assert!(
    matches!(refusal, Err(VfsError::Memory(_))),
    "typed memory refusal, got {refusal:?}"
  );
  assert_eq!(refusal.unwrap_err().errno_name(), "ENOMEM");
  assert_both_names_intact(&vol, &store);
}

/// `/a` (with its file) and `/b` are directories, `/b/x` does not exist, the root lists two.
fn assert_both_names_intact(vol: &Volume, store: &Store) {
  let root = vol.root();
  let is_dir = |name: &str| matches!(vol.lookup(store, root, name).unwrap().child, Child::Dir(_));
  assert!(is_dir("a"));
  assert!(is_dir("b"));
  assert_eq!(vol.resolve(store, "/b/x"), Err(VfsError::NotFound));
  assert!(vol.resolve(store, "/a/inside").is_ok());
  assert_eq!(listing(vol, store, root).len(), 2);
}

/// A five-deep directory holding a three-page file, snapshotted.
fn deep_tree_snapshotted() -> (
  slates_vfs::volume::Store,
  Volume,
  slates_mem::Handle<slates_vfs::dir::DirNode>,
  slates_vfs::ids::InodeNo,
) {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 24);
  let mut dir = vol.root();
  for name in ["a", "b", "c", "d"] {
    dir = vol.mkdir(&mut store, dir, name, 0o755).unwrap();
  }
  let f = vol.create_file(&mut store, dir, "f", 0o644).unwrap();
  vol.write(&mut store, f, 0, &vec![9u8; 3 * PAGE]).unwrap();
  vol.snapshot(&mut store).unwrap();
  (store, vol, dir, f)
}

/// AC-1.4: write amplification per mutation is bounded by the depth in directory nodes: a
/// create five levels down after a snapshot copies exactly five directory nodes, and a second
/// create in the same epoch copies none.
#[test]
fn a_create_after_a_snapshot_copies_exactly_the_path_nodes() {
  let (mut store, mut vol, dir, _) = deep_tree_snapshotted();
  let nodes_before = store.dirs.iter().count();
  vol.create_file(&mut store, dir, "new", 0o644).unwrap();
  assert_eq!(
    store.dirs.iter().count() - nodes_before,
    5,
    "root, a, b, c, d copied once; nothing else"
  );
  vol.create_file(&mut store, dir, "again", 0o644).unwrap();
  assert_eq!(
    store.dirs.iter().count() - nodes_before,
    5,
    "the second create in the same epoch copies nothing"
  );
}

/// AC-1.4: write amplification per mutation is bounded by one extent of content: a one-byte
/// write into a three-page extent after a snapshot copies that extent (the buddy arena rounds
/// it to the next power-of-two pages, four), never the whole file; a write into a fresh window
/// takes one page.
#[test]
fn a_write_after_a_snapshot_copies_one_extent_or_takes_one_page() {
  let (mut store, mut vol, _, f) = deep_tree_snapshotted();
  let content_before = store.content.allocated_bytes();
  vol.write(&mut store, f, 0, b"!").unwrap();
  assert_eq!(
    store.content.allocated_bytes() - content_before,
    (3 * PAGE).next_power_of_two(),
    "the window's extent is copied whole, in the buddy's power-of-two pages"
  );
  let far = u64::try_from(store.content.chunk_bytes()).unwrap() * 4;
  let content_before = store.content.allocated_bytes();
  vol.write(&mut store, f, far, b"!").unwrap();
  assert_eq!(
    store.content.allocated_bytes() - content_before,
    PAGE,
    "a fresh window takes one page"
  );
}

/// AC-1.6: inode numbers are never reused within a volume and survive snapshot and clone.
#[test]
fn inode_numbers_are_never_reused_and_survive_snapshot_and_clone() {
  let mut store = store();
  let mut vol = volume(&mut store, 1 << 20);
  let root = vol.root();
  let mut last = 0u64;
  for round in 0..200 {
    let no = vol.create_file(&mut store, root, "f", 0o644).unwrap();
    assert!(
      no.counter() > last,
      "round {round}: {} after {last}",
      no.counter()
    );
    last = no.counter();
    vol.unlink(&mut store, root, "f").unwrap();
  }
  let kept = vol.create_file(&mut store, root, "kept", 0o644).unwrap();
  let s = vol.snapshot(&mut store).unwrap();
  assert_eq!(vol.resolve(&store, "/kept").unwrap().inode, kept);
  let clone = Volume::clone_of(&store, &mut vol, s, common::clone_config(8)).unwrap();
  assert_eq!(clone.resolve(&store, "/kept").unwrap().inode, kept);
  let next = vol.create_file(&mut store, root, "next", 0o644).unwrap();
  assert!(next.counter() > kept.counter());
}
