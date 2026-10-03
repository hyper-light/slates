//! The paths that changed between two snapshots (`Volume::paths_changed_between`; §4.4 `advance` of an
//! immutable reader, which "invalidates old caches"; AUD-29-76): what a re-pinned snapshot mount reports so
//! its reader drops exactly what moved. The oracle compares each snapshot's whole tree by path; the diff must
//! name every path whose state differs, by its own name, and name nothing that neither snapshot holds.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;

use common::drive::{PathState, apply_volume, head_state, snapshot_state};
use common::steps::{Step, step};
use common::{store, volume};
use proptest::prelude::*;
use slates_vfs::volume::{Store, Volume};

/// Shape: the quota of the test volumes, far above what a generated history writes.
const QUOTA: u64 = 1 << 24;

/// Applies `steps` to the volume, the picks drawn from its head's files.
fn apply_all(steps: &[Step], vol: &mut Volume, store: &mut Store) {
  for step in steps {
    let files = head_state(vol, store).file_paths();
    let _ = apply_volume(step, vol, store, &files);
  }
}

/// The oracle's path for a tree path (the root is the empty string there, `/` in a report).
fn reported(path: &str) -> String {
  if path.is_empty() {
    "/".to_owned()
  } else {
    path.to_owned()
  }
}

/// Every path whose state differs between `before` and `after`: a file's bytes or link count, a symlink's
/// target, a file's extended attributes, a directory's listing.
fn differing(before: &PathState, after: &PathState) -> BTreeSet<String> {
  let mut out = BTreeSet::new();
  for path in before.files.keys().chain(after.files.keys()) {
    if before.files.get(path) != after.files.get(path) {
      out.insert(reported(path));
    }
  }
  for path in before.symlinks.keys().chain(after.symlinks.keys()) {
    if before.symlinks.get(path) != after.symlinks.get(path) {
      out.insert(reported(path));
    }
  }
  for path in before.xattrs.keys().chain(after.xattrs.keys()) {
    if before.xattrs.get(path) != after.xattrs.get(path) {
      out.insert(reported(path));
    }
  }
  let listing = |state: &PathState, path: &str| {
    state
      .dirs
      .iter()
      .find(|(dir, _)| dir == path)
      .map(|(_, rows)| rows.clone())
  };
  for (path, _) in before.dirs.iter().chain(&after.dirs) {
    if listing(before, path) != listing(after, path) {
      out.insert(reported(path));
    }
  }
  out
}

/// Every path a tree holds: directories, and every name a directory lists (any kind).
fn held(state: &PathState) -> BTreeSet<String> {
  let mut out = BTreeSet::new();
  for (dir, rows) in &state.dirs {
    out.insert(reported(dir));
    for (name, _) in rows {
      out.insert(format!("{dir}/{name}"));
    }
  }
  out
}

proptest! {
  #![proptest_config(slates_test_seeds::seeded(ProptestConfig::with_cases(256), include_str!("snapshot_diff.proptest-regressions")).unwrap())]

  /// AUD-29-76 (`advance` of a snapshot mount). Do: apply a generated history, snapshot, apply a second,
  /// snapshot; ask which paths changed between the two. Expect: every path whose file bytes, link count,
  /// symlink target, extended attributes or directory listing differ is named by its own path; every path
  /// named exists in one snapshot or the other.
  #[test]
  fn the_diff_names_every_changed_path_and_nothing_neither_snapshot_holds(
    first in proptest::collection::vec(step(), 0..32),
    second in proptest::collection::vec(step(), 0..32),
  ) {
    let mut store = store();
    let mut vol = volume(&mut store, QUOTA);
    apply_all(&first, &mut vol, &mut store);
    let from = vol.snapshot(&mut store).unwrap();
    apply_all(&second, &mut vol, &mut store);
    let to = vol.snapshot(&mut store).unwrap();
    let before = snapshot_state(&vol, &store, from);
    let after = snapshot_state(&vol, &store, to);
    let named: BTreeSet<String> = vol.paths_changed_between(&store, from, to).unwrap().into_iter().collect();
    let missing: Vec<String> = differing(&before, &after).into_iter().filter(|path| !named.contains(path)).collect();
    prop_assert!(missing.is_empty(), "changed but not named: {:?}; named {:?}", missing, named);
    let exists: BTreeSet<String> = held(&before).union(&held(&after)).cloned().collect();
    let invented: Vec<&String> = named.iter().filter(|path| !exists.contains(*path)).collect();
    prop_assert!(invented.is_empty(), "named but in neither snapshot: {:?}", invented);
  }
}

