//! The node-exclusive rule's tests (A-542, §4.17.4 item 3 "The verdict", rules 1 and 2; D-27, D-28): a work whose
//! operations are node-exclusive, as a Subversion client's commit is, conflicts on every path that changed after its
//! base in any dimension, however disjoint the bytes and however identical the change, as FSFS's commit-time merge
//! judges a node (`libsvn_fs_fs/tree.c` `merge()`: a file changed on both sides is a conflict, "even an identical
//! replacement", and "two additions are not auto-mergeable"); and it is accepted, path by path, where nothing it
//! touches changed. Each case runs the same work without the flag too, which §4.16's verdict accepts, so the flag is
//! what refuses it. The property holds the engine to a reference written here, apart from the engine: the paths a
//! node-exclusive work conflicts on are exactly the paths it touches whose last change is past its base.

use super::{Green, Increment, Outcome};
use crate::ops_doc::{NODE_EXCLUSIVE, Op, OpKind, OpsDoc};
use crate::verdict::MergeConflictClass;
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

/// Builds an increment as the deriver lays one out (paths interned, bytes in the post-state named by offset), every
/// operation carrying `flags`.
struct Work {
  doc: OpsDoc,
  post: Vec<u8>,
  flags: u8,
}

impl Work {
  /// A work whose operations carry no flag: §4.16's verdict judges it.
  fn plain() -> Work {
    Work {
      doc: OpsDoc::default(),
      post: Vec::new(),
      flags: 0,
    }
  }

  /// A work whose operations are node-exclusive, as a Subversion client's commit is.
  fn exclusive() -> Work {
    Work {
      flags: NODE_EXCLUSIVE,
      ..Work::plain()
    }
  }

  fn push(&mut self, kind: OpKind, path: &str, at: u64, len: u64, src: u64) -> &mut Work {
    let path = self.doc.paths.intern(path);
    self.doc.ops.push(Op {
      kind,
      flags: self.flags,
      path,
      at,
      len,
      src,
    });
    self
  }

  fn stash(&mut self, bytes: &[u8]) -> u64 {
    let offset = u64::try_from(self.post.len()).unwrap();
    self.post.extend_from_slice(bytes);
    offset
  }

  fn create(&mut self, path: &str, bytes: &[u8]) -> &mut Work {
    self.push(OpKind::Create, path, 0, 0, u64::MAX);
    self.overwrite_kind(OpKind::Insert, path, 0, bytes)
  }

  fn overwrite(&mut self, path: &str, at: u64, bytes: &[u8]) -> &mut Work {
    self.overwrite_kind(OpKind::Overwrite, path, at, bytes)
  }

  fn overwrite_kind(&mut self, kind: OpKind, path: &str, at: u64, bytes: &[u8]) -> &mut Work {
    let src = self.stash(bytes);
    self.push(kind, path, at, u64::try_from(bytes.len()).unwrap(), src)
  }

  fn set_xattr(&mut self, path: &str, name: &str, value: &[u8]) -> &mut Work {
    let name = self.doc.paths.intern(name);
    let src = self.stash(value);
    self.push(
      OpKind::SetXattr,
      path,
      name,
      u64::try_from(value.len()).unwrap(),
      src,
    )
  }

  fn rename(&mut self, from: &str, to: &str) -> &mut Work {
    let from = self.doc.paths.intern(from);
    self.push(OpKind::Rename, to, 0, 0, from)
  }

  fn unlink(&mut self, path: &str) -> &mut Work {
    self.push(OpKind::Unlink, path, 0, 0, u64::MAX)
  }

  /// The increment, identified by `id`, based on `base`.
  fn at(&self, id: u64, base: u64) -> Increment {
    let mut identity = [0u8; 32];
    identity[..8].copy_from_slice(&id.to_le_bytes());
    identity[8] = self.flags;
    Increment {
      id: identity,
      base,
      doc: self.doc.clone(),
      post_state: self.post.clone(),
      evidence: Vec::new(),
      named: Default::default(),
    }
  }
}

/// The property list a Subversion repository green keeps as one attribute of each node (§4.17.4 item 1).
const PROPS: &str = "slates.svn.props";

/// A green at version 1 with `f` and `g`, each twenty dots, made by a plain work.
fn green() -> Green {
  let mut green = Green::new();
  let outcome = green.submit(
    &Work::plain()
      .create("f", b"....................")
      .create("g", b"....................")
      .at(1, 0),
  );
  assert_eq!(outcome, Outcome::Accepted { version: 1 });
  green
}

/// The distinct paths a conflict names, or `None` for an accept.
fn conflicted(outcome: &Outcome) -> Option<BTreeSet<String>> {
  match outcome {
    Outcome::Conflict { windows } => {
      Some(windows.iter().map(|window| window.path.clone()).collect())
    }
    Outcome::Accepted { .. }
    | Outcome::BaseUnavailable { .. }
    | Outcome::CopyRefused { .. }
    | Outcome::ContentUnread { .. } => None,
  }
}

