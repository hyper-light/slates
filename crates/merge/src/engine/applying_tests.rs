//! A recorded version applied a slice at a time ([`super::Applying`]) held to the same version applied whole
//! ([`Green::apply_recorded`], that run carried to its end): goal 93's piece 3, one version's apply resumable, so an
//! import's or a replay's apply of a revision of thousands of files never holds its shard for the whole of it.

use proptest::prelude::*;

use super::fold_tests::{PATHS, change, folded_state};
use super::replaced_node_tests::Work;
use super::{ApplyStep, Begun, Green, Judgment, Outcome};

/// Applies `applied` for `inc` to `green` a unit a step: the version it answered and the steps it took.
fn applied_a_unit_a_step(
  green: &mut Green,
  inc: &super::Increment,
  applied: &[crate::chain_entry::Applied],
) -> (u64, u64) {
  let mut inc = inc.clone();
  match green.begin_applying(&mut inc, applied) {
    Begun::Answered(version) => (version, 0),
    Begun::Applying(mut run) => {
      let mut steps = 0u64;
      loop {
        steps += 1;
        // Spent after every unit: one unit a step.
        match green.apply_step(&mut run, &mut inc, applied, &mut || true) {
          ApplyStep::More => {}
          ApplyStep::Done(version) | ApplyStep::Answered(version) => return (version, steps),
          ApplyStep::Lend(unlent) => panic!("no named text here, yet it asks to lend {unlent:?}"),
        }
      }
    }
  }
}

/// Everything a reader of `green` reads that a version's apply changes: its histories ([`folded_state`]), its head and
/// the head's identity, and its change index (each version's entries, each path's last change, their bytes).
fn read_whole(green: &mut Green) -> String {
  format!(
    "{} {:?}\n{}\n{:?}\n{:?} {}",
    green.head(),
    green.head_identity(),
    folded_state(green),
    green.changes,
    green.latest_change,
    green.changes_bytes,
  )
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// Goal 93 piece 3 (oracle). Do: submit generated works changing random paths in every dimension to a green, taking
  /// each committed version's recorded effects; replay each into two greens, one whole (`apply_recorded`), one a unit a
  /// step (`begin_applying`, `apply_step` with one unit). Expect the same version answered, and after every version the
  /// two replayed greens and the submitting one read alike: histories, recounted bytes, head and the head's identity.
  #[test]
  fn a_version_applied_a_unit_a_step_is_the_version_applied_whole(
    steps in prop::collection::vec(
      (prop::collection::vec((0usize..5, any::<u8>()), 1..5), any::<u8>()),
      1..16,
    ),
  ) {
    let (mut source, mut whole, mut sliced) = (Green::new(), Green::new(), Green::new());
    for (id, (changes, mark)) in (0u64..).zip(&steps) {
      let mut work = Work::default();
      for (index, how) in changes {
        work = change(work, PATHS[*index], *how, *mark);
      }
      let inc = work.at(source.head(), id);
      let (outcome, applied) = source.submit_resolved(&inc);
      // An identical work commits a version too, with no effects (D-27, A-782).
      let (Outcome::Accepted { version } | Outcome::Identical { version }) = outcome else {
        continue;
      };
      let applied = applied.unwrap_or_default();
      prop_assert_eq!(whole.apply_recorded(&mut inc.clone(), &applied), version);
      let (answered, _) = applied_a_unit_a_step(&mut sliced, &inc, &applied);
      prop_assert_eq!(answered, version);
      let expected = read_whole(&mut source);
      prop_assert_eq!(&read_whole(&mut whole), &expected);
      prop_assert_eq!(&read_whole(&mut sliced), &expected);
    }
  }
}

/// Goal 93 piece 3 (counted). Do: apply a recorded version creating 200 files a unit a step. Expect more steps than the
/// files (its effects made one a step, then its commit one effect, one removal or one file a step), so no step held the
/// whole version; red before: an apply was one call.
#[test]
fn a_version_of_many_files_is_applied_in_many_steps() {
  const FILES: usize = 200;
  let mut source = Green::new();
  let mut work = Work::default();
  for file in 0..FILES {
    work = work.create(&format!("f{file:03}"), b"abc");
  }
  let inc = work.at(0, 1);
  let (outcome, applied) = source.submit_resolved(&inc);
  assert_eq!(outcome, Outcome::Accepted { version: 1 });
  let applied = applied.expect("its effects");
  let mut sliced = Green::new();
  let (version, steps) = applied_a_unit_a_step(&mut sliced, &inc, &applied);
  assert_eq!(version, 1);
  assert!(
    steps > FILES as u64,
    "{steps} steps for a version of {FILES} files"
  );
}