/// AUD-29-76. Do: build `/keep/a`, `/work/b` and `/linked` (a hard link of `/keep/a`'s sibling `c`); snapshot;
/// write `/work/b`; snapshot; rename `/work` to `/moved`; snapshot; write through `/keep/c`; snapshot. Expect:
/// the first span names `/work/b` alone (an untouched file and its directory are not named); the rename names
/// the directory's old and new paths, everything beneath both, and the root whose listing changed; the write to
/// a hard-linked file names both of its names.
#[test]
fn the_diff_is_exact_for_a_write_a_moved_directory_and_a_linked_file() {
  let mut store = store();
  let mut vol = volume(&mut store, QUOTA);
  let root = vol.root_inode(&store).unwrap();
  let keep = vol.mkdir_no(&mut store, root, "keep", 0o755).unwrap();
  let work = vol.mkdir_no(&mut store, root, "work", 0o755).unwrap();
  let a = vol.create_file_no(&mut store, keep, "a", 0o644).unwrap();
  vol.write(&mut store, a, 0, b"untouched").unwrap();
  let b = vol.create_file_no(&mut store, work, "b", 0o644).unwrap();
  let c = vol.create_file_no(&mut store, keep, "c", 0o644).unwrap();
  vol.link_no(&mut store, root, "linked", c).unwrap();
  let first = vol.snapshot(&mut store).unwrap();
  vol.write(&mut store, b, 0, b"changed").unwrap();
  let written = vol.snapshot(&mut store).unwrap();
  assert_eq!(
    vol.paths_changed_between(&store, first, written).unwrap(),
    ["/work/b"]
  );
  vol
    .rename_no(&mut store, root, "work", root, "moved")
    .unwrap();
  let moved = vol.snapshot(&mut store).unwrap();
  assert_eq!(
    vol.paths_changed_between(&store, written, moved).unwrap(),
    ["/", "/moved", "/moved/b", "/work", "/work/b"]
  );
  vol.write(&mut store, c, 0, b"through an alias").unwrap();
  let linked = vol.snapshot(&mut store).unwrap();
  assert_eq!(
    vol.paths_changed_between(&store, moved, linked).unwrap(),
    ["/keep/c", "/linked"]
  );
}

/// Shape: files in the wide span below, past one inode-table node's fan-out so the span copies many nodes at
/// more than one level (the generated histories above stay within a few).
const WIDE_FILES: usize = 10_000;
/// Shape: files per directory in the wide span, so its tree has both breadth and depth.
const WIDE_FILES_PER_DIR: usize = 256;

/// AUD-29-76 (the large-span measurement found it). Do: create `WIDE_FILES` empty files, `WIDE_FILES_PER_DIR` to
/// a directory; snapshot; write one byte to every file; snapshot. Expect: the diff names every file, by its
/// path, and nothing else (no listing changed).
#[test]
fn a_write_to_every_file_of_a_wide_volume_names_every_file() {
  let mut store = store();
  let mut vol = volume(&mut store, QUOTA);
  let root = vol.root_inode(&store).unwrap();
  let mut files = Vec::with_capacity(WIDE_FILES);
  let mut dir = root;
  for index in 0..WIDE_FILES {
    if index % WIDE_FILES_PER_DIR == 0 {
      dir = vol
        .mkdir_no(
          &mut store,
          root,
          &format!("dir{}", index / WIDE_FILES_PER_DIR),
          0o755,
        )
        .unwrap();
    }
    let path = format!("/dir{}/f{index}", index / WIDE_FILES_PER_DIR);
    files.push((
      vol
        .create_file_no(&mut store, dir, &format!("f{index}"), 0o644)
        .unwrap(),
      path,
    ));
  }
  let from = vol.snapshot(&mut store).unwrap();
  for (file, _) in &files {
    vol.write(&mut store, *file, 0, b"x").unwrap();
  }
  let to = vol.snapshot(&mut store).unwrap();
  let named: BTreeSet<String> = vol
    .paths_changed_between(&store, from, to)
    .unwrap()
    .into_iter()
    .collect();
  let expected: BTreeSet<String> = files.into_iter().map(|(_, path)| path).collect();
  let missing: Vec<&String> = expected.difference(&named).take(8).collect();
  let extra: Vec<&String> = named.difference(&expected).take(8).collect();
  assert!(
    missing.is_empty() && extra.is_empty(),
    "{} named of {}; first missing {missing:?}; first extra {extra:?}",
    named.len(),
    expected.len()
  );
}
