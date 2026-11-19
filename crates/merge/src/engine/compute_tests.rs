//! A-658's engine tests (A-645 "Merges computed anywhere, committed by position"; §4.16 "The verdict", "Commit"): the
//! verdict and the rebase as pure functions of the green's versions. A copy of a green rebuilt from its owner's chain
//! entries alone, as a holder's replica is, computes the owner's verdict and the owner's rebase for every work; computing
//! changes nothing and keeps no conflict; and a green that lacks a version the work is judged over refuses it, typed.

// proptest's strategy types carry `Arc` (D-8's harness exception).
#![allow(clippy::disallowed_types)]

use std::cell::RefCell;
use std::collections::BTreeSet;

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

use super::{ComputeRefused, Computed, Green, Increment, Judged, Judgment, Outcome, Rebased};
use crate::increment::Base;
use crate::ops_doc::{Op, OpKind, OpsDoc};
use crate::origin::Origin;
use crate::push::{HEAD_ATTRIBUTE, ROOT};

/// Shape: the files a generated work names, two at the root and two in a directory, few so works collide often.
const FILES: [&str; 4] = ["a", "b", "d/c", "d/e"];
/// Shape: the directories a generated work makes or removes: one holding files, one that starts absent.
const DIRS: [&str; 2] = ["d", "n"];
/// Shape: the attributes a generated work sets or removes.
const NAMES: [&str; 2] = ["user.x", "user.y"];
/// Shape: the modes a generated work sets.
const MODES: [u32; 2] = [0o644, 0o755];
/// Shape: the path of the hard link a generated work makes.
const LINK: &str = "h";

/// The origin every generated history starts from: the four files, their directory, and one mode.
fn origin() -> Origin {
  let mut origin = Origin::default();
  let contents: [&[u8]; 4] = [b"alpha\n", b"bravo\n", b"charlie\n", b"echo\n"];
  for (path, bytes) in FILES.iter().zip(contents) {
    origin.files.push(((*path).to_owned(), bytes.to_vec()));
  }
  origin.dirs.push(DIRS[0].to_owned());
  origin.modes.push((FILES[0].to_owned(), MODES[0]));
  origin
}

/// One edit of a generated work, its paths named by their indices into the shapes above; offsets are taken modulo the
/// file's length at the work's base.
#[derive(Clone, Debug)]
enum Edit {
  /// The file's bytes replaced whole, or the file made with them.
  Write(usize, Vec<u8>),
  /// Bytes overwritten in place from an offset, as many as fit.
  Overwrite(usize, u8, Vec<u8>),
  /// Bytes inserted at an offset.
  Insert(usize, u8, Vec<u8>),
  /// The file removed.
  Remove(usize),
  /// The file renamed to another of `FILES`, over whatever file stands there.
  Rename(usize, usize),
  /// The file's mode set.
  Mode(usize, usize),
  /// An attribute set on the file, or removed when the value is empty.
  Attribute(usize, usize, Vec<u8>),
  /// The root's head attribute set (§4.17.1 item 3).
  Head(Vec<u8>),
  /// A directory made (`true`) or removed.
  Directory(usize, bool),
  /// The hard link made to the file.
  Link(usize),
}

/// What a version holds of the paths a generated work names.
struct At {
  base: Base,
}

impl At {
  /// What `green` held at `version`.
  fn of(green: &Green, version: u64) -> At {
    At {
      base: green.base_at(version),
    }
  }

  /// The file's length, if a file stood at `path`.
  fn len(&self, path: &str) -> Option<u64> {
    self
      .base
      .files
      .iter()
      .find(|(file, _)| file == path)
      .map(|(_, len)| *len)
  }

  /// Whether a directory stood at `path`.
  fn dir(&self, path: &str) -> bool {
    self.base.dirs.iter().any(|dir| dir == path)
  }

  /// Whether anything stood at `path`: a file, a directory or a hard link.
  fn holds(&self, path: &str) -> bool {
    self.len(path).is_some()
      || self.dir(path)
      || self.base.hardlinks.iter().any(|(link, _)| link == path)
  }
}

