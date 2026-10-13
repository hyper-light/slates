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
use common::{clone_config, store, volume};

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

/// §4.6 (POSIX `EPERM` "the target is a directory"). A hard link to a *directory* would make the tree a graph
/// with cycles — the same corruption rename-into-subtree is refused for, reached a different way. slates refuses
/// it and plants no alias; a hard link to a *file* works (non-vacuity).
#[test]
fn a_hard_link_to_a_directory_is_refused() {
  let mut store = store();
  let mut v = volume(&mut store, 1 << 20);
  let root = v.root_inode(&store).unwrap();
  let dir = v.mkdir_no(&mut store, root, "d", 0o755).unwrap();
  let file = v.create_file_no(&mut store, root, "f", 0o644).unwrap();

  assert!(
    v.link_no(&mut store, root, "dir_alias", dir).is_err(),
    "a hard link to a directory is refused"
  );
  assert!(
    v.lookup_no(&store, root, "dir_alias").is_err(),
    "no alias was planted for the directory"
  );
  // Non-vacuity: a hard link to a file works and names the same inode.
  assert!(
    v.link_no(&mut store, root, "f_alias", file).is_ok(),
    "a hard link to a file works"
  );
  assert_eq!(
    v.lookup_no(&store, root, "f_alias").unwrap().inode,
    file,
    "the file alias names the file's inode"
  );
}

/// §4.6 cross-tenant containment. Two volumes share the shard's one store; an inode number is prefix-encoded per
/// volume. A hard link that names ANOTHER volume's inode as its target must be refused — otherwise a tenant could
/// give itself a name for another tenant's file, a cross-volume read/write escape. Driven against the volume core
/// with two volumes of different prefixes in one store.
#[test]
fn a_hard_link_cannot_target_another_volumes_inode() {
  let mut store = store();
  let mut a = volume(&mut store, 1 << 20); // prefix 7
  let mut b = Volume::create(&mut store, clone_config(9)).unwrap(); // prefix 9, same store
  let root_a = a.root_inode(&store).unwrap();
  let root_b = b.root_inode(&store).unwrap();
  let b_file = b.create_file_no(&mut store, root_b, "secret", 0o644).unwrap();

  assert!(
    a.link_no(&mut store, root_a, "stolen", b_file).is_err(),
    "volume A cannot hard-link volume B's inode"
  );
  assert!(
    a.lookup_no(&store, root_a, "stolen").is_err(),
    "no cross-volume alias was planted"
  );
}
