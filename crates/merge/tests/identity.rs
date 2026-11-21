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
use slates_merge::engine::{Green, Judgment, Lineage, Outcome};
use slates_merge::origin::Origin;

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]
  /// A-202: do apply generated increments (files, directories, modes, symlinks, hard links, attributes, renames and
  /// removals; some conflict and change nothing) to a green over a generated origin, after one create that always
  /// commits, reading the identity at some steps and folding the history below the head at others; expect the
  /// running identity to equal the from-scratch one at every read. A-579: and bringing a few keys up alone at others
  /// (`fold_identity_step`), the rest left for the read.
  #[test]
  fn the_running_identity_equals_a_recount_after_every_step(
    origin in proptest::collection::vec(raw(), 0..24),
    history in proptest::collection::vec(raw(), 1..40),
  ) {
    let mut green = Green::with_origin(origin_of(&origin));
    prop_assert_eq!(green.head_identity(), green.head_identity_from_scratch(), "after the origin");
    // A create at a path outside the alphabet always commits, so every case runs the commit path at least once.
    let mut accepted = 0u32;
    if matches!(green.submit(&Build::new().create("outside/new", b"n").at(u8::MAX, green.head())), Outcome::Accepted { .. } | Outcome::Identical { .. }) {
      accepted += 1;
    }
    prop_assert_eq!(green.head_identity(), green.head_identity_from_scratch(), "after the first commit");
    for (index, step) in history.iter().enumerate() {
      let id = u8::try_from(index).unwrap_or(u8::MAX);
      let increment = increment_of(step).at(id, green.head());
      let head = green.head();
      let (taken, viewed) = view_of(green, &increment);
      green = taken;
      if matches!(green.submit(&increment), Outcome::Accepted { .. } | Outcome::Identical { .. }) {
        accepted += 1;
      }
      // A-805: a held verdict's view reads as the green does once the verdict commits.
      if let Some((changed, reads)) = viewed
        && green.head() == head + 1
      {
        prop_assert_eq!(&changed, &green.changed_between(head, head + 1));
        let paths: std::collections::BTreeSet<String> = reads.keys().cloned().chain(paths_of(&green)).collect();
        for path in &paths {
          prop_assert_eq!(reads.get(path).cloned().unwrap_or_else(|| reads_at(&green, path, head)), reads_at(&green, path, head), "path {}", path);
          prop_assert!(reads.contains_key(path), "the view lacks {}", path);
        }
      }
      // The identity is read only at some steps, so the changes of several commits fold at once; and the history
      // is sometimes folded below the head in between, which must bring the identity up first.
      if step.3 % 4 == 0 {
        let floor = green.head();
        green.fold_history_before(floor);
      }
      // A-579: part of the backlog brought up alone at some steps (one to three keys), so keys folded at different
      // versions are changed again, folded again and read beside keys never folded alone.
      if step.3 % 3 == 1 {
        green.fold_identity_step(1 + usize::from(step.4 % 3));
      }
      if step.4 % 3 == 0 {
        prop_assert_eq!(green.head_identity(), green.head_identity_from_scratch(), "after step {} {:?}", index, step);
      }
    }
    prop_assert_eq!(green.head_identity(), green.head_identity_from_scratch(), "at the end");
    prop_assert!(accepted > 0, "not even the first commit was accepted");
  }
}

/// One path's reads as a Subversion revision is made of them (A-805): bytes, property list, kind, link target, IPC node,
/// hard link's primary, and whether a copy after `since` brought it.
type Reads = (
  Option<Vec<u8>>,
  Option<Vec<u8>>,
  bool,
  Option<String>,
  bool,
  Option<String>,
  bool,
);

/// The property list's attribute name, as a Subversion repository's green keeps it.
const PROPS: &str = "slates.svn.props";

fn reads_at(green: &Green, path: &str, since: u64) -> Reads {
  (
    green.content(path).map(<[u8]>::to_vec),
    green.xattr(path, PROPS).map(<[u8]>::to_vec),
    green.is_dir(path),
    green.symlink(path).map(str::to_owned),
    green.special(path).is_some(),
    green.hardlink(path).map(str::to_owned),
    green.copied_after(path, since),
  )
}

