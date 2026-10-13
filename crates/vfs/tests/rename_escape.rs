//! Rename containment (§4.6; POSIX `EINVAL` "An attempt was made to make a directory a subdirectory of
//! itself"). Moving a directory into its own subtree — into itself, a child, or a deep descendant — would make
//! the tree a cycle and orphan the moved subtree. slates refuses every such rename before anything changes
//! (`Volume::admit_rename` → `is_ancestor`), and a legitimate move still works. Driven directly against the
//! volume core.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_vfs::error::VfsError;
use slates_vfs::ids::InodeNo;
use slates_vfs::volume::{Store, Volume};

mod common;
use common::{store, volume};

/// Builds `/a/b/c/d` and returns `(a, b, c, d)` inode numbers.
fn tree(store: &mut Store, v: &mut Volume) -> (InodeNo, InodeNo, InodeNo, InodeNo) {
  let root = v.root_inode(store).unwrap();
  let a = v.mkdir_no(store, root, "a", 0o755).unwrap();
  let b = v.mkdir_no(store, a, "b", 0o755).unwrap();
  let c = v.mkdir_no(store, b, "c", 0o755).unwrap();
  let d = v.mkdir_no(store, c, "d", 0o755).unwrap();
  (a, b, c, d)
}

/// Do: try to rename `a` under itself, its child `b`, its deep descendant `c`, and `b` under its descendant
/// `c`. Expect: each refused `Invalid` (EINVAL), and `/a/b/c/d` unchanged afterwards.
#[test]
fn rename_refuses_a_directory_into_its_own_subtree() {
  let mut store = store();
  let mut v = volume(&mut store, 1 << 20);
  let root = v.root_inode(&store).unwrap();
  let (a, b, c, d) = tree(&mut store, &mut v);

  assert_eq!(
    v.rename_no(&mut store, root, "a", a, "moved"),
    Err(VfsError::Invalid),
    "'a' into itself"
  );
  assert_eq!(
    v.rename_no(&mut store, root, "a", b, "moved"),
    Err(VfsError::Invalid),
    "'a' into its child 'b'"
  );
  assert_eq!(
    v.rename_no(&mut store, root, "a", c, "moved"),
    Err(VfsError::Invalid),
    "'a' into its deep descendant 'c'"
  );
  assert_eq!(
    v.rename_no(&mut store, a, "b", c, "moved"),
    Err(VfsError::Invalid),
    "'b' into its descendant 'c'"
  );

  // The refused renames changed nothing: the chain still resolves to the same inodes.
  assert_eq!(v.lookup_no(&store, root, "a").unwrap().inode, a);
  assert_eq!(v.lookup_no(&store, a, "b").unwrap().inode, b);
  assert_eq!(v.lookup_no(&store, b, "c").unwrap().inode, c);
  assert_eq!(v.lookup_no(&store, c, "d").unwrap().inode, d);
}

/// Non-vacuity: a legitimate move of `a` into a sibling succeeds and carries its subtree.
#[test]
fn rename_into_a_sibling_carries_the_subtree() {
  let mut store = store();
  let mut v = volume(&mut store, 1 << 20);
  let root = v.root_inode(&store).unwrap();
  let (a, b, _c, _d) = tree(&mut store, &mut v);
  let sib = v.mkdir_no(&mut store, root, "sib", 0o755).unwrap();

  assert!(v.rename_no(&mut store, root, "a", sib, "a").is_ok());
  assert_eq!(v.lookup_no(&store, sib, "a").unwrap().inode, a, "'a' moved under 'sib'");
  assert_eq!(v.lookup_no(&store, a, "b").unwrap().inode, b, "its subtree came along");
}
