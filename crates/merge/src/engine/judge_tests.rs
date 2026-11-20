//! A-635's engine tests (§4.17.1 "A push is a work"; §4.16): the verdict split into a judgment and its commit. A
//! judgment applies nothing; a held verdict commits as the version after the head it was judged at, or is refused once
//! the green moved; and judging then committing is a submit, over generated histories.

// proptest's strategy types carry `Arc` (D-8's harness exception).
#![allow(clippy::disallowed_types)]

use std::cell::RefCell;
use std::collections::BTreeSet;

use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

use super::{Green, HeadMoved, Increment, Judgment, Outcome};
use crate::ops_doc::{Op, OpKind, OpsDoc};
use crate::push::{HEAD_ATTRIBUTE, ROOT};

/// Shape: the files a generated work edits, few so works collide often.
const FILES: [&str; 3] = ["a", "b", "c"];

/// One edit of a generated work, its file named by its index into [`FILES`]; offsets are taken modulo the file's
/// length at the work's base.
#[derive(Clone, Debug)]
enum Edit {
  /// The file's bytes replaced whole, or the file created with them.
  Write(usize, Vec<u8>),
  /// Bytes inserted at an offset.
  Insert(usize, u8, Vec<u8>),
  /// The file removed, when it is there.
  Remove(usize),
  /// The root's head set (§4.17.1 item 3).
  Head(Vec<u8>),
}

/// An increment under construction, laid out as the deriver lays one out.
#[derive(Default)]
struct Work {
  doc: OpsDoc,
  post: Vec<u8>,
}

impl Work {
  fn op(&mut self, kind: OpKind, path: &str, (at, len, src): (u64, u64, u64)) {
    let path = self.doc.paths.intern(path);
    self.doc.ops.push(Op {
      kind,
      flags: 0,
      path,
      at,
      len,
      src,
    });
  }

  fn stash(&mut self, bytes: &[u8]) -> u64 {
    let at = self.post.len() as u64;
    self.post.extend_from_slice(bytes);
    at
  }

  /// Inserts `bytes` at `at`; an insert of nothing changes nothing and is left out.
  fn insert(&mut self, path: &str, at: u64, bytes: &[u8]) {
    if bytes.is_empty() {
      return;
    }
    let src = self.stash(bytes);
    self.op(OpKind::Insert, path, (at, bytes.len() as u64, src));
  }
}

impl Edit {
  /// The path the edit names.
  fn path(&self) -> &'static str {
    match self {
      Edit::Write(file, _) | Edit::Insert(file, ..) | Edit::Remove(file) => FILES[*file],
      Edit::Head(_) => ROOT,
    }
  }

  /// Lays the edit's ops into `work`, `len` the path's length at the base when a file is there.
  fn emit(&self, work: &mut Work, len: Option<u64>) {
    let path = self.path();
    match (self, len) {
      (Edit::Write(_, bytes), Some(len)) => {
        if len > 0 {
          work.op(OpKind::Delete, path, (0, len, 0));
        }
        work.insert(path, 0, bytes);
      }
      (Edit::Write(_, bytes), None) => {
        work.op(OpKind::Create, path, (0, 0, u64::MAX));
        work.insert(path, 0, bytes);
      }
      (Edit::Insert(_, at, bytes), Some(len)) => {
        work.insert(path, u64::from(*at) % (len + 1), bytes);
      }
      (Edit::Remove(_), Some(_)) => work.op(OpKind::Unlink, path, (0, 0, u64::MAX)),
      (Edit::Head(value), _) => {
        let name = work.doc.paths.intern(HEAD_ATTRIBUTE);
        let src = work.stash(value);
        work.op(OpKind::SetXattr, path, (name, value.len() as u64, src));
      }
      // An insert into or a removal of a file the base does not hold changes nothing: left out.
      (Edit::Insert(..) | Edit::Remove(_), None) => {}
    }
  }
}

/// The increment `id` of `edits` against `green`'s `base`: an edit naming a path an earlier one named is left out.
fn work(green: &Green, base: u64, edits: &[Edit], id: u64) -> Increment {
  let mut work = Work::default();
  let mut used: BTreeSet<&str> = BTreeSet::new();
  for edit in edits {
    if !used.insert(edit.path()) {
      continue;
    }
    let len = match edit {
      Edit::Head(_) => None,
      _ => green
        .content_len_at(edit.path(), base)
        .map(|len| len as u64),
    };
    edit.emit(&mut work, len);
  }
  let mut identity = [0u8; 32];
  identity[..8].copy_from_slice(&id.to_le_bytes());
  Increment {
    id: identity,
    base,
    doc: work.doc,
    post_state: work.post,
    evidence: Vec::new(),
  }
}

