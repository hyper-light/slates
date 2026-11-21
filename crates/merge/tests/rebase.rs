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
use common::generated::{Raw, increment_of, origin_of, raw};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use slates_merge::engine::{Green, Outcome, Rebased};
use slates_merge::origin::Origin;

/// A green's files, owned.
type Files = BTreeMap<String, Vec<u8>>;

/// The green's files at its head, owned.
fn head_files(green: &Green) -> Files {
  green
    .files()
    .map(|(path, bytes)| (path.to_owned(), bytes.to_vec()))
    .collect()
}

/// Two greens built alike: the same origin, the same increments, in order.
fn twins(origin: &[Raw], history: &[Raw]) -> (Green, Green) {
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
  #![proptest_config(slates_test_seeds::seeded(proptest::test_runner::Config::default(), include_str!("rebase.proptest-regressions")).unwrap())]
  /// A-202: do build twin greens by generated increments, then rebase a generated work (one operation of any
  /// dimension) from a generated older base on one and submit it on the other; expect the same verdict, the rebasing
  /// green unchanged, and, when clean, the accepting twin's head files equal to the rebasing green's head files with the
  /// rebase's changes over them, where every change differs from the head (so the work holds nothing the head already
  /// does). The seed file holds the cases A-677 was found by.
  #[test]
  fn a_rebases_changes_are_the_whole_restatements_difference(
    origin in proptest::collection::vec(raw(), 0..24),
    history in proptest::collection::vec(raw(), 0..12),
    work in raw(),
    base_pick in any::<u8>(),
  ) {
    the_restatement_is_the_submits(&origin, &history, &work, base_pick)?;
  }
}

/// A-202's oracle over one case: twins built by `origin` and `history`, and the work `work` based at
/// `base_pick` modulo one past the head, rebased on one twin and submitted on the other; the first
/// disagreement, as a failed case.
fn the_restatement_is_the_submits(
  origin: &[Raw],
  history: &[Raw],
  work: &Raw,
  base_pick: u8,
) -> Result<(), TestCaseError> {
  let (rebasing, mut accepting) = twins(origin, history);
  let head = rebasing.head();
  let base = u64::from(base_pick) % (head + 1);
  let increment = increment_of(work).with_id([0xA5; 32], base);
  let before = head_files(&rebasing);
  let rebased = rebasing.rebase(&increment);
  prop_assert_eq!(rebasing.head(), head, "a rebase commits nothing");
  prop_assert_eq!(
    head_files(&rebasing),
    before.clone(),
    "a rebase changes no file of the green"
  );
  match (rebased, accepting.submit(&increment)) {
    (
      Rebased::Rebased {
        version, changed, ..
      },
      Outcome::Accepted { .. } | Outcome::Identical { .. },
    ) => {
      prop_assert_eq!(version, head);
      restated_is(&before, &changed, &head_files(&accepting))
    }
    (Rebased::Conflict { .. }, Outcome::Conflict { .. }) => Ok(()),
    (rebased, accepted) => Err(TestCaseError::fail(format!(
      "the rebase answered {rebased:?} and the submit {accepted:?}"
    ))),
  }
}

/// A clean rebase's `changed` over the head's files `before` must be the accepting twin's files
/// `accepted`, and every change must differ from `before`.
fn restated_is(
  before: &Files,
  changed: &BTreeMap<String, Option<Vec<u8>>>,
  accepted: &Files,
) -> Result<(), TestCaseError> {
  let mut restated = before.clone();
  for (path, after) in changed {
    prop_assert_ne!(
      after.as_ref(),
      before.get(path),
      "{} is no change but was kept",
      path
    );
    match after {
      Some(bytes) => restated.insert(path.clone(), bytes.clone()),
      None => restated.remove(path),
    };
  }
  prop_assert_eq!(&restated, accepted);
  Ok(())
}

/// One case of the oracle: origin rows, history, work and base pick.
type Case = (&'static [Raw], &'static [Raw], Raw, u8);

/// A-677: do run the oracle's check on the cases its generated runs failed on (kiln r55 and r113,
/// macOS arm64), a work removing the home of a file a hard link names: the link made at version 1
/// and the work at the head, and both in the origin with the work at version 0. Expect the rebase's
/// files to be the submit's, the link holding the node. Before, the rebase restated the removal
/// alone, and its files lost the link the submit kept (`{}` against `{"b": []}`).
#[test]
fn a_removed_homes_link_holds_its_node_in_the_rebases_files() -> Result<(), TestCaseError> {
  // The two cases the oracle shrank to, as it printed them (r55, then r113).
  let found: [Case; 2] = [
    (
      &[(91, 161, 0, 0, 0), (3, 221, 0, 0, 0)],
      &[(224, 15, 0, 0, 0)],
      (65, 133, 0, 0, 0),
      29,
    ),
    (
      &[(39, 1, 21, 0, 0), (189, 77, 0, 0, 0)],
      &[],
      (95, 7, 0, 0, 0),
      0,
    ),
  ];
  for (origin, history, work, base_pick) in found {
    the_restatement_is_the_submits(origin, history, &work, base_pick)?;
  }
  Ok(())
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
