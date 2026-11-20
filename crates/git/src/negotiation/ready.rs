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

use super::{Counters, Graph, Peak, Refusal, charge};
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

/// The search for a commit the client has, below each want in turn, a unit at a time (A-745, §4.17.1 "A fetch's
/// answer" rule 3: the walk keeps its place across steps, each lent the graph anew): each unit a parent of the commit
/// on top of the search's path, or the next want begun. It remembers across wants what each commit it finished reaches.
pub(super) struct Readying<N> {
  /// Whether there is no common have, so no readiness.
  none_common: bool,
  /// The commits the client has.
  they_have: BTreeSet<N>,
  /// The least generation among them: a commit below it reaches none.
  floor: u64,
  /// What each commit the search met reaches: `true` once a path from it met a commit the client has, `false` while it
  /// is searched and once it is searched whole.
  known: BTreeMap<N, bool>,
  /// The want searched next, by its index among the wants.
  at: usize,
  /// The path from the want being searched, depth first: each commit with the index of its parent taken next.
  path: Vec<(N, usize)>,
  /// That path's cost: its deepest.
  path_peak: Peak,
}

impl<N: Copy + Ord> Readying<N> {
  /// The search over the client's `common` haves, begun: what the client has marked (bounded by the haves the request
  /// sent and their parents).
  pub(super) fn begin<S: Graph<Node = N>>(
    graph: &S,
    common: &[N],
    admission: &mut Admission,
  ) -> Result<Readying<N>, Refusal> {
    let none_common = common.is_empty();
    let they_have = if none_common {
      BTreeSet::new()
    } else {
      they_have(graph, common, admission)?
    };
    let floor = they_have
      .iter()
      .map(|node| generation(graph, *node))
      .min()
      .unwrap_or(u64::MAX);
    Ok(Readying {
      none_common,
      they_have,
      floor,
      known: BTreeMap::new(),
      at: 0,
      path: Vec::new(),
      path_peak: Peak::default(),
    })
  }

  /// One unit of the search over `wants`, but those `answered` by a run: whether the server is ready once it is
  /// decided, `None` while it is not.
  pub(super) fn unit<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    wants: &[N],
    answered: &[N],
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<Option<bool>, Refusal> {
    if self.none_common {
      return Ok(Some(false));
    }
    let Some((node, next)) = self.path.last().copied() else {
      return self.next_want(graph, wants, answered, admission, counters);
    };
    let Some(parent) = graph.parents(node).nth(next) else {
      self.path.pop();
      // The want's search ended without meeting a commit the client has.
      return Ok(self.path.is_empty().then_some(false));
    };
    if let Some(top) = self.path.last_mut() {
      top.1 = next.saturating_add(1);
    }
    if self.they_have.contains(&parent) || self.known.get(&parent) == Some(&true) {
      for (node, _) in self.path.drain(..) {
        self.known.insert(node, true);
      }
      return Ok(None);
    }
    if self.known.contains_key(&parent) || generation(graph, parent) < self.floor {
      return Ok(None);
    }
    self.visit(parent, admission, counters)?;
    self.path.push((parent, 0));
    self
      .path_peak
      .reach::<(N, usize)>(self.path.len(), admission)?;
    Ok(None)
  }

  /// Begins the next want's search: ready once none is left; not ready at a want known not to reach.
  fn next_want<S: Graph<Node = N>>(
    &mut self,
    graph: &S,
    wants: &[N],
    answered: &[N],
    admission: &mut Admission,
    counters: &mut Counters,
  ) -> Result<Option<bool>, Refusal> {
    while let Some(want) = wants.get(self.at).copied() {
      self.at = self.at.saturating_add(1);
      if answered.contains(&want) {
        continue;
      }
      let Some(start) = peel(graph, want).filter(|node| graph.commit(*node).is_some()) else {
        continue;
      };
      if self.they_have.contains(&start) {
        continue;
      }
      match self.known.get(&start) {
        Some(true) => continue,
        Some(false) => return Ok(Some(false)),
        None => {}
      }
      self.visit(start, admission, counters)?;
      self.path.push((start, 0));
      self
        .path_peak
        .reach::<(N, usize)>(self.path.len(), admission)?;
      return Ok(None);
    }
    Ok(Some(true))
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
