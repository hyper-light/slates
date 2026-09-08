//! Tests for the single-node merge engine (§4.16; T-6.x). An increment is the deriver's ops
//! document plus its sealed post-state; the engine resolves it and decides every dimension against
//! the intervening history. These tests build increments with a small builder that lays out an ops
//! document and a consistent post-state the way the deriver does (paths interned, content named by
//! post-state offsets), then submit them: disjoint edits merge, overlapping ones conflict unless
//! identical, namespace changes (create, unlink, mkdir, rmdir, mode, symlink, hard link, rename,
//! xattr) merge per path with their conflict classes, several dimensions merge on one path in one
//! increment, a directory move merges as its child ops, and retries are idempotent by identity.

use proptest::prelude::*;
use slates_merge::engine::{Green, Increment, Outcome, Rebased};
use slates_merge::increment::VolumeOp;
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

/// Per-range identity, not whole-file (§4.16 "The verdict, two pure passes", D-27: "memcmp only
/// for same-range candidates"). An increment makes a disjoint edit (a new one, elsewhere in the
/// file) *and* an edit on a range an intervening change already made identically. The design's
/// verdict decides each range on its own: the disjoint range accepts at its position, the same-span
/// range is accept-identical by a memcmp of that span alone. The disjoint edit elsewhere must not
/// turn the identical overlap into a conflict — which a whole-file compare would. T-6.x.
#[test]
fn a_disjoint_edit_plus_an_identical_overlap_accepts_per_range() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"01234567").at(1, 0));
  // An intervening change overwrites the second half with "WXYZ" (green head = "0123WXYZ").
  green.submit(&Build::new().overwrite("f", 4, b"WXYZ").at(2, 1));
  // An agent based on v1 overwrites the first half (disjoint, new) and the second half with the
  // *same* bytes the intervening change produced. Per range: accept the first, accept-identical the
  // second; overall accept. A whole-file identity check conflicts here (the file differs by the new
  // first half), which is the bug this pins.
  let outcome = green.submit(
    &Build::new()
      .overwrite("f", 0, b"ABCD")
      .overwrite("f", 4, b"WXYZ")
      .at(3, 1),
  );
  assert_eq!(
    outcome,
    Outcome::Accepted { version: 3 },
    "per-range: a disjoint edit plus an identical overlap accepts"
  );
  assert_eq!(green.content("f"), Some(b"ABCDWXYZ".as_slice()));
}

/// Unlinking a symlink removes it (§4.16: the unlink op applies to a symlink path, not only a
/// regular file). A conflict-free removal from a version that still holds the symlink accepts.
#[test]
fn unlinking_a_symlink_removes_it() {
  let mut green = Green::new();
  green.submit(&Build::new().symlink("l", "target").at(1, 0));
  assert_eq!(green.symlink("l"), Some("target"));
  let outcome = green.submit(&Build::new().remove("l").at(2, 1));
  assert_eq!(outcome, Outcome::Accepted { version: 2 });
  assert_eq!(green.symlink("l"), None, "unlink removes the symlink");
}

/// Unlinking a hard link removes that name (§4.16; the shared file's fate is the volume's concern
/// at apply time, not the merge's — the merge removes the namespace edge).
#[test]
fn unlinking_a_hardlink_removes_it() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"shared").at(1, 0));
  green.submit(&Build::new().link("h", "f").at(2, 1));
  assert_eq!(green.hardlink("h"), Some("f"));
  let outcome = green.submit(&Build::new().remove("h").at(3, 2));
  assert_eq!(outcome, Outcome::Accepted { version: 3 });
  assert_eq!(green.hardlink("h"), None, "unlink removes the hard link");
}