/// What a reader sees of a green: its head, its identity, every file's bytes and the root's head attribute.
type Seen = (u64, [u8; 32], Vec<Option<Vec<u8>>>, Option<Vec<u8>>);

/// What a submit answers and what its chain would record of it.
type Answered = (Outcome, Option<Vec<crate::chain_entry::Applied>>);

/// What a reader sees of `green` ([`Seen`]).
fn seen(green: &mut Green) -> Seen {
  let files = FILES
    .iter()
    .map(|path| green.content(path).map(<[u8]>::to_vec))
    .collect();
  let head = green.xattr(ROOT, HEAD_ATTRIBUTE).map(<[u8]>::to_vec);
  (green.head(), green.head_identity(), files, head)
}

/// Submits `edits` at the head, accepted: the version made.
fn landed(green: &mut Green, edits: &[Edit], id: u64) -> u64 {
  let inc = work(green, green.head(), edits, id);
  match green.submit(&inc) {
    Outcome::Accepted { version } => version,
    other => panic!("not accepted: {other:?}"),
  }
}

/// A-635 (the verdict split: nothing applied until the commit). Do: judge a work that accepts, a work another lands
/// before it commits, a work that conflicts and the same work again. Expect: a judgment changes nothing a reader sees;
/// the held verdict commits as the version after the head it was judged at, with what a submit would record; a held
/// verdict whose green moved is refused `HeadMoved`, nothing applied, and judged again meets what moved it; a conflict's
/// windows answer it again from the cache; a work accepted before is answered its version.
#[test]
fn a_held_verdict_commits_at_its_head_and_is_refused_once_the_green_moved() {
  let based = || {
    let mut green = Green::new();
    landed(&mut green, &[Edit::Write(0, b"base".to_vec())], 1);
    green
  };
  let mut green = based();
  assert_a_held_verdict_commits_as_a_submit_would(&mut green, based());
  assert_a_held_verdict_is_refused_once_its_green_moved(&mut green);
  assert_conflicts_and_retries_are_answered(&mut green);
}

/// A-635's first part: a work judged at version 1 applies nothing, and commits as version 2 with what a submit would
/// record, leaving the green as the submit leaves its twin, a green made by the same steps.
fn assert_a_held_verdict_commits_as_a_submit_would(green: &mut Green, mut submitted: Green) {
  let before = seen(green);
  let accepting = work(green, 1, &[Edit::Write(1, b"b".to_vec())], 2);
  let held = accepted(green.judge(&accepting));
  assert_eq!(held.at(), 1);
  assert_eq!(seen(green), before, "a judgment applied something");
  let answered = submitted.submit_resolved(&accepting);
  assert_eq!(answered.1.as_deref(), Some(held.applied()));
  let committed = green
    .commit_judged(held)
    .map(|(version, applied)| (Outcome::Accepted { version }, Some(applied)));
  assert_eq!(committed, Ok(answered));
  assert_eq!(seen(green), seen(&mut submitted));
}

/// A-635's second part: a work judged at version 2, then another landed: its held verdict is refused `HeadMoved` with
/// nothing applied, and judged again it is held at version 3.
fn assert_a_held_verdict_is_refused_once_its_green_moved(green: &mut Green) {
  let late = work(green, 2, &[Edit::Write(2, b"c".to_vec())], 3);
  let stale = accepted(green.judge(&late));
  landed(green, &[Edit::Write(0, b"moved".to_vec())], 4);
  let moved = seen(green);
  assert_eq!(
    green.commit_judged(stale).map(|(version, _)| version),
    Err(HeadMoved {
      judged_at: 2,
      head: 3
    })
  );
  assert_eq!(seen(green), moved, "a refused commit applied something");
  assert_eq!(accepted(green.judge(&late)).at(), 3);
}

/// The held verdict of an accepting judgment.
fn accepted(judgment: Judgment) -> super::Judged {
  match judgment {
    Judgment::Accepted(held) => held,
    other => panic!("not accepted: {other:?}"),
  }
}

/// A-635's second half: a conflict judged twice is answered its windows both times, the second from the cache, and a
/// work accepted before its version.
fn assert_conflicts_and_retries_are_answered(green: &mut Green) {
  let head = green.head();
  let first = work(green, head, &[Edit::Head(b"one".to_vec())], 10);
  let second = work(green, head, &[Edit::Head(b"two".to_vec())], 11);
  let version = landed_increment(green, &first);
  let windows = |judgment: Judgment| match judgment {
    Judgment::Conflict { windows } => windows,
    other => panic!("not a conflict: {other:?}"),
  };
  let met = windows(green.judge(&second));
  assert!(!met.is_empty());
  assert_eq!(windows(green.judge(&second)), met);
  assert!(matches!(
    green.judge(&first),
    Judgment::AcceptedBefore { version: before } if before == version
  ));
}

