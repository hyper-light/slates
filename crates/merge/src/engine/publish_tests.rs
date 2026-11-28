//! A commit's head moves at its publish, last, and the mode history is appended in place and taken back at a refusal
//! (A-752 part 3c U1; main 01a20182's rulings of 2026-11-28 01:35 and 01:42): a verdict staged one unit a step leaves
//! the head where it was, and every mode read up to it, until it is published; given up at any turn, it leaves the
//! mode history, the head and the accepted increments as they were. The other maps are staged by later units.

use proptest::prelude::*;

use super::fold_tests::{PATHS, change};
use super::replaced_node_tests::Work;
use super::{Green, Increment, Judgment, OpKind, StagingStep};

/// A generated work: each path's change, a mode set, a file made again or removed, marked.
type Changes = (Vec<(usize, u8)>, u8);

/// `changes` as an increment based at `green`'s head, numbered `id`: a mode set, a file made, or one removed (its mode
/// let go with it, `Green::drop_metadata`).
fn work_of(green: &Green, (changes, mark): &Changes, id: u64) -> Increment {
  let mut work = Work::default();
  for (index, how) in changes {
    let path = PATHS[*index];
    work = match how % 3 {
      0 => work.mode(path, 0o600 + u32::from(mark % 8)),
      1 => work.create(path, &[*mark; 2]),
      _ => work.op(OpKind::Unlink, path, (0, 0, u64::MAX)),
    };
  }
  work.at(green.head(), id)
}

/// Every path made first, so the generated works set modes on files that stand.
fn made() -> Work {
  PATHS
    .iter()
    .fold(Work::default(), |work, path| work.create(path, b"made"))
}

/// The modes every reader reads at each version up to the head, and the head.
fn modes_read(green: &Green) -> String {
  let head = green.head();
  let mut read = format!("head {head}\n");
  for version in 0..=head {
    for path in PATHS {
      read.push_str(&format!(
        "{version} {path} {:?}\n",
        green.mode_at(path, version)
      ));
    }
  }
  read
}

/// The mode history as it is held, the head and the increments accepted: what a refusal leaves as it was.
fn held_as_it_was(green: &Green) -> String {
  format!(
    "{} {:?}\n{:?}",
    green.head(),
    green.mode_history,
    green.accepted
  )
}

/// A green given `made` and then `works` whole.
fn submitted(works: &[Changes]) -> Green {
  let mut green = Green::new();
  let _ = green.submit_resolved(&made().at(0, 0));
  for (id, changes) in (1u64..).zip(works) {
    let inc = work_of(&green, changes, id);
    let _ = green.submit_resolved(&inc);
  }
  green
}

/// Stages `inc`'s accepted verdict on `green` one unit a step, the modes read and the head after each step held to
/// before, then publishes it: its version, none for any other judgment.
fn staged_unseen(green: &mut Green, inc: &Increment) -> Result<Option<u64>, String> {
  let Judgment::Accepted(held) = green.judge(inc) else {
    return Ok(None);
  };
  let before = modes_read(green);
  let mut staging = green.begin_staging(held);
  loop {
    let stepped = green.staging_step(&mut staging, &mut || true);
    if modes_read(green) != before {
      return Err(format!(
        "a reader saw the stage at unit {}",
        staging.units()
      ));
    }
    match stepped {
      StagingStep::More => {}
      StagingStep::Staged => break,
      StagingStep::Lend(unlent) => return Err(format!("no named text here, yet {unlent:?}")),
    }
  }
  Ok(green.publish(staging).map(|(version, _)| version))
}

/// Gives up the stage of `works`' last work after `turns` steps, on a green given the others whole: whether the stage
/// finished within its turns (and so was not given up), refused where the refusal left the mode history, the head or
/// the accepted increments changed.
fn given_up_after(works: &[Changes], turns: u64) -> Result<bool, String> {
  let Some((last, before)) = works.split_last() else {
    return Ok(true);
  };
  let mut green = submitted(before);
  let inc = work_of(&green, last, u64::MAX);
  let Judgment::Accepted(held) = green.judge(&inc) else {
    return Ok(true);
  };
  let was = held_as_it_was(&green);
  let mut staging = green.begin_staging(held);
  for _ in 0..turns {
    if let StagingStep::Staged = green.staging_step(&mut staging, &mut || true) {
      return Ok(true);
    }
  }
  green.abandon_staging(staging);
  if held_as_it_was(&green) != was {
    return Err(format!(
      "a stage given up after {turns} turns left the mode history changed"
    ));
  }
  Ok(false)
}

