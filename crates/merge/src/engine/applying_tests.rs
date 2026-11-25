//! A recorded version applied a slice at a time ([`super::Applying`]) held to the same version applied whole
//! ([`Green::apply_recorded`], that run carried to its end): goal 93's piece 3, one version's apply resumable, so an
//! import's or a replay's apply of a revision of thousands of files never holds its shard for the whole of it.

use proptest::prelude::*;

use super::fold_tests::{PATHS, change, folded_state};
use super::replaced_node_tests::Work;
use super::{ApplyStep, Begun, Green, Outcome};

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
        }
      }
    }
  }
}

/// Everything a reader of `green` reads that a version's apply changes: its histories ([`folded_state`]), its head and
/// the head's identity.
fn read_whole(green: &mut Green) -> String {
  format!(
    "{} {:?}\n{}",
    green.head(),
    green.head_identity(),
    folded_state(green)
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