/// The classes a conflict names on `path`.
fn classes_on(outcome: &Outcome, path: &str) -> Vec<MergeConflictClass> {
  match outcome {
    Outcome::Conflict { windows } => windows
      .iter()
      .filter(|window| window.path == path)
      .map(|window| window.class)
      .collect(),
    Outcome::Accepted { .. }
    | Outcome::BaseUnavailable { .. }
    | Outcome::CopyRefused { .. }
    | Outcome::ContentUnread { .. } => Vec::new(),
  }
}

/// Submits `intervening` to a fresh green as version 2 (based on 1), then `work` built twice, plain and node-exclusive,
/// each based on 1 against its own green: the plain outcome and the node-exclusive one.
fn both(intervening: &dyn Fn(&mut Work), work: &dyn Fn(&mut Work)) -> (Outcome, Outcome) {
  let mut outcomes = Vec::new();
  for exclusive in [false, true] {
    let mut green = green();
    let mut change = Work::plain();
    intervening(&mut change);
    assert_eq!(
      green.submit(&change.at(2, 1)),
      Outcome::Accepted { version: 2 }
    );
    let mut submitted = if exclusive {
      Work::exclusive()
    } else {
      Work::plain()
    };
    work(&mut submitted);
    outcomes.push(green.submit(&submitted.at(3, 1)));
  }
  let exclusive = outcomes.pop().unwrap();
  (outcomes.pop().unwrap(), exclusive)
}

/// A-542 (FSFS rule 1, "even an identical replacement"): after another work overwrote bytes 5 to 9 of `f`, a work based
/// on the version before overwrites the same bytes with the same bytes: plain, §4.16 accepts it as identical;
/// node-exclusive, expect a conflict on `f`.
#[test]
fn an_identical_change_after_its_base_conflicts_for_a_node_exclusive_work() {
  let (plain, exclusive) = both(
    &|w| {
      w.overwrite("f", 5, b"AAAA");
    },
    &|w| {
      w.overwrite("f", 5, b"AAAA");
    },
  );
  assert_eq!(plain, Outcome::Accepted { version: 3 });
  assert_eq!(
    conflicted(&exclusive),
    Some(BTreeSet::from(["f".to_owned()]))
  );
  assert!(
    classes_on(&exclusive, "f").contains(&MergeConflictClass::Overlap),
    "{exclusive:?}"
  );
}

/// A-542 (FSFS rule 1, a file changed on both sides): another work overwrote bytes 0 to 4 of `f`; a work based on the
/// version before overwrites bytes 10 to 14: plain, §4.16 merges the disjoint ranges; node-exclusive, expect a conflict.
#[test]
fn a_disjoint_change_after_its_base_conflicts_for_a_node_exclusive_work() {
  let (plain, exclusive) = both(
    &|w| {
      w.overwrite("f", 0, b"AAAA");
    },
    &|w| {
      w.overwrite("f", 10, b"BBBB");
    },
  );
  assert_eq!(plain, Outcome::Accepted { version: 3 });
  assert_eq!(
    conflicted(&exclusive),
    Some(BTreeSet::from(["f".to_owned()]))
  );
}

/// A-542 (FSFS rule 1 across dimensions: text against properties): another work set `f`'s property list; a work based
/// on the version before changes `f`'s text: plain, the dimensions merge; node-exclusive, expect a conflict.
#[test]
fn a_property_change_after_its_base_conflicts_with_a_node_exclusive_text_change() {
  let (plain, exclusive) = both(
    &|w| {
      w.set_xattr("f", PROPS, b"K 3\nfoo\nV 3\nbar\nEND\n");
    },
    &|w| {
      w.overwrite("f", 0, b"CCCC");
    },
  );
  assert_eq!(plain, Outcome::Accepted { version: 3 });
  assert_eq!(
    conflicted(&exclusive),
    Some(BTreeSet::from(["f".to_owned()]))
  );
}

/// A-542 (FSFS rule 2, "two additions are not auto-mergeable"): another work created `h`; a work based on the version
/// before creates `h` with the same bytes: plain, §4.16 accepts the identical create; node-exclusive, expect a conflict.
#[test]
fn an_identical_addition_after_its_base_conflicts_for_a_node_exclusive_work() {
  let (plain, exclusive) = both(
    &|w| {
      w.create("h", b"same");
    },
    &|w| {
      w.create("h", b"same");
    },
  );
  assert_eq!(plain, Outcome::Accepted { version: 3 });
  assert_eq!(
    conflicted(&exclusive),
    Some(BTreeSet::from(["h".to_owned()]))
  );
  assert!(
    classes_on(&exclusive, "h").contains(&MergeConflictClass::CreateCreate),
    "{exclusive:?}"
  );
}

