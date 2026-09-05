//! Tests for the single-node merge engine (§4.16; T-6.x). Increments based on a version are
//! submitted against the green; disjoint edits merge, overlapping ones conflict unless identical,
//! the fast path skips range work when a path is unchanged since the base, and retries are
//! idempotent by identity.

use std::collections::BTreeMap;

use slates_merge::engine::{Green, Increment, Outcome, PathChange};
use slates_merge::ops_doc::{Op, OpKind};

/// An op with a path index of zero (the engine keys by the change's path, not the op's index).
fn op(kind: OpKind, at: u64, len: u64, src: u64) -> Op {
  Op {
    kind,
    flags: 0,
    path: 0,
    at,
    len,
    src,
  }
}

/// A change that creates a file with `bytes`.
fn create(bytes: &[u8]) -> PathChange {
  PathChange::Create {
    post_state: bytes.to_vec(),
  }
}

/// A change that overwrites `base[at .. at + new.len())` in place with `new`.
fn overwrite(base: &[u8], at: usize, new: &[u8]) -> PathChange {
  let mut post_state = base.to_vec();
  post_state[at..at + new.len()].copy_from_slice(new);
  PathChange::Modify {
    ops: vec![op(
      OpKind::Overwrite,
      at as u64,
      new.len() as u64,
      at as u64,
    )],
    post_state,
  }
}

/// A change that inserts `new` at `at` in `base`.
fn insert(base: &[u8], at: usize, new: &[u8]) -> PathChange {
  let mut post_state = base.to_vec();
  post_state.splice(at..at, new.iter().copied());
  PathChange::Modify {
    ops: vec![op(OpKind::Insert, at as u64, new.len() as u64, at as u64)],
    post_state,
  }
}

/// An increment over one path.
fn increment(id: u8, base: u64, path: &str, change: PathChange) -> Increment {
  let mut changes = BTreeMap::new();
  changes.insert(path.to_owned(), change);
  Increment {
    id: [id; 32],
    base,
    changes,
  }
}

/// A single submit accepts and updates the green.
#[test]
fn a_submit_accepts_and_updates_the_green() {
  let mut green = Green::new();
  let outcome = green.submit(&increment(1, 0, "a", create(b"hello")));
  assert_eq!(outcome, Outcome::Accepted { version: 1 });
  assert_eq!(green.content("a"), Some(b"hello".as_slice()));
  assert_eq!(green.head(), 1);
}

/// Two increments editing disjoint files both accept.
#[test]
fn disjoint_files_both_accept() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "a", create(b"aaaa")));
  green.submit(&increment(2, 0, "b", create(b"bbbb")));
  assert_eq!(green.content("a"), Some(b"aaaa".as_slice()));
  assert_eq!(green.content("b"), Some(b"bbbb".as_slice()));
  assert_eq!(green.head(), 2);
}

/// Two increments editing disjoint ranges of the same file, both based on the same version, both
/// accept — the second's edit is applied at the position shifted past the first (the range merge).
#[test]
fn disjoint_ranges_of_one_file_both_accept() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "f", create(b"....................")));
  // Two agents clone version 1 and edit disjoint spans.
  let base = b"....................".to_vec();
  let first = green.submit(&increment(2, 1, "f", overwrite(&base, 0, b"AAAA")));
  assert_eq!(first, Outcome::Accepted { version: 2 });
  let second = green.submit(&increment(3, 1, "f", overwrite(&base, 10, b"BBBB")));
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
  green.submit(&increment(1, 0, "f", create(b"....................")));
  let base = b"....................".to_vec();
  let first = green.submit(&increment(2, 1, "f", overwrite(&base, 5, b"AAAA")));
  assert_eq!(first, Outcome::Accepted { version: 2 });
  let second = green.submit(&increment(3, 1, "f", overwrite(&base, 5, b"BBBB")));
  assert!(
    matches!(second, Outcome::Conflict { .. }),
    "same range conflicts"
  );
  // The green kept the first edit; the second changed nothing.
  assert_eq!(&green.content("f").expect("present")[5..9], b"AAAA");
  assert_eq!(green.head(), 2);
}