/// `base_at` reconstructs every dimension at an intervening version, not only file content
/// (§4.16: a lagging work derives against the base it declared, so directories, modes, symlinks,
/// hard links and xattrs at that version must be exact). Each dimension is changed after version 1,
/// so a correct reconstruction must return version 1's value, never the head's.
#[test]
fn base_at_reconstructs_every_dimension_at_an_intervening_version() {
  let mut green = Green::new();
  // Version 1: the paths exist.
  green.submit(&Build::new().create("f", b"hello").mkdir("d").at(1, 0));
  // Version 2: metadata set on the existing file, a symlink and a hard link created.
  green.submit(
    &Build::new()
      .setmode("f", 0o600)
      .symlink("l", "target")
      .link("h", "f")
      .setxattr("f", "user.k", b"v")
      .at(2, 1),
  );
  // Version 3 changes every dimension and removes the directory, so version 2's state cannot be
  // read from the head — it must be reconstructed.
  green.submit(
    &Build::new()
      .overwrite("f", 0, b"world")
      .setmode("f", 0o644)
      .symlink("l", "elsewhere")
      .setxattr("f", "user.k", b"w")
      .rmdir("d")
      .at(3, 2),
  );

  let base = green.base_at(2);
  let mode_of = |p: &str| base.modes.iter().find(|(q, _)| q == p).map(|(_, m)| *m);
  let symlink_of = |p: &str| {
    base
      .symlinks
      .iter()
      .find(|(q, _)| q == p)
      .map(|(_, t)| t.clone())
  };
  let hardlink_of = |p: &str| {
    base
      .hardlinks
      .iter()
      .find(|(q, _)| q == p)
      .map(|(_, t)| t.clone())
  };
  let xattr_of = |p: &str, n: &str| {
    base
      .xattrs
      .iter()
      .find(|(q, m, _)| q == p && m == n)
      .map(|(_, _, v)| v.clone())
  };
  let file_len = |p: &str| base.files.iter().find(|(q, _)| q == p).map(|(_, l)| *l);

  assert_eq!(file_len("f"), Some(5), "f is 'hello' at v2");
  assert!(base.dirs.contains(&"d".to_string()), "dir d exists at v2");
  assert_eq!(mode_of("f"), Some(0o600), "f's mode at v2");
  assert_eq!(symlink_of("l").as_deref(), Some("target"), "symlink at v2");
  assert_eq!(hardlink_of("h").as_deref(), Some("f"), "hard link at v2");
  assert_eq!(
    xattr_of("f", "user.k").as_deref(),
    Some(b"v".as_slice()),
    "xattr at v2"
  );
}

/// A file created and chmod'd in one increment accepts, with the mode applied (§4.16: the
/// increment's own create establishes the path for its metadata dimensions — the deriver composed
/// them into one increment, so the engine must not conflict the mode against a not-yet-committed
/// file). T-6.x.
#[test]
fn create_then_setmode_in_one_increment_accepts() {
  let mut green = Green::new();
  let outcome = green.submit(&Build::new().create("f", b"hi").setmode("f", 0o600).at(1, 0));
  assert_eq!(outcome, Outcome::Accepted { version: 1 });
  assert_eq!(green.content("f"), Some(b"hi".as_slice()));
  assert_eq!(
    green.mode("f"),
    Some(0o600),
    "the mode set at creation applies"
  );
}

/// A file created and given an xattr in one increment accepts, with the xattr applied.
#[test]
fn create_then_setxattr_in_one_increment_accepts() {
  let mut green = Green::new();
  let outcome = green.submit(
    &Build::new()
      .create("f", b"hi")
      .setxattr("f", "user.k", b"v")
      .at(1, 0),
  );
  assert_eq!(outcome, Outcome::Accepted { version: 1 });
  assert_eq!(green.xattr("f", "user.k"), Some(b"v".as_slice()));
}

/// A directory created and chmod'd in one increment accepts, with the mode applied.
#[test]
fn mkdir_then_setmode_in_one_increment_accepts() {
  let mut green = Green::new();
  let outcome = green.submit(&Build::new().mkdir("d").setmode("d", 0o700).at(1, 0));
  assert_eq!(outcome, Outcome::Accepted { version: 1 });
  assert_eq!(green.mode("d"), Some(0o700));
}