/// Judges `inc` on `green` and commits an accepted verdict a unit a step (`Green::begin_committing`,
/// `Green::committing_step`): the version it committed, none for any other judgment.
/// `inc`'s reservation on `green` taken a unit a step ([`Green::begin_reserving`], [`Green::reserving_step`], every step
/// spent once its first unit is taken), then its copies' and replaces' shares ([`Green::reservation_shares`]).
fn reserved_a_unit_a_step(green: &Green, inc: &super::Increment) -> usize {
  let mut reserving = green.begin_reserving(inc);
  loop {
    if let Some(walked) = green.reserving_step(inc, &mut reserving, &mut || true) {
      return green.reservation_shares(inc, walked);
    }
  }
}

/// `work` with `path` changed as `how` names ([`change`]'s changes, and a rename onto `path` from the path `mark` picks,
/// whose source the two reservations charge apart).
fn reserved_change(work: Work, path: &str, how: u8, mark: u8) -> Work {
  if how % 12 >= 10 {
    let source = PATHS[usize::from(mark) % PATHS.len()];
    return work.naming(super::OpKind::Rename, path, source);
  }
  change(work, path, how, mark)
}

/// `inc` judged on `green` a unit a step ([`Green::begin_judging`], [`Green::judging_step`], every step spent once
/// its first unit is taken): the judgment, and the steps it took.
pub(super) fn judged_a_unit_a_step(
  green: &mut Green,
  inc: &mut super::Increment,
) -> (Judgment, usize) {
  let mut judging = green.begin_judging(inc);
  let mut steps = 0;
  loop {
    steps += 1;
    if let Some(judgment) = green.judging_step(inc, &mut judging, &mut || true) {
      return (judgment, steps);
    }
  }
}

/// `judgment`'s verdict committed on `green` where it accepted the work ([`Green::commit_judged`]): the version.
fn committed(green: &mut Green, judgment: Judgment) -> Option<u64> {
  let Judgment::Accepted(held) = judgment else {
    return None;
  };
  green.commit_judged(held).ok().map(|(version, _)| version)
}

/// The change index's rule (§4.16 `changed_since`, A-264), on its own: every path a version changed has exactly one
/// entry marked last, at the version `latest_change` names for it; every entry marked last is that path's
/// `latest_change`; and every path an entry names has a `latest_change`. Where it fails, how.
fn change_index_holds(green: &Green) -> Result<(), String> {
  let mut marked: std::collections::BTreeMap<&str, Vec<u64>> = std::collections::BTreeMap::new();
  for (version, entries) in &green.changes {
    for entry in entries {
      if !green.latest_change.contains_key(&entry.path) {
        return Err(format!("{} at {version} has no latest change", entry.path));
      }
      if entry.last {
        marked
          .entry(entry.path.as_str())
          .or_default()
          .push(*version);
      }
    }
  }
  for (path, latest) in &green.latest_change {
    let last = marked
      .get(path.as_str())
      .map(Vec::as_slice)
      .unwrap_or_default();
    if last != [*latest] {
      return Err(format!(
        "{path}: latest change {latest}, marked last at {last:?}"
      ));
    }
  }
  Ok(())
}

