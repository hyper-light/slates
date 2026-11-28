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

/// `inc`'s entry recording `applied` measured a unit a step ([`crate::chain_entry::measuring`],
/// [`crate::chain_entry::measure_step`], every step spent once its first unit is taken), with a head identity where
/// `identity`.
fn measured_a_unit_a_step(
  inc: &super::Increment,
  applied: &[crate::chain_entry::Applied],
  identity: bool,
) -> usize {
  let mut measuring = crate::chain_entry::measuring(identity);
  loop {
    if let Some(len) =
      crate::chain_entry::measure_step((inc, applied), &mut measuring, &mut || true)
    {
      return len;
    }
  }
}

/// `work` with `path` changed as `how` names ([`reserved_change`]'s changes), or edited: a byte of the file
/// overwritten from the work's post-state, which the entry records as a splice.
fn measured_change(work: Work, path: &str, how: u8, mark: u8) -> Work {
  if how % 16 >= 12 {
    return work.op(super::OpKind::Overwrite, path, (u64::from(mark % 3), 1, 0));
  }
  reserved_change(work, path, how, mark)
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

/// [`an_entry_measured_a_unit_a_step_is_the_entry_measured_whole`]'s one history: `steps` judged on a green, each
/// accepted verdict's entry measured whole and a unit a step, with a head identity and without, then committed. The
/// splices the verdicts recorded; where the two lengths differ, how.
fn measured_alike(steps: &[(Vec<(usize, u8)>, u8)]) -> Result<usize, TestCaseError> {
  let mut green = Green::new();
  let mut spliced = 0usize;
  for (id, (changes, mark)) in (0u64..).zip(steps) {
    // A file of its own whose bytes lead the work's post-state, which its edits overwrite from.
    let mut work = Work::default().create(&format!("post{id}"), &[*mark; 4]);
    for (index, how) in changes {
      work = measured_change(work, PATHS[*index], *how, *mark);
    }
    let inc = work.at(green.head(), id);
    let Judgment::Accepted(held) = green.judge(&inc) else {
      continue;
    };
    let splices = held
      .applied()
      .iter()
      .filter(|applied| matches!(applied, crate::chain_entry::Applied::Spliced(..)));
    spliced += splices.count();
    for identity in [false, true] {
      prop_assert_eq!(
        measured_a_unit_a_step(&inc, held.applied(), identity),
        crate::chain_entry::encoded_len(&inc, held.applied(), identity)
      );
    }
    prop_assert!(green.commit_judged(held).is_ok(), "the verdict committed");
  }
  Ok(spliced)
}

/// A-920 piece 1c (oracle). Do: judge generated works of every kind, renames and edits on a green, and measure each
/// accepted verdict's chain entry whole (`chain_entry::encoded_len`) and a unit a step, with a head identity and
/// without; then commit it. Expect the same length every time, and some verdict recording a splice, the effect whose
/// length reads what the work declares.
#[test]
fn an_entry_measured_a_unit_a_step_is_the_entry_measured_whole() {
  let steps = prop::collection::vec(
    (
      prop::collection::vec((0usize..5, any::<u8>()), 1..5),
      any::<u8>(),
    ),
    1..16,
  );
  let spliced = std::cell::Cell::new(0usize);
  let mut runner =
    proptest::test_runner::TestRunner::new(slates_test_seeds::unseeded(ProptestConfig::default()));
  let result = runner.run(&steps, |steps| {
    spliced.set(spliced.get() + measured_alike(&steps)?);
    Ok(())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  assert!(spliced.get() > 0, "no verdict recorded a splice");
}

/// The import's r593 refusal (main's git-2000 release rounds of 2026-11-27, 4 imports of 5 refused `ContentUnavailable`:
/// "an imported revision's version was not staged: NotAccepted(Conflict { .. DeleteModify .. })" on the directories
/// r593 removes with every file under them, each window repeated hundreds of times). A step that decided one directory
/// and was spent counting the next one's children left the phase's place before the first, so the next step decided
/// the first again over the second's half-made count (A-920's [`super::deciding`] walk, ee0c95e8f): a unit pushed
/// twice, and a removal the work makes whole judged `DeleteModify`. Changes of one revision are sequential and compose,
/// never conflict (D-27's verdicts are for concurrent work). Do: make `trunk/contrib/emacs` and
/// `trunk/contrib/persistent-https` with three files each and other files beside them; judge a work at the head removing
/// every file under both directories, both directories and the other files, once whole ([`Green::judge`]), and once
/// with its steps spent every `n`th check of the clock for `n` from 1 to [`R593_EVERY`]. Expect each judgment accepted.
#[test]
fn a_revision_removing_a_directorys_files_then_the_directory_is_accepted_however_it_is_judged() {
  let removal = r593_removal();
  let mut whole = r593_green();
  let judged_whole = whole.judge(&removal);
  assert!(
    matches!(judged_whole, Judgment::Accepted(_)),
    "judged whole: {judged_whole:?}"
  );
  for every in 1..=R593_EVERY {
    let mut sliced = r593_green();
    let judged = judged_spent_every(&mut sliced, &mut removal.clone(), every);
    assert!(
      matches!(judged, Judgment::Accepted(_)),
      "judged with a step spent every {every} checks: {judged:?}"
    );
  }
}

/// A green holding r593's directories, their files and the other files beside them, at version 1.
fn r593_green() -> Green {
  let mut green = Green::new();
  let mut work = Work::default()
    .op(super::OpKind::Mkdir, "trunk", (0, 0, u64::MAX))
    .op(super::OpKind::Mkdir, "trunk/contrib", (0, 0, u64::MAX));
  for dir in R593_DIRS {
    work = work.op(super::OpKind::Mkdir, dir, (0, 0, u64::MAX));
    for file in R593_FILES {
      work = work.create(&format!("{dir}/{file}"), file.as_bytes());
    }
  }
  for other in 0..R593_OTHERS {
    work = work.create(&format!("trunk/other-{other:02}"), b"other");
  }
  let (outcome, _) = green.submit_resolved(&work.at(0, 1));
  assert_eq!(outcome, Outcome::Accepted { version: 1 });
  green
}

/// r593's work at version 1: every other file, and each directory's files then the directory, removed.
fn r593_removal() -> super::Increment {
  let mut removal = Work::default();
  for other in 0..R593_OTHERS {
    removal = removal.op(
      super::OpKind::Unlink,
      &format!("trunk/other-{other:02}"),
      (0, 0, u64::MAX),
    );
  }
  for dir in R593_DIRS {
    for file in R593_FILES {
      removal = removal.op(
        super::OpKind::Unlink,
        &format!("{dir}/{file}"),
        (0, 0, u64::MAX),
      );
    }
    removal = removal.op(super::OpKind::Rmdir, dir, (0, 0, u64::MAX));
  }
  removal.at(1, 2)
}

/// `inc` judged on `green` with each step spent at every `every`th check of its clock ([`Green::judging_step`]), so a
/// step ends at every place a unit or a count can end one.
fn judged_spent_every(green: &mut Green, inc: &mut super::Increment, every: u64) -> Judgment {
  let mut judging = green.begin_judging(inc);
  let mut checks = 0u64;
  for _ in 0..R593_STEP_BOUND {
    let mut spent = || {
      checks += 1;
      checks % every == 0
    };
    if let Some(judgment) = green.judging_step(inc, &mut judging, &mut spent) {
      return judgment;
    }
  }
  panic!("a judge spent every {every} checks took more than {R593_STEP_BOUND} steps");
}

/// Shape: the steps a judge of r593's work may take before the test calls it endless: the work's 62 operations, each
/// with its few units and its directory's children, at one check a step, are well under it.
const R593_STEP_BOUND: u64 = 100_000;

/// Shape: the most checks between two spent steps the r593 test tries, past a directory's files and its neighbour's, so
/// some step decides one directory whole and is spent counting the next one's children.
const R593_EVERY: u64 = 12;

/// Shape: r593's two directories removed with their files (git/git's `contrib/emacs` and `contrib/persistent-https`).
const R593_DIRS: [&str; 2] = ["trunk/contrib/emacs", "trunk/contrib/persistent-https"];
/// Shape: the files under each (`contrib/emacs` held three: README, git-blame.el, git.el).
const R593_FILES: [&str; 3] = ["README", "git-blame.el", "git.el"];
/// Shape: the revision's other removals beside them (r593 removed 62 paths in all).
const R593_OTHERS: usize = 54;