/// Two agents making the identical edit: the second accepts as a no-op (both produced the same
/// bytes), not a conflict.
#[test]
fn an_identical_edit_accepts() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "f", create(b"....................")));
  let base = b"....................".to_vec();
  green.submit(&increment(2, 1, "f", overwrite(&base, 5, b"AAAA")));
  let same = green.submit(&increment(3, 1, "f", overwrite(&base, 5, b"AAAA")));
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
  green.submit(&increment(1, 0, "f", create(b"0123456789")));
  let base = b"0123456789".to_vec();
  // Agent A inserts "XX" at 0 (shifts everything right by 2).
  green.submit(&increment(2, 1, "f", insert(&base, 0, b"XX")));
  // Agent B (based on 1) overwrites [8,10) — disjoint from A's insert at 0.
  let outcome = green.submit(&increment(3, 1, "f", overwrite(&base, 8, b"YY")));
  assert_eq!(outcome, Outcome::Accepted { version: 3 });
  // The green is "XX" + "01234567" + "YY" (B's edit shifted right by 2).
  assert_eq!(green.content("f"), Some(b"XX01234567YY".as_slice()));
}

/// The fast path fires when a path is unchanged since the increment's base, and it is counted.
#[test]
fn the_fast_path_fires_for_an_unchanged_path() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "a", create(b"aaaa")));
  green.submit(&increment(2, 1, "b", create(b"bbbb")));
  let before = green.fast_path_hits();
  // Editing "a" based on version 2: "a" was last changed at version 1 <= 2, so the fast path.
  green.submit(&increment(3, 2, "a", overwrite(b"aaaa", 0, b"AA")));
  assert!(green.fast_path_hits() > before, "the fast path was taken");
  assert_eq!(green.content("a"), Some(b"AAaa".as_slice()));
}

/// A resubmit of the same increment returns the cached result and does not double-apply.
#[test]
fn a_resubmit_is_idempotent() {
  let mut green = Green::new();
  let inc = increment(1, 0, "a", create(b"hello"));
  let first = green.submit(&inc);
  let again = green.submit(&inc);
  assert_eq!(first, again);
  assert_eq!(green.head(), 1, "not committed twice");
}

/// After a conflict, rebasing the increment onto the head accepts it.
#[test]
fn rebase_after_a_conflict_accepts() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "f", create(b"....................")));
  let base = b"....................".to_vec();
  green.submit(&increment(2, 1, "f", overwrite(&base, 5, b"AAAA")));
  let conflict = green.submit(&increment(3, 1, "f", overwrite(&base, 5, b"BBBB")));
  assert!(matches!(conflict, Outcome::Conflict { .. }));
  // The agent reads the head, rewrites its bytes onto it, and resubmits based on the head.
  let head = green.content("f").expect("present").to_vec();
  let rebased = green.submit(&increment(
    4,
    green.head(),
    "f",
    overwrite(&head, 5, b"BBBB"),
  ));
  assert_eq!(rebased, Outcome::Accepted { version: 3 });
  assert_eq!(&green.content("f").expect("present")[5..9], b"BBBB");
}

/// A `remove` change.
fn remove() -> PathChange {
  PathChange::Remove
}

/// Two agents creating the same path with different bytes: the second conflicts (create/create).
#[test]
fn create_create_conflicts() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "seed", create(b"x")));
  let a = green.submit(&increment(2, 1, "new", create(b"from A")));
  assert_eq!(a, Outcome::Accepted { version: 2 });
  let b = green.submit(&increment(3, 1, "new", create(b"from B")));
  match b {
    Outcome::Conflict { windows } => assert_eq!(
      windows[0].class,
      slates_merge::verdict::MergeConflictClass::CreateCreate
    ),
    other => panic!("expected create/create conflict, got {other:?}"),
  }
}

