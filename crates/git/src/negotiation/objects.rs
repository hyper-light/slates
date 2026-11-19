//! The objects a pack carries one by one (A-615 part 2; git's `list-objects.c` `traverse_commit_list_filtered` and
//! `mark_edges_uninteresting`, `list-objects-filter.c`, and `builtin/pack-objects.c` `add_tag_chain`, without bitmaps;
//! git v2.50.1).
//!
//! 1. **Commits.** The walk paints the wants answered by no run (and the parents of commits a cut made whole) as wanted,
//!    and the common haves, peeled to commits (and the commits a cut made whole, and the wants a run answers, whose
//!    histories the runs carry) as had. The commits to send are those wanted and not had.
//! 2. **Left out.** The trees of the edge commits, each had parent of a commit sent, and the trees and blobs a have
//!    names that is no commit: nothing in them is sent. For a thin pack in a shallow exchange (the client is shallow,
//!    or the cut is new), git's server runs `pack-objects --objects-edge-aggressive`, which leaves out the tree of
//!    every had commit on its command line (`mark_edges_uninteresting`): each have `do_got_oid` lists (`have_obj`: a
//!    commit only when no have before it named it or named it as a parent) and each commit a cut made whole
//!    (`send_unshallow`'s `extra_edge_obj`); so does this. Without a thin pack only the edges are left out. (A-640
//!    part 3 found the aggressive rule missing: git's own `--deepen` fetch, served, sent the shallow tip's blobs again;
//!    git's stateless `deepen 2` without `thin-pack` sent them, which held the rule to thin packs alone.)
//! 3. **Trees and blobs.** Each sent commit's root tree is opened at depth 0, its entries at depth 1, and so on; a tree
//!    or blob is sent when nothing left out holds it and the filter admits it at the least depth it is met at. A tree met
//!    again shallower is opened again, as git's `tree:` filter reopens it; under any other filter each tree opens once.
//! 4. **The wants themselves**, and every tag a want's chain of tags passes through, are sent whatever the filter; a
//!    wanted tree's entries are met at depth 0, as git meets the entries of a tree a client named.
//! 5. **`include-tag`**: each annotated tag under `refs/tags/` whose chain ends at an object the pack carries is sent,
//!    with every tag of its chain, whatever the filter.

use std::collections::{BTreeMap, BTreeSet};

use super::paint::{HAVE, Paint, WANT};
use super::ready::peel;
use super::shallow::Cut;
use super::{Counters, Filter, Graph, Refusal, Request, Run, charge};
use crate::Admission;
use crate::object_name::{Kind, ObjectId};

/// The sides of a pack: what the request wants and has, and the cut its shallow lines make.
pub(super) struct Sides<'s, N> {
  /// The wants.
  pub(super) wants: &'s [N],
  /// The wants a run answers.
  pub(super) one_round: &'s [N],
  /// The common haves.
  pub(super) common: &'s [N],
  /// The cut.
  pub(super) cut: &'s Cut<N>,
}

/// The objects to send one by one, each once, in handle order.
pub(super) fn select<S: Graph>(
  graph: &S,
  request: &Request<'_>,
  sides: &Sides<'_, S::Node>,
  runs: &[Run<S::Green>],
  admission: &mut Admission,
  counters: &mut Counters,
) -> Result<Vec<S::Node>, Refusal> {
  let sent = commits(graph, sides, admission, counters)?;
  let left_out = left_out(graph, request, sides, &sent, admission)?;
  let mut chosen = Chosen {
    filter: request.filter,
    left_out: &left_out,
    objects: BTreeSet::new(),
    opened: BTreeMap::new(),
  };
  for commit in &sent {
    if chosen.admits(Kind::Commit, 0, 0) {
      chosen.take(*commit, admission)?;
    }
    if let Some(row) = graph.commit(*commit) {
      chosen.root(graph, row.tree, admission, counters)?;
    }
  }
  for want in sides
    .wants
    .iter()
    .filter(|want| !sides.one_round.contains(want))
  {
    chosen.want(graph, *want, admission, counters)?;
  }
  if request.include_tag {
    let carried = carried_by_runs(graph, runs, request.filter, admission)?;
    chosen.tags(graph, &carried, admission)?;
  }
  Ok(chosen.objects.into_iter().collect())
}

