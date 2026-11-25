//! The fold over the change index's paths (`Green::fold_history_before`) held to the fold over every path's history
//! (`Green::fold_history_before_every_path`): the premise that only a path the index names from the last floor to the
//! new one can gain entries below the new floor, proved over generated histories in every dimension (goal 93; main's
//! condition of 2026-11-24).

use proptest::prelude::*;

use super::Green;
use super::replaced_node_tests::Work;
use crate::ops_doc::OpKind;

/// Shape: the paths the generated works change: files, links and a directory among them by what each step makes.
const PATHS: [&str; 5] = ["a", "b", "c", "d", "e"];

/// `work` with `path` changed as `how` names, `mark` its bytes or mode: a file made or removed, its mode, an attribute
/// set or removed, a symbolic or a hard link made, a FIFO made, a directory made or removed. A change the green
/// refuses is refused alike on both twins.
fn change(work: Work, path: &str, how: u8, mark: u8) -> Work {
  match how % 10 {
    0 => work.create(path, &[mark; 3]),
    1 => work.op(OpKind::Unlink, path, (0, 0, u64::MAX)),
    2 => work.mode(path, 0o600 + u32::from(mark % 8)),
    3 => work.set_xattr(path, "user.k", &[mark]),
    4 => work.remove_xattr(path, "user.k"),
    5 => work.naming(OpKind::Symlink, path, "a"),
    6 => work.naming(OpKind::Link, path, "b"),
    7 => work.fifo(path),
    8 => work.op(OpKind::Mkdir, path, (0, 0, u64::MAX)),
    _ => work.op(OpKind::Rmdir, path, (0, 0, u64::MAX)),
  }
}

/// Every history the fold touches, and what the green counts of them: what the twins must agree on.
fn folded_state(green: &Green) -> String {
  format!(
    "{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{} {} {:?}",
    green.content_history,
    green.special_history,
    green.mode_history,
    green.symlink_history,
    green.hardlink_history,
    green.xattr_history,
    green.dir_history,
    green.history_bytes_recounted(),
    green.folded_below,
    green.retained_bytes(),
  )
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// Goal 93 (oracle). Do: two greens take the same generated works, each changing random paths in every dimension at
  /// the head, with the floor moved between them to a random version: one folds the change index's paths, the other
  /// every path's history. Expect the same outcome for every work and, after every fold, the same histories, the same
  /// recounted history bytes and the same retained bytes.
  #[test]
  fn the_fold_of_the_change_indexs_paths_equals_the_fold_of_every_path(
    steps in prop::collection::vec(
      (prop::collection::vec((0usize..5, any::<u8>()), 1..4), any::<u8>(), any::<bool>()),
      1..24,
    ),
  ) {
    let mut twins = [Green::new(), Green::new()];
    for ((changes, mark, fold), id) in steps.iter().zip(0u64..) {
      for green in &mut twins {
        let mut work = Work::default();
        for (index, how) in changes {
          work = change(work, PATHS[*index], *how, *mark);
        }
        let _ = green.submit(&work.at(green.head(), id));
      }
      prop_assert_eq!(twins[0].head(), twins[1].head());
      let head = twins[0].head();
      let below = twins[0].folded_below;
      if *fold && head > below {
        let floor = below + 1 + u64::from(*mark) % (head - below);
        let [indexed, every] = &mut twins;
        indexed.fold_history_before(floor);
        every.fold_history_before_every_path(floor);
        prop_assert_eq!(folded_state(indexed), folded_state(every));
      }
    }
  }
}

/// Goal 93 (counted). Do: make 50 files at version 1, change one at version 2 and fold to it, then change another at
/// version 3 and fold to it. Expect the second fold to visit only the paths changed at the last floor's version and
/// since (two), never the green's 50: the first fold visits the 50 made since the green began.
#[test]
fn a_fold_visits_the_paths_changed_since_the_last_floor_and_no_other() {
  let mut green = Green::new();
  let mut work = Work::default();
  let files: Vec<String> = (0..50).map(|file| format!("f{file:02}")).collect();
  for file in &files {
    work = work.create(file, b"abc");
  }
  assert!(matches!(
    green.submit(&work.at(0, 1)),
    super::Outcome::Accepted { version: 1 }
  ));
  let mut visits = Vec::new();
  for (version, file) in [(2u64, "f00"), (3, "f01")] {
    let edit = Work::default().mode(file, 0o600);
    assert!(matches!(
      green.submit(&edit.at(version - 1, version)),
      super::Outcome::Accepted { .. }
    ));
    let before = super::FOLD_PATHS_VISITED.with(std::cell::Cell::get);
    green.fold_history_before(version);
    visits.push(super::FOLD_PATHS_VISITED.with(std::cell::Cell::get) - before);
  }
  assert_eq!(visits, vec![50, 2], "paths each fold visited");
}
