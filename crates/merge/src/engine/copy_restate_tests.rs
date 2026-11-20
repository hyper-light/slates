//! A rebase that restates a copy (A-626 part 4; §4.16 "Rebase, the only corrective path"; A-586's same-increment rule).
//! Works are made as an agent makes them, a journal composed by the deriver over `Green::base_for` (a copy of a tree at
//! a version, then edits and files made in it, or a rename out of it or a link into it), based behind the head. Each
//! must rebase as it submits: a copy and what its work does in the copied tree restate cleanly, and the restated journal
//! composed again over the head and submitted gives the green the original submit gives; a rename or a hard link with an
//! end in a copied tree is refused (`TypeChanged`) by both.

use std::cell::RefCell;
use std::collections::BTreeMap;

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

use super::{Green, Increment, Outcome, Rebased};
use crate::increment::{VolumeOp, compose_volume_planned};
use crate::ops_doc::OpKind;
use crate::verdict::MergeConflictClass;

/// Shape: the trunk's files at version 1, then the head's change at version 2 (`trunk/c`'s first byte), so a copy from
/// version 1 brings bytes its source no longer holds.
const TRUNK: [(&str, &[u8]); 3] = [
  ("trunk/a", b"alpha"),
  ("trunk/c", b"gamma"),
  ("trunk/d/b", b"beta"),
];
/// Shape: the sources a work copies, its destinations, and the files it makes.
const SOURCES: [&str; 3] = ["trunk", "trunk/d", ""];
/// Shape: see `SOURCES`.
const DESTINATIONS: [&str; 2] = ["b", "tags/t"];

/// A work's files: each path's bytes.
type Files = BTreeMap<String, Vec<u8>>;

/// The increment composing `journal` over `green`'s version `base`, its runs read from `files` (the seal's reading).
fn compose(
  green: &Green,
  (base, id): (u64, u8),
  journal: &[VolumeOp],
  files: &Files,
) -> Result<Increment, String> {
  let (doc, plan) = compose_volume_planned(&green.base_for(base, journal), journal)
    .map_err(|e| format!("{e:?}"))?;
  let mut post_state = Vec::new();
  for op in doc
    .ops
    .iter()
    .filter(|op| matches!(op.kind, OpKind::Overwrite | OpKind::Insert | OpKind::Extend))
  {
    let path = doc.paths.path(op.path).ok_or("an op names no path")?;
    let from = plan
      .get(&op.src)
      .and_then(|from| usize::try_from(*from).ok())
      .ok_or("no plan")?;
    let len = usize::try_from(op.len).map_err(|e| e.to_string())?;
    let run = files
      .get(path)
      .and_then(|bytes| bytes.get(from..from.saturating_add(len)))
      .ok_or("no run")?;
    let start = usize::try_from(op.src).map_err(|e| e.to_string())?;
    if post_state.len() < start.saturating_add(len) {
      post_state.resize(start.saturating_add(len), 0);
    }
    post_state
      .get_mut(start..start.saturating_add(len))
      .ok_or("outside")?
      .copy_from_slice(run);
  }
  Ok(Increment {
    id: [id; 32],
    base,
    doc,
    post_state,
    evidence: Vec::new(),
  })
}

/// Submits `journal` (its files `files`) at `green`'s head, which must accept it.
fn accept(green: &mut Green, id: u8, journal: &[VolumeOp], files: &Files) -> Result<(), String> {
  let increment = compose(green, (green.head(), id), journal, files)?;
  match green.submit(&increment) {
    Outcome::Accepted { .. } => Ok(()),
    other => Err(format!("a fixture's work was answered {other:?}")),
  }
}