/// Submits `inc`, accepted: the version made.
fn landed_increment(green: &mut Green, inc: &Increment) -> u64 {
  match green.submit(inc) {
    Outcome::Accepted { version } => version,
    other => panic!("not accepted: {other:?}"),
  }
}

/// One step of a generated history: a work at `back` versions behind the head, submitted to one green and judged then
/// committed on the other; or the work of an earlier step again.
#[derive(Clone, Debug)]
enum Step {
  Work { back: u8, edits: Vec<Edit> },
  Again { step: u8 },
}

fn bytes() -> impl Strategy<Value = Vec<u8>> {
  proptest::collection::vec(any::<u8>(), 0..6)
}

fn edit() -> impl Strategy<Value = Edit> {
  (0..4u8, 0..FILES.len(), any::<u8>(), bytes()).prop_map(|(kind, file, at, bytes)| match kind {
    0 => Edit::Write(file, bytes),
    1 => Edit::Insert(file, at, bytes),
    2 => Edit::Remove(file),
    _ => Edit::Head(bytes),
  })
}

fn step() -> impl Strategy<Value = Step> {
  (
    0..8u8,
    0..3u8,
    proptest::collection::vec(edit(), 1..4),
    any::<u8>(),
  )
    .prop_map(|(kind, back, edits, step)| match kind {
      0 => Step::Again { step },
      _ => Step::Work { back, edits },
    })
}

/// What the generated histories met: the oracle's non-vacuity counters.
#[derive(Debug, Default)]
struct Census {
  accepted: u64,
  accepted_before: u64,
  conflicts: u64,
}

/// `inc` judged on `green` and, when accepted, committed: answered as a submit answers, counted.
fn judged_and_committed(
  green: &mut Green,
  inc: &Increment,
  census: &mut Census,
) -> Result<Answered, TestCaseError> {
  Ok(match green.judge(inc) {
    Judgment::Accepted(held) => {
      census.accepted += 1;
      let (version, applied) = green
        .commit_judged(held)
        .map_err(|moved| TestCaseError::fail(format!("{moved:?}")))?;
      (Outcome::Accepted { version }, Some(applied))
    }
    Judgment::AcceptedBefore { version } => {
      census.accepted_before += 1;
      (Outcome::Accepted { version }, None)
    }
    Judgment::Conflict { windows } => {
      census.conflicts += 1;
      (Outcome::Conflict { windows }, None)
    }
    Judgment::BaseUnavailable { base, floor } => (Outcome::BaseUnavailable { base, floor }, None),
    Judgment::CopyRefused { path, case } => (Outcome::CopyRefused { path, case }, None),
  })
}

/// Runs `steps` on two greens, one submitting and one judging then committing: after every step their answers, what
/// their chains would record, and what a reader sees of them are equal.
fn judging_equals_submitting(steps: &[Step], census: &mut Census) -> Result<(), TestCaseError> {
  let mut submitted = Green::new();
  let mut judged = Green::new();
  let mut made: Vec<Increment> = Vec::new();
  for (n, step) in steps.iter().enumerate() {
    let inc = match step {
      Step::Work { back, edits } => {
        let base = submitted.head().saturating_sub(u64::from(*back));
        work(&submitted, base, edits, n as u64 + 1)
      }
      Step::Again { step } => {
        let earlier = usize::from(*step) % made.len().max(1);
        let Some(inc) = made.get(earlier).cloned() else {
          continue;
        };
        inc
      }
    };
    let answered = submitted.submit_resolved(&inc);
    let held = judged_and_committed(&mut judged, &inc, census)?;
    prop_assert_eq!(&held, &answered, "step {}: {:?}", n, step);
    prop_assert_eq!(seen(&mut judged), seen(&mut submitted), "step {}", n);
    made.push(inc);
  }
  Ok(())
}

/// A-635 (one verdict for a submit and a push). Do: run generated histories of works (files written whole, inserted
/// into, removed, and the root's head set, based at the head or behind it) on two greens, one submitting each and one
/// judging it and committing what it accepted, with earlier works sent again. Expect: after every step both answer the
/// same (the version, or the same conflict windows), would record the same chain entry, and read the same head,
/// identity, files and head attribute; the census shows accepts, accepts of a work sent again and conflicts were met.
#[test]
fn judging_then_committing_equals_a_submit_over_every_history() {
  let census = RefCell::new(Census::default());
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config {
    cases: slates_test_seeds::cases(256),
    ..Config::default()
  }));
  let result = runner.run(&proptest::collection::vec(step(), 1..24), |steps| {
    judging_equals_submitting(&steps, &mut census.borrow_mut())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  assert!(
    [census.accepted, census.accepted_before, census.conflicts]
      .iter()
      .all(|&count| count > 0),
    "a kind of verdict never met: {census:?}"
  );
}