/// Renaming a symlink moves the link, not only a file (§4.16: rename applies to whatever the source
/// names). The source name is gone and the target is unchanged at the destination.
#[test]
fn renaming_a_symlink_moves_it() {
  let mut green = Green::new();
  green.submit(&Build::new().symlink("l", "target").at(1, 0));
  let outcome = green.submit(&Build::new().rename("l", "m").at(2, 1));
  assert_eq!(outcome, Outcome::Accepted { version: 2 });
  assert_eq!(green.symlink("l"), None, "the source name is gone");
  assert_eq!(
    green.symlink("m"),
    Some("target"),
    "the link moved to the destination"
  );
}

/// Renaming a hard link moves that name (the shared file is untouched).
#[test]
fn renaming_a_hardlink_moves_it() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"shared").at(1, 0));
  green.submit(&Build::new().link("h", "f").at(2, 1));
  let outcome = green.submit(&Build::new().rename("h", "g").at(3, 2));
  assert_eq!(outcome, Outcome::Accepted { version: 3 });
  assert_eq!(green.hardlink("h"), None, "the source name is gone");
  assert_eq!(green.hardlink("g"), Some("f"), "the hard link moved");
  assert_eq!(
    green.content("f"),
    Some(b"shared".as_slice()),
    "the shared file is untouched"
  );
}

/// A symlink created where an intervening change put a hard link is a type conflict, not a second
/// entry at one path (§4.16: one kind per path). Without the cross-kind check the path would hold
/// both a symlink and a hard link — a corrupt state.
#[test]
fn a_symlink_over_an_intervening_hardlink_conflicts() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"x").at(1, 0));
  green.submit(&Build::new().link("p", "f").at(2, 1)); // an intervening hard link at p
  let outcome = green.submit(&Build::new().symlink("p", "t").at(3, 1)); // agent (based on v1) symlinks p
  assert_eq!(
    conflict_class(&outcome),
    Some(MergeConflictClass::TypeChanged),
    "a symlink over an intervening hard link is a type conflict"
  );
  assert_eq!(
    green.symlink("p"),
    None,
    "no symlink was created over the hard link"
  );
}

/// A hard link created where an intervening change put a symlink is a type conflict (the mirror).
#[test]
fn a_hardlink_over_an_intervening_symlink_conflicts() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"x").at(1, 0));
  green.submit(&Build::new().symlink("p", "t").at(2, 1)); // an intervening symlink at p
  let outcome = green.submit(&Build::new().link("p", "f").at(3, 1)); // agent (based on v1) hard-links p
  assert_eq!(
    conflict_class(&outcome),
    Some(MergeConflictClass::TypeChanged),
    "a hard link over an intervening symlink is a type conflict"
  );
  assert_eq!(
    green.hardlink("p"),
    None,
    "no hard link was created over the symlink"
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

/// T-6.x §4.16 "Rebase, the only corrective path": a work whose pending operations map cleanly onto
/// the head is rebased — its base becomes the head, its content becomes the head's files with the
/// mapped edits re-applied, and its journal is restated in head coordinates — while the green itself
/// is committed nothing. Here a work based on version 1 overwrites the tail of `f`; the head moved
/// under it by inserting two bytes at the front, so the tail op maps two bytes forward.
#[test]
fn rebase_maps_a_clean_work_forward_and_commits_nothing() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"0123456789").at(1, 0));
  green.submit(&Build::new().content(OpKind::Insert, "f", 0, b"AB").at(2, 1));
  assert_eq!(green.head(), 2);

  let rebased = green.rebase(&Build::new().overwrite("f", 8, b"YY").at(3, 1));

  // The green did not move: a rebase commits nothing.
  assert_eq!(green.head(), 2, "rebase commits nothing to the green");
  assert_eq!(
    green.content("f"),
    Some(b"AB0123456789".as_slice()),
    "the green's bytes are unchanged by a rebase"
  );
  let Rebased::Rebased {
    version,
    files,
    journal,
  } = rebased
  else {
    panic!("the clean work rebases");
  };
  assert_eq!(version, 2, "the work is now based on the head");
  assert_eq!(
    files.get("f").map(Vec::as_slice),
    Some(b"AB01234567YY".as_slice()),
    "the head's file with the tail overwrite mapped two bytes forward"
  );
  assert_eq!(
    journal,
    vec![VolumeOp::Overwrite {
      path: "f".to_owned(),
      at: 10,
      len: 2,
    }],
    "the journal is restated at the head offset (10), not the base offset (8)"
  );
}

