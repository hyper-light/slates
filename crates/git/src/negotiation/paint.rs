//! A walk over commits in decreasing generation order, painting each with the sides that reach it (A-615 part 2; git's
//! `revision.c` `limit_list` with `UNINTERESTING`, and `commit-reach.c` `paint_down_to_common`, ordered by generation).
//!
//! The wants paint [`WANT`], the haves paint [`HAVE`], and each commit taken from the queue passes its paint to its
//! parents. A commit's generation is greater than each parent's, so every commit that reaches a commit is taken before
//! it, and a commit's paint is whole when it is taken. The walk stops when no queued commit is painted [`WANT`] alone:
//! what lies below the rest is reached from a have. A graft (a shallow commit) passes nothing on: its parents are not
//! followed. A commit `cut` on being taken (one older than `deepen-since`) is painted [`HAVE`] then, so its parents are.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use super::ready::generation;
use super::{Counters, Graph, Refusal, charge};
use crate::Admission;

/// Paint: reached from a want.
pub(super) const WANT: u8 = 1;

/// Paint: reached from a have (or cut).
pub(super) const HAVE: u8 = 2;

/// Paint: taken from the queue, its paint passed on.
/// Shape: the third paint bit, after `WANT` and `HAVE`.
const TAKEN: u8 = 4;

/// The walk's paint and queue.
pub(super) struct Paint<N> {
  /// Each commit met, and its paint.
  pub(super) flags: BTreeMap<N, u8>,
  /// The commits not yet taken, by generation, highest first.
  queue: BinaryHeap<(u64, N)>,
  /// How many queued commits are painted [`WANT`] alone.
  pending: u64,
}

/// Whether paint `flags` marks a queued commit reached from a want and from no have.
fn is_pending(flags: u8) -> bool {
  flags & (WANT | HAVE | TAKEN) == WANT
}

impl<N: Copy + Ord> Paint<N> {
  /// An empty walk.
  pub(super) fn new() -> Paint<N> {
    Paint {
      flags: BTreeMap::new(),
      queue: BinaryHeap::new(),
      pending: 0,
    }
  }

  /// Adds `sides` to `commit`'s paint, queueing it the first time it is painted.
  pub(super) fn paint<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    commit: N,
    sides: u8,
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    let before = match self.flags.get(&commit) {
      Some(flags) => *flags,
      None => {
        charge::<(N, u8)>(admission)?;
        charge::<(u64, N)>(admission)?;
        self.queue.push((generation(graph, commit), commit));
        0
      }
    };
    self.set(commit, before, before | sides);
    Ok(())
  }

  /// Sets `commit`'s paint from `before` to `after`, keeping the count of pending commits.
  fn set(&mut self, commit: N, before: u8, after: u8) {
    self.flags.insert(commit, after);
    match (is_pending(before), is_pending(after)) {
      (false, true) => self.pending = self.pending.saturating_add(1),
      (true, false) => self.pending = self.pending.saturating_sub(1),
      _ => {}
    }
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
    while self.pending > 0 {
      let Some((_, commit)) = self.queue.pop() else {
        break;
      };
      let before = self.flags.get(&commit).copied().unwrap_or(0);
      if before & TAKEN != 0 {
        continue;
      }
      let mut after = before | TAKEN;
      if after & WANT != 0 && cut(commit) {
        after |= HAVE;
      }
      self.set(commit, before, after);
      counters.walked = counters.walked.saturating_add(1);
      if grafts.contains(&commit) {
        continue;
      }
      for parent in graph.parents(commit) {
        self.paint(graph, parent, after & (WANT | HAVE), admission)?;
      }
    }
    Ok(())
  }

  /// The commits taken painted [`WANT`] alone: reached from a want and from no have, in handle order.
  pub(super) fn chosen(&self) -> BTreeSet<N> {
    self
      .flags
      .iter()
      .filter(|(_, flags)| **flags & (WANT | HAVE | TAKEN) == WANT | TAKEN)
      .map(|(node, _)| *node)
      .collect()
  }
}
