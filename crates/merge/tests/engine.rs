//! Tests for the single-node merge engine (§4.16; T-6.x). An increment is the deriver's ops
//! document plus its sealed post-state; the engine resolves it and decides every dimension against
//! the intervening history. These tests build increments with a small builder that lays out an ops
//! document and a consistent post-state the way the deriver does (paths interned, content named by
//! post-state offsets), then submit them: disjoint edits merge, overlapping ones conflict unless
//! identical, namespace changes (create, unlink, mkdir, rmdir, mode, symlink, hard link, rename,
//! xattr) merge per path with their conflict classes, several dimensions merge on one path in one
//! increment, a directory move merges as its child ops, and retries are idempotent by identity.

use slates_merge::engine::{Green, Increment, Outcome};
use slates_merge::ops_doc::{Op, OpKind, OpsDoc};
use slates_merge::verdict::MergeConflictClass;

/// Builds an increment: an ops document with a consistent post-state (content and xattr bytes are
/// appended to the post-state and named by their offset). Paths are interned in use order; the
/// document is left un-canonicalized (the engine resolves by index, which the order does not
/// affect), and the identity is supplied by the test.
#[derive(Default)]
struct Build {
  doc: OpsDoc,
  post: Vec<u8>,
}

impl Build {
  fn new() -> Build {
    Build::default()
  }

  fn idx(&mut self, path: &str) -> u16 {
    self.doc.paths.intern(path)
  }

  fn stash(&mut self, bytes: &[u8]) -> u64 {
    let offset = self.post.len() as u64;
    self.post.extend_from_slice(bytes);
    offset
  }

  fn content(&mut self, kind: OpKind, path: &str, at: u64, bytes: &[u8]) -> &mut Build {
    let path_idx = self.idx(path);
    let src = self.stash(bytes);
    self.doc.ops.push(Op {
      kind,
      flags: 0,
      path: path_idx,
      at,
      len: bytes.len() as u64,
      src,
    });
    self
  }

  fn create(&mut self, path: &str, bytes: &[u8]) -> &mut Build {
    let path_idx = self.idx(path);
    self.doc.ops.push(Op {
      kind: OpKind::Create,
      flags: 0,
      path: path_idx,
      at: 0,
      len: 0,
      src: u64::MAX,
    });
    self.content(OpKind::Insert, path, 0, bytes)
  }

  fn overwrite(&mut self, path: &str, at: u64, bytes: &[u8]) -> &mut Build {
    self.content(OpKind::Overwrite, path, at, bytes)
  }

  fn insert(&mut self, path: &str, at: u64, bytes: &[u8]) -> &mut Build {
    self.content(OpKind::Insert, path, at, bytes)
  }

  fn edge(&mut self, kind: OpKind, path: &str, target: &str) -> &mut Build {
    let path_idx = self.idx(path);
    let target_idx = self.idx(target);
    self.doc.ops.push(Op {
      kind,
      flags: 0,
      path: path_idx,
      at: 0,
      len: 0,
      src: u64::from(target_idx),
    });
    self
  }

  fn rename(&mut self, from: &str, to: &str) -> &mut Build {
    // The rename op is keyed at the destination; its source is the `src` path index.
    self.edge(OpKind::Rename, to, from)
  }

  fn symlink(&mut self, path: &str, target: &str) -> &mut Build {
    self.edge(OpKind::Symlink, path, target)
  }

  fn link(&mut self, path: &str, target: &str) -> &mut Build {
    self.edge(OpKind::Link, path, target)
  }

  fn name_op(&mut self, kind: OpKind, path: &str) -> &mut Build {
    let path_idx = self.idx(path);
    self.doc.ops.push(Op {
      kind,
      flags: 0,
      path: path_idx,
      at: 0,
      len: 0,
      src: u64::MAX,
    });
    self
  }

  fn remove(&mut self, path: &str) -> &mut Build {
    self.name_op(OpKind::Unlink, path)
  }

  fn mkdir(&mut self, path: &str) -> &mut Build {
    self.name_op(OpKind::Mkdir, path)
  }

  fn rmdir(&mut self, path: &str) -> &mut Build {
    self.name_op(OpKind::Rmdir, path)
  }

  fn setmode(&mut self, path: &str, mode: u32) -> &mut Build {
    let path_idx = self.idx(path);
    self.doc.ops.push(Op {
      kind: OpKind::SetMode,
      flags: 0,
      path: path_idx,
      at: 0,
      len: u64::from(mode),
      src: u64::MAX,
    });
    self
  }

