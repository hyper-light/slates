//! Worked cases of a work over a tree (A-659's second half; `tree`, its module docs).

use std::borrow::Cow;
use std::collections::BTreeMap;

use super::tests::{Work, green_of, text};
use super::{Landed, TreeBase, TreeNode};
use crate::engine::{ConflictWindow, Green, Outcome};
use crate::range::Range;
use crate::verdict::MergeConflictClass;

/// A tree of files and their modes, as a tool's store gives one.
#[derive(Default)]
struct Tree {
  files: BTreeMap<String, Vec<u8>>,
  modes: BTreeMap<String, u32>,
}

impl Tree {
  fn of(files: &[(&str, &[u8])]) -> Tree {
    Tree {
      files: files
        .iter()
        .map(|(path, bytes)| ((*path).to_owned(), bytes.to_vec()))
        .collect(),
      modes: BTreeMap::new(),
    }
  }
}

impl TreeBase for Tree {
  fn node(&self, path: &str) -> TreeNode<'_> {
    match self.files.get(path) {
      Some(bytes) => TreeNode::File {
        bytes: Cow::Borrowed(bytes),
        mode: self.modes.get(path).copied(),
      },
      None => TreeNode::Absent,
    }
  }
}

/// A green holding f `0123456789` and g `ggg`, where a work based at version 0 made `change` (version 1).
fn green_changed(change: Work) -> Green {
  let mut green = green_of(&[("f", b"0123456789"), ("g", b"ggg")]);
  let outcome = green.submit(&change.increment(9, 0));
  assert_eq!(outcome, Outcome::Accepted { version: 1 });
  green
}

/// The tree the green held at version 0.
fn first_tree() -> Tree {
  Tree::of(&[("f", b"0123456789"), ("g", b"ggg")])
}

/// A-659, §4.17.1 "A push is a work" item 6: a work over a tree lands where the head holds the tree's files. Do: after a
/// work changed g, land a work over the first tree overwriting `[2, 4)` of f. Expect it based on the head (version 1)
/// with its operations as declared, landed as version 2: `01AB456789`.
#[test]
fn a_work_over_a_tree_lands_where_the_head_holds_the_tree_s_files() {
  let mut green = green_changed(Work::default().overwrite("g", 0, b"X"));
  let work = Work::default().overwrite("f", 2, b"AB").upper(1);
  let landed = green.land_over_tree(&first_tree(), &work);
  let Landed::Accepted {
    version,
    restated: Some(restated),
    ..
  } = landed
  else {
    panic!("{landed:?}");
  };
  assert_eq!((version, restated.base), (2, 1));
  assert_eq!(restated.doc.ops, work.doc.ops);
  assert_eq!(text(&green, "f").as_deref(), Some(&b"01AB456789"[..]));
}

/// A-659, item 6: "what the green changed since it is read as the whole-file differences between that tree and the
/// head's ... (a conflict a finer base would have avoided, never a wrong accept)". Do: after a work overwrote `[8, 10)`
/// of f, land over the first tree a work overwriting `[2, 4)` of f, then one making exactly the head's bytes. Expect the
/// first an overlap over the whole file `[0, 10)` with nothing applied, the second accepted with f unchanged.
#[test]
fn a_work_over_a_tree_conflicts_whole_on_a_file_the_head_changed_unless_it_makes_the_head_s_bytes()
{
  let mut green = green_changed(Work::default().overwrite("f", 8, b"YZ"));
  let disjoint = Work::default().overwrite("f", 2, b"AB").upper(1);
  let whole = ConflictWindow {
    path: "f".to_owned(),
    range: Range::new(0, 10),
    class: MergeConflictClass::Overlap,
  };
  let refused = green.land_over_tree(&first_tree(), &disjoint);
  assert_eq!(
    refused,
    Landed::Conflict {
      windows: vec![whole]
    }
  );
  assert_eq!(green.head(), 1);
  let same = Work::default().overwrite("f", 8, b"YZ").upper(2);
  let landed = green.land_over_tree(&first_tree(), &same);
  assert!(
    matches!(landed, Landed::Accepted { version: 2, .. }),
    "{landed:?}"
  );
  assert_eq!(text(&green, "f").as_deref(), Some(&b"01234567YZ"[..]));
}

/// A-659, item 6, the namespace: a removal and a mode are judged against the whole values the tree and the head hold.
/// Do: after a work changed f's bytes and set g's mode to 0o755, land over the first tree works unlinking f, unlinking g,
/// setting g's mode to 0o700 and setting it to 0o755. Expect delete against modify, delete against modify (its mode
/// changed), meta against meta, and accepted (the head's value).
#[test]
fn a_work_over_a_tree_removes_and_sets_modes_against_whole_values() {
  let change = Work::default().overwrite("f", 0, b"Z").mode("g", 0o755);
  let outcome = |work: Work| {
    let mut green = green_changed(change.clone());
    green.land_over_tree(&first_tree(), &work.upper(1))
  };
  let window = |path: &str, class| Landed::Conflict {
    windows: vec![ConflictWindow {
      path: path.to_owned(),
      range: Range::new(0, 0),
      class,
    }],
  };
  assert_eq!(
    outcome(Work::default().unlink("f")),
    window("f", MergeConflictClass::DeleteModify)
  );
  assert_eq!(
    outcome(Work::default().unlink("g")),
    window("g", MergeConflictClass::DeleteModify)
  );
  assert_eq!(
    outcome(Work::default().mode("g", 0o700)),
    window("g", MergeConflictClass::MetaMeta)
  );
  let same = outcome(Work::default().mode("g", 0o755));
  assert!(
    matches!(same, Landed::Accepted { version: 2, .. }),
    "{same:?}"
  );
}
