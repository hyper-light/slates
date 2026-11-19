//! Shallow histories (A-615 part 2; `upload-pack.c` `process_shallow`, `deepen`, `deepen_by_rev_list`, `send_shallow`,
//! `send_unshallow` and `send_shallow_info`; `shallow.c` `get_shallow_commits` and `get_shallow_commits_by_rev_list`;
//! git v2.50.1).
//!
//! - `deepen <n>`: the depth of a commit is 1 at a want and one more per parent step, its least over every path; the
//!   commits at depth `n` are shallow and the ones above them whole (`get_shallow_commits`, whose depth-first walk keeps
//!   each commit's least depth; here a breadth-first walk finds the same). A commit at depth `n` is shallow even when it
//!   has no parent, as git marks it.
//! - `deepen-relative`: the same, measured from the client's shallow commits that the refs reach (`get_reachable_list`),
//!   to depth `n + 1`.
//! - [`super::INFINITE_DEPTH`]: every client shallow commit is made whole, and none is new.
//! - `deepen-since` and `deepen-not`: the commits `rev-list` selects, those the wants reach that are neither older than
//!   the time nor reached from a commit that is (`--max-age` marks such a commit uninteresting, and its parents with it)
//!   nor reached from the `deepen-not` objects; a selected commit with a parent not selected is shallow. Selecting none
//!   is refused, as git dies on it.
//!
//! A shallow commit is sent as `shallow` unless the client already holds it shallow; a client shallow commit the cut
//! makes whole is sent as `unshallow`, and its parents are wanted, itself treated as a have (`extra_edge_obj`). Every
//! client shallow commit and every new one is a graft for the walk. A client that is shallow and asks no deepening gets
//! an empty shallow-info section, as git sends one. A client's shallow line naming an object the store does not hold is
//! left out, as git leaves it out, and one naming an object that is no commit is refused.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::paint::{HAVE, Paint, WANT};
use super::ready::peel;
use super::{Counters, Deepen, Graph, INFINITE_DEPTH, Refusal, Request, ShallowInfo, charge};
use crate::Admission;
use crate::object_name::ObjectId;

/// What a request's shallow lines and deepening make of the walk.
pub(super) struct Cut<N> {
  /// The shallow-info section, when one is due.
  pub(super) info: Option<ShallowInfo>,
  /// The commits whose parents the walk does not follow: the client's shallow commits and the new ones.
  pub(super) grafts: BTreeSet<N>,
  /// Commits wanted besides the request's: the parents of each commit made whole.
  pub(super) wants: Vec<N>,
  /// Commits the client has besides its haves: each commit made whole.
  pub(super) haves: Vec<N>,
}

/// The client's shallow commits the store holds, each once; refused at one that is no commit.
pub(super) fn client<S: Graph>(
  graph: &S,
  ids: &[ObjectId],
  admission: &mut Admission,
) -> Result<Vec<S::Node>, Refusal> {
  let mut seen = BTreeSet::new();
  let mut commits = Vec::new();
  for id in ids {
    let Some(node) = graph.node(id) else {
      continue;
    };
    if graph.commit(node).is_none() {
      return Err(Refusal::ShallowNotCommit { id: *id });
    }
    if seen.insert(node) {
      charge::<(S::Node, S::Node)>(admission)?;
      commits.push(node);
    }
  }
  Ok(commits)
}

/// The cut a request makes.
pub(super) fn cut<S: Graph>(
  graph: &S,
  request: &Request<'_>,
  wants: &[S::Node],
  client: &[S::Node],
  admission: &mut Admission,
  counters: &mut Counters,
) -> Result<Cut<S::Node>, Refusal> {
  let grafts: BTreeSet<S::Node> = client.iter().copied().collect();
  let Some(deepen) = request.deepen else {
    let info = (!client.is_empty()).then(ShallowInfo::default);
    return Ok(Cut {
      info,
      grafts,
      wants: Vec::new(),
      haves: Vec::new(),
    });
  };
  let (shallow, whole) = match deepen {
    Deepen::Depth { depth: 0, .. } => return Err(Refusal::ZeroDepth),
    Deepen::Depth {
      depth: INFINITE_DEPTH,
      ..
    } => (BTreeSet::new(), grafts.clone()),
    Deepen::Depth { depth, relative } => {
      by_depth(graph, wants, client, depth, relative, admission)?
    }
    Deepen::Cut { since, not } => by_rev_list(graph, wants, since, not, admission, counters)?,
  };
  finish(graph, client, grafts, shallow, &whole, admission)
}