  fn setxattr(&mut self, path: &str, name: &str, value: &[u8]) -> &mut Build {
    let path_idx = self.idx(path);
    let name_idx = self.idx(name);
    let src = self.stash(value);
    self.doc.ops.push(Op {
      kind: OpKind::SetXattr,
      flags: 0,
      path: path_idx,
      at: u64::from(name_idx),
      len: value.len() as u64,
      src,
    });
    self
  }

  fn removexattr(&mut self, path: &str, name: &str) -> &mut Build {
    let path_idx = self.idx(path);
    let name_idx = self.idx(name);
    self.doc.ops.push(Op {
      kind: OpKind::RemoveXattr,
      flags: 0,
      path: path_idx,
      at: u64::from(name_idx),
      len: 0,
      src: u64::MAX,
    });
    self
  }

  fn at(&self, id: u8, base: u64) -> Increment {
    Increment {
      id: [id; 32],
      base,
      doc: self.doc.clone(),
      post_state: self.post.clone(),
    }
  }
}

/// The class of the first conflict window, or `None` when the outcome is not a conflict (so an
/// unexpected accept fails the comparison rather than needing a panic in this helper).
fn conflict_class(outcome: &Outcome) -> Option<MergeConflictClass> {
  match outcome {
    Outcome::Conflict { windows } => windows.first().map(|window| window.class),
    Outcome::Accepted { .. } => None,
  }
}

/// A single submit accepts and updates the green.
#[test]
fn a_submit_accepts_and_updates_the_green() {
  let mut green = Green::new();
  let outcome = green.submit(&Build::new().create("a", b"hello").at(1, 0));
  assert_eq!(outcome, Outcome::Accepted { version: 1 });
  assert_eq!(green.content("a"), Some(b"hello".as_slice()));
  assert_eq!(green.head(), 1);
}

/// Two increments editing disjoint files both accept.
#[test]
fn disjoint_files_both_accept() {
  let mut green = Green::new();
  green.submit(&Build::new().create("a", b"aaaa").at(1, 0));
  green.submit(&Build::new().create("b", b"bbbb").at(2, 0));
  assert_eq!(green.content("a"), Some(b"aaaa".as_slice()));
  assert_eq!(green.content("b"), Some(b"bbbb".as_slice()));
  assert_eq!(green.head(), 2);
}

/// Two increments editing disjoint ranges of the same file, both based on the same version, both
/// accept — the second's edit lands at the position shifted past the first (the range merge).
#[test]
fn disjoint_ranges_of_one_file_both_accept() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"....................").at(1, 0));
  let first = green.submit(&Build::new().overwrite("f", 0, b"AAAA").at(2, 1));
  assert_eq!(first, Outcome::Accepted { version: 2 });
  let second = green.submit(&Build::new().overwrite("f", 10, b"BBBB").at(3, 1));
  assert_eq!(
    second,
    Outcome::Accepted { version: 3 },
    "disjoint span merges"
  );
  let content = green.content("f").expect("present");
  assert_eq!(&content[0..4], b"AAAA");
  assert_eq!(&content[10..14], b"BBBB");
}

/// Two increments overwriting the same range from the same base conflict on the second.
#[test]
fn overlapping_edits_conflict() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"....................").at(1, 0));
  green.submit(&Build::new().overwrite("f", 5, b"AAAA").at(2, 1));
  let second = green.submit(&Build::new().overwrite("f", 5, b"BBBB").at(3, 1));
  assert!(
    matches!(second, Outcome::Conflict { .. }),
    "same range conflicts"
  );
  assert_eq!(&green.content("f").expect("present")[5..9], b"AAAA");
  assert_eq!(green.head(), 2);
}

/// Two agents making the identical edit: the second accepts as a no-op (both produced the same
/// bytes), not a conflict.
#[test]
fn an_identical_edit_accepts() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"....................").at(1, 0));
  green.submit(&Build::new().overwrite("f", 5, b"AAAA").at(2, 1));
  let same = green.submit(&Build::new().overwrite("f", 5, b"AAAA").at(3, 1));
  assert_eq!(
    same,
    Outcome::Accepted { version: 3 },
    "identical edit accepts"
  );
}