fn committed_a_unit_a_step(green: &mut Green, inc: &super::Increment) -> Option<u64> {
  let Judgment::Accepted(held) = green.judge(inc) else {
    return None;
  };
  let mut committing = green.begin_committing(held);
  loop {
    // Spent after every unit: one unit a step.
    if let super::CommitStep::Committed(version, _) =
      green.committing_step(&mut committing, &mut || true)
    {
      return Some(version);
    }
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-920 piece 1 (oracle). Do: give two greens the same generated works changing random paths in every dimension, one
  /// submitting each whole (`submit_resolved`), the other judging it and committing an accepted verdict a unit a step.
  /// Expect the same version for every accepted work, and after every work the two read alike: histories, recounted
  /// bytes, head and the head's identity.
  #[test]
  fn a_verdict_committed_a_unit_a_step_is_the_verdict_committed_whole(
    steps in prop::collection::vec(
      (prop::collection::vec((0usize..5, any::<u8>()), 1..5), any::<u8>()),
      1..16,
    ),
  ) {
    let (mut whole, mut sliced) = (Green::new(), Green::new());
    for (id, (changes, mark)) in (0u64..).zip(&steps) {
      let mut work = Work::default();
      for (index, how) in changes {
        work = change(work, PATHS[*index], *how, *mark);
      }
      let inc = work.at(whole.head(), id);
      let (outcome, _) = whole.submit_resolved(&inc);
      let committed = committed_a_unit_a_step(&mut sliced, &inc);
      if let Outcome::Accepted { version } | Outcome::Identical { version } = outcome {
        prop_assert_eq!(committed, Some(version));
      } else {
        prop_assert_eq!(committed, None);
      }
      prop_assert_eq!(read_whole(&mut sliced), read_whole(&mut whole));
    }
  }

  /// A-920 piece 1b (the change index's rule, stated on its own; main's order of 2026-11-25). Do: make every path on a
  /// green, then submit generated works of every kind and renames changing them again, each committed a unit a step.
  /// Expect after every one that each changed path has exactly one entry marked last, at the version its last change
  /// names, and no entry names a path without a last change.
  #[test]
  fn only_a_paths_newest_change_is_marked_last(
    steps in prop::collection::vec(
      (prop::collection::vec((0usize..5, any::<u8>()), 1..5), any::<u8>()),
      1..16,
    ),
  ) {
    let mut green = Green::new();
    // Every path made first, so the generated works change paths that hold entries already.
    let made = PATHS.iter().fold(Work::default(), |work, path| work.create(path, b"made"));
    prop_assert!(committed_a_unit_a_step(&mut green, &made.at(0, 0)).is_some(), "the paths made");
    for (id, (changes, mark)) in (1u64..).zip(&steps) {
      let mut work = Work::default();
      for (index, how) in changes {
        work = reserved_change(work, PATHS[*index], *how, *mark);
      }
      let inc = work.at(green.head(), id);
      let _ = committed_a_unit_a_step(&mut green, &inc);
      change_index_holds(&green).map_err(TestCaseError::fail)?;
    }
  }

  /// A-920 piece 3b (oracle). Do: on a green, take each generated work's reservation (creates, removals, modes,
  /// attributes, links, nodes, directories and renames on random paths) a unit a step and whole
  /// ([`Green::history_reservation`] plus [`Green::changes_reservation`]), before the work is submitted and after.
  /// Expect the same bytes every time, over the histories every earlier work left, nothing for a work accepted.
  #[test]
  fn a_reservation_taken_a_unit_a_step_is_the_reservation_taken_whole(
    steps in prop::collection::vec(
      (prop::collection::vec((0usize..5, any::<u8>()), 1..5), any::<u8>()),
      1..16,
    ),
  ) {
    let mut green = Green::new();
    for (id, (changes, mark)) in (0u64..).zip(&steps) {
      let mut work = Work::default();
      for (index, how) in changes {
        work = reserved_change(work, PATHS[*index], *how, *mark);
      }
      let inc = work.at(green.head(), id);
      for _ in 0..2 {
        let whole = green.history_reservation(&inc).saturating_add(green.changes_reservation(&inc));
        prop_assert_eq!(reserved_a_unit_a_step(&green, &inc), whole);
        let _ = green.submit_resolved(&inc);
      }
    }
  }

  /// A-920 piece 3c (oracle). Do: give two greens the same generated works (creates, removals, modes, attributes,
  /// links, nodes, directories and renames on random paths), each judged twice, one green judging whole
  /// (`judge_named_reads_later`), the other a unit a step, and commit what each accepts. Expect the same judgment
  /// every time, an accepted, a conflict cached or an answer the cache gives alike, more than one step for a first
  /// judge of a work of more than one op (a second is answered at once), and the two greens alike after every judge.
  #[test]
  fn a_work_judged_a_unit_a_step_is_the_work_judged_whole(
    steps in prop::collection::vec(
      (prop::collection::vec((0usize..5, any::<u8>()), 1..5), any::<u8>()),
      1..16,
    ),
  ) {
    let (mut whole, mut sliced) = (Green::new(), Green::new());
    for (id, (changes, mark)) in (0u64..).zip(&steps) {
      let mut work = Work::default();
      for (index, how) in changes {
        work = reserved_change(work, PATHS[*index], *how, *mark);
      }
      let inc = work.at(whole.head(), id);
      for round in 0..2 {
        let judged_whole = whole.judge_named_reads_later(&mut inc.clone());
        let (judged_sliced, taken) = judged_a_unit_a_step(&mut sliced, &mut inc.clone());
        prop_assert_eq!(format!("{judged_sliced:?}"), format!("{judged_whole:?}"));
        let first_of_many = round == 0 && inc.doc.ops.len() > 1;
        prop_assert!(!first_of_many || taken > 1, "{} ops judged in {} step", inc.doc.ops.len(), taken);
        prop_assert_eq!(committed(&mut sliced, judged_sliced), committed(&mut whole, judged_whole));
        prop_assert_eq!(read_whole(&mut sliced), read_whole(&mut whole));
      }
    }
  }
}
