//! The rebase's oracle (§4.16 "Rebase, the only corrective path"; A-202): a rebase returns only what the work changed
//! over the head (`Rebased::changed`), and that must be the whole restatement's difference. The restatement is held to
//! an independent reference: a twin green built by the same increments accepts the same work, and its head's files
//! must equal the rebasing green's head files with the rebase's changes over them. The two must also agree on the
//! verdict: a clean rebase is an accept, a conflicting one a conflict, and the rebasing green is never changed.

// Test code may panic (CLAUDE.md §2 item 6 applies to shipped code).
#![allow(clippy::indexing_slicing)]

mod common;

use std::collections::BTreeMap;

use common::Build;
use common::generated::{increment_of, origin_of, raw};
use proptest::prelude::*;
use slates_merge::engine::{Green, Outcome, Rebased};
use slates_merge::origin::Origin;

/// The green's files at its head, owned.
fn head_files(green: &Green) -> BTreeMap<String, Vec<u8>> {
  green
    .files()
    .map(|(path, bytes)| (path.to_owned(), bytes.to_vec()))
    .collect()
}

/// Two greens built alike: the same origin, the same increments, in order.
fn twins(origin: &[common::generated::Raw], history: &[common::generated::Raw]) -> (Green, Green) {
  let mut built = [
    Green::with_origin(origin_of(origin)),
    Green::with_origin(origin_of(origin)),
  ];
  for green in &mut built {
    for (index, step) in history.iter().enumerate() {
      let id = u8::try_from(index).unwrap_or(u8::MAX);
      let _ = green.submit(&increment_of(step).at(id, green.head()));
    }
  }
  let [rebasing, accepting] = built;
  (rebasing, accepting)
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]
  /// A-202: do build twin greens by generated increments, then rebase a generated work (one operation of any dimension) from a generated older base on one
  /// and submit it on the other; expect the same verdict, the rebasing green unchanged, and, when clean, the accepting
  /// twin's head files equal to the rebasing green's head files with the rebase's changes over them, where every
  /// change differs from the head (so the work holds nothing the head already does).
  #[test]
  fn a_rebases_changes_are_the_whole_restatements_difference(
    origin in proptest::collection::vec(raw(), 0..24),
    history in proptest::collection::vec(raw(), 0..12),
    work in raw(),
    base_pick in any::<u8>(),
  ) {
    let (mut rebasing, mut accepting) = twins(&origin, &history);
    let head = rebasing.head();
    let base = u64::from(base_pick) % (head + 1);
    let increment = increment_of(&work).with_id([0xA5; 32], base);
    let before = head_files(&rebasing);
    let rebased = rebasing.rebase(&increment);
    prop_assert_eq!(rebasing.head(), head, "a rebase commits nothing");
    prop_assert_eq!(head_files(&rebasing), before.clone(), "a rebase changes no file of the green");
    match (rebased, accepting.submit(&increment)) {
      (Rebased::Rebased { version, changed, .. }, Outcome::Accepted { .. }) => {
        prop_assert_eq!(version, head);
        let mut restated = before.clone();
        for (path, after) in &changed {
          prop_assert_ne!(after.as_ref(), before.get(path), "{} is no change but was kept", path);
          match after {
            Some(bytes) => restated.insert(path.clone(), bytes.clone()),
            None => restated.remove(path),
          };
        }
        prop_assert_eq!(restated, head_files(&accepting));
      }
      (Rebased::Conflict { .. }, Outcome::Conflict { .. }) => {}
      (rebased, accepted) => {
        prop_assert!(false, "the rebase answered {:?} and the submit {:?}", rebased, accepted);
      }
    }
  }
}

/// A-202 non-vacuity for the oracle: a work that changes one file of a thousand rebases to that one file alone.
#[test]
fn a_rebase_of_one_files_edit_holds_one_file_of_a_thousand() {
  let origin = Origin {
    files: (0..1000)
      .map(|n| (format!("f{n:04}"), vec![1; 16]))
      .collect(),
    ..Origin::default()
  };
  let mut green = Green::with_origin(origin.clone());
  assert!(matches!(
    green.submit(&Build::new().overwrite("f0001", 0, b"head").at(1, 0)),
    Outcome::Accepted { version: 1 }
  ));
  let Rebased::Rebased {
    version, changed, ..
  } = green.rebase(&Build::new().overwrite("f0002", 4, b"work").at(2, 0))
  else {
    panic!("a disjoint edit rebases");
  };
  assert_eq!(version, 1);
  assert_eq!(changed.keys().collect::<Vec<_>>(), vec!["f0002"]);
}