/// An insert before an accepted disjoint edit shifts the later one, and both apply.
#[test]
fn an_intervening_insert_shifts_a_later_edit() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"0123456789").at(1, 0));
  green.submit(&Build::new().insert("f", 0, b"XX").at(2, 1));
  let outcome = green.submit(&Build::new().overwrite("f", 8, b"YY").at(3, 1));
  assert_eq!(outcome, Outcome::Accepted { version: 3 });
  assert_eq!(green.content("f"), Some(b"XX01234567YY".as_slice()));
}

/// The fast path fires when a path is unchanged since the increment's base, and it is counted.
#[test]
fn the_fast_path_fires_for_an_unchanged_path() {
  let mut green = Green::new();
  green.submit(&Build::new().create("a", b"aaaa").at(1, 0));
  green.submit(&Build::new().create("b", b"bbbb").at(2, 1));
  let before = green.fast_path_hits();
  green.submit(&Build::new().overwrite("a", 0, b"AA").at(3, 2));
  assert!(green.fast_path_hits() > before, "the fast path was taken");
  assert_eq!(green.content("a"), Some(b"AAaa".as_slice()));
}

/// A resubmit of the same increment returns the cached result and does not double-apply.
#[test]
fn a_resubmit_is_idempotent() {
  let mut green = Green::new();
  let inc = Build::new().create("a", b"hello").at(1, 0);
  let first = green.submit(&inc);
  let again = green.submit(&inc);
  assert_eq!(first, again);
  assert_eq!(green.head(), 1, "not committed twice");
}

/// After a conflict, rebasing the increment onto the head accepts it.
#[test]
fn rebase_after_a_conflict_accepts() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"....................").at(1, 0));
  green.submit(&Build::new().overwrite("f", 5, b"AAAA").at(2, 1));
  let conflict = green.submit(&Build::new().overwrite("f", 5, b"BBBB").at(3, 1));
  assert!(matches!(conflict, Outcome::Conflict { .. }));
  let head = green.head();
  let rebased = green.submit(&Build::new().overwrite("f", 5, b"BBBB").at(4, head));
  assert_eq!(rebased, Outcome::Accepted { version: 3 });
  assert_eq!(&green.content("f").expect("present")[5..9], b"BBBB");
}

/// Two agents creating the same path with different bytes: the second conflicts (create/create).
#[test]
fn create_create_conflicts() {
  let mut green = Green::new();
  green.submit(&Build::new().create("seed", b"x").at(1, 0));
  green.submit(&Build::new().create("new", b"from A").at(2, 1));
  let b = green.submit(&Build::new().create("new", b"from B").at(3, 1));
  assert_eq!(conflict_class(&b), Some(MergeConflictClass::CreateCreate));
}

/// Two agents creating the same path with identical bytes: the second accepts (no-op).
#[test]
fn identical_create_accepts() {
  let mut green = Green::new();
  green.submit(&Build::new().create("seed", b"x").at(1, 0));
  green.submit(&Build::new().create("new", b"same").at(2, 1));
  let b = green.submit(&Build::new().create("new", b"same").at(3, 1));
  assert_eq!(b, Outcome::Accepted { version: 3 });
  assert_eq!(green.content("new"), Some(b"same".as_slice()));
}

/// One agent removes a file while another modifies it: delete/modify conflict.
#[test]
fn delete_versus_modify_conflicts() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"hello world").at(1, 0));
  let a = green.submit(&Build::new().remove("f").at(2, 1));
  assert_eq!(a, Outcome::Accepted { version: 2 });
  assert_eq!(green.content("f"), None);
  let b = green.submit(&Build::new().overwrite("f", 0, b"HELLO").at(3, 1));
  assert_eq!(conflict_class(&b), Some(MergeConflictClass::DeleteModify));
}

/// Removing a file unchanged since the base accepts; removing an already-gone file is a no-op.
#[test]
fn removes_accept() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"bye").at(1, 0));
  assert_eq!(
    green.submit(&Build::new().remove("f").at(2, 1)),
    Outcome::Accepted { version: 2 }
  );
  assert_eq!(green.content("f"), None);
  let again = green.submit(&Build::new().remove("f").at(3, 1));
  assert_eq!(
    again,
    Outcome::Accepted { version: 3 },
    "already gone is a no-op"
  );
}

