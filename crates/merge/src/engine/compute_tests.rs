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

use super::{
  ComputeRefused, Computed, Green, Increment, Judged, Judgment, Outcome, PositionRefused, Rebased,
};
use crate::chain_entry::Applied;
use crate::increment::Base;
use crate::ops_doc::{NodeOrigin, NodeRef, Op, OpKind, OpsDoc};
use crate::origin::Origin;
use crate::push::{HEAD_ATTRIBUTE, ROOT};
use crate::verdict::MergeConflictClass;

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

  /// A namespace op on `path` naming `other`: a rename's source, or a link's target, the node there at the work's base
  /// in the document's node table (A-646), which is where the verdict reads a link's node; a link naming a path table
  /// entry names no node and is refused as invalid.
  fn naming(&mut self, kind: OpKind, path: &str, other: &str) {
    let other = self.doc.paths.intern(other);
    let src = if kind == OpKind::Link {
      self.doc.nodes.intern(NodeRef {
        path: other,
        origin: NodeOrigin::Base,
      })
    } else {
      other
    };
    self.op(kind, path, (0, 0, src));
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
      Edit::Mode(_, mode) => draft.op(
        OpKind::SetMode,
        file,
        (0, u64::from(MODES[*mode]), u64::MAX),
      ),
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
  let fits = usize::try_from(len - start)
    .unwrap_or(usize::MAX)
    .min(bytes.len());
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
  increment(draft, base, id)
}

