//! The head identity's oracle (A-202; §4.16 "Apply on holders"): the identity a green keeps as its entries change
//! equals the identity recounted from scratch over its state, after every step of generated histories in every
//! dimension; two greens that reach one state by different orders have one identity; and a single differing byte
//! anywhere gives another. A holder recomputes the identity by replaying the increments through the same engine, so
//! the running form is what it compares, and these are the properties its comparison rests on.

// Test code may panic (CLAUDE.md §2 item 6 applies to shipped code).
#![allow(clippy::indexing_slicing)]

mod common;

use common::Build;
use common::generated::{increment_of, origin_of, raw};
use proptest::prelude::*;
use slates_merge::engine::{Green, Outcome};
use slates_merge::origin::Origin;

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]
  /// A-202: do apply generated increments (files, directories, modes, symlinks, hard links, attributes, renames and
  /// removals; some conflict and change nothing) to a green over a generated origin, after one create that always
  /// commits, reading the identity at some steps and folding the history below the head at others; expect the
  /// running identity to equal the from-scratch one at every read.
  #[test]
  fn the_running_identity_equals_a_recount_after_every_step(
    origin in proptest::collection::vec(raw(), 0..24),
    history in proptest::collection::vec(raw(), 1..40),
  ) {
    let mut green = Green::with_origin(origin_of(&origin));
    prop_assert_eq!(green.head_identity(), green.head_identity_from_scratch(), "after the origin");
    // A create at a path outside the alphabet always commits, so every case runs the commit path at least once.
    let mut accepted = 0u32;
    if matches!(green.submit(&Build::new().create("outside/new", b"n").at(u8::MAX, green.head())), Outcome::Accepted { .. }) {
      accepted += 1;
    }
    prop_assert_eq!(green.head_identity(), green.head_identity_from_scratch(), "after the first commit");
    for (index, step) in history.iter().enumerate() {
      let id = u8::try_from(index).unwrap_or(u8::MAX);
      if matches!(green.submit(&increment_of(step).at(id, green.head())), Outcome::Accepted { .. }) {
        accepted += 1;
      }
      // The identity is read only at some steps, so the changes of several commits fold at once; and the history
      // is sometimes folded below the head in between, which must bring the identity up first.
      if step.3 % 4 == 0 {
        let floor = green.head();
        green.fold_history_before(floor);
      }
      if step.4 % 3 == 0 {
        prop_assert_eq!(green.head_identity(), green.head_identity_from_scratch(), "after step {} {:?}", index, step);
      }
    }
    prop_assert_eq!(green.head_identity(), green.head_identity_from_scratch(), "at the end");
    prop_assert!(accepted > 0, "not even the first commit was accepted");
  }
}

/// A-202 (order independence, what a holder relies on): do build one state twice, the same disjoint edits applied in
/// opposite orders; expect one identity, equal to the recount.
#[test]
fn one_state_reached_in_two_orders_has_one_identity() {
  let origin = Origin {
    files: vec![("a".to_owned(), vec![1; 8]), ("b".to_owned(), vec![2; 8])],
    dirs: vec!["d".to_owned()],
    ..Origin::default()
  };
  let steps = [
    Build::new()
      .overwrite("a", 0, b"xx")
      .setmode("b", 0o640)
      .at(1, 0),
    Build::new()
      .create("c", b"new")
      .setxattr("d", "user.k", b"v")
      .at(2, 0),
  ];
  let mut forward = Green::with_origin(origin.clone());
  let mut backward = Green::with_origin(origin.clone());
  for step in &steps {
    assert!(matches!(forward.submit(step), Outcome::Accepted { .. }));
  }
  for step in steps.iter().rev() {
    assert!(matches!(backward.submit(step), Outcome::Accepted { .. }));
  }
  assert_eq!(forward.head_identity(), backward.head_identity());
  assert_eq!(
    forward.head_identity(),
    forward.head_identity_from_scratch()
  );
}

/// A-202 (sensitivity): do change one byte of one file, one mode bit, or a file into a directory of the same name;
/// expect a different identity each time, and the same identity back when the byte is restored.
#[test]
fn one_differing_byte_anywhere_changes_the_identity() {
  let origin = Origin {
    files: vec![("f".to_owned(), vec![7; 64])],
    modes: vec![("f".to_owned(), 0o644)],
    ..Origin::default()
  };
  let mut green = Green::with_origin(origin.clone());
  let start = green.head_identity();
  assert!(matches!(
    green.submit(&Build::new().overwrite("f", 63, &[8]).at(1, 0)),
    Outcome::Accepted { .. }
  ));
  let byte = green.head_identity();
  assert_ne!(byte, start);
  assert!(matches!(
    green.submit(&Build::new().overwrite("f", 63, &[7]).at(2, 1)),
    Outcome::Accepted { .. }
  ));
  assert_eq!(
    green.head_identity(),
    start,
    "restoring the byte restores the identity"
  );
  let mut moded = Green::with_origin(Origin {
    modes: vec![("f".to_owned(), 0o645)],
    ..origin.clone()
  });
  assert_ne!(moded.head_identity(), start);
  let mut as_dir = Green::with_origin(Origin {
    files: Vec::new(),
    dirs: vec!["f".to_owned()],
    ..origin
  });
  assert_ne!(as_dir.head_identity(), start);
}