/// The cut's sections and grafts, from the new shallow commits and the commits the cut keeps whole.
fn finish<S: Graph>(
  graph: &S,
  client: &[S::Node],
  mut grafts: BTreeSet<S::Node>,
  shallow: BTreeSet<S::Node>,
  whole: &BTreeSet<S::Node>,
  admission: &mut Admission,
) -> Result<Cut<S::Node>, Refusal> {
  let made_whole: Vec<S::Node> = client
    .iter()
    .copied()
    .filter(|node| whole.contains(node))
    .collect();
  let mut wants = Vec::new();
  for parent in made_whole.iter().flat_map(|node| graph.parents(*node)) {
    charge::<S::Node>(admission)?;
    wants.push(parent);
  }
  let mut info = ShallowInfo {
    shallow: shallow
      .iter()
      .filter(|node| !client.contains(node))
      .map(|node| graph.id(*node))
      .collect(),
    unshallow: made_whole.iter().map(|node| graph.id(*node)).collect(),
  };
  info.shallow.sort();
  info.unshallow.sort();
  for node in shallow {
    if grafts.insert(node) {
      charge::<S::Node>(admission)?;
    }
  }
  Ok(Cut {
    info: Some(info),
    grafts,
    wants,
    haves: made_whole,
  })
}

/// `deepen <depth>`, or `deepen-relative`: the shallow commits, and the commits above them, kept whole.
fn by_depth<S: Graph>(
  graph: &S,
  wants: &[S::Node],
  client: &[S::Node],
  depth: u32,
  relative: bool,
  admission: &mut Admission,
) -> Result<(BTreeSet<S::Node>, BTreeSet<S::Node>), Refusal> {
  let (roots, limit) = if relative {
    (
      reached_from_tips(graph, client, admission)?,
      depth.saturating_add(1),
    )
  } else {
    let roots = wants
      .iter()
      .filter_map(|want| peel(graph, *want))
      .filter(|node| graph.commit(*node).is_some())
      .collect();
    (roots, depth)
  };
  let mut depths: BTreeMap<S::Node, u32> = BTreeMap::new();
  let mut queue = VecDeque::new();
  for root in roots {
    if let Entry::Vacant(slot) = depths.entry(root) {
      charge::<(S::Node, u32)>(admission)?;
      slot.insert(1);
      queue.push_back(root);
    }
  }
  let (mut shallow, mut whole) = (BTreeSet::new(), BTreeSet::new());
  while let Some(commit) = queue.pop_front() {
    let at = depths.get(&commit).copied().unwrap_or(limit);
    charge::<S::Node>(admission)?;
    if at >= limit {
      shallow.insert(commit);
      continue;
    }
    whole.insert(commit);
    for parent in graph.parents(commit) {
      if let Entry::Vacant(slot) = depths.entry(parent) {
        charge::<(S::Node, u32)>(admission)?;
        slot.insert(at.saturating_add(1));
        queue.push_back(parent);
      }
    }
  }
  Ok((shallow, whole))
}

/// The client's shallow commits that some ref reaches, searched from the refs down to the least generation among them.
fn reached_from_tips<S: Graph>(
  graph: &S,
  client: &[S::Node],
  admission: &mut Admission,
) -> Result<Vec<S::Node>, Refusal> {
  let floor = client
    .iter()
    .map(|node| super::ready::generation(graph, *node))
    .min()
    .unwrap_or(u64::MAX);
  let mut seen = BTreeSet::new();
  let mut stack: Vec<S::Node> = graph
    .tips()
    .filter_map(|tip| peel(graph, tip))
    .filter(|node| graph.commit(*node).is_some())
    .collect();
  while let Some(commit) = stack.pop() {
    if super::ready::generation(graph, commit) < floor || !seen.insert(commit) {
      continue;
    }
    charge::<S::Node>(admission)?;
    for parent in graph.parents(commit) {
      charge::<S::Node>(admission)?;
      stack.push(parent);
    }
  }
  Ok(
    client
      .iter()
      .copied()
      .filter(|node| seen.contains(node))
      .collect(),
  )
}

/// `deepen-since` and `deepen-not`: the shallow commits, and the selected commits kept whole.
fn by_rev_list<S: Graph>(
  graph: &S,
  wants: &[S::Node],
  since: Option<u64>,
  not: &[ObjectId],
  admission: &mut Admission,
  counters: &mut Counters,
) -> Result<(BTreeSet<S::Node>, BTreeSet<S::Node>), Refusal> {
  let mut paint = Paint::new();
  for want in wants.iter().filter_map(|want| peel(graph, *want)) {
    if graph.commit(want).is_some() {
      paint.paint(graph, want, WANT, admission)?;
    }
  }
  for id in not {
    let node = graph.node(id).ok_or(Refusal::DeepenNotHeld { id: *id })?;
    if let Some(commit) = peel(graph, node).filter(|node| graph.commit(*node).is_some()) {
      paint.paint(graph, commit, HAVE, admission)?;
    }
  }
  let old =
    |commit| since.is_some_and(|time| graph.commit(commit).is_some_and(|row| row.date < time));
  paint.run(graph, &BTreeSet::new(), old, admission, counters)?;
  let selected = paint.chosen();
  admission
    .charge_items::<S::Node>(selected.len())
    .map_err(Refusal::PastAdmission)?;
  if selected.is_empty() {
    return Err(Refusal::NothingSelected);
  }
  let (mut shallow, mut whole) = (BTreeSet::new(), BTreeSet::new());
  for commit in &selected {
    charge::<(S::Node, S::Node)>(admission)?;
    if graph
      .parents(*commit)
      .any(|parent| !selected.contains(&parent))
    {
      shallow.insert(*commit);
    } else {
      whole.insert(*commit);
    }
  }
  Ok((shallow, whole))
}