/// The increment `id` of `draft`, based at `base`.
fn increment(draft: Draft, base: u64, id: u64) -> Increment {
  let mut identity = [0u8; 32];
  identity[..8].copy_from_slice(&id.to_le_bytes());
  Increment {
    id: identity,
    base,
    doc: draft.doc,
    post_state: draft.post,
    evidence: Vec::new(),
    named: Default::default(),
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
  (
    0..10u8,
    0..FILES.len(),
    0..FILES.len(),
    any::<u8>(),
    bytes(),
  )
    .prop_map(|(kind, file, other, offset, bytes)| match kind {
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
    })
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
    prop_assert_eq!(
      rebased,
      self.owner.rebase(inc),
      "the copy rebased otherwise"
    );
    Ok(())
  }

  /// The owner commits `held`, the copy applies what the owner's chain records of it, and both read alike.
  fn commit(&mut self, inc: &Increment, held: Judged) -> Result<(), TestCaseError> {
    let (version, applied) = self
      .owner
      .commit_judged(held)
      .map_err(|moved| TestCaseError::fail(format!("{moved:?}")))?;
    prop_assert_eq!(self.copy.apply_recorded(inc, &applied), version);
    prop_assert_eq!(
      seen(&mut self.copy),
      seen(&mut self.owner),
      "the copy differs"
    );
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
    Judgment::CopyRefused { path, case } => Err(TestCaseError::fail(format!(
      "the owner refused the copy to {path}: {case:?}"
    ))),
    Judgment::ContentUnread { identity } => Err(TestCaseError::fail(format!(
      "the owner refused an unread run of {identity:?}"
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
  same_answer(
    (computed.at(), computed.applied()),
    (held.at(), Some(held.applied())),
  )?;
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
  eprintln!("A-658 computed-on-a-copy census: {census:?}");
  assert!(census.met(), "a kind of answer never met: {census:?}");
}

/// Submits `edits` at the head, accepted: the version made.
fn landed(green: &mut Green, edits: &[Edit], id: u64) -> u64 {
  let inc = work(green, green.head(), edits, id);
  match green.submit(&inc) {
    Outcome::Accepted { version } | Outcome::Identical { version } => version,
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
  assert!(
    conflict
      .windows()
      .is_some_and(|windows| !windows.is_empty())
  );
  assert_eq!(
    (seen(&mut green), green.rejected_cache()),
    (before, (0, 0, 0))
  );
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
  assert_eq!(
    copy.compute(&ahead).map(|computed| computed.at()),
    Err(refused)
  );
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
  assert_eq!(
    restored.compute(&behind).map(|computed| computed.at()),
    Err(unavailable)
  );
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
  assert!(matches!(
    green.submit(&ahead),
    Outcome::BaseUnavailable { base: 5, .. }
  ));
  assert_eq!(
    green.rebase(&ahead),
    Rebased::BaseAhead { base: 5, head: 1 }
  );
  assert_eq!(seen(&mut green), before);
}

/// One event of a generated concurrent history (A-658).
#[derive(Clone, Debug)]
enum Event {
  /// A work based `back` versions behind the owner's head, computed on the copy once the copy has followed the owner to
  /// its base and up to `lead` versions further; held until a commit takes it.
  Compute {
    back: u8,
    lead: u8,
    edits: Vec<Edit>,
  },
  /// The work of an earlier event computed on the copy again and held: a work sent again.
  Again { which: u8 },
  /// A work submitted to the serial green and to the owner at once, moving both heads under what is held.
  Land { back: u8, edits: Vec<Edit> },
  /// A held computation committed: by position on the owner, as a submit judges it on the serial green.
  Commit { which: u8 },
}

fn event() -> impl Strategy<Value = Event> {
  (
    0..10u8,
    0..4u8,
    0..4u8,
    proptest::collection::vec(edit(), 1..4),
    any::<u8>(),
  )
    .prop_map(|(kind, back, lead, edits, which)| match kind {
      0..=2 => Event::Compute { back, lead, edits },
      3 => Event::Again { which },
      4..=6 => Event::Land { back, edits },
      _ => Event::Commit { which },
    })
}

/// The kind of an answer, for the census.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
  Accepted,
  Before,
  Conflict,
  Unknown,
}

/// `computed` as the owner takes it from a holder (A-658 part 3a): sent in its wire form and read back by `owner`
/// against `inc`, wherever the owner's head stands now; the form read back writes the same bytes again.
fn carried(owner: &Green, inc: &Increment, computed: &Computed) -> Result<Computed, TestCaseError> {
  let wire = computed.encode(inc);
  let carried = owner
    .computed_from(inc, &wire)
    .map_err(|refused| TestCaseError::fail(format!("the owner could not read it: {refused:?}")))?;
  prop_assert_eq!(
    carried.encode(inc),
    wire,
    "the wire form read back writes otherwise"
  );
  Ok(carried)
}

/// The kind of a computed verdict.
fn computed_answer(computed: &Computed) -> Answer {
  match (computed.accepted_before(), computed.windows()) {
    (Some(_), _) => Answer::Before,
    (None, Some(_)) => Answer::Conflict,
    (None, None) => Answer::Accepted,
  }
}

/// The kind of a judgment.
fn judged_answer(judgment: &Judgment) -> Answer {
  match judgment {
    Judgment::Accepted(_) => Answer::Accepted,
    Judgment::AcceptedBefore { .. } => Answer::Before,
    Judgment::Conflict { .. } => Answer::Conflict,
    Judgment::BaseUnavailable { .. }
    | Judgment::CopyRefused { .. }
    | Judgment::ContentUnread { .. } => Answer::Unknown,
  }
}

/// What the concurrent histories met: the oracle's non-vacuity counters (A-658).
#[derive(Debug, Default)]
struct Moves {
  /// Commits at the head their verdict was computed against.
  at_position: u64,
  /// Commits after the green moved since.
  moved: u64,
  /// Moved commits that judged units again: the increment since met the work.
  increments: u64,
  /// Moved commits that judged nothing again: the increment since met nothing the work read.
  untouched: u64,
  accepted: u64,
  conflicts: u64,
  before: u64,
  /// Commits answered another kind than computed (an identical span changed since, a conflict met since).
  flipped: u64,
}

impl Moves {
  /// Counts one commit: whether the green moved since its verdict was computed, the units it judged again, and the
  /// kinds of answer computed and committed.
  fn count(&mut self, moved: bool, again: u64, (computed, judged): (Answer, Answer)) {
    let counter = match (moved, again) {
      (false, _) => &mut self.at_position,
      (true, 0) => &mut self.untouched,
      (true, _) => &mut self.increments,
    };
    *counter += 1;
    self.moved += u64::from(moved);
    self.flipped += u64::from(computed != judged);
    self.count_answer(judged);
  }

  fn count_answer(&mut self, judged: Answer) {
    match judged {
      Answer::Accepted => self.accepted += 1,
      Answer::Before => self.before += 1,
      Answer::Conflict => self.conflicts += 1,
      Answer::Unknown => {}
    }
  }

  /// Whether every kind the oracle must meet was met.
  fn met(&self) -> bool {
    [
      self.at_position,
      self.moved,
      self.increments,
      self.untouched,
      self.accepted,
      self.conflicts,
      self.before,
      self.flipped,
    ]
    .iter()
    .all(|&count| count > 0)
  }
}

/// Three greens over one origin: one judging every work serially at its head, the owner committing by position the
/// verdicts its copy computed, and the copy, rebuilt from the owner's chain entries alone and lagging behind it.
struct Concurrent {
  serial: Green,
  owner: Green,
  copy: Green,
  /// The owner's chain entries: version `v`'s at `v - 1`.
  chain: Vec<(Increment, Vec<Applied>)>,
  /// The verdicts computed and not yet committed, each with its work.
  held: Vec<(Increment, Computed)>,
  made: Vec<Increment>,
}

impl Concurrent {
  fn new() -> Concurrent {
    let caching = || {
      let mut green = Green::with_origin(origin());
      green.set_rejected_budget(usize::MAX);
      green
    };
    Concurrent {
      serial: caching(),
      owner: caching(),
      copy: Green::with_origin(origin()),
      chain: Vec::new(),
      held: Vec::new(),
      made: Vec::new(),
    }
  }

  /// Runs event `n`.
  fn run(&mut self, n: usize, event: &Event, moves: &mut Moves) -> Result<(), TestCaseError> {
    let id = n as u64 + 1;
    match event {
      Event::Compute { back, lead, edits } => {
        let base = self.owner.head().saturating_sub(u64::from(*back));
        self.compute(work(&self.owner, base, edits, id), *lead)
      }
      Event::Again { which } => {
        let earlier = self
          .made
          .get(usize::from(*which) % self.made.len().max(1))
          .cloned();
        earlier.map_or(Ok(()), |inc| self.compute(inc, 0))
      }
      Event::Land { back, edits } => {
        let base = self.owner.head().saturating_sub(u64::from(*back));
        self.land(work(&self.owner, base, edits, id))
      }
      Event::Commit { which } => self.commit(*which, moves),
    }
  }

  /// The copy brought to `target` by the owner's chain entries, one version at a time, as a holder follows its owner.
  fn follow(&mut self, target: u64) -> Result<(), TestCaseError> {
    while self.copy.head() < target {
      let next = usize::try_from(self.copy.head()).unwrap_or(usize::MAX);
      let Some((inc, applied)) = self.chain.get(next) else {
        return Err(TestCaseError::fail(
          "the chain ends before the owner's head",
        ));
      };
      let version = self.copy.apply_recorded(inc, applied);
      prop_assert_eq!(
        version,
        self.copy.head(),
        "the copy followed to another version"
      );
    }
    Ok(())
  }

  /// `inc` computed on the copy, brought first to the work's base and up to `lead` versions further, and held.
  fn compute(&mut self, inc: Increment, lead: u8) -> Result<(), TestCaseError> {
    let target = self
      .copy
      .head()
      .max(inc.base)
      .saturating_add(u64::from(lead))
      .min(self.owner.head());
    self.follow(target)?;
    let computed = self
      .copy
      .compute(&inc)
      .map_err(|refused| TestCaseError::fail(format!("the copy refused: {refused:?}")))?;
    self.made.push(inc.clone());
    self.held.push((inc, computed));
    Ok(())
  }

  /// `inc` submitted to the serial green and to the owner, answered alike, and the owner's version kept in its chain.
  fn land(&mut self, inc: Increment) -> Result<(), TestCaseError> {
    let serial = self.serial.submit_resolved(&inc);
    let owner = self.owner.submit_resolved(&inc);
    prop_assert_eq!(&serial, &owner, "a submit answered otherwise");
    if let (Outcome::Accepted { .. } | Outcome::Identical { .. }, Some(applied)) = owner {
      self.chain.push((inc.clone(), applied));
    }
    self.made.push(inc);
    Ok(())
  }

  /// A held verdict committed: by position on the owner, serially on the serial green, both answers held equal and
  /// both accepted verdicts committed.
  fn commit(&mut self, which: u8, moves: &mut Moves) -> Result<(), TestCaseError> {
    if self.held.is_empty() {
      return Ok(());
    }
    let (inc, computed) = self.held.remove(usize::from(which) % self.held.len());
    let moved = computed.at() < self.owner.head();
    let (again, answer) = (self.owner.judged_again(), computed_answer(&computed));
    let carried = carried(&self.owner, &inc, &computed)?;
    let by_position = self
      .owner
      .judge_computed(&inc, carried)
      .map_err(|refused| TestCaseError::fail(format!("the owner refused: {refused:?}")))?;
    let serially = self.serial.judge(&inc);
    let answers = (answer, judged_answer(&serially));
    moves.count(moved, self.owner.judged_again() - again, answers);
    if let Some(held) = same_judgment(serially, by_position)? {
      self.commit_both(&inc, held)?;
    }
    Ok(())
  }

  /// Commits both held verdicts, each at its green's head, and keeps the owner's version in its chain.
  fn commit_both(
    &mut self,
    inc: &Increment,
    (serial, owner): (Judged, Judged),
  ) -> Result<(), TestCaseError> {
    let fail = |moved| TestCaseError::fail(format!("{moved:?}"));
    let serial = self.serial.commit_judged(serial).map_err(fail)?;
    let owner = self.owner.commit_judged(owner).map_err(fail)?;
    prop_assert_eq!(&serial, &owner, "committed otherwise");
    self.chain.push((inc.clone(), owner.1));
    Ok(())
  }
}

/// The serial judgment and the judgment by position agree; both held verdicts, when they accept.
fn same_judgment(
  serially: Judgment,
  by_position: Judgment,
) -> Result<Option<(Judged, Judged)>, TestCaseError> {
  match (serially, by_position) {
    (Judgment::Accepted(serial), Judgment::Accepted(owner)) => {
      same_answer(
        (serial.at(), serial.applied()),
        (owner.at(), owner.applied()),
      )?;
      Ok(Some((serial, owner)))
    }
    (Judgment::AcceptedBefore { version }, Judgment::AcceptedBefore { version: again }) => {
      same_answer(version, again).map(|()| None)
    }
    (Judgment::Conflict { windows }, Judgment::Conflict { windows: again }) => {
      same_answer(windows, again).map(|()| None)
    }
    (serially, by_position) => Err(TestCaseError::fail(format!(
      "serially {serially:?}, by position {by_position:?}"
    ))),
  }
}

/// Runs `events` on [`Concurrent`]'s three greens: after every event the serial green and the owner read the same.
fn committed_by_position_is_serial(
  events: &[Event],
  moves: &mut Moves,
) -> Result<(), TestCaseError> {
  let mut greens = Concurrent::new();
  for (n, event) in events.iter().enumerate() {
    greens.run(n, event, moves)?;
    prop_assert_eq!(
      seen(&mut greens.serial),
      seen(&mut greens.owner),
      "event {}: {:?}",
      n,
      event
    );
  }
  Ok(())
}

/// A-658 (committed by position; A-645 "Merges computed anywhere, committed by position"). Do: run generated concurrent
/// histories on three greens: one judging every work serially at its head as a submit does, an owner taking by
/// position the verdicts a copy computed, and the copy, rebuilt from the owner's chain entries alone and lagging behind
/// it. A work is computed on the copy at some head and held; other works land on both greens or commit meanwhile; held
/// verdicts commit in any order; works are sent again. Expect: every verdict taken by position equals the serial verdict
/// at the same head (the version and what the chain records, the windows, a version accepted before), both greens
/// read the same after every event, and the census shows commits at their position and after the green moved, moved
/// commits that judged the increment since again and ones that judged nothing again, accepts, conflicts, works sent
/// again, and commits answered another kind than computed.
#[test]
fn a_verdict_committed_by_position_is_the_serial_verdict_over_every_concurrent_history() {
  let moves = RefCell::new(Moves::default());
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config {
    cases: slates_test_seeds::cases(256),
    ..Config::default()
  }));
  let result = runner.run(&proptest::collection::vec(event(), 1..48), |events| {
    committed_by_position_is_serial(&events, &mut moves.borrow_mut())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let moves = moves.into_inner();
  eprintln!("A-658 committed-by-position census: {moves:?}");
  assert!(moves.met(), "a kind of commit never met: {moves:?}");
}

/// Shape: the versions that overwrite the hot file before the work is computed, each 8 bytes further on.
const HOT_VERSIONS: u64 = 50;
/// Shape: the files the work edits beside the hot file, each a unit of its own.
const COLD_FILES: usize = 100;

/// An overwrite of each `(path, at, bytes)`, as increment `id` based at `base`.
fn overwrites(base: u64, id: u64, edits: &[(String, u64, &[u8])]) -> Increment {
  let mut draft = Draft::default();
  for (path, at, bytes) in edits {
    draft.content(OpKind::Overwrite, path, *at, bytes);
  }
  increment(draft, base, id)
}

/// A green holding a 4 KiB `hot` file, `COLD_FILES` small ones and `other`, then `HOT_VERSIONS` versions, each an
/// overwrite of the next 8 bytes of `hot`.
fn hot_green() -> Green {
  let mut origin = Origin::default();
  origin.files.push(("hot".to_owned(), vec![b'.'; 4096]));
  origin.files.push(("other".to_owned(), b"other".to_vec()));
  for cold in 0..COLD_FILES {
    origin.files.push((format!("c{cold:03}"), b"cold".to_vec()));
  }
  let mut green = Green::with_origin(origin);
  for version in 1..=HOT_VERSIONS {
    let edit = [("hot".to_owned(), (version - 1) * 8, &b"hothotho"[..])];
    let landed = green.submit(&overwrites(version - 1, version, &edit));
    assert_eq!(landed, Outcome::Accepted { version });
  }
  green
}

/// A-658 (a moved green judges again only the increment since, never the whole work: the counter). Do: compute, on a
/// copy at version 50, a work based at version 0 that overwrites the hot file (changed at each of versions 1 to 50)
/// and the 100 cold files; land version 51 on the owner, an overwrite of the hot file and of `other`; hand the owner
/// the computed verdict. Expect: one unit of the work's 102 judged again (the hot file's) through one delta (version
/// 51's), where the serial verdict at version 51 maps the hot file through all 51; both accept, recording the same.
#[test]
fn a_moved_green_judges_again_only_the_increment_since_the_position() {
  let (copy, mut owner, mut serial) = (hot_green(), hot_green(), hot_green());
  let mut edits = vec![("hot".to_owned(), 1000, &b"workwork"[..])];
  edits.extend((0..COLD_FILES).map(|cold| (format!("c{cold:03}"), 0, &b"WARM"[..])));
  let job = overwrites(0, 1000, &edits);
  let computed = copy.compute(&job).expect("computed at version 50");
  assert_eq!(computed.at(), HOT_VERSIONS);
  let moving = [
    ("hot".to_owned(), 2000, &b"newnewne"[..]),
    ("other".to_owned(), 0, &b"OTHER"[..]),
  ];
  for green in [&mut owner, &mut serial] {
    let landed = green.submit(&overwrites(HOT_VERSIONS, 1001, &moving));
    assert_eq!(
      landed,
      Outcome::Accepted {
        version: HOT_VERSIONS + 1
      }
    );
  }
  let counted = |green: &Green| (green.judged_again(), green.deltas_visited());
  let before = counted(&owner);
  let by_position = owner
    .judge_computed(&job, computed)
    .expect("taken by position");
  let after = counted(&owner);
  assert_eq!(
    (after.0 - before.0, after.1 - before.1),
    (1, 1),
    "judged again"
  );
  let serial_before = serial.deltas_visited();
  let serially = serial.judge(&job);
  assert_eq!(serial.deltas_visited() - serial_before, HOT_VERSIONS + 1);
  let both = same_judgment(serially, by_position).expect("the same verdict");
  assert!(both.is_some_and(|(held, _)| held.applied().len() == COLD_FILES + 1));
}

/// A-658 part 3b, A-697 part c (the owner commits a verdict a holder computed, through its wire form). Do: compute, on a
/// copy at version 50, the work of the test above; land version 51 on the owner and on a serial green; hand the owner the
/// verdict through its wire form (`encode`, `computed_from`) to `submit_computed`, and submit the work serially; hand
/// the owner the same verdict again. Expect: both accepted as version 52, recording the same, and reading alike; the
/// second answered version 52 from the owner's record, nothing committed.
#[test]
fn a_verdict_computed_on_a_copy_commits_through_its_wire_form_as_the_serial_submit() {
  let (copy, mut owner, mut serial) = (hot_green(), hot_green(), hot_green());
  let mut edits = vec![("hot".to_owned(), 1000, &b"workwork"[..])];
  edits.extend((0..COLD_FILES).map(|cold| (format!("c{cold:03}"), 0, &b"WARM"[..])));
  let job = overwrites(0, 1000, &edits);
  let form = copy
    .compute(&job)
    .expect("computed at version 50")
    .encode(&job);
  let moving = [("hot".to_owned(), 2000, &b"newnewne"[..])];
  for green in [&mut owner, &mut serial] {
    let landed = green.submit(&overwrites(HOT_VERSIONS, 1001, &moving));
    assert_eq!(
      landed,
      Outcome::Accepted {
        version: HOT_VERSIONS + 1
      }
    );
  }
  let computed = owner.computed_from(&job, &form).expect("read back");
  let by_position = owner
    .submit_computed(&job, computed)
    .expect("taken by position");
  let serially = serial.submit_resolved(&job);
  assert_eq!(
    by_position.0,
    Outcome::Accepted {
      version: HOT_VERSIONS + 2
    }
  );
  assert_eq!(
    by_position, serially,
    "the same outcome and the same record"
  );
  assert_eq!(seen(&mut owner), seen(&mut serial), "the greens read alike");
  let again = owner.computed_from(&job, &form).expect("read back again");
  let before = seen(&mut owner);
  assert_eq!(
    owner.submit_computed(&job, again),
    Ok((
      Outcome::Accepted {
        version: HOT_VERSIONS + 2
      },
      None
    ))
  );
  assert_eq!(seen(&mut owner), before, "nothing committed again");
}

/// A-658 (the owner's typed refusals). Do: hand the owner, at version 1, a verdict computed for another increment, one
/// computed on a green at version 2, and one from a green that had accepted, as version 2, an increment the owner
/// never accepted. Expect: `OtherWork`; `Ahead` naming both heads; `NotRecorded` naming the version; the owner reads
/// as before each time.
#[test]
fn the_owner_refuses_a_verdict_it_cannot_take_typed() {
  let mut owner = caching_green();
  let before = seen(&mut owner);
  let one = work(&owner, 1, &[Edit::Write(1, b"x".to_vec())], 7);
  let another = work(&owner, 1, &[Edit::Write(1, b"y".to_vec())], 8);
  let computed = owner.compute(&one).expect("computed");
  let taken = owner.judge_computed(&another, computed).map(|_| ());
  assert_eq!(taken, Err(PositionRefused::OtherWork));
  let (at_two, _) = owner_and_copy();
  let ahead = at_two.compute(&one).expect("computed at version 2");
  let taken = owner.judge_computed(&one, ahead).map(|_| ());
  assert_eq!(taken, Err(PositionRefused::Ahead { at: 2, head: 1 }));
  assert_eq!(seen(&mut owner), before);
}

/// A-658 (a copy of another green). Do: compute, on a green that accepted increment 2 as its version 2, increment 2
/// again, and hand it to a green at version 2 that never accepted it. Expect: `NotRecorded` naming version 2; the green
/// reads as before.
#[test]
fn a_verdict_from_a_copy_of_another_green_is_refused_typed() {
  let (at_two, _) = owner_and_copy();
  let mut stranger = Green::with_origin(origin());
  landed(&mut stranger, &[Edit::Write(0, b"one".to_vec())], 1);
  let second = work(&stranger, 1, &[Edit::Write(1, b"two".to_vec())], 2);
  landed(&mut stranger, &[Edit::Write(2, b"nine".to_vec())], 9);
  let before = seen(&mut stranger);
  let claimed = at_two.compute(&second).expect("answered");
  assert_eq!(claimed.accepted_before(), Some(2));
  let taken = stranger.judge_computed(&second, claimed).map(|_| ());
  assert_eq!(taken, Err(PositionRefused::NotRecorded { version: 2 }));
  assert_eq!(seen(&mut stranger), before);
}

/// An owner and a serial green at version 1, which links `LINK` to `a`.
fn linked_pair() -> (Green, Green) {
  let (mut owner, mut serial) = (Green::with_origin(origin()), Green::with_origin(origin()));
  landed(&mut owner, &[Edit::Link(0)], 1);
  landed(&mut serial, &[Edit::Link(0)], 1);
  (owner, serial)
}

/// Removes the hard link `LINK` at `green`'s head, as increment `id`: the version made.
fn link_removed(green: &mut Green, id: u64) -> u64 {
  let mut draft = Draft::default();
  draft.alone(OpKind::Unlink, LINK);
  match green.submit(&increment(draft, green.head(), id)) {
    Outcome::Accepted { version } | Outcome::Identical { version } => version,
    other => panic!("not accepted: {other:?}"),
  }
}

/// The serial green's verdict of `inc` at its head, its kind and each window's path and class, once the owner has taken
/// `computed` by position as the same verdict and judged at least one unit again.
fn taken_as_serial(
  (owner, serial): (&mut Green, &mut Green),
  inc: &Increment,
  computed: Computed,
) -> (Answer, Vec<(String, MergeConflictClass)>) {
  let again = owner.judged_again();
  let by_position = owner
    .judge_computed(inc, computed)
    .expect("taken by position");
  assert!(owner.judged_again() > again, "no unit judged again");
  let serially = serial.judge(inc);
  let answer = judged_answer(&serially);
  let classes = match &serially {
    Judgment::Conflict { windows } => windows
      .iter()
      .map(|window| (window.path.clone(), window.class))
      .collect(),
    _ => Vec::new(),
  };
  same_judgment(serially, by_position).expect("the serial verdict");
  (answer, classes)
}

/// A-658 with A-670 (a link made after the position meets a removal): a removal reads the links naming its file, and a
/// link's change is recorded at the link's name, never at the file's. Do: compute, on a copy at version 0, a work based
/// there that removes `a`; land, on the owner and the serial green, version 1 linking `h` to `a`; hand the owner the
/// computed verdict. Expect: the copy accepted the removal; the owner refuses it as the serial green does, delete
/// against modify at `a`, a link made after the work's base keeping the node; the unit judged again.
#[test]
fn a_link_made_after_the_position_meets_the_removal_of_its_file() {
  let copy = Green::with_origin(origin());
  let removing = work(&copy, 0, &[Edit::Remove(0)], 2);
  let computed = copy.compute(&removing).expect("computed at version 0");
  assert_eq!(computed_answer(&computed), Answer::Accepted);
  let (mut owner, mut serial) = linked_pair();
  let refused = (FILES[0].to_owned(), MergeConflictClass::DeleteModify);
  assert_eq!(
    taken_as_serial((&mut owner, &mut serial), &removing, computed),
    (Answer::Conflict, vec![refused])
  );
}

/// A-658 with A-670 (a link made after the position meets a replacement): a rename over a file reads the links naming
/// the file it replaces. Do: compute, on a copy at version 0, a work based there that renames `b` over `a`; land, on
/// the owner and the serial green, version 1 linking `h` to `a`; hand the owner the computed verdict. Expect: the copy
/// accepted the rename; the owner refuses it as the serial green does, rename against rename at `a`.
#[test]
fn a_link_made_after_the_position_meets_a_rename_over_its_file() {
  let copy = Green::with_origin(origin());
  let replacing = work(&copy, 0, &[Edit::Rename(1, 0)], 3);
  let computed = copy.compute(&replacing).expect("computed at version 0");
  assert_eq!(computed_answer(&computed), Answer::Accepted);
  let (mut owner, mut serial) = linked_pair();
  let refused = (FILES[0].to_owned(), MergeConflictClass::RenameRename);
  assert_eq!(
    taken_as_serial((&mut owner, &mut serial), &replacing, computed),
    (Answer::Conflict, vec![refused])
  );
}

/// A-658 with A-670 (a link removed after the position meets a removal of the file it named): the links naming a file
/// at the earlier head count as well as those at the later. Do: compute, on a copy at version 1 (`h` linked to `a`), a
/// work based at version 0 that removes `a`; land, on the owner and the serial green, version 2 removing `h`; hand the
/// owner the computed verdict. Expect: the copy refused the removal, the link made after its base; the owner accepts
/// it as the serial green does, no link naming `a` any longer.
#[test]
fn a_link_removed_after_the_position_meets_the_removal_of_the_file_it_named() {
  let (copy, _) = linked_pair();
  let removing = work(&copy, 0, &[Edit::Remove(0)], 3);
  let computed = copy.compute(&removing).expect("computed at version 1");
  assert_eq!(computed_answer(&computed), Answer::Conflict);
  let (mut owner, mut serial) = linked_pair();
  for green in [&mut owner, &mut serial] {
    assert_eq!(link_removed(green, 2), 2);
  }
  assert_eq!(
    taken_as_serial((&mut owner, &mut serial), &removing, computed),
    (Answer::Accepted, Vec::new())
  );
}

/// A-658 with A-669 (the writes a node takes to a link, and what they fit): an edit declared at a link edits the file
/// whose node the work hands on to the link, which the links naming the files the work removes decide. Do: compute, on
/// a copy at version 1 (`h` linked to `a`), a work based there that writes two bytes at `h`, as the deriver declares
/// writes to a removed file at its base link, and removes `a`; land, on the owner and the serial green, version 2
/// removing `h`; hand the owner the computed verdict. Expect: the copy accepted it, the node and the writes handed on to
/// `h`; the owner refuses it as the serial green does: the writes fit no file at `h` (`TypeChanged`) and the file they
/// edit at `h` is gone (delete against modify), in that order.
#[test]
fn a_link_removed_after_the_position_meets_the_fit_of_the_writes_handed_to_it() {
  let (copy, _) = linked_pair();
  let mut draft = Draft::default();
  draft.content(OpKind::Overwrite, LINK, 0, b"AL");
  draft.alone(OpKind::Unlink, FILES[0]);
  let handing = increment(draft, 1, 3);
  let computed = copy.compute(&handing).expect("computed at version 1");
  assert_eq!(computed_answer(&computed), Answer::Accepted);
  let (mut owner, mut serial) = linked_pair();
  for green in [&mut owner, &mut serial] {
    assert_eq!(link_removed(green, 2), 2);
  }
  let unfit = (LINK.to_owned(), MergeConflictClass::TypeChanged);
  let gone = (LINK.to_owned(), MergeConflictClass::DeleteModify);
  assert_eq!(
    taken_as_serial((&mut owner, &mut serial), &handing, computed),
    (Answer::Conflict, vec![unfit, gone])
  );
}
