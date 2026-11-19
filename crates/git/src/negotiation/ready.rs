//! The wants, the haves the store holds, and whether the server is ready (A-615 part 2; `upload-pack.c` `parse_want`,
//! `got_oid`, `do_got_oid` and `ok_to_give_up`, `commit-reach.c` `can_all_from_reach_with_flag`, git v2.50.1).
//!
//! A have the store holds is common, and is acknowledged once, in the order sent: the document's rule ("The server will
//! respond with "ACK obj-id" for all of the object ids sent as have lines which are common"), where git also leaves out
//! a have it had marked as an earlier have's parent. The server is ready when each want, peeled through its tags,
//! reaches a commit the client has: a common commit, or one of its parents, which git marks `THEY_HAVE`; a want that is
//! no commit, or whose tags name nothing held, has no history to judge and does not hold the answer back. No common
//! have, no readiness. git searches from each want down to the oldest have's commit date, so a parent dated before its
//! child can hide a have; here the search stops below the least generation number among the commits the client has,
//! which no commit below can reach, so it misses nothing.

use std::collections::{BTreeMap, BTreeSet};

use super::{Counters, Graph, Refusal, charge};
use crate::Admission;
use crate::object_name::{Kind, ObjectId};

/// The wants: each the object the store holds under its name, each once, in the order asked. Refused at the first the
/// store does not hold.
pub(super) fn wants<S: Graph>(
  graph: &S,
  ids: &[ObjectId],
  admission: &mut Admission,
) -> Result<Vec<S::Node>, Refusal> {
  let mut seen = BTreeSet::new();
  let mut wants = Vec::new();
  for id in ids {
    let node = graph.node(id).ok_or(Refusal::NotOurs { id: *id })?;
    if seen.insert(node) {
      charge::<(S::Node, S::Node)>(admission)?;
      wants.push(node);
    }
  }
  Ok(wants)
}

/// The haves the store holds, each once, in the order sent: what the acknowledgments acknowledge.
pub(super) fn common<S: Graph>(
  graph: &S,
  ids: &[ObjectId],
  admission: &mut Admission,
) -> Result<Vec<S::Node>, Refusal> {
  let mut seen = BTreeSet::new();
  let mut common = Vec::new();
  for node in ids.iter().filter_map(|id| graph.node(id)) {
    if seen.insert(node) {
      charge::<(S::Node, S::Node)>(admission)?;
      common.push(node);
    }
  }
  Ok(common)
}

/// What `node` names through its tags: itself when it is no tag; `None` when a tag names nothing the store holds. A
/// tag's name hashes what it names, so a chain of tags cannot come back on itself and ends.
pub(super) fn peel<S: Graph>(graph: &S, node: S::Node) -> Option<S::Node> {
  let mut current = node;
  while graph.kind(current) == Kind::Tag {
    current = graph.target(current)?;
  }
  Some(current)
}

/// `node`'s generation number; 0 for an object that is no commit.
pub(super) fn generation<S: Graph>(graph: &S, node: S::Node) -> u64 {
  graph.commit(node).map_or(0, |row| row.generation)
}

/// Whether the server is ready: there is a common have, and every want, but those `answered` by a run of versions,
/// reaches a commit the client has.
pub(super) fn is_ready<S: Graph>(
  graph: &S,
  wants: &[S::Node],
  answered: &[S::Node],
  common: &[S::Node],
  admission: &mut Admission,
  counters: &mut Counters,
) -> Result<bool, Refusal> {
  if common.is_empty() {
    return Ok(false);
  }
  let they_have = they_have(graph, common, admission)?;
  let floor = they_have
    .iter()
    .map(|node| generation(graph, *node))
    .min()
    .unwrap_or(u64::MAX);
  let mut search = Search {
    they_have: &they_have,
    floor,
    known: BTreeMap::new(),
  };
  for want in wants.iter().filter(|want| !answered.contains(want)) {
    let Some(commit) = peel(graph, *want).filter(|node| graph.commit(*node).is_some()) else {
      continue;
    };
    if !search.reaches(graph, commit, admission, counters)? {
      return Ok(false);
    }
  }
  Ok(true)
}

/// The commits the client has as git marks them `THEY_HAVE`: each common have that is a commit, and its parents.
fn they_have<S: Graph>(
  graph: &S,
  common: &[S::Node],
  admission: &mut Admission,
) -> Result<BTreeSet<S::Node>, Refusal> {
  let mut they_have = BTreeSet::new();
  for have in common.iter().filter(|node| graph.commit(**node).is_some()) {
    for node in core::iter::once(*have).chain(graph.parents(*have)) {
      if they_have.insert(node) {
        charge::<S::Node>(admission)?;
      }
    }
  }
  Ok(they_have)
}

/// The search for a commit the client has, below each want, remembering across wants what each commit it finished
/// reaches.
struct Search<'h, N> {
  /// The commits the client has.
  they_have: &'h BTreeSet<N>,
  /// The least generation among them: a commit below it reaches none.
  floor: u64,
  /// What each commit the search met reaches: `true` once a path from it met a commit the client has, `false` while it
  /// is searched and once it is searched whole.
  known: BTreeMap<N, bool>,
}

impl<N: Copy + Ord> Search<'_, N> {
  /// Whether `start` reaches a commit the client has, searched depth first without recursion, the path's commits all
  /// marked as reaching the moment one is found.
  fn reaches<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    start: N,
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<bool, Refusal> {
    if self.they_have.contains(&start) {
      return Ok(true);
    }
    if let Some(known) = self.known.get(&start) {
      return Ok(*known);
    }
    self.visit(start, admission, counters)?;
    let mut path = vec![(start, graph.parents(start))];
    while let Some((_, parents)) = path.last_mut() {
      let Some(parent) = parents.next() else {
        path.pop();
        continue;
      };
      if self.they_have.contains(&parent) || self.known.get(&parent) == Some(&true) {
        for (node, _) in &path {
          self.known.insert(*node, true);
        }
        return Ok(true);
      }
      if self.known.contains_key(&parent) || generation(graph, parent) < self.floor {
        continue;
      }
      self.visit(parent, admission, counters)?;
      let frame = (parent, graph.parents(parent));
      admission
        .charge(crate::to_u64(core::mem::size_of_val(&frame)))
        .map_err(Refusal::PastAdmission)?;
      path.push(frame);
    }
    Ok(false)
  }

  /// Marks `node` as searched, not yet known to reach.
  fn visit(
    &mut self,
    node: N,
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<(), Refusal> {
    charge::<(N, bool)>(admission)?;
    self.known.insert(node, false);
    counters.searched = counters.searched.saturating_add(1);
    Ok(())
  }
}