/// mkdir accepts, two mkdirs accept, and a file and directory at one path is a type conflict.
#[test]
fn mkdir_and_type_conflicts() {
  let mut green = Green::new();
  assert_eq!(
    green.submit(&Build::new().mkdir("d").at(1, 0)),
    Outcome::Accepted { version: 1 }
  );
  assert!(green.is_dir("d"));
  assert_eq!(
    green.submit(&Build::new().mkdir("d").at(2, 1)),
    Outcome::Accepted { version: 2 }
  );
  green.submit(&Build::new().create("a", b"file").at(3, 2));
  assert_eq!(
    conflict_class(&green.submit(&Build::new().mkdir("a").at(4, 3))),
    Some(MergeConflictClass::TypeChanged)
  );
}

/// A mode change; two differing changes conflict, an identical one accepts, and it is independent
/// of a content edit.
#[test]
fn mode_merges() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"0123456789").at(1, 0));
  green.submit(&Build::new().setmode("f", 0o600).at(2, 1));
  assert_eq!(green.mode("f"), Some(0o600));
  assert_eq!(
    conflict_class(&green.submit(&Build::new().setmode("f", 0o644).at(3, 1))),
    Some(MergeConflictClass::MetaMeta)
  );
  assert_eq!(
    green.submit(&Build::new().setmode("f", 0o600).at(4, 1)),
    Outcome::Accepted { version: 3 }
  );
  // A content edit based on version 1 does not conflict with the intervening mode change.
  let edit = green.submit(&Build::new().overwrite("f", 0, b"AB").at(5, 1));
  assert_eq!(edit, Outcome::Accepted { version: 4 });
  assert_eq!(&green.content("f").expect("present")[0..2], b"AB");
  assert_eq!(green.mode("f"), Some(0o600));
}

/// Symlink merges: identical accepts, differing conflicts, file/symlink is a type conflict.
#[test]
fn symlink_merges() {
  let mut green = Green::new();
  green.submit(&Build::new().symlink("l", "target").at(1, 0));
  assert_eq!(green.symlink("l"), Some("target"));
  assert_eq!(
    green.submit(&Build::new().symlink("l", "target").at(2, 1)),
    Outcome::Accepted { version: 2 }
  );
  assert_eq!(
    conflict_class(&green.submit(&Build::new().symlink("l", "other").at(3, 1))),
    Some(MergeConflictClass::CreateCreate)
  );
  green.submit(&Build::new().create("a", b"file").at(4, 2));
  assert_eq!(
    conflict_class(&green.submit(&Build::new().symlink("a", "t").at(5, 4))),
    Some(MergeConflictClass::TypeChanged)
  );
}

/// A file rename moves the content and removes the source; a moved source conflicts.
#[test]
fn rename_merges() {
  let mut green = Green::new();
  green.submit(&Build::new().create("a", b"payload").at(1, 0));
  assert_eq!(
    green.submit(&Build::new().rename("a", "b").at(2, 1)),
    Outcome::Accepted { version: 2 }
  );
  assert_eq!(green.content("b"), Some(b"payload".as_slice()));
  assert_eq!(green.content("a"), None);
  green.submit(&Build::new().create("c", b"x").at(3, 2));
  green.submit(&Build::new().rename("c", "d").at(4, 3));
  assert_eq!(
    conflict_class(&green.submit(&Build::new().rename("c", "e").at(5, 3))),
    Some(MergeConflictClass::RenameRename)
  );
}

/// A rename moves the source's current content, so an intervening edit to the source follows it.
#[test]
fn rename_carries_an_intervening_edit() {
  let mut green = Green::new();
  green.submit(&Build::new().create("a", b"0123456789").at(1, 0));
  green.submit(&Build::new().overwrite("a", 0, b"XX").at(2, 1));
  let outcome = green.submit(&Build::new().rename("a", "b").at(3, 1));
  assert_eq!(outcome, Outcome::Accepted { version: 3 });
  assert_eq!(green.content("b"), Some(b"XX23456789".as_slice()));
  assert_eq!(green.content("a"), None);
}

/// rmdir removes an empty directory, a directory emptied by this increment, but conflicts on a live
/// child (including one an intervening change added).
#[test]
fn rmdir_merges() {
  let mut green = Green::new();
  green.submit(&Build::new().mkdir("d").at(1, 0));
  assert_eq!(
    green.submit(&Build::new().rmdir("d").at(2, 1)),
    Outcome::Accepted { version: 2 }
  );
  assert!(!green.is_dir("d"));
  // Emptied in the same increment (remove the child and the directory together).
  green.submit(&Build::new().mkdir("e").at(3, 2));
  green.submit(&Build::new().create("e/f", b"x").at(4, 3));
  let together = green.submit(&Build::new().remove("e/f").rmdir("e").at(5, 4));
  assert_eq!(together, Outcome::Accepted { version: 5 });
  assert!(!green.is_dir("e"));
  // A live child blocks removal.
  green.submit(&Build::new().mkdir("g").at(6, 5));
  green.submit(&Build::new().create("g/f", b"x").at(7, 6));
  assert_eq!(
    conflict_class(&green.submit(&Build::new().rmdir("g").at(8, 6))),
    Some(MergeConflictClass::DeleteModify)
  );
}