/// An increment under construction, laid out as the deriver lays one out.
#[derive(Default)]
struct Draft {
  doc: OpsDoc,
  post: Vec<u8>,
}

impl Draft {
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

  /// A content op adding `bytes` (from the post-state) at `at`.
  fn content(&mut self, kind: OpKind, path: &str, at: u64, bytes: &[u8]) {
    let src = self.post.len() as u64;
    self.post.extend_from_slice(bytes);
    self.op(kind, path, (at, bytes.len() as u64, src));
  }

  /// A namespace op on `path` naming `other` (a rename's source, a link's target).
  fn naming(&mut self, kind: OpKind, path: &str, other: &str) {
    let other = self.doc.paths.intern(other);
    self.op(kind, path, (0, 0, other));
  }

  /// A namespace op on `path` alone.
  fn alone(&mut self, kind: OpKind, path: &str) {
    self.op(kind, path, (0, 0, u64::MAX));
  }

  /// `name` set to `value` on `path`, or removed when `value` is empty.
  fn attribute(&mut self, path: &str, name: &str, value: &[u8]) {
    let name = self.doc.paths.intern(name);
    if value.is_empty() {
      self.op(OpKind::RemoveXattr, path, (name, 0, u64::MAX));
      return;
    }
    let src = self.post.len() as u64;
    self.post.extend_from_slice(value);
    self.op(OpKind::SetXattr, path, (name, value.len() as u64, src));
  }
}

impl Edit {
  /// The paths the edit names: a work names each path once.
  fn paths(&self) -> Vec<&'static str> {
    match self {
      Edit::Write(file, _)
      | Edit::Overwrite(file, ..)
      | Edit::Insert(file, ..)
      | Edit::Remove(file)
      | Edit::Mode(file, _)
      | Edit::Attribute(file, ..) => vec![FILES[*file]],
      Edit::Rename(from, to) => vec![FILES[*from], FILES[*to]],
      Edit::Head(_) => vec![ROOT],
      Edit::Directory(dir, _) => vec![DIRS[*dir]],
      Edit::Link(file) => vec![FILES[*file], LINK],
    }
  }

  /// Lays the edit's ops into `draft` against what the base holds; an edit the base gives no meaning to is left out.
  fn emit(&self, draft: &mut Draft, at: &At) {
    match self {
      Edit::Write(file, bytes) => write(draft, at, FILES[*file], bytes),
      Edit::Overwrite(file, offset, bytes) => overwrite(draft, at, FILES[*file], (*offset, bytes)),
      Edit::Insert(file, offset, bytes) => insert(draft, at, FILES[*file], (*offset, bytes)),
      Edit::Head(value) => draft.attribute(ROOT, HEAD_ATTRIBUTE, value),
      Edit::Directory(dir, make) => directory(draft, at, DIRS[*dir], *make),
      _ => self.emit_on_a_file(draft, at),
    }
  }

  /// The edits that need a file at the base: a removal, a rename, a mode, an attribute and a link.
  fn emit_on_a_file(&self, draft: &mut Draft, at: &At) {
    let Some(file) = self.paths().first().copied() else {
      return;
    };
    if at.len(file).is_none() {
      return;
    }
    match self {
      Edit::Remove(_) => draft.alone(OpKind::Unlink, file),
      Edit::Rename(_, to) if FILES[*to] != file && !at.dir(FILES[*to]) => {
        draft.naming(OpKind::Rename, FILES[*to], file);
      }
      Edit::Mode(_, mode) => draft.op(OpKind::SetMode, file, (0, u64::from(MODES[*mode]), u64::MAX)),
      Edit::Attribute(_, name, value) => draft.attribute(file, NAMES[*name], value),
      Edit::Link(_) if !at.holds(LINK) => draft.naming(OpKind::Link, LINK, file),
      _ => {}
    }
  }
}