/// A rebase whose operations still conflict returns the windows and changes nothing — the green is
/// untouched and the agent resolves each window and rebases again (§4.16).
#[test]
fn rebase_returns_windows_and_changes_nothing_on_conflict() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"0123456789").at(1, 0));
  green.submit(&Build::new().overwrite("f", 2, b"XXX").at(2, 1));
  assert_eq!(green.head(), 2);

  let rebased = green.rebase(&Build::new().overwrite("f", 3, b"YYY").at(3, 1));

  assert!(
    matches!(rebased, Rebased::Conflict { windows } if windows.iter().any(|w| w.path == "f")),
    "the overlapping edit conflicts on rebase"
  );
  assert_eq!(green.head(), 2, "a conflicting rebase commits nothing");
  assert_eq!(
    green.content("f"),
    Some(b"01XXX56789".as_slice()),
    "the green's bytes are unchanged by a conflicting rebase"
  );
}

/// The rebase restates the journal as fine-grained operations, not a whole-file rewrite: after a
/// rebase, a further disjoint move of the head still merges the rebased operation rather than
/// conflicting. Non-vacuity for the mapping — a whole-file restatement would overlap the front
/// change and conflict.
#[test]
fn a_rebased_operation_still_merges_a_later_disjoint_head_move() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"0123456789").at(1, 0));
  green.submit(&Build::new().content(OpKind::Insert, "f", 0, b"AB").at(2, 1));
  let Rebased::Rebased {
    version, journal, ..
  } = green.rebase(&Build::new().overwrite("f", 8, b"YY").at(3, 1))
  else {
    panic!("clean rebase");
  };
  assert_eq!(version, 2);
  assert_eq!(
    journal,
    vec![VolumeOp::Overwrite {
      path: "f".to_owned(),
      at: 10,
      len: 2,
    }],
    "the rebased journal is the fine-grained tail op at the head offset"
  );

  // The head moves again, disjoint from the rebased op (the front, not the tail).
  green.submit(&Build::new().overwrite("f", 0, b"CD").at(4, 2));
  assert_eq!(green.head(), 3);

  // Resubmitting the rebased op (base 2) merges past the disjoint front change.
  let out = green.submit(&Build::new().overwrite("f", 10, b"YY").at(5, 2));
  assert_eq!(
    out,
    Outcome::Accepted { version: 4 },
    "the fine-grained rebased op merges the disjoint head move"
  );
}

/// An increment round-trips through encode/decode exactly (§4.8 chain persistence): the identity,
/// base, ops document and post-state all survive, so a replayed chain rebuilds the same green.
#[test]
fn an_increment_round_trips_through_encode_and_decode() {
  let inc = Build::new()
    .create("f", b"hello")
    .overwrite("f", 0, b"HELLO")
    .setxattr("f", "user.k", b"v")
    .at(7, 3);
  let decoded = Increment::decode(&inc.encode()).expect("a valid increment decodes");
  assert_eq!(decoded, inc, "decode(encode(increment)) == increment");
}

/// A truncated increment refuses by type rather than panicking (§4.8 — a torn chain entry).
#[test]
fn a_truncated_increment_refuses() {
  let inc = Build::new().create("f", b"hello").at(1, 0);
  let bytes = inc.encode();
  assert!(
    Increment::decode(&bytes[..bytes.len() - 1]).is_err(),
    "a truncated increment does not decode"
  );
  assert!(
    Increment::decode(&[]).is_err(),
    "empty bytes do not decode to an increment"
  );
}

// ---------------------------------------------------------------------------
// The content verdict's generative oracle (§4.16 "The verdict, two pure passes", D-27; D-20's
// model-based tests). A serial, obviously-correct reference decides the merge block by block; the
// engine must agree on every generated history. To keep the reference free of coordinate reasoning
// (which would just re-implement the engine), every edit is a length-preserving overwrite of a
// whole fixed-size block, so no position ever shifts: the verdict is then purely per-block identity,
// which is exactly the design's per-range rule made trivial to state.

