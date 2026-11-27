//! Which named runs a replay of a chain entry reads ([`Increment::replay_reads`], A-887's rule), decided a unit at a time
//! (A-920, main's ruling of 2026-11-27 02:03: a pass over the entry's every change and run in one unit is a defect; a
//! recovery's replay made it whole at each step of an entry's reads).
//!
//! **What.** A run is read unless one change of the entry makes its file whole of it (one operation adds the run's
//! bytes from its first to its last, the file's length after being the run's whatever its base, the run its object
//! whole) and no other change, nor a payload of the document (an attribute's value, a node's, a link's), reads a byte of
//! it. [`ReplayReads`] takes the same decision in units: each run's place in the post-state, a run a unit; each changed
//! file, a unit, its whole run named and the runs its adding operations read marked (the run it makes whole excepted);
//! each payload operation of the document, a unit; then each run's answer, a run a unit. A change's whole run only
//! excuses that change's own operations, so one pass over the changes decides what the whole rule's two did.

use super::{Increment, Op, OpKind, adds_content, mark_runs_read, whole_adding_op};
use crate::chain_entry::Applied;

/// A replay's choice of runs being made, a unit a step ([`ReplayReads::step`]).
#[derive(Debug, Default)]
pub struct ReplayReads {
  /// The part under way.
  part: Part,
  /// The next run, change or operation of the part under way.
  at: usize,
  /// Each run's place in the post-state's address space, from its first byte to past its last.
  spans: Vec<(u64, u64)>,
  /// Where the next run's place begins.
  start: u64,
  /// Whether a change makes each run's file whole of it.
  named: Vec<bool>,
  /// Whether a change or a payload reads a byte of each run; once answered, whether the replay reads it.
  read: Vec<bool>,
}

/// The parts of a [`ReplayReads`], in order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Part {
  /// Each run's place, a run a unit.
  #[default]
  Spans,
  /// Each changed file, a change a unit.
  Changes,
  /// Each payload operation of the document, an operation a unit.
  Payloads,
  /// Each run's answer, a run a unit.
  Answers,
  /// Decided.
  Done,
}

impl ReplayReads {
  /// One unit of the choice for `inc`'s chain entry `applied`: whether it is made.
  pub fn step(&mut self, inc: &Increment, applied: &[Applied]) -> bool {
    match self.part {
      Part::Spans => self.span_one(inc),
      Part::Changes => self.change_one(inc, applied),
      Part::Payloads => self.payload_one(inc),
      Part::Answers => self.answer_one(),
      Part::Done => return true,
    }
    false
  }

  /// The choice made: for each run by its place, whether the replay reads it ([`Increment::replay_reads`]).
  pub fn into_reads(self) -> Vec<bool> {
    self.read
  }

  /// The next part, begun from its first.
  fn next(&mut self, part: Part) {
    (self.part, self.at) = (part, 0);
  }

  /// [`Part::Spans`]: the next run's place.
  fn span_one(&mut self, inc: &Increment) {
    if self.at == 0 {
      self.start = u64::try_from(inc.post_state.len()).unwrap_or(u64::MAX);
    }
    let Some(run) = inc.named.runs.get(self.at) else {
      self.next(Part::Changes);
      return;
    };
    let span = (self.start, self.start.saturating_add(run.len));
    self.start = span.1;
    self.spans.push(span);
    self.named.push(false);
    self.read.push(false);
    self.at = self.at.saturating_add(1);
  }

  /// [`Part::Changes`]: the next changed file's whole run named, and the runs its adding operations read marked.
  fn change_one(&mut self, inc: &Increment, applied: &[Applied]) {
    let Some(change) = applied.get(self.at) else {
      self.next(Part::Payloads);
      return;
    };
    self.at = self.at.saturating_add(1);
    let Some((ops, created)) = changed_file(change) else {
      return;
    };
    let whole = self.whole_run(inc, (ops, created));
    if let Some(named) = whole.and_then(|run| self.named.get_mut(run)) {
      *named = true;
    }
    for op in ops.iter().filter(|op| adds_content(op.kind)) {
      mark_runs_read(&self.spans, op, whole, &mut self.read);
    }
  }

  /// The run a change of `ops` makes its file whole of, its object whole: none where no one operation does.
  fn whole_run(&self, inc: &Increment, (ops, created): (&[Op], bool)) -> Option<usize> {
    let op = whole_adding_op(ops, created)?;
    let run = self
      .spans
      .binary_search_by_key(&op.src, |span| span.0)
      .ok()?;
    inc
      .named
      .runs
      .get(run)
      .is_some_and(|named| {
        op.len == named.len && named.offset == 0 && named.len == named.identity.len
      })
      .then_some(run)
  }

  /// [`Part::Payloads`]: the runs the document's next payload operation reads marked.
  fn payload_one(&mut self, inc: &Increment) {
    let Some(op) = inc.doc.ops.get(self.at) else {
      self.next(Part::Answers);
      return;
    };
    if matches!(op.kind, OpKind::SetXattr | OpKind::Mknod | OpKind::Symlink) {
      mark_runs_read(&self.spans, op, None, &mut self.read);
    }
    self.at = self.at.saturating_add(1);
  }

  /// [`Part::Answers`]: the next run read unless a change makes its file whole of it and nothing else reads it.
  fn answer_one(&mut self) {
    let (Some(read), Some(named)) = (self.read.get_mut(self.at), self.named.get(self.at)) else {
      self.part = Part::Done;
      return;
    };
    *read = !*named || *read;
    self.at = self.at.saturating_add(1);
  }
}

/// A changed file's operations, and whether the change creates it: none for a change of no file's bytes.
fn changed_file(change: &Applied) -> Option<(&[Op], bool)> {
  match change {
    Applied::Spliced(_, ops) => Some((ops.as_slice(), false)),
    Applied::Created(_, ops) => Some((ops.as_slice(), true)),
    Applied::Copied { ops, .. } => Some((ops.as_slice(), false)),
    _ => None,
  }
}