/// `changes` as an increment based at `green`'s head, numbered `id`, changing its paths in every way but their bytes'
/// edits ([`change`]: a file made or removed, a mode, an attribute set or removed, a symbolic or hard link, a FIFO, a
/// directory made or removed).
fn work_in_every_dimension(green: &Green, (changes, mark): &Changes, id: u64) -> Increment {
  let mut work = Work::default();
  for (index, how) in changes {
    work = change(work, PATHS[*index], *how, *mark);
  }
  work.at(green.head(), id)
}

/// Every value every reader reads at each version up to the head but a file's bytes (the content is staged by a later
/// unit): each path's IPC node, directory, mode, symbolic and hard link and attribute, and the head.
fn values_read(green: &Green) -> String {
  let head = green.head();
  let mut read = format!("head {head}\n");
  for version in 0..=head {
    for path in PATHS {
      read.push_str(&format!(
        "{version} {path} {:?} {} {:?} {:?} {:?} {:?}\n",
        green.special_at(path, version),
        green.dir_at(path, version),
        green.mode_at(path, version),
        green.symlink_at(path, version),
        green.hardlink_at(path, version),
        green.xattr_at(path, "user.k", version),
      ));
    }
  }
  read
}

/// Every value history as it is held, the head and the increments accepted: what a refusal leaves as it was.
fn histories_as_they_were(green: &Green) -> String {
  format!(
    "{} {:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}",
    green.head(),
    green.mode_history,
    green.special_history,
    green.dir_history,
    green.symlink_history,
    green.hardlink_history,
    green.xattr_history,
    green.accepted
  )
}

/// A green given every path and then `works` whole, each changing its paths in every dimension.
fn submitted_in_every_dimension(works: &[Changes]) -> Green {
  let mut green = Green::new();
  let _ = green.submit_resolved(&made().at(0, 0));
  for (id, changes) in (1u64..).zip(works) {
    let inc = work_in_every_dimension(&green, changes, id);
    let _ = green.submit_resolved(&inc);
  }
  green
}

/// Stages `inc`'s accepted verdict on `green` one unit a step, every value read and the head after each step held to
/// before ([`values_read`]), then publishes it: its version, none for any other judgment.
fn staged_unseen_in_every_dimension(
  green: &mut Green,
  inc: &Increment,
) -> Result<Option<u64>, String> {
  let Judgment::Accepted(held) = green.judge(inc) else {
    return Ok(None);
  };
  let before = values_read(green);
  let mut staging = green.begin_staging(held);
  loop {
    let stepped = green.staging_step(&mut staging, &mut || true);
    if values_read(green) != before {
      return Err(format!(
        "a reader saw the stage at unit {}",
        staging.units()
      ));
    }
    match stepped {
      StagingStep::More => {}
      StagingStep::Staged => break,
      StagingStep::Lend(unlent) => return Err(format!("no named text here, yet {unlent:?}")),
    }
  }
  Ok(green.publish(staging).map(|(version, _)| version))
}

/// [`given_up_after`] over works in every dimension, every value history held to as it was.
fn given_up_in_every_dimension_after(works: &[Changes], turns: u64) -> Result<bool, String> {
  let Some((last, before)) = works.split_last() else {
    return Ok(true);
  };
  let mut green = submitted_in_every_dimension(before);
  let inc = work_in_every_dimension(&green, last, u64::MAX);
  let Judgment::Accepted(held) = green.judge(&inc) else {
    return Ok(true);
  };
  let was = histories_as_they_were(&green);
  let mut staging = green.begin_staging(held);
  for _ in 0..turns {
    if let StagingStep::Staged = green.staging_step(&mut staging, &mut || true) {
      return Ok(true);
    }
  }
  green.abandon_staging(staging);
  if histories_as_they_were(&green) != was {
    return Err(format!(
      "a stage given up after {turns} turns left a value history changed"
    ));
  }
  Ok(false)
}