/// The fixed block width; a whole block is overwritten at once (length-preserving, so no shifts).
const BLOCK_LEN: usize = 4;
/// How many blocks the file has.
const BLOCKS: usize = 5;

/// The base file: block `b` is four copies of `b`, so the blocks are distinct and an untouched
/// block is recognisable in the merged result.
fn base_file() -> Vec<u8> {
  let mut file = Vec::with_capacity(BLOCKS * BLOCK_LEN);
  for b in 0..BLOCKS {
    file.extend(std::iter::repeat_n(u8::try_from(b).unwrap_or(0), BLOCK_LEN));
  }
  file
}

/// The bytes an edit with tag `t` writes into a block: four copies of `100 + t`, independent of
/// which side wrote them — so the same tag on both sides is byte-identical (a convergent edit) and
/// different tags differ. Tags are 1..=3; tag 0 means the side left the block untouched.
fn edit_bytes(tag: u8) -> Vec<u8> {
  vec![100 + tag; BLOCK_LEN]
}

/// Overwrites into one `Build`, one op per edited block (tag != 0), at the block's fixed offset.
fn block_edits(edits: &[u8]) -> Build {
  let mut build = Build::new();
  for (b, &tag) in edits.iter().enumerate() {
    if tag != 0 {
      build.overwrite("f", (b * BLOCK_LEN) as u64, &edit_bytes(tag));
    }
  }
  build
}

/// The reference merged file when the verdict accepts: per block, the agent's bytes if it edited the
/// block, else the intervening (green) bytes if it did, else the base bytes. Because an accepted
/// merge has no block both sides changed differently, this is well-defined.
fn reference_merge(green: &[u8], agent: &[u8]) -> Vec<u8> {
  let base = base_file();
  let mut out = Vec::with_capacity(base.len());
  for b in 0..BLOCKS {
    let range = b * BLOCK_LEN..(b + 1) * BLOCK_LEN;
    if agent[b] != 0 {
      out.extend_from_slice(&edit_bytes(agent[b]));
    } else if green[b] != 0 {
      out.extend_from_slice(&edit_bytes(green[b]));
    } else {
      out.extend_from_slice(&base[range]);
    }
  }
  out
}

proptest! {
  // Each block independently: 0 = untouched, 1..=3 = overwritten with that tag. The agent must
  // touch at least one block (an empty content increment is a different path).
  #[test]
  fn the_content_verdict_matches_the_block_oracle(
    green in prop::collection::vec(0u8..=3, BLOCKS),
    agent in prop::collection::vec(0u8..=3, BLOCKS),
  ) {
    prop_assume!(agent.iter().any(|&t| t != 0));

    // The design's per-block rule: a conflict iff some block was changed by both sides to
    // different bytes; otherwise every block accepts (disjoint) or accepts-identical (same tag).
    let expect_conflict =
      (0..BLOCKS).any(|b| green[b] != 0 && agent[b] != 0 && green[b] != agent[b]);

    let mut volume = Green::new();
    volume.submit(&Build::new().create("f", &base_file()).at(1, 0)); // version 1 = base
    let any_green = green.iter().any(|&t| t != 0);
    if any_green {
      volume.submit(&block_edits(&green).at(2, 1)); // version 2 = the intervening changes
    }
    // The agent is based on version 1, behind the intervening changes when there were any.
    let outcome = volume.submit(&block_edits(&agent).at(3, 1));

    if expect_conflict {
      prop_assert!(
        matches!(outcome, Outcome::Conflict { .. }),
        "a block changed differently by both sides must conflict; got {outcome:?}"
      );
    } else {
      prop_assert_eq!(&outcome, &Outcome::Accepted { version: if any_green { 3 } else { 2 } });
      let expected = reference_merge(&green, &agent);
      prop_assert_eq!(volume.content("f").unwrap(), expected.as_slice());
    }
  }
}