/// A file's bytes replaced whole (a delete of the base's length and an insert, as the deriver composes a rewrite), or
/// the file made with them.
fn write(draft: &mut Draft, at: &At, path: &str, bytes: &[u8]) {
  match at.len(path) {
    Some(len) => {
      if len > 0 {
        draft.op(OpKind::Delete, path, (0, len, 0));
      }
      if !bytes.is_empty() {
        draft.content(OpKind::Insert, path, 0, bytes);
      }
    }
    None if !at.holds(path) => {
      draft.alone(OpKind::Create, path);
      if !bytes.is_empty() {
        draft.content(OpKind::Insert, path, 0, bytes);
      }
    }
    None => {}
  }
}

/// Bytes overwritten in place, as many as fit before the file's end.
fn overwrite(draft: &mut Draft, at: &At, path: &str, (offset, bytes): (u8, &[u8])) {
  let Some(len) = at.len(path).filter(|len| *len > 0) else {
    return;
  };
  let start = u64::from(offset) % len;
  let fits = usize::try_from(len - start).unwrap_or(usize::MAX).min(bytes.len());
  if fits > 0 {
    draft.content(OpKind::Overwrite, path, start, &bytes[..fits]);
  }
}

/// Bytes inserted at an offset.
fn insert(draft: &mut Draft, at: &At, path: &str, (offset, bytes): (u8, &[u8])) {
  if let Some(len) = at.len(path)
    && !bytes.is_empty()
  {
    draft.content(OpKind::Insert, path, u64::from(offset) % (len + 1), bytes);
  }
}

/// A directory made where nothing stands, or removed where one stands.
fn directory(draft: &mut Draft, at: &At, path: &str, make: bool) {
  if make && !at.holds(path) {
    draft.alone(OpKind::Mkdir, path);
  } else if !make && at.dir(path) {
    draft.alone(OpKind::Rmdir, path);
  }
}

/// The increment `id` of `edits` against `green`'s version `base`: an edit naming a path an earlier one named is left
/// out.
fn work(green: &Green, base: u64, edits: &[Edit], id: u64) -> Increment {
  let at = At::of(green, base);
  let mut draft = Draft::default();
  let mut used: BTreeSet<&str> = BTreeSet::new();
  for edit in edits {
    let paths = edit.paths();
    if paths.iter().any(|path| used.contains(path)) {
      continue;
    }
    used.extend(paths);
    edit.emit(&mut draft, &at);
  }
  let mut identity = [0u8; 32];
  identity[..8].copy_from_slice(&id.to_le_bytes());
  Increment {
    id: identity,
    base,
    doc: draft.doc,
    post_state: draft.post,
    evidence: Vec::new(),
  }
}

/// What a reader sees of a green: its head and the identity of its whole state (every file, directory, mode, link and
/// attribute, A-202).
fn seen(green: &mut Green) -> (u64, [u8; 32]) {
  (green.head(), green.head_identity())
}

/// Bytes from a small alphabet, so two works often write the same bytes at the same span.
fn bytes() -> impl Strategy<Value = Vec<u8>> {
  proptest::collection::vec(b'a'..b'd', 0..5)
}

fn edit() -> impl Strategy<Value = Edit> {
  (0..10u8, 0..FILES.len(), 0..FILES.len(), any::<u8>(), bytes()).prop_map(
    |(kind, file, other, offset, bytes)| match kind {
      0 => Edit::Write(file, bytes),
      1 => Edit::Overwrite(file, offset, bytes),
      2 => Edit::Insert(file, offset, bytes),
      3 => Edit::Remove(file),
      4 => Edit::Rename(file, other),
      5 => Edit::Mode(file, other % MODES.len()),
      6 => Edit::Attribute(file, other % NAMES.len(), bytes),
      7 => Edit::Head(bytes),
      8 => Edit::Directory(other % DIRS.len(), offset.is_multiple_of(2)),
      _ => Edit::Link(file),
    },
  )
}

/// One step of a generated history: a work based `back` versions behind the head, or the work of an earlier step sent
/// again.
#[derive(Clone, Debug)]
enum Step {
  Work { back: u8, edits: Vec<Edit> },
  Again { step: u8 },
}