/// The change index as every reader reads it at the head: its walk, each entry's version, place, path and whether it is
/// its path's last, and the paths changed between each version and the head.
fn changes_read(green: &Green) -> String {
  let head = green.head();
  let walk: Vec<String> = green
    .changes_after(0, None)
    .map(|change| format!("{change:?}"))
    .collect();
  let mut read = format!("head {head} walk {walk:?}\n");
  for version in 0..=head {
    read.push_str(&format!(
      "{version}: {:?}\n",
      green.changed_between(version, head)
    ));
  }
  read
}

/// [`staged_unseen_in_every_dimension`]'s steps with the change index read between them ([`changes_read`]).
fn changes_unseen(green: &mut Green, inc: &Increment) -> Result<Option<u64>, String> {
  let Judgment::Accepted(held) = green.judge(inc) else {
    return Ok(None);
  };
  let before = changes_read(green);
  let mut staging = green.begin_staging(held);
  loop {
    let stepped = green.staging_step(&mut staging, &mut || true);
    if changes_read(green) != before {
      return Err(format!(
        "a reader saw the stage's changes at unit {}",
        staging.units()
      ));
    }
    match stepped {
      StagingStep::More => {}
      StagingStep::Staged => break,
      StagingStep::Lend(unlent) => return Err(format!("no named text here, yet {unlent:?}")),
    }
  }
  Ok(green.publish(staging).map(|(version, _)| version))
}

/// [`given_up_in_every_dimension_after`] with the change index as it is held ([`super::Green::changes`]) the guard,
/// given up after the stage's last step too.
fn changes_given_up_after(works: &[Changes], turns: u64) -> Result<bool, String> {
  let Some((last, before)) = works.split_last() else {
    return Ok(true);
  };
  let mut green = submitted_in_every_dimension(before);
  let inc = work_in_every_dimension(&green, last, u64::MAX);
  let Judgment::Accepted(held) = green.judge(&inc) else {
    return Ok(true);
  };
  let was = format!("{:?}", green.changes);
  let mut staging = green.begin_staging(held);
  let mut staged = false;
  for _ in 0..turns {
    if let StagingStep::Staged = green.staging_step(&mut staging, &mut || true) {
      staged = true;
      break;
    }
  }
  green.abandon_staging(staging);
  if format!("{:?}", green.changes) != was {
    return Err(format!(
      "a stage given up after {turns} turns left the change index changed"
    ));
  }
  Ok(staged)
}