/// Two agents creating the same path with identical bytes: the second accepts (no-op).
#[test]
fn identical_create_accepts() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "seed", create(b"x")));
  green.submit(&increment(2, 1, "new", create(b"same")));
  let b = green.submit(&increment(3, 1, "new", create(b"same")));
  assert_eq!(b, Outcome::Accepted { version: 3 });
  assert_eq!(green.content("new"), Some(b"same".as_slice()));
}

/// One agent removes a file while another modifies it: delete/modify conflict.
#[test]
fn delete_versus_modify_conflicts() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "f", create(b"hello world")));
  // Agent A removes f at version 2.
  let a = green.submit(&increment(2, 1, "f", remove()));
  assert_eq!(a, Outcome::Accepted { version: 2 });
  assert_eq!(green.content("f"), None, "f is gone");
  // Agent B (based on 1) modifies f — but f was deleted.
  let b = green.submit(&increment(
    3,
    1,
    "f",
    overwrite(b"hello world", 0, b"HELLO"),
  ));
  match b {
    Outcome::Conflict { windows } => assert_eq!(
      windows[0].class,
      slates_merge::verdict::MergeConflictClass::DeleteModify
    ),
    other => panic!("expected delete/modify conflict, got {other:?}"),
  }
}

/// Removing a file unchanged since the base accepts.
#[test]
fn a_remove_accepts() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "f", create(b"bye")));
  let outcome = green.submit(&increment(2, 1, "f", remove()));
  assert_eq!(outcome, Outcome::Accepted { version: 2 });
  assert_eq!(green.content("f"), None);
}

/// Removing an already-removed file is a no-op accept (both agents deleted it).
#[test]
fn removing_an_already_removed_file_is_a_no_op() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "f", create(b"bye")));
  green.submit(&increment(2, 1, "f", remove()));
  let again = green.submit(&increment(3, 1, "f", remove()));
  assert_eq!(again, Outcome::Accepted { version: 3 });
  assert_eq!(green.content("f"), None);
}

/// An agent removes a file another modified since the base: delete/modify conflict.
#[test]
fn remove_of_a_modified_file_conflicts() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "f", create(b"hello world")));
  // Agent A modifies f at version 2.
  green.submit(&increment(
    2,
    1,
    "f",
    overwrite(b"hello world", 0, b"HELLO"),
  ));
  // Agent B (based on 1) removes f — but f was modified intervening.
  let b = green.submit(&increment(3, 1, "f", remove()));
  assert!(
    matches!(b, Outcome::Conflict { .. }),
    "removing a modified file conflicts"
  );
}

/// A change that creates a directory at the path.
fn mkdir() -> PathChange {
  PathChange::Mkdir
}

/// A change that sets the mode of the path.
fn set_mode(mode: u32) -> PathChange {
  PathChange::SetMode { mode }
}

/// A mkdir creates a directory.
#[test]
fn mkdir_creates_a_directory() {
  let mut green = Green::new();
  let outcome = green.submit(&increment(1, 0, "d", mkdir()));
  assert_eq!(outcome, Outcome::Accepted { version: 1 });
  assert!(green.is_dir("d"));
}

/// Two agents making the same directory: the second accepts as a no-op (both made it).
#[test]
fn two_mkdirs_of_the_same_path_accept() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "d", mkdir()));
  let again = green.submit(&increment(2, 0, "d", mkdir()));
  assert_eq!(again, Outcome::Accepted { version: 2 });
  assert!(green.is_dir("d"));
}

/// A mkdir where an intervening change created a file is a type conflict.
#[test]
fn mkdir_over_a_file_conflicts() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "a", create(b"file")));
  let outcome = green.submit(&increment(2, 1, "a", mkdir()));
  match outcome {
    Outcome::Conflict { windows } => assert_eq!(
      windows[0].class,
      slates_merge::verdict::MergeConflictClass::TypeChanged
    ),
    other => panic!("expected a type conflict, got {other:?}"),
  }
}