/// The fixture: the trunk at version 1 (its directories, its files), `trunk/c` changed at version 2.
fn fixture() -> Result<Green, String> {
  let mut green = Green::new();
  let mut journal = vec![
    VolumeOp::Mkdir {
      path: "trunk".to_owned(),
    },
    VolumeOp::Mkdir {
      path: "trunk/d".to_owned(),
    },
  ];
  let mut files = Files::new();
  for (path, bytes) in TRUNK {
    let len = u64::try_from(bytes.len()).map_err(|e| e.to_string())?;
    journal.push(VolumeOp::Create {
      path: path.to_owned(),
    });
    journal.push(VolumeOp::Extend {
      path: path.to_owned(),
      at: 0,
      len,
    });
    files.insert(path.to_owned(), bytes.to_vec());
  }
  accept(&mut green, 1, &journal, &files)?;
  let changed = [VolumeOp::Overwrite {
    path: "trunk/c".to_owned(),
    at: 0,
    len: 1,
  }];
  accept(
    &mut green,
    2,
    &changed,
    &Files::from([("trunk/c".to_owned(), b"Gamma".to_vec())]),
  )?;
  Ok(green)
}

/// One step a generated work takes after its copy.
#[derive(Clone, Copy, Debug)]
enum Extra {
  /// The first byte of a copied file (the choice's, among them) made `E`.
  Edit(u8),
  /// A new file made in the copied tree.
  Create(u8),
  /// A copied file renamed out of the tree.
  RenameOut(u8),
  /// A hard link made in the copied tree, to a file outside it.
  LinkIn,
}

/// A generated work: its copy (source, destination, version) and its extras.
type Raw = ((u8, u8, bool), Vec<(u8, u8)>);

/// A work being made: the agent's files and its journal.
struct Work {
  files: Files,
  journal: Vec<VolumeOp>,
  /// The copied tree's files, as the copy brings them.
  copied: Vec<String>,
  /// Whether the work renamed out of the tree or linked into it (`TypeChanged` by A-586).
  refused: bool,
}

impl Work {
  /// The work `raw` makes over `green` at version 1.
  fn of(green: &Green, ((source, destination, at_head), extras): &Raw) -> Work {
    let from = SOURCES
      .get(usize::from(*source) % SOURCES.len())
      .copied()
      .unwrap_or_default();
    let to = DESTINATIONS
      .get(usize::from(*destination) % DESTINATIONS.len())
      .copied()
      .unwrap_or_default();
    let version = if *at_head { green.head() } else { 1 };
    let mut work = Work {
      files: green
        .files_at(1)
        .into_iter()
        .map(|(p, b)| (p, b.into_owned()))
        .collect(),
      journal: Vec::new(),
      copied: Vec::new(),
      refused: false,
    };
    work.journal.push(VolumeOp::Copy {
      from: from.to_owned(),
      to: to.to_owned(),
      version,
      replace: false,
    });
    for (path, bytes) in green.files_at(version) {
      if let Some(rel) = super::copy::relative(&path, from) {
        let at = super::copy::joined(to, rel);
        work.copied.push(at.clone());
        work.files.insert(at, bytes.into_owned());
      }
    }
    for (index, (kind, choice)) in extras.iter().enumerate() {
      let extra = match kind % 4 {
        0 => Extra::Edit(*choice),
        1 => Extra::Create(u8::try_from(index).unwrap_or(u8::MAX)),
        2 => Extra::RenameOut(*choice),
        _ => Extra::LinkIn,
      };
      work.take(extra, to);
    }
    work
  }

  /// The copied file `choice` picks, if the copy brought any that the work still holds.
  fn pick(&self, choice: u8) -> Option<String> {
    let held: Vec<&String> = self
      .copied
      .iter()
      .filter(|path| self.files.contains_key(*path))
      .collect();
    held
      .get(usize::from(choice) % held.len().max(1))
      .map(|path| (*path).clone())
  }