/// The works [`proptest`] generates: one to eight, each of one to four changes.
fn works() -> impl Strategy<Value = Vec<Changes>> {
  prop::collection::vec(
    (
      prop::collection::vec((0usize..5, any::<u8>()), 1..5),
      any::<u8>(),
    ),
    1..9,
  )
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-752 part 3c U1 (the reader at v). Do: give two greens every path, then the same generated mode, make and remove
  /// works, one submitting each whole, the other staging each accepted verdict one unit a step and publishing it.
  /// Expect: between every two steps the head and every mode read up to it as before the stage; the same version for
  /// every accepted work, and after every one the two greens' modes and heads alike. Red before: the head moved when
  /// the commit's delta was pushed, before its links and change index were recorded and before it was published.
  #[test]
  fn a_staged_verdicts_head_and_modes_move_only_at_its_publish(steps in works()) {
    let (mut whole, mut staged) = (submitted(&[]), submitted(&[]));
    for (id, changes) in (1u64..).zip(&steps) {
      let inc = work_of(&whole, changes, id);
      let (outcome, _) = whole.submit_resolved(&inc);
      let version = staged_unseen(&mut staged, &inc).map_err(TestCaseError::fail)?;
      let accepted = match outcome {
        super::Outcome::Accepted { version } | super::Outcome::Identical { version } => Some(version),
        _ => None,
      };
      prop_assert_eq!(version, accepted);
      prop_assert_eq!(modes_read(&staged), modes_read(&whole));
    }
  }

  /// A-752 part 3c U1 (a refusal at every turn). Do: for each prefix of the generated works, on a green given all but
  /// its last whole, stage the last's accepted verdict and give it up after no step, one, two and so on until a stage
  /// finishes within its turns. Expect after every refusal the mode history held exactly as before, the head and the
  /// accepted increments unchanged. Red before: a refusal left the mode history's entries the stage appended.
  #[test]
  fn a_stage_given_up_at_any_turn_takes_its_mode_entries_back(steps in works()) {
    for end in 1..=steps.len() {
      let prefix = steps.get(..end).unwrap_or_default();
      for turns in 0u64.. {
        if given_up_after(prefix, turns).map_err(TestCaseError::fail)? {
          break;
        }
      }
    }
  }

  /// A-752 part 3c U2 (the reader at v). Do: as U1's, with works changing paths in every dimension but their bytes.
  /// Expect between every two steps the head and every value read up to it, IPC nodes, directories, modes, symbolic and
  /// hard links and attributes, as before the stage; and the same version for every accepted work as the twin's.
  #[test]
  fn a_staged_verdicts_values_move_only_at_its_publish(steps in works()) {
    let (mut whole, mut staged) = (submitted_in_every_dimension(&[]), submitted_in_every_dimension(&[]));
    for (id, changes) in (1u64..).zip(&steps) {
      let inc = work_in_every_dimension(&whole, changes, id);
      let (outcome, _) = whole.submit_resolved(&inc);
      let version = staged_unseen_in_every_dimension(&mut staged, &inc).map_err(TestCaseError::fail)?;
      let accepted = match outcome {
        super::Outcome::Accepted { version } | super::Outcome::Identical { version } => Some(version),
        _ => None,
      };
      prop_assert_eq!(version, accepted);
      prop_assert_eq!(values_read(&staged), values_read(&whole));
    }
  }

  /// A-752 part 3c U2 (a refusal at every turn). Do: as U1's, with works changing paths in every dimension. Expect
  /// after every refusal every value history held exactly as before, the head and the accepted increments unchanged.
  /// Red before: a refusal took the mode history's entries back alone, the others' kept.
  #[test]
  fn a_stage_given_up_at_any_turn_takes_every_value_historys_entries_back(steps in works()) {
    for end in 1..=steps.len() {
      let prefix = steps.get(..end).unwrap_or_default();
      for turns in 0u64.. {
        if given_up_in_every_dimension_after(prefix, turns).map_err(TestCaseError::fail)? {
          break;
        }
      }
    }
  }

  /// A-752 part 3c U3 (the change index, the reader at v). Do: as U2's, the change index read between every two steps.
  /// Expect its walk (each entry's last flag too) and the paths changed between each version and the head as before the
  /// stage, and the same version for every accepted work as the twin's, whose change index reads alike after each.
  /// Red before: the commit marked the path's earlier entry no longer its last as it recorded the path, mid-commit.
  #[test]
  fn a_staged_verdicts_change_index_moves_only_at_its_publish(steps in works()) {
    let (mut whole, mut staged) = (submitted_in_every_dimension(&[]), submitted_in_every_dimension(&[]));
    for (id, changes) in (1u64..).zip(&steps) {
      let inc = work_in_every_dimension(&whole, changes, id);
      let (outcome, _) = whole.submit_resolved(&inc);
      let version = changes_unseen(&mut staged, &inc).map_err(TestCaseError::fail)?;
      let accepted = match outcome {
        super::Outcome::Accepted { version } | super::Outcome::Identical { version } => Some(version),
        _ => None,
      };
      prop_assert_eq!(version, accepted);
      prop_assert_eq!(changes_read(&staged), changes_read(&whole));
    }
  }

  /// A-752 part 3c U3 (the change index, a refusal at every turn, its last too). Do: as U2's, the stage given up after
  /// every turn up to and past its last. Expect the change index held exactly as before. Red before: a stage given up
  /// once its run had ended kept its version's entries, and the earlier entries it had marked no longer last.
  #[test]
  fn a_stage_given_up_at_any_turn_leaves_the_change_index_as_it_was(steps in works()) {
    for end in 1..=steps.len() {
      let prefix = steps.get(..end).unwrap_or_default();
      for turns in 0u64.. {
        if changes_given_up_after(prefix, turns).map_err(TestCaseError::fail)? {
          break;
        }
      }
    }
  }
}