fn step() -> impl Strategy<Value = Step> {
  (
    0..8u8,
    0..4u8,
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
  behind: u64,
  rebased_behind: u64,
}

impl Census {
  /// Whether every kind was met.
  fn met(&self) -> bool {
    [
      self.accepted,
      self.accepted_before,
      self.conflicts,
      self.behind,
      self.rebased_behind,
    ]
    .iter()
    .all(|&count| count > 0)
  }
}

/// A green's owner and a copy of it that holds nothing of the owner's: rebuilt from the owner's chain entries alone
/// (`Green::apply_recorded`), as a holder's replica is.
struct Pair {
  owner: Green,
  copy: Green,
  made: Vec<Increment>,
}

impl Pair {
  fn new() -> Pair {
    Pair {
      owner: Green::with_origin(origin()),
      copy: Green::with_origin(origin()),
      made: Vec::new(),
    }
  }

  /// The increment step `n` sends: a new work, or an earlier one again.
  fn increment(&self, step: &Step, n: usize) -> Option<Increment> {
    match step {
      Step::Work { back, edits } => {
        let base = self.owner.head().saturating_sub(u64::from(*back));
        Some(work(&self.owner, base, edits, n as u64 + 1))
      }
      Step::Again { step } => self
        .made
        .get(usize::from(*step) % self.made.len().max(1))
        .cloned(),
    }
  }

  /// `inc` computed on the copy and judged on the owner, the two held equal, then committed on the owner and followed
  /// on the copy.
  fn check(&mut self, inc: &Increment, census: &mut Census) -> Result<(), TestCaseError> {
    let behind = inc.base < self.owner.head();
    census.behind += u64::from(behind);
    let before = seen(&mut self.copy);
    let computed = self.copy.compute(inc);
    prop_assert_eq!(seen(&mut self.copy), before, "computing changed the copy");
    self.same_rebase(inc, behind, census)?;
    let judged = self.owner.judge(inc);
    if let Some(held) = same_verdict(judged, &computed, census)? {
      self.commit(inc, held)?;
    }
    Ok(())
  }

  /// The copy's rebase of `inc` is the owner's.
  fn same_rebase(
    &mut self,
    inc: &Increment,
    behind: bool,
    census: &mut Census,
  ) -> Result<(), TestCaseError> {
    let rebased = self.copy.rebase(inc);
    census.rebased_behind += u64::from(behind && matches!(rebased, Rebased::Rebased { .. }));
    prop_assert_eq!(rebased, self.owner.rebase(inc), "the copy rebased otherwise");
    Ok(())
  }

  /// The owner commits `held`, the copy applies what the owner's chain records of it, and both read alike.
  fn commit(&mut self, inc: &Increment, held: Judged) -> Result<(), TestCaseError> {
    let (version, applied) = self
      .owner
      .commit_judged(held)
      .map_err(|moved| TestCaseError::fail(format!("{moved:?}")))?;
    prop_assert_eq!(self.copy.apply_recorded(inc, &applied), version);
    prop_assert_eq!(seen(&mut self.copy), seen(&mut self.owner), "the copy differs");
    Ok(())
  }
}

/// The owner's judgment and the copy's computation agree; the owner's held verdict, when it accepted.
fn same_verdict(
  judged: Judgment,
  computed: &Result<Computed, ComputeRefused>,
  census: &mut Census,
) -> Result<Option<Judged>, TestCaseError> {
  let computed = computed
    .as_ref()
    .map_err(|refused| TestCaseError::fail(format!("the copy refused: {refused:?}")))?;
  match judged {
    Judgment::Accepted(held) => same_accept(held, computed, census).map(Some),
    Judgment::AcceptedBefore { version } => {
      census.accepted_before += 1;
      same_answer(computed.accepted_before(), Some(version)).map(|()| None)
    }
    Judgment::Conflict { windows } => {
      census.conflicts += 1;
      same_answer(computed.windows(), Some(windows.as_slice())).map(|()| None)
    }
    Judgment::BaseUnavailable { base, floor } => Err(TestCaseError::fail(format!(
      "the owner refused base {base} below {floor}"
    ))),
  }
}

/// The copy's answer is the owner's.
fn same_answer<T: PartialEq + core::fmt::Debug>(copy: T, owner: T) -> Result<(), TestCaseError> {
  prop_assert_eq!(copy, owner, "the copy computed another answer");
  Ok(())
}

/// An accepting judgment and the computation agree on the head and on what the chain records.
fn same_accept(
  held: Judged,
  computed: &Computed,
  census: &mut Census,
) -> Result<Judged, TestCaseError> {
  census.accepted += 1;
  same_answer((computed.at(), computed.applied()), (held.at(), Some(held.applied())))?;
  Ok(held)
}

/// Runs `steps` on an owner and its copy: before every verdict the copy computes it, and after every commit the copy
/// follows the owner's chain entry.
fn the_copy_computes_the_owners_verdict(
  steps: &[Step],
  census: &mut Census,
) -> Result<(), TestCaseError> {
  let mut pair = Pair::new();
  for (n, step) in steps.iter().enumerate() {
    let Some(inc) = pair.increment(step, n) else {
      continue;
    };
    pair.check(&inc, census)?;
    pair.made.push(inc);
  }
  Ok(())
}

/// A-658 (the verdict and the rebase pure over immutable versions, computed anywhere). Do: run generated histories of
/// works (files written whole, overwritten, inserted into, removed and renamed; modes, attributes and the root's head
/// set; a directory made or removed; a hard link made), based at the head or behind it, some sent again, on an owner and
/// on a copy rebuilt from the owner's chain entries alone. Expect: before every verdict the copy's computation equals
/// the owner's judgment (the head, what the chain records, the conflict windows, a version accepted before) and the
/// copy's rebase the owner's; computing leaves the copy as it was; after every commit both read the same state; the
/// census shows accepts, works sent again, conflicts, works behind the head and rebases of them were met.
#[test]
fn a_verdict_computed_on_a_copy_of_the_chain_is_the_owners_over_every_history() {
  let census = RefCell::new(Census::default());
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config {
    cases: slates_test_seeds::cases(256),
    ..Config::default()
  }));
  let result = runner.run(&proptest::collection::vec(step(), 1..32), |steps| {
    the_copy_computes_the_owners_verdict(&steps, &mut census.borrow_mut())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  assert!(census.met(), "a kind of answer never met: {census:?}");
}

/// Submits `edits` at the head, accepted: the version made.
fn landed(green: &mut Green, edits: &[Edit], id: u64) -> u64 {
  let inc = work(green, green.head(), edits, id);
  match green.submit(&inc) {
    Outcome::Accepted { version } => version,
    other => panic!("not accepted: {other:?}"),
  }
}

/// A green at version 1, `a` written whole there, that keeps every conflict in its cache.
fn caching_green() -> Green {
  let mut green = Green::with_origin(origin());
  green.set_rejected_budget(usize::MAX);
  landed(&mut green, &[Edit::Write(0, b"one".to_vec())], 1);
  green
}

/// Work 3 of [`caching_green`]: based before `a` was written whole, it writes `a` whole too.
fn conflicting(green: &Green) -> Increment {
  work(green, 0, &[Edit::Write(0, b"three".to_vec())], 3)
}

/// A-658 (computing applies nothing). Do: compute, twice, a work that accepts, then a work that conflicts, on a green
/// that keeps conflicts in its cache. Expect: the accepting work is computed at the head with the same record both
/// times; the conflicting one answers its windows; the green reads as before and its cache holds nothing.
#[test]
fn computing_changes_nothing_and_keeps_no_conflict() {
  let mut green = caching_green();
  let before = seen(&mut green);
  let accepting = work(&green, 1, &[Edit::Write(1, b"two".to_vec())], 2);
  let first = green.compute(&accepting).expect("computed");
  let again = green.compute(&accepting).expect("computed");
  assert_eq!((first.at(), first.applied()), (1, again.applied()));
  assert!(first.applied().is_some_and(|applied| !applied.is_empty()));
  let conflict = green.compute(&conflicting(&green)).expect("computed");
  assert!(conflict.windows().is_some_and(|windows| !windows.is_empty()));
  assert_eq!((seen(&mut green), green.rejected_cache()), (before, (0, 0, 0)));
}

/// A-658 (the owner keeps what it answers). Do: judge, as the owner, the conflicting work of
/// [`computing_changes_nothing_and_keeps_no_conflict`]. Expect: the windows computed for it, kept in the owner's cache.
#[test]
fn the_owners_judgment_keeps_the_conflict_it_answers() {
  let mut green = caching_green();
  let computed = green.compute(&conflicting(&green)).expect("computed");
  let Judgment::Conflict { windows } = green.judge(&conflicting(&green)) else {
    panic!("not a conflict");
  };
  assert_eq!(Some(windows.as_slice()), computed.windows());
  assert_eq!(green.rejected_cache().0, 1);
}

/// An owner at version 2 (`a`, then `b`, written whole) and a copy of it at version 1.
fn owner_and_copy() -> (Green, Green) {
  let mut owner = Green::with_origin(origin());
  let mut copy = Green::with_origin(origin());
  landed(&mut owner, &[Edit::Write(0, b"one".to_vec())], 1);
  landed(&mut copy, &[Edit::Write(0, b"one".to_vec())], 1);
  landed(&mut owner, &[Edit::Write(1, b"two".to_vec())], 2);
  (owner, copy)
}

/// A-658 (typed refusals: a copy behind the work). Do: compute and rebase, on a copy at version 1, a work based at the
/// owner's version 2. Expect: the computation refused `BaseAhead` naming the copy's head, and the rebase answered so,
/// never a rebase onto an older version.
#[test]
fn a_copy_behind_the_works_base_refuses_it_typed() {
  let (owner, copy) = owner_and_copy();
  let ahead = work(&owner, 2, &[Edit::Write(2, b"three".to_vec())], 3);
  let refused = ComputeRefused::BaseAhead { base: 2, head: 1 };
  assert_eq!(copy.compute(&ahead).map(|computed| computed.at()), Err(refused));
  assert_eq!(copy.rebase(&ahead), Rebased::BaseAhead { base: 2, head: 1 });
}

/// A-658 (typed refusals: a base below a checkpoint). Do: compute, on a green restored from the owner's checkpoint at
/// version 2, a work based at version 1. Expect: refused `BaseUnavailable` naming the restored green's floor.
#[test]
fn a_green_restored_past_the_works_base_refuses_it_typed() {
  let (owner, _) = owner_and_copy();
  let checkpoint = owner.checkpoint(2).expect("a checkpoint at the head");
  let restored = Green::restore(origin(), &checkpoint);
  let behind = work(&owner, 1, &[Edit::Write(2, b"four".to_vec())], 4);
  let unavailable = ComputeRefused::BaseUnavailable { base: 1, floor: 2 };
  assert_eq!(restored.compute(&behind).map(|computed| computed.at()), Err(unavailable));
}

/// A-658 (the owner refuses a base it never made). Do: judge, submit and rebase, on the owner at version 1, a work
/// based at version 5. Expect: each answers the base unknown (`BaseUnavailable`; `BaseAhead` for the rebase), never a
/// verdict judged as if the work were based at the head; the green reads as before.
#[test]
fn the_owner_answers_a_base_past_its_head_unknown() {
  let mut green = caching_green();
  let before = seen(&mut green);
  let ahead = Increment {
    base: 5,
    ..work(&green, 1, &[Edit::Write(1, b"two".to_vec())], 5)
  };
  let unknown = |judgment: Judgment| matches!(judgment, Judgment::BaseUnavailable { base: 5, .. });
  assert!(unknown(green.judge(&ahead)), "judged a base past the head");
  assert!(matches!(green.submit(&ahead), Outcome::BaseUnavailable { base: 5, .. }));
  assert_eq!(green.rebase(&ahead), Rebased::BaseAhead { base: 5, head: 1 });
  assert_eq!(seen(&mut green), before);
}