  /// Takes one extra step in the copied tree at `to`.
  fn take(&mut self, extra: Extra, to: &str) {
    match extra {
      Extra::Edit(choice) => {
        let Some(path) = self.pick(choice) else {
          return;
        };
        self.journal.push(VolumeOp::Overwrite {
          path: path.clone(),
          at: 0,
          len: 1,
        });
        if let Some(byte) = self
          .files
          .get_mut(&path)
          .and_then(|bytes| bytes.first_mut())
        {
          *byte = b'E';
        }
      }
      Extra::Create(index) => {
        let path = super::copy::joined(to, &format!("new{index}"));
        if self.files.contains_key(&path) {
          return;
        }
        self.journal.push(VolumeOp::Create { path: path.clone() });
        self.journal.push(VolumeOp::Extend {
          path: path.clone(),
          at: 0,
          len: 3,
        });
        self.files.insert(path, b"new".to_vec());
      }
      Extra::RenameOut(choice) => {
        let Some(path) = self.pick(choice) else {
          return;
        };
        if self.files.contains_key("out") {
          return;
        }
        self.journal.push(VolumeOp::Rename {
          from: path.clone(),
          to: "out".to_owned(),
        });
        let bytes = self.files.remove(&path).unwrap_or_default();
        self.files.insert("out".to_owned(), bytes);
        self.refused = true;
      }
      Extra::LinkIn => {
        let path = super::copy::joined(to, "link");
        if self
          .journal
          .iter()
          .any(|op| matches!(op, VolumeOp::Link { .. }))
        {
          return;
        }
        self.journal.push(VolumeOp::Link {
          path,
          target: "trunk/a".to_owned(),
        });
        self.refused = true;
      }
    }
  }
}

/// What the oracle met, counted over the works it checked.
#[derive(Debug, Default)]
struct Census {
  copies_restated: u64,
  edits_restated: u64,
  creates_restated: u64,
  renames_out_refused: u64,
  links_in_refused: u64,
}

/// Whether the windows name a type change (A-586: a rename out of a copied tree, a link into one), among any others the
/// work's other steps add: an edit of the file it then renames out adds a delete against modify on the rename's
/// destination, whose source the base never held (the oracle's first run, kiln r193, Mac Studio).
fn refused_typed(windows: &[super::ConflictWindow]) -> bool {
  windows
    .iter()
    .any(|window| window.class == MergeConflictClass::TypeChanged)
}

/// The oracle on one work: rebase on one fixture, submit on a twin; refused alike, or the restated journal, composed
/// again over the head with the rebased work's files, submitted on a third twin, giving the twin's green.
fn restate(work: &Work, census: &mut Census) -> Result<(), String> {
  let (rebasing, mut accepting) = (fixture()?, fixture()?);
  let increment = compose(&rebasing, (1, 9), &work.journal, &work.files)?;
  match (rebasing.rebase(&increment), accepting.submit(&increment)) {
    (Rebased::Conflict { windows }, Outcome::Conflict { windows: refused }) if work.refused => {
      if !refused_typed(&windows) || windows != refused {
        return Err(format!("refused otherwise: {windows:?} and {refused:?}"));
      }
      let renamed = work
        .journal
        .iter()
        .any(|op| matches!(op, VolumeOp::Rename { .. }));
      census.renames_out_refused += u64::from(renamed);
      census.links_in_refused += u64::from(!renamed);
      Ok(())
    }
    (
      Rebased::Rebased {
        version,
        changed,
        journal,
      },
      Outcome::Accepted { .. },
    ) if !work.refused => {
      let mut files: Files = rebasing
        .files()
        .map(|(path, bytes)| (path, bytes.to_vec()))
        .collect();
      for (path, after) in changed {
        match after {
          Some(bytes) => files.insert(path, bytes),
          None => files.remove(&path),
        };
      }
      let mut again = fixture()?;
      let restated = compose(&again, (version, 10), &journal, &files)?;
      let answer = again.submit(&restated);
      let read = |green: &mut Green| {
        (
          green.base_at(green.head()),
          green
            .files_at(green.head())
            .into_iter()
            .map(|(p, b)| (p, b.into_owned()))
            .collect::<Files>(),
          green.head_identity(),
        )
      };
      if !matches!(answer, Outcome::Accepted { .. }) || read(&mut again) != read(&mut accepting) {
        return Err(format!(
          "the restated journal {journal:?} was answered {answer:?}"
        ));
      }
      census.copies_restated +=
        u64::from(journal.iter().any(|op| matches!(op, VolumeOp::Copy { .. })));
      census.edits_restated += u64::from(
        work
          .journal
          .iter()
          .any(|op| matches!(op, VolumeOp::Overwrite { .. })),
      );
      census.creates_restated += u64::from(
        work
          .journal
          .iter()
          .any(|op| matches!(op, VolumeOp::Create { .. })),
      );
      Ok(())
    }
    (rebased, submitted) => Err(format!(
      "the rebase answered {rebased:?} and the submit {submitted:?}"
    )),
  }
}