/// A-542 (FSFS rule 2 for a move, which Subversion sends as a copy and a delete of the source): another work changed
/// `f`; a work based on the version before moves `f` to `h`: plain, §4.16 carries the edit along; node-exclusive, expect
/// a conflict on the source.
#[test]
fn a_node_exclusive_move_whose_source_changed_after_its_base_conflicts() {
  let (plain, exclusive) = both(
    &|w| {
      w.overwrite("f", 0, b"DDDD");
    },
    &|w| {
      w.rename("f", "h");
    },
  );
  assert_eq!(plain, Outcome::Accepted { version: 3 });
  assert_eq!(
    conflicted(&exclusive),
    Some(BTreeSet::from(["f".to_owned()]))
  );
}

/// A-542 (FSFS's merge takes a node changed on one side alone): another work changed `g`; a node-exclusive work based on
/// the version before changes `f` and deletes nothing else: expect it accepted, as Subversion accepts a commit whose
/// paths did not move, with the other work's `g` kept.
#[test]
fn a_node_exclusive_work_on_nodes_unchanged_after_its_base_is_accepted() {
  let (plain, exclusive) = both(
    &|w| {
      w.overwrite("g", 0, b"EEEE");
    },
    &|w| {
      w.overwrite("f", 0, b"FFFF");
    },
  );
  assert_eq!(plain, Outcome::Accepted { version: 3 });
  assert_eq!(exclusive, Outcome::Accepted { version: 3 });
}

/// A-542 (FSFS rule 2, a delete against a change): another work changed `f`; a node-exclusive work based on the version
/// before deletes `f`: expect a conflict, as the plain work's delete/modify also is.
#[test]
fn a_node_exclusive_delete_of_a_node_changed_after_its_base_conflicts() {
  let (plain, exclusive) = both(
    &|w| {
      w.overwrite("f", 0, b"GGGG");
    },
    &|w| {
      w.unlink("f");
    },
  );
  assert!(conflicted(&plain).is_some(), "{plain:?}");
  assert_eq!(
    conflicted(&exclusive),
    Some(BTreeSet::from(["f".to_owned()]))
  );
}

/// The paths of the property's green, each a four-byte file.
const PATHS: [&str; 5] = ["a", "b", "c", "d", "e"];

/// How one generated change touches its path: its text, its property list, or both.
fn change(work: &mut Work, path: &str, how: u8, mark: u8) {
  if how % 3 != 1 {
    work.overwrite(path, u64::from(how % 4), &[mark]);
  }
  if !how.is_multiple_of(3) {
    work.set_xattr(path, PROPS, &[mark]);
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-542 (oracle): plain works change random paths at versions 2 onward; then a node-exclusive work based on a
  /// random earlier version changes a random set of paths. Expect exactly the reference's answer: a conflict naming the
  /// paths it changed whose last change is past its base, or an accept when there are none.
  #[test]
  fn a_node_exclusive_work_conflicts_on_exactly_the_paths_changed_after_its_base(
    history in prop::collection::vec(prop::collection::vec((0usize..5, any::<u8>()), 1..4), 1..8),
    base_pick in any::<usize>(),
    touched in prop::collection::btree_map(0usize..5, any::<u8>(), 1..5),
  ) {
    let mut green = Green::new();
    let mut setup = Work::plain();
    for path in PATHS {
      setup.create(path, b"....");
    }
    prop_assert_eq!(green.submit(&setup.at(1, 0)), Outcome::Accepted { version: 1 });
    // The reference: the last version each path changed at.
    let mut last: BTreeMap<&str, u64> = PATHS.iter().map(|path| (*path, 1)).collect();
    for (step, changes) in history.iter().enumerate() {
      let version = u64::try_from(step).unwrap() + 2;
      let mut work = Work::plain();
      let mut seen = BTreeSet::new();
      for (index, how) in changes {
        if seen.insert(*index) {
          change(&mut work, PATHS[*index], *how, u8::try_from(version % 251).unwrap());
          last.insert(PATHS[*index], version);
        }
      }
      prop_assert_eq!(green.submit(&work.at(version, version - 1)), Outcome::Accepted { version });
    }
    let head = green.head();
    let base = 1 + u64::try_from(base_pick).unwrap() % head;
    let mut work = Work::exclusive();
    for (index, how) in &touched {
      change(&mut work, PATHS[*index], *how, 0xff);
    }
    let expected: BTreeSet<String> = touched
      .keys()
      .map(|index| PATHS[*index])
      .filter(|path| last[path] > base)
      .map(str::to_owned)
      .collect();
    let outcome = green.submit(&work.at(head + 1000, base));
    if expected.is_empty() {
      prop_assert_eq!(outcome, Outcome::Accepted { version: head + 1 });
    } else {
      prop_assert_eq!(conflicted(&outcome), Some(expected));
    }
  }
}
