//! A walk over commits in decreasing generation order, painting each with the sides that reach it (A-615 part 2; git's
//! `revision.c` `limit_list` with `UNINTERESTING`, and `commit-reach.c` `paint_down_to_common`, ordered by generation).
//!
//! The wants paint [`WANT`], the haves paint [`HAVE`], and each commit taken from the queue passes its paint to its
//! parents. A commit's generation is greater than each parent's, so every commit that reaches a commit is taken before
//! it, and a commit's paint is whole when it is taken. The walk stops when no queued commit is painted [`WANT`] alone:
//! what lies below the rest is reached from a have. A graft (a shallow commit) passes nothing on: its parents are not
//! followed. A commit `cut` on being taken (one older than `deepen-since`) is painted [`HAVE`] then, so its parents are.
//!
//! **What it keeps** (A-745, §4.17.1 "A fetch's answer" rule 1): each paint is a set of positions kept as runs ([`Held`]),
//! so a history pushed in one piece paints a run whatever its length, and the queue is charged at its longest, the
//! frontier of the walk, never once a commit taken.

use std::collections::{BTreeSet, BinaryHeap};

use super::ready::generation;
use super::{Counters, Graph, Held, Peak, Refusal};
use crate::Admission;
use crate::positions::Positions;

/// Paint: reached from a want.
pub(super) const WANT: u8 = 1;

/// Paint: reached from a have (or cut).
pub(super) const HAVE: u8 = 2;

/// Paint: taken from the queue, its paint passed on.
/// Shape: the third paint bit, after `WANT` and `HAVE`.
const TAKEN: u8 = 4;

/// The walk's paint and queue.
pub(super) struct Paint<N> {
  /// The commits painted [`WANT`].
  want: Held,
  /// The commits painted [`HAVE`].
  have: Held,
  /// The commits taken.
  taken: Held,
  /// The commits not yet taken, by generation, highest first.
  queue: BinaryHeap<(u64, N)>,
  /// The queue's cost: its longest.
  queue_peak: Peak,
  /// How many queued commits are painted [`WANT`] alone.
  pending: u64,
}

/// Whether paint `flags` marks a queued commit reached from a want and from no have.
fn is_pending(flags: u8) -> bool {
  flags & (WANT | HAVE | TAKEN) == WANT
}

impl<N: Copy + Ord + Into<u64>> Paint<N> {
  /// An empty walk.
  pub(super) fn new() -> Paint<N> {
    Paint {
      want: Held::default(),
      have: Held::default(),
      taken: Held::default(),
      queue: BinaryHeap::new(),
      queue_peak: Peak::default(),
      pending: 0,
    }
  }

  /// `commit`'s paint: none where it was never painted.
  fn flags(&self, commit: N) -> u8 {
    let mut flags = 0;
    if self.want.contains(commit) {
      flags |= WANT;
    }
    if self.have.contains(commit) {
      flags |= HAVE;
    }
    if self.taken.contains(commit) {
      flags |= TAKEN;
    }
    flags
  }

  /// Adds `sides` to `commit`'s paint, queueing it the first time it is painted.
  pub(super) fn paint<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    commit: N,
    sides: u8,
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    let before = self.flags(commit);
    if before == 0 {
      self.queue.push((generation(graph, commit), commit));
      self
        .queue_peak
        .reach::<(u64, N)>(self.queue.len(), admission)?;
    }
    self.set(commit, before, before | sides, admission)
  }

  /// Sets `commit`'s paint from `before` to `after` (paint is only ever added), keeping the count of pending commits.
  fn set(
    &mut self,
    commit: N,
    before: u8,
    after: u8,
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    let added = after & !before;
    if added & WANT != 0 {
      self.want.insert(commit, admission)?;
    }
    if added & HAVE != 0 {
      self.have.insert(commit, admission)?;
    }
    if added & TAKEN != 0 {
      self.taken.insert(commit, admission)?;
    }
    match (is_pending(before), is_pending(after)) {
      (false, true) => self.pending = self.pending.saturating_add(1),
      (true, false) => self.pending = self.pending.saturating_sub(1),
      _ => {}
    }
    Ok(())
  }

  /// Walks until no queued commit is painted [`WANT`] alone. A commit in `grafts` passes nothing to its parents; a commit
  /// for which `cut` says so is painted [`HAVE`] as it is taken.
  pub(super) fn run<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    grafts: &BTreeSet<N>,
    cut: impl Fn(N) -> bool,
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<(), Refusal> {
    while self.step(graph, grafts, &cut, admission, counters)? {}
    Ok(())
  }

  /// Takes the next commit from the queue and passes its paint on (A-745: the walk a unit at a time, its owner choosing
  /// how many a turn): whether the walk goes on, false once no queued commit is painted [`WANT`] alone.
  pub(super) fn step<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    grafts: &BTreeSet<N>,
    cut: &dyn Fn(N) -> bool,
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<bool, Refusal> {
    while self.pending > 0 {
      let Some((_, commit)) = self.queue.pop() else {
        break;
      };
      let before = self.flags(commit);
      if before & TAKEN != 0 {
        continue;
      }
      let mut after = before | TAKEN;
      if after & WANT != 0 && cut(commit) {
        after |= HAVE;
      }
      self.set(commit, before, after, admission)?;
      counters.walked = counters.walked.saturating_add(1);
      if !grafts.contains(&commit) {
        for parent in graph.parents(commit) {
          self.paint(graph, parent, after & (WANT | HAVE), admission)?;
        }
      }
      return Ok(self.pending > 0);
    }
    Ok(false)
  }

  /// The commits taken painted [`WANT`] alone: reached from a want and from no have, as runs of positions.
  pub(super) fn chosen(&self) -> Positions {
    self
      .taken
      .positions()
      .intersection(self.want.positions())
      .difference(self.have.positions())
  }
}