/// Every file the green holds and every directory above one.
fn paths_of(green: &Green) -> std::collections::BTreeSet<String> {
  let mut paths = std::collections::BTreeSet::new();
  for (path, _) in green.files() {
    let mut at = path.as_str();
    while let Some(cut) = at.rfind('/') {
      at = &at[..cut];
      paths.insert(at.to_owned());
    }
    paths.insert(path);
  }
  paths
}

/// A-805: `green` lent to a lineage, `increment` judged there and, when held accepted, viewed as it would commit
/// (`Lineage::preview`): the paths it changes and every path's reads, the fork's numbering (the head its 0). The green
/// is given back, judged but unchanged.
fn view_of(
  green: Green,
  increment: &slates_merge::engine::Increment,
) -> (
  Green,
  Option<(Vec<String>, std::collections::BTreeMap<String, Reads>)>,
) {
  let (mut lineage, handle) = Lineage::new(green);
  let judgment = lineage
    .with(handle, |green| green.judge(increment))
    .unwrap();
  let viewed = match judgment {
    Judgment::Accepted(judged) => {
      let fork = lineage.preview(handle, judged).unwrap();
      let viewed = lineage
        .with(fork, |view| {
          let changed = view.changed_between(0, 1);
          let reads = paths_of(view)
            .into_iter()
            .chain(changed.iter().cloned())
            .map(|path| {
              let read = reads_at(view, &path, 0);
              (path, read)
            })
            .collect();
          (changed, reads)
        })
        .unwrap();
      let mut release = lineage.retire(fork).unwrap();
      while !lineage.release_step(&mut release).unwrap() {}
      Some(viewed)
    }
    _ => None,
  };
  (lineage.take(handle).unwrap(), viewed)
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
    assert!(matches!(
      forward.submit(step),
      Outcome::Accepted { .. } | Outcome::Identical { .. }
    ));
  }
  for step in steps.iter().rev() {
    assert!(matches!(
      backward.submit(step),
      Outcome::Accepted { .. } | Outcome::Identical { .. }
    ));
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
    Outcome::Accepted { .. } | Outcome::Identical { .. }
  ));
  let byte = green.head_identity();
  assert_ne!(byte, start);
  assert!(matches!(
    green.submit(&Build::new().overwrite("f", 63, &[7]).at(2, 1)),
    Outcome::Accepted { .. } | Outcome::Identical { .. }
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

/// A-805's preview, found by its oracle (2026-11-21), the oracle's shrunk case. Do: from a one-entry origin, the oracle's
/// first create, then create `dd`, link `d` to it, rename `dd` to `b`, set `b`'s mode; preview each version before it
/// commits. Expect: each preview names the paths its commit names; the mode change names `b` and `d` (the rename gave
/// `b` a special history, so its links change with it, `Green::record_changes`), as the change index's model does.
/// Before the fix the preview, a fork whose histories start empty, could not tell and named `b` alone.
#[test]
fn a_preview_records_a_renamed_homes_links_as_its_commit_does() {
  let origin: Vec<common::generated::Raw> = vec![(48, 120, 0, 0, 0)];
  let history: Vec<common::generated::Raw> = vec![
    (30, 131, 0, 0, 0),
    (94, 23, 180, 0, 0),
    (182, 40, 43, 0, 0),
    (8, 1, 0, 0, 0),
  ];
  let mut green = Green::with_origin(origin_of(&origin));
  green.submit(
    &common::Build::new()
      .create("outside/new", b"n")
      .at(u8::MAX, green.head()),
  );
  for (index, step) in history.iter().enumerate() {
    let increment = increment_of(step).at(u8::try_from(index).unwrap(), green.head());
    let head = green.head();
    let (taken, viewed) = view_of(green, &increment);
    green = taken;
    assert!(matches!(green.submit(&increment), Outcome::Accepted { .. }));
    let (changed, _) = viewed.expect("held accepted");
    assert_eq!(
      changed,
      green.changed_between(head, head + 1),
      "step {index}"
    );
  }
  let head = green.head();
  assert_eq!(green.changed_between(head - 1, head), ["b", "d"]);
}