/// A hard link merges as a namespace edge: identical accepts, differing conflicts, file/link is a
/// type conflict.
#[test]
fn hard_link_merges() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"shared").at(1, 0));
  assert_eq!(
    green.submit(&Build::new().link("l", "f").at(2, 1)),
    Outcome::Accepted { version: 2 }
  );
  assert_eq!(green.hardlink("l"), Some("f"));
  assert_eq!(
    green.submit(&Build::new().link("l", "f").at(3, 2)),
    Outcome::Accepted { version: 3 },
    "identical link accepts"
  );
  assert_eq!(
    conflict_class(&green.submit(&Build::new().link("l", "other").at(4, 2))),
    Some(MergeConflictClass::CreateCreate)
  );
  assert_eq!(
    conflict_class(&green.submit(&Build::new().create("l", b"file").at(5, 3))),
    Some(MergeConflictClass::TypeChanged)
  );
}

/// Xattrs merge per (path, name): a set, an identical re-set, a differing conflict, independence
/// across names, and a removal.
#[test]
fn xattr_merges() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"body").at(1, 0));
  assert_eq!(
    green.submit(&Build::new().setxattr("f", "user.a", b"1").at(2, 1)),
    Outcome::Accepted { version: 2 }
  );
  assert_eq!(green.xattr("f", "user.a"), Some(b"1".as_slice()));
  assert_eq!(
    green.submit(&Build::new().setxattr("f", "user.a", b"1").at(3, 1)),
    Outcome::Accepted { version: 3 }
  );
  assert_eq!(
    conflict_class(&green.submit(&Build::new().setxattr("f", "user.a", b"2").at(4, 1))),
    Some(MergeConflictClass::MetaMeta)
  );
  // A different name is independent (based on version 1, before user.a existed).
  let other = green.submit(&Build::new().setxattr("f", "user.b", b"z").at(5, 1));
  assert_eq!(other, Outcome::Accepted { version: 4 });
  assert_eq!(green.xattr("f", "user.b"), Some(b"z".as_slice()));
  // A removal.
  let head = green.head();
  assert_eq!(
    green.submit(&Build::new().removexattr("f", "user.a").at(6, head)),
    Outcome::Accepted { version: 5 }
  );
  assert_eq!(green.xattr("f", "user.a"), None);
}

/// One increment that edits a file, changes its mode, and sets an xattr — several dimensions merge
/// together (only the ops document can express this).
#[test]
fn several_dimensions_on_one_path_in_one_increment() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"0123456789").at(1, 0));
  let combined = green.submit(
    &Build::new()
      .overwrite("f", 0, b"AB")
      .setmode("f", 0o640)
      .setxattr("f", "user.k", b"v")
      .at(2, 1),
  );
  assert_eq!(combined, Outcome::Accepted { version: 2 });
  assert_eq!(&green.content("f").expect("present")[0..2], b"AB");
  assert_eq!(green.mode("f"), Some(0o640));
  assert_eq!(green.xattr("f", "user.k"), Some(b"v".as_slice()));
}

/// A directory move is the deriver's child ops (a file rename plus mkdir and rmdir); the engine
/// merges it with no directory-rename special case.
#[test]
fn a_directory_move_merges_as_child_ops() {
  let mut green = Green::new();
  green.submit(&Build::new().mkdir("dir1").at(1, 0));
  green.submit(&Build::new().create("dir1/f", b"content").at(2, 1));
  // mv dir1 dir2 == mkdir dir2, rename dir1/f -> dir2/f, rmdir dir1.
  let moved = green.submit(
    &Build::new()
      .mkdir("dir2")
      .rename("dir1/f", "dir2/f")
      .rmdir("dir1")
      .at(3, 2),
  );
  assert_eq!(moved, Outcome::Accepted { version: 3 });
  assert!(green.is_dir("dir2"));
  assert!(!green.is_dir("dir1"));
  assert_eq!(green.content("dir2/f"), Some(b"content".as_slice()));
  assert_eq!(green.content("dir1/f"), None);
}