/// A create where an intervening change made the path a directory is a type conflict.
#[test]
fn create_over_a_directory_conflicts() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "d", mkdir()));
  let outcome = green.submit(&increment(2, 1, "d", create(b"file")));
  match outcome {
    Outcome::Conflict { windows } => assert_eq!(
      windows[0].class,
      slates_merge::verdict::MergeConflictClass::TypeChanged
    ),
    other => panic!("expected a type conflict, got {other:?}"),
  }
}

/// A setmode sets the mode of a file, and of a directory.
#[test]
fn setmode_sets_the_mode() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "f", create(b"x")));
  green.submit(&increment(2, 1, "f", set_mode(0o644)));
  assert_eq!(green.mode("f"), Some(0o644));
  green.submit(&increment(3, 2, "d", mkdir()));
  green.submit(&increment(4, 3, "d", set_mode(0o755)));
  assert_eq!(green.mode("d"), Some(0o755));
}

/// Two agents setting differing modes on one path from the same base: the second conflicts.
#[test]
fn two_differing_setmodes_conflict() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "f", create(b"x")));
  let a = green.submit(&increment(2, 1, "f", set_mode(0o600)));
  assert_eq!(a, Outcome::Accepted { version: 2 });
  let b = green.submit(&increment(3, 1, "f", set_mode(0o644)));
  match b {
    Outcome::Conflict { windows } => assert_eq!(
      windows[0].class,
      slates_merge::verdict::MergeConflictClass::MetaMeta
    ),
    other => panic!("expected a metadata conflict, got {other:?}"),
  }
  assert_eq!(green.mode("f"), Some(0o600), "the first mode stands");
}

/// Two agents setting the identical mode: the second accepts as a no-op.
#[test]
fn identical_setmodes_accept() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "f", create(b"x")));
  green.submit(&increment(2, 1, "f", set_mode(0o600)));
  let same = green.submit(&increment(3, 1, "f", set_mode(0o600)));
  assert_eq!(same, Outcome::Accepted { version: 3 });
}

/// A setmode on a file an intervening change deleted is a delete/modify conflict.
#[test]
fn setmode_on_a_deleted_file_conflicts() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "f", create(b"x")));
  green.submit(&increment(2, 1, "f", remove()));
  let b = green.submit(&increment(3, 1, "f", set_mode(0o600)));
  match b {
    Outcome::Conflict { windows } => assert_eq!(
      windows[0].class,
      slates_merge::verdict::MergeConflictClass::DeleteModify
    ),
    other => panic!("expected a delete/modify conflict, got {other:?}"),
  }
}

/// A content edit and a mode change are independent dimensions: an intervening mode change does not
/// conflict with a content edit unchanged since its base.
#[test]
fn a_content_edit_and_a_mode_change_are_independent() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "f", create(b"0123456789")));
  // Agent A sets the mode at version 2.
  green.submit(&increment(2, 1, "f", set_mode(0o600)));
  // Agent B (based on 1) edits the content — the mode change is a different dimension.
  let outcome = green.submit(&increment(3, 1, "f", overwrite(b"0123456789", 0, b"AB")));
  assert_eq!(
    outcome,
    Outcome::Accepted { version: 3 },
    "independent dimensions do not conflict"
  );
  assert_eq!(&green.content("f").expect("present")[0..2], b"AB");
  assert_eq!(green.mode("f"), Some(0o600), "the mode is retained");
}

/// A modify of a path an intervening change turned into a directory is a type conflict.
#[test]
fn modify_of_a_path_now_a_directory_conflicts() {
  let mut green = Green::new();
  green.submit(&increment(1, 0, "x", mkdir()));
  let outcome = green.submit(&increment(2, 1, "x", overwrite(b"", 0, b"")));
  match outcome {
    Outcome::Conflict { windows } => assert_eq!(
      windows[0].class,
      slates_merge::verdict::MergeConflictClass::TypeChanged
    ),
    other => panic!("expected a type conflict, got {other:?}"),
  }
}