/// The commits to send: wanted and not had.
fn commits<S: Graph>(
  graph: &S,
  sides: &Sides<'_, S::Node>,
  admission: &mut Admission,
  counters: &mut Counters,
) -> Result<BTreeSet<S::Node>, Refusal> {
  let mut paint = Paint::new();
  let walked = sides
    .wants
    .iter()
    .filter(|want| !sides.one_round.contains(want))
    .chain(&sides.cut.wants);
  for want in walked.filter_map(|node| commit_of(graph, *node)) {
    paint.paint(graph, want, WANT, admission)?;
  }
  let had = sides
    .common
    .iter()
    .chain(&sides.cut.haves)
    .chain(sides.one_round);
  for have in had.filter_map(|node| commit_of(graph, *node)) {
    paint.paint(graph, have, HAVE, admission)?;
  }
  paint.run(graph, &sides.cut.grafts, |_| false, admission, counters)?;
  let sent = paint.chosen();
  admission
    .charge_items::<S::Node>(sent.len())
    .map_err(Refusal::PastAdmission)?;
  Ok(sent)
}

/// The commit `node` names through its tags, if it names one.
fn commit_of<S: Graph>(graph: &S, node: S::Node) -> Option<S::Node> {
  peel(graph, node).filter(|peeled| graph.commit(*peeled).is_some())
}

/// What the client holds of trees and blobs: everything in the edge commits' trees and in the haves that are no
/// commits; and, for a thin pack in a shallow exchange, everything in each had commit's tree on git's command line.
fn left_out<S: Graph>(
  graph: &S,
  request: &Request<'_>,
  sides: &Sides<'_, S::Node>,
  sent: &BTreeSet<S::Node>,
  admission: &mut Admission,
) -> Result<BTreeSet<S::Node>, Refusal> {
  let mut held = BTreeSet::new();
  for commit in sent
    .iter()
    .filter(|commit| !sides.cut.grafts.contains(commit))
  {
    for parent in graph
      .parents(*commit)
      .filter(|parent| !sent.contains(parent))
    {
      if let Some(row) = graph.commit(parent) {
        mark(graph, row.tree, &mut held, admission)?;
      }
    }
  }
  left_out_aggressively(graph, request, sides, &mut held, admission)?;
  for have in sides.common.iter().filter_map(|node| peel(graph, *node)) {
    if graph.commit(have).is_none() {
      mark(graph, have, &mut held, admission)?;
    }
  }
  Ok(held)
}

/// For a thin pack in a shallow exchange, adds to `held` everything in the tree of each had commit git's command line
/// names: each have it lists, and each commit a cut made whole (`--objects-edge-aggressive`).
fn left_out_aggressively<S: Graph>(
  graph: &S,
  request: &Request<'_>,
  sides: &Sides<'_, S::Node>,
  held: &mut BTreeSet<S::Node>,
  admission: &mut Admission,
) -> Result<(), Refusal> {
  if !request.thin || sides.cut.grafts.is_empty() {
    return Ok(());
  }
  let listed = listed_haves(graph, request.haves, admission)?;
  for had in listed.iter().chain(&sides.cut.haves) {
    if let Some(row) = graph.commit(*had) {
      mark(graph, row.tree, held, admission)?;
    }
  }
  Ok(())
}

/// The haves git lists for `pack-objects` (`do_got_oid`'s `have_obj`), those the store holds: in the order sent, a
/// commit only when no have before it named it or named it as a parent, any other object each time.
fn listed_haves<S: Graph>(
  graph: &S,
  haves: &[ObjectId],
  admission: &mut Admission,
) -> Result<Vec<S::Node>, Refusal> {
  let mut marked = BTreeSet::new();
  let mut listed = Vec::new();
  for node in haves.iter().filter_map(|id| graph.node(id)) {
    let counts = if graph.commit(node).is_some() {
      let new = marked.insert(node);
      for parent in graph.parents(node) {
        charge::<S::Node>(admission)?;
        marked.insert(parent);
      }
      new
    } else {
      true
    };
    if counts {
      charge::<(S::Node, S::Node)>(admission)?;
      listed.push(node);
    }
  }
  Ok(listed)
}

/// Adds `root` and everything below it to `held`, without recursion, a tree already held skipped whole.
fn mark<S: Graph>(
  graph: &S,
  root: S::Node,
  held: &mut BTreeSet<S::Node>,
  admission: &mut Admission,
) -> Result<(), Refusal> {
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    if !held.insert(node) {
      continue;
    }
    charge::<S::Node>(admission)?;
    if graph.kind(node) == Kind::Tree {
      for entry in graph.entries(node) {
        charge::<S::Node>(admission)?;
        stack.push(entry);
      }
    }
  }
  Ok(())
}

/// The objects the runs carry that the filter admits, kept only for `include-tag` to ask of.
fn carried_by_runs<S: Graph>(
  graph: &S,
  runs: &[Run<S::Green>],
  filter: Option<&Filter>,
  admission: &mut Admission,
) -> Result<BTreeSet<S::Node>, Refusal> {
  let mut carried = BTreeSet::new();
  for run in runs {
    for version in run.after.saturating_add(1)..=run.through {
      for node in graph.brought(run.green, version) {
        let kind = graph.kind(node);
        let size = if kind == Kind::Blob {
          graph.size(node)
        } else {
          0
        };
        if filter.is_none_or(|filter| filter.admits(kind, size, 0)) && carried.insert(node) {
          charge::<S::Node>(admission)?;
        }
      }
    }
  }
  Ok(carried)
}

