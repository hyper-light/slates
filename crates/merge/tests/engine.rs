//! Tests for the single-node merge engine (§4.16; T-6.x). An increment is the deriver's ops
//! document plus its sealed post-state; the engine resolves it and decides every dimension against
//! the intervening history. These tests build increments with a small builder that lays out an ops
//! document and a consistent post-state the way the deriver does (paths interned, content named by
//! post-state offsets), then submit them: disjoint edits merge, overlapping ones conflict unless
//! identical, namespace changes (create, unlink, mkdir, rmdir, mode, symlink, hard link, rename,
//! xattr) merge per path with their conflict classes, several dimensions merge on one path in one
//! increment, a directory move merges as its child ops, and retries are idempotent by identity.
// Test harness code: an unwrap or a panic in a helper is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{BLOCKS, Build, base_file, block_edits, reference_merge};
use proptest::prelude::*;
use slates_merge::engine::{Green, Increment, Outcome, Rebased};
use slates_merge::increment::VolumeOp;
use slates_merge::ops_doc::OpKind;
use slates_merge::verdict::MergeConflictClass;

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

/// An rmdir of a path that names a hard link is a type conflict, not a silent no-op (§4.16: the
/// agent's view had a directory there; the green has a hard link). merge_rmdir already refused a
/// file or symlink at the path; a hard link is the same kind of type mismatch.
#[test]
fn rmdir_of_a_hardlink_conflicts() {
  let mut green = Green::new();
  green.submit(&Build::new().create("f", b"x").at(1, 0));
  green.submit(&Build::new().link("h", "f").at(2, 1));
  let outcome = green.submit(&Build::new().rmdir("h").at(3, 2));
  assert_eq!(
    conflict_class(&outcome),
    Some(MergeConflictClass::TypeChanged),
    "rmdir of a hard link is a type conflict"
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
// model-based tests): the block reference in `tests/common/mod.rs`, shared with the sixteen-agent
// schedule of T-6.7 (`tests/shuttle_green.rs`). A serial, obviously-correct reference decides the
// merge block by block; the engine must agree on every generated history.

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

/// Shape: a large file the small-edit history amplification is measured on — 64 KiB, so each edit's
/// retained full copy is visible in whole kibibytes against the few bytes the edit itself carries.
const LARGE_FILE_BYTES: usize = 64 * 1024;
/// Shape: how many small edits are applied to it.
const SMALL_EDITS: u64 = 8;
/// Shape: the rejected-result cache budget the conflict flood is bounded by — room for a few
/// windows on a short path, so the flood reaches the bound in a handful of submissions.
const REJECTED_BUDGET_BYTES: usize = 256;
/// Shape: how many distinct conflicting increments the flood submits, several times the budget's worth.
const CONFLICT_FLOOD: u8 = 40;

/// A distinct increment identity for the `n`th conflicting submission.
fn flood_id(n: u8) -> [u8; 32] {
  let mut id = [0xC0u8; 32];
  id[0] = n;
  id
}

/// §4.2 bounded growth, §4.16 (AUD-16): a flood of distinct increments that all conflict against an
/// old base grows the rejected-result cache only to its byte budget — the oldest results are evicted
/// first, every eviction is counted, the running byte total never exceeds the budget and is balanced
/// (each retained entry's bytes are what the cache holds) — while the accepted history stays fixed
/// (the head does not move). A retry of a result still retained is served from the cache; a retry of
/// an evicted one is judged again and reaches the same verdict. Non-vacuous: evictions happened.
#[test]
fn a_conflict_flood_reaches_the_rejected_result_bound_with_balanced_accounting() {
  let mut green = Green::new();
  green.set_rejected_budget(REJECTED_BUDGET_BYTES);
  green.submit(&Build::new().create("f", b"....................").at(1, 0));
  // Version 2 changes the range every flood increment will also change, from the old base 1.
  assert_eq!(
    green.submit(&Build::new().overwrite("f", 0, b"AAAA").at(2, 1)),
    Outcome::Accepted { version: 2 }
  );
  let head = green.head();
  let first = flood_conflicts(&mut green);
  let (entries, bytes, evicted) = green.rejected_cache();
  assert!(
    evicted >= 1 && u64::from(CONFLICT_FLOOD) == u64::try_from(entries).unwrap() + evicted,
    "the flood was bounded by eviction: {entries} retained + {evicted} evicted = {CONFLICT_FLOOD}"
  );
  assert_eq!(
    (green.head(), green.retained_bytes().rejected),
    (head, bytes),
    "the accepted history is fixed (nothing was committed) and the one running total is reported consistently"
  );
  assert_retry_semantics(&mut green, &first);
}

/// The flood increment `n`: the same four-byte overwrite of the range version 2 changed, from the
/// old base 1, under its own identity.
fn flood_increment(n: u8) -> slates_merge::engine::Increment {
  Build::new()
    .overwrite("f", 0, &[b'B' + (n % 20), b'x', b'y', b'z'])
    .with_id(flood_id(n), 1)
}

/// Submits the whole conflict flood, asserting after each that it conflicted and that the cache stayed
/// within its budget with at least one entry; returns the first conflict's outcome.
fn flood_conflicts(green: &mut Green) -> Outcome {
  let mut first = None;
  for n in 0..CONFLICT_FLOOD {
    let outcome = green.submit(&flood_increment(n));
    assert!(
      matches!(outcome, Outcome::Conflict { .. }),
      "flood increment {n} conflicts with version 2: {outcome:?}"
    );
    let (entries, bytes, _) = green.rejected_cache();
    assert!(
      bytes <= REJECTED_BUDGET_BYTES && entries >= 1,
      "after {n}: {entries} entries, {bytes} bytes within the {REJECTED_BUDGET_BYTES}-byte budget"
    );
    first.get_or_insert(outcome);
  }
  first.expect("the flood submitted at least one increment")
}

/// The retry semantics after the flood: the oldest result was evicted, so its retry is judged again
/// (the same deterministic verdict) and re-enters the bounded cache; the newest is still retained,
/// so its retry is served from the cache with the cache unchanged.
fn assert_retry_semantics(green: &mut Green, first: &Outcome) {
  let (before_entries, _, before_evicted) = green.rejected_cache();
  assert_eq!(
    &green.submit(&flood_increment(0)),
    first,
    "the same deterministic verdict"
  );
  let (after_entries, _, after_evicted) = green.rejected_cache();
  assert!(
    after_evicted >= before_evicted && after_entries <= before_entries.max(1),
    "re-judging the evicted result re-entered the bounded cache (evicting the oldest again if full)"
  );
  let cache_before = green.rejected_cache();
  let _ = green.submit(&flood_increment(CONFLICT_FLOOD - 1));
  assert_eq!(
    green.rejected_cache(),
    cache_before,
    "a retained result is served from the cache: nothing evicted, nothing added"
  );
}

/// §4.2 all-cost admission, §4.16 "delta retention before folding" (AUD-16): small edits to a large
/// file retain a full copy of the file per edit — **measured** here: after `SMALL_EDITS` four-byte
/// overwrites of a 64 KiB file the content history holds one 64 KiB copy per superseded version, the
/// engine's running total equals the recount, and every version still reconstructs. Folding the
/// histories to the head releases every copy no reader can name (the running total and the recount
/// agree at zero), keeps the current file exactly, and keeps a version at or above the floor
/// reconstructible; folding to a lower floor keeps the one copy in effect there.
#[test]
fn small_edits_to_a_large_file_are_charged_as_retained_history_and_fold_below_the_floor() {
  let mut green = Green::new();
  let large = vec![b'.'; LARGE_FILE_BYTES];
  green.submit(&Build::new().create("big", &large).at(1, 0));
  assert_eq!(
    green.retained_bytes().history,
    0,
    "the current file is content, not history"
  );
  apply_small_edits(&mut green);
  assert_amplified(&green);
  let head = green.head();
  assert_fold_bounded_by_the_reachable_floor(&mut green, head);
  assert_fold_to_head(&mut green, head);
}

/// Folding is budget-driven and floor-bounded: under an ample budget nothing folds (an earlier
/// version can still be re-pinned); under a budget of five copies with a live reader at version 3 the
/// oldest copies go one version at a time until the floor binds — six copies remain, balanced, and
/// versions at or above 3 still reconstruct.
fn assert_fold_bounded_by_the_reachable_floor(green: &mut Green, head: u64) {
  let copies = usize::try_from(SMALL_EDITS).unwrap();
  assert_eq!(
    (
      green.fold_history_to_budget(3, copies * LARGE_FILE_BYTES),
      green.folded_below()
    ),
    (0, 0),
    "an ample budget folds nothing: every version stays reconstructible"
  );
  let released = green.fold_history_to_budget(3, (copies - 3) * LARGE_FILE_BYTES);
  assert_eq!(
    (
      released,
      green.folded_below(),
      green.retained_bytes().history
    ),
    (2 * LARGE_FILE_BYTES, 3, (copies - 2) * LARGE_FILE_BYTES),
    "the two copies below the reader's version were released, then the floor bound the fold above \
     the budget"
  );
  assert_eq!(
    green.retained_bytes().history,
    green.history_bytes_recounted(),
    "balanced after the fold"
  );
  let at_three = green.content_at("big", 3).expect("the floor reconstructs");
  assert_eq!(&at_three[..16], b"EDIT....EDIT....");
  assert_eq!(
    green.content_at("big", head).as_deref(),
    green.content("big")
  );
}

/// Applies `SMALL_EDITS` four-byte overwrites to `big`, each based on the head it found, each accepted.
fn apply_small_edits(green: &mut Green) {
  for edit in 0..SMALL_EDITS {
    let base = green.head();
    let outcome = green.submit(
      &Build::new()
        .overwrite("big", edit * 8, b"EDIT")
        .with_id(flood_id(u8::try_from(edit).unwrap()), base),
    );
    assert_eq!(outcome, Outcome::Accepted { version: base + 1 });
  }
}

/// The measured amplification: one full copy of the superseded file per small edit, the running
/// total equal to the recount, the current file held once, and an early version reconstructing.
fn assert_amplified(green: &Green) {
  let retained = green.retained_bytes();
  assert_eq!(
    (retained.history, retained.content),
    (
      usize::try_from(SMALL_EDITS).unwrap() * LARGE_FILE_BYTES,
      LARGE_FILE_BYTES
    ),
    "each small edit retained a full copy of the superseded file (the measured amplification); the \
     current file once"
  );
  assert_eq!(
    retained.history,
    green.history_bytes_recounted(),
    "the running total is what the histories hold"
  );
  let at_two = green.content_at("big", 2).expect("version 2 reconstructs");
  assert_eq!(
    &at_two[..8],
    b"EDIT....",
    "version 2 holds the first edit only"
  );
}

/// With no reader below the head and no budget at all, folding releases every retained copy, keeps
/// the current file exactly, and is idempotent.
fn assert_fold_to_head(green: &mut Green, head: u64) {
  let released = green.fold_history_to_budget(head, 0);
  assert!(released > 0, "something was left to release");
  assert_eq!(
    (
      green.retained_bytes().history,
      green.history_bytes_recounted(),
      green.content("big").map(|bytes| bytes.len())
    ),
    (0, 0, Some(LARGE_FILE_BYTES)),
    "nothing is retained beyond the current file, which is untouched"
  );
  assert_eq!(green.fold_history_to_budget(head, 0), 0, "idempotent");
}
