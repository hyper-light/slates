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

/// §4.6 cross-tenant containment at the volume core. Two volumes share the shard's one store, and each keys its
/// inodes by counter within its own table (`inode_root`, resolved by `current_dir`/`inode`), so the volume prefix
/// in an `InodeNo` is the caller's promise, not a routing key the core re-checks — the primary cross-tenant
/// boundary is the bridge's capability, which binds every handle to its volume (proven at the NFS edge by
/// `nfs_hostile::a_valid_capability_cannot_be_restamped_onto_another_tenants_handle`). This proves the core is
/// *confined regardless*: handed another volume's inode number, a Volume resolves its OWN same-counter inode (or
/// misses) and never reaches the other volume's bytes, and the other volume stays intact. (A deliberate run with
/// the actual inode numbers established that a foreign `InodeNo` aliases the local same-counter inode — it is not
/// a path to another volume's data; an earlier framing of this test as "the op is refused" was wrong, since the
/// foreign number simply resolves locally.)
#[test]
fn a_foreign_inode_number_never_reaches_another_volumes_data() {
  let mut store = store();
  let mut a = volume(&mut store, 1 << 20); // prefix 7
  let mut b = Volume::create(&mut store, clone_config(9)).unwrap(); // prefix 9, same store
  let root_a = a.root_inode(&store).unwrap();
  let root_b = b.root_inode(&store).unwrap();
  // Each volume's first file lands at the same counter but a different prefix.
  let a_file = a.create_file_no(&mut store, root_a, "f", 0o644).unwrap();
  a.write(&mut store, a_file, 0, b"AAA").unwrap();
  let b_file = b.create_file_no(&mut store, root_b, "f", 0o644).unwrap();
  b.write(&mut store, b_file, 0, b"BBB").unwrap();
  assert_ne!(a_file, b_file, "the two files differ by their volume prefix");

  // A, handed B's inode number, reads its OWN same-counter bytes — never B's. A miss would be containment too.
  let mut buf = vec![0u8; 3];
  if let Ok(n) = a.read(&store, b_file, 0, &mut buf) {
    assert_eq!(
      &buf[..n],
      b"AAA",
      "A read its own file through B's inode number, never B's bytes"
    );
  }

  // B's file is intact and still its own — A's foreign-number access reached nothing of B's.
  let mut bbuf = vec![0u8; 3];
  let bn = b.read(&store, b_file, 0, &mut bbuf).unwrap();
  assert_eq!(&bbuf[..bn], b"BBB", "B's file is untouched and unreachable from A");
}