/// The objects chosen so far, and what the choice reads.
struct Chosen<'c, N> {
  /// The request's filter.
  filter: Option<&'c Filter>,
  /// What the client holds of trees and blobs.
  left_out: &'c BTreeSet<N>,
  /// The objects chosen.
  objects: BTreeSet<N>,
  /// Each tree opened, and the depth its entries were met at the last time it was, the least so far.
  opened: BTreeMap<N, u64>,
}

impl<N: Copy + Ord> Chosen<'_, N> {
  /// Chooses `node`.
  fn take(&mut self, node: N, admission: &mut Admission) -> Result<(), Refusal> {
    if self.objects.insert(node) {
      charge::<N>(admission)?;
    }
    Ok(())
  }

  /// Whether the filter admits an object of `kind` and `size` met at `depth`.
  fn admits(&self, kind: Kind, size: u64, depth: u64) -> bool {
    self
      .filter
      .is_none_or(|filter| filter.admits(kind, size, depth))
  }

  /// Whether the depth a tree is met at decides what is sent below it: under a `tree:` filter alone.
  fn depth_decides(&self) -> bool {
    self.filter.is_some_and(|filter| !filter.is_per_object())
  }

  /// A sent commit's root tree, at depth 0, and what lies below it.
  fn root<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    tree: N,
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<(), Refusal> {
    if self.left_out.contains(&tree) {
      return Ok(());
    }
    if self.admits(Kind::Tree, 0, 0) {
      self.take(tree, admission)?;
    }
    self.open(graph, tree, 1, admission, counters)
  }

  /// Opens `tree`, its entries met at `depth`, and every tree below it, without recursion.
  fn open<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    tree: N,
    depth: u64,
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<(), Refusal> {
    let mut stack = vec![(tree, depth)];
    while let Some((tree, depth)) = stack.pop() {
      if !self.filter.is_none_or(|filter| filter.reaches(depth)) {
        continue;
      }
      let opened_before = self
        .opened
        .get(&tree)
        .is_some_and(|before| !self.depth_decides() || *before <= depth);
      if opened_before {
        continue;
      }
      if self.opened.insert(tree, depth).is_none() {
        charge::<(N, u64)>(admission)?;
      }
      counters.trees = counters.trees.saturating_add(1);
      for entry in graph
        .entries(tree)
        .filter(|entry| !self.left_out.contains(entry))
      {
        let kind = graph.kind(entry);
        let size = if kind == Kind::Blob {
          graph.size(entry)
        } else {
          0
        };
        if self.admits(kind, size, depth) {
          self.take(entry, admission)?;
        }
        if kind == Kind::Tree {
          charge::<(N, u64)>(admission)?;
          stack.push((entry, depth.saturating_add(1)));
        }
      }
    }
    Ok(())
  }

  /// A want that is no commit, or a tag: the want and every tag of its chain, whatever the filter; a wanted tree's
  /// entries met at depth 0.
  fn want<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    want: N,
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<(), Refusal> {
    let mut current = want;
    loop {
      match graph.kind(current) {
        Kind::Tag => {
          self.take(current, admission)?;
          let Some(next) = graph.target(current) else {
            return Ok(());
          };
          current = next;
        }
        Kind::Tree => {
          self.take(current, admission)?;
          return self.open(graph, current, 0, admission, counters);
        }
        Kind::Blob => return self.take(current, admission),
        Kind::Commit => return Ok(()),
      }
    }
  }

  /// `include-tag`: each annotated tag whose chain ends at an object the pack carries, with its chain.
  fn tags<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    carried: &BTreeSet<N>,
    admission: &mut Admission,
  ) -> Result<(), Refusal> {
    for tag in graph.tags() {
      charge::<N>(admission)?;
      let mut chain = vec![tag];
      let mut current = tag;
      let mut ends = None;
      while graph.kind(current) == Kind::Tag {
        let Some(next) = graph.target(current) else {
          break;
        };
        if graph.kind(next) == Kind::Tag {
          charge::<N>(admission)?;
          chain.push(next);
        } else {
          ends = Some(next);
        }
        current = next;
      }
      let packed = ends.is_some_and(|end| self.objects.contains(&end) || carried.contains(&end));
      if packed {
        for link in chain {
          self.take(link, admission)?;
        }
      }
    }
    Ok(())
  }
}