/// A-626 part 4 (the deriver's `VolumeOp::Copy`; a rebase that restates a copy). Do: copy `trunk` at version 1 to `b`
/// and overwrite `b/a`'s first byte, in a work based at version 1 and rebased past version 2. Expect the journal
/// restated as the copy and the edit over the bytes it brings, `b/a` the only changed file, and the restated journal,
/// composed again over the head and submitted, giving the green the original submit gives (`restate`).
#[test]
fn a_rebase_restates_a_copy_and_the_edit_beneath_it() -> Result<(), String> {
  let green = fixture()?;
  let raw: Raw = ((0, 0, false), vec![(0, 0)]);
  let work = Work::of(&green, &raw);
  let rebasing = fixture()?;
  let increment = compose(&rebasing, (1, 9), &work.journal, &work.files)?;
  let Rebased::Rebased {
    journal, changed, ..
  } = rebasing.rebase(&increment)
  else {
    return Err("not rebased".to_owned());
  };
  let expected = vec![
    VolumeOp::Copy {
      from: "trunk".to_owned(),
      to: "b".to_owned(),
      version: 1,
      replace: false,
    },
    VolumeOp::Overwrite {
      path: "b/a".to_owned(),
      at: 0,
      len: 1,
    },
  ];
  if journal != expected || changed.keys().collect::<Vec<_>>() != vec!["b/a"] {
    return Err(format!("{journal:?} {changed:?}"));
  }
  restate(&work, &mut Census::default())
}

/// A-626 part 4 (the oracle's first run, kiln r193, shrunk): a work that copies `trunk` to `b`, edits a copied file and
/// renames a copied file out of the tree. Do: rebase it and submit it. Expect both refused alike, a type change among
/// the windows (A-586), beside the delete against modify the edit before the rename adds on its destination.
#[test]
fn an_edit_then_a_rename_out_of_a_copy_is_refused_alike() -> Result<(), String> {
  let green = fixture()?;
  let raw: Raw = ((0, 0, false), vec![(188, 150), (130, 81)]);
  let work = Work::of(&green, &raw);
  let mut census = Census::default();
  restate(&work, &mut census)?;
  if census.renames_out_refused != 1 {
    return Err(format!("not refused as a rename out: {census:?}"));
  }
  Ok(())
}

/// A-626 part 4's oracle. Do: generate works over the fixture that copy `trunk`, `trunk/d` or the root, at version 1 or
/// the head, to `b` or `tags/t`, then edit copied files, make files in the copied tree, rename a copied file out of it,
/// or link a name in it to a file outside; each based at version 1 and rebased past version 2. Expect each to rebase as
/// it submits: refused alike, a type change among the windows, where it renames out of the tree or links into it
/// (A-586; an edit before the rename out adds a delete against modify on its destination), else rebased,
/// its restated journal composed again over the head and submitted giving the green the original submit gives; and the
/// census to show copies, edits and made files restated, and renames out and links in refused.
#[test]
fn every_rebased_copy_restates_as_its_work_submits() {
  let census = RefCell::new(Census::default());
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config::default()));
  let raw = (
    any::<(u8, u8, bool)>(),
    prop::collection::vec(any::<(u8, u8)>(), 0..4),
  );
  let result = runner.run(&raw, |raw| {
    let green = fixture().map_err(TestCaseError::fail)?;
    let work = Work::of(&green, &raw);
    restate(&work, &mut census.borrow_mut()).map_err(TestCaseError::fail)
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  let met = [
    census.copies_restated,
    census.edits_restated,
    census.creates_restated,
    census.renames_out_refused,
    census.links_in_refused,
  ];
  assert!(
    met.iter().all(|&count| count > 0),
    "a case never met: {census:?}"
  );
}
