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

/// A change that creates a file with `bytes` (from an empty base).
fn create(bytes: &[u8]) -> PathChange {
  PathChange {
    ops: vec![op(OpKind::Insert, 0, bytes.len() as u64, 0)],
    post_state: bytes.to_vec(),
  }
}

/// A change that overwrites `base[at .. at + new.len())` in place with `new`.
fn overwrite(base: &[u8], at: usize, new: &[u8]) -> PathChange {
  let mut post_state = base.to_vec();
  post_state[at..at + new.len()].copy_from_slice(new);
  PathChange {
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
  PathChange {
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
