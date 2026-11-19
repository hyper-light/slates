//! A-615 part 2: the negotiation against an oracle. A test graph implements [`Graph`] over generated histories; the
//! oracle computes every answer from full reachability, by the rules the module documents and never by its walks, and
//! the property test holds [`negotiate`] to it on every history, with counters showing which path each want took.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};

use super::{
  Acknowledgments, Answer, CommitRow, Deepen, Filter, Graph, INFINITE_DEPTH, Pack, Refusal,
  Request, Run, ShallowInfo, negotiate,
};
use crate::Admission;
use crate::object_name::{Kind, ObjectFormat, ObjectId};

/// The index of `node`.
fn at(node: u32) -> usize {
  usize::try_from(node).unwrap()
}

/// One object of a test graph.
#[derive(Clone, Debug)]
struct Object {
  kind: Kind,
  parents: Vec<u32>,
  tree: u32,
  date: u64,
  generation: u64,
  entries: Vec<u32>,
  size: u64,
  target: Option<u32>,
}

/// A green's versions: version `v` is `heads[v - 1]`.
#[derive(Clone, Debug)]
struct Chain {
  heads: Vec<u32>,
}

/// A repository for the tests.
#[derive(Clone, Debug, Default)]
struct TestGraph {
  objects: Vec<Object>,
  greens: Vec<Chain>,
  tips: Vec<u32>,
  tags: Vec<u32>,
}

impl TestGraph {
  /// Adds `object`: its handle.
  fn add(&mut self, object: Object) -> u32 {
    self.objects.push(object);
    u32::try_from(self.objects.len() - 1).unwrap()
  }

  fn new_blob(&mut self, size: u64) -> u32 {
    self.add(Object {
      kind: Kind::Blob,
      parents: vec![],
      tree: 0,
      date: 0,
      generation: 0,
      entries: vec![],
      size,
      target: None,
    })
  }

  fn new_tree(&mut self, entries: &[u32]) -> u32 {
    let entries = entries.to_vec();
    self.add(Object {
      kind: Kind::Tree,
      parents: vec![],
      tree: 0,
      date: 0,
      generation: 0,
      entries,
      size: 0,
      target: None,
    })
  }

  fn new_commit(&mut self, parents: &[u32], tree: u32, date: u64) -> u32 {
    let generation = parents
      .iter()
      .map(|parent| self.objects[at(*parent)].generation)
      .max()
      .unwrap_or(0)
      + 1;
    let parents = parents.to_vec();
    self.add(Object {
      kind: Kind::Commit,
      parents,
      tree,
      date,
      generation,
      entries: vec![],
      size: 0,
      target: None,
    })
  }

  fn new_tag(&mut self, target: u32) -> u32 {
    let target = Some(target);
    self.add(Object {
      kind: Kind::Tag,
      parents: vec![],
      tree: 0,
      date: 0,
      generation: 0,
      entries: vec![],
      size: 0,
      target,
    })
  }

  /// A green whose versions are `heads`, in order.
  fn new_green(&mut self, heads: &[u32]) -> u32 {
    self.greens.push(Chain {
      heads: heads.to_vec(),
    });
    u32::try_from(self.greens.len() - 1).unwrap()
  }

  /// Everything `starts` reach: commits through parents and trees, trees through entries, tags through targets.
  fn reach(&self, starts: impl IntoIterator<Item = u32>) -> BTreeSet<u32> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<u32> = starts.into_iter().collect();
    while let Some(node) = stack.pop() {
      if !seen.insert(node) {
        continue;
      }
      let object = &self.objects[at(node)];
      match object.kind {
        Kind::Commit => {
          stack.extend(&object.parents);
          stack.push(object.tree);
        }
        Kind::Tree => stack.extend(&object.entries),
        Kind::Tag => stack.extend(object.target),
        Kind::Blob => {}
      }
    }
    seen
  }

  /// The commits `starts` reach through parents, a graft's parents not followed.
  fn reach_commits(
    &self,
    starts: impl IntoIterator<Item = u32>,
    grafts: &BTreeSet<u32>,
  ) -> BTreeSet<u32> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<u32> = starts.into_iter().collect();
    while let Some(node) = stack.pop() {
      if !seen.insert(node) || grafts.contains(&node) {
        continue;
      }
      stack.extend(&self.objects[at(node)].parents);
    }
    seen
  }

  /// D(version) of `green`.
  fn brought_set(&self, green: u32, version: u64) -> BTreeSet<u32> {
    let heads = &self.greens[at(green)].heads;
    let index = usize::try_from(version).unwrap() - 1;
    let now = self.reach([heads[index]]);
    let before = if index == 0 {
      BTreeSet::new()
    } else {
      self.reach([heads[index - 1]])
    };
    now.difference(&before).copied().collect()
  }
}

impl Graph for TestGraph {
  type Node = u32;
  type Green = u32;

  fn node(&self, id: &ObjectId) -> Option<u32> {
    (0..self.objects.len())
      .find(|index| object_id(*index) == *id)
      .map(|index| u32::try_from(index).unwrap())
  }

  fn id(&self, node: u32) -> ObjectId {
    object_id(at(node))
  }

  fn kind(&self, node: u32) -> Kind {
    self.objects[at(node)].kind
  }

  fn commit(&self, node: u32) -> Option<CommitRow<u32>> {
    let object = &self.objects[at(node)];
    (object.kind == Kind::Commit).then_some(CommitRow {
      generation: object.generation,
      date: object.date,
      tree: object.tree,
    })
  }

  fn parents(&self, commit: u32) -> impl Iterator<Item = u32> {
    self.objects[at(commit)].parents.iter().copied()
  }

  fn entries(&self, tree: u32) -> impl Iterator<Item = u32> {
    self.objects[at(tree)].entries.iter().copied()
  }

  fn size(&self, blob: u32) -> u64 {
    self.objects[at(blob)].size
  }

  fn target(&self, tag: u32) -> Option<u32> {
    self.objects[at(tag)].target
  }

  fn tips(&self) -> impl Iterator<Item = u32> {
    self.tips.iter().copied()
  }

  fn tags(&self) -> impl Iterator<Item = u32> {
    self.tags.iter().copied()
  }

  fn greens_at(&self, head: u32) -> impl Iterator<Item = (u32, u64)> {
    self
      .greens
      .iter()
      .enumerate()
      .filter_map(move |(green, chain)| {
        (chain.heads.last() == Some(&head)).then(|| {
          (
            u32::try_from(green).unwrap(),
            u64::try_from(chain.heads.len()).unwrap(),
          )
        })
      })
  }

  fn versions_at(&self, green: u32, head: u32) -> impl Iterator<Item = u64> {
    self.greens[at(green)]
      .heads
      .iter()
      .enumerate()
      .filter(move |(_, version_head)| **version_head == head)
      .map(|(index, _)| u64::try_from(index + 1).unwrap())
  }

  fn fast_forward(&self, green: u32, version: u64) -> bool {
    let heads = &self.greens[at(green)].heads;
    let index = usize::try_from(version).unwrap() - 1;
    index == 0
      || self
        .reach_commits([heads[index]], &BTreeSet::new())
        .contains(&heads[index - 1])
  }

  fn brought(&self, green: u32, version: u64) -> impl Iterator<Item = u32> {
    self.brought_set(green, version).into_iter()
  }
}

/// The name the test graph gives object `index`: scrambled, so name order is not handle order.
fn object_id(index: usize) -> ObjectId {
  let mut bytes = [0u8; 20];
  let scrambled = u32::try_from(index)
    .unwrap()
    .wrapping_mul(2_654_435_761)
    .to_be_bytes();
  bytes[..4].copy_from_slice(&scrambled);
  bytes[19] = 1;
  ObjectId::from_bytes(ObjectFormat::Sha1, &bytes).unwrap()
}

/// A request's parts, owned.
#[derive(Clone, Debug, Default)]
struct Asks {
  wants: Vec<ObjectId>,
  haves: Vec<ObjectId>,
  done: bool,
  wait_for_done: bool,
  shallow: Vec<ObjectId>,
  deepen: Option<OwnedDeepen>,
  filter: Option<Filter>,
  include_tag: bool,
  thin: bool,
}

/// A deepening, owned.
#[derive(Clone, Debug)]
enum OwnedDeepen {
  Depth(u32, bool),
  Cut(Option<u64>, Vec<ObjectId>),
}

impl Asks {
  fn request(&self) -> Request<'_> {
    Request {
      wants: &self.wants,
      haves: &self.haves,
      done: self.done,
      wait_for_done: self.wait_for_done,
      shallow: &self.shallow,
      deepen: self.deepen.as_ref().map(|deepen| match deepen {
        OwnedDeepen::Depth(depth, relative) => Deepen::Depth {
          depth: *depth,
          relative: *relative,
        },
        OwnedDeepen::Cut(since, not) => Deepen::Cut { since: *since, not },
      }),
      filter: self.filter.as_ref(),
      include_tag: self.include_tag,
      thin: self.thin,
    }
  }
}

/// What the oracle expects, and which wants a run answers.
struct Expected {
  answer: Answer<u32, u32>,
  one_round: u64,
  walk_wants: usize,
}

/// The object `node` names through its tags.
fn peeled(graph: &TestGraph, node: u32) -> Option<u32> {
  let mut current = node;
  while graph.objects[at(current)].kind == Kind::Tag {
    current = graph.objects[at(current)].target?;
  }
  Some(current)
}

/// `node` peeled, when it is a commit.
fn peeled_commit(graph: &TestGraph, node: u32) -> Option<u32> {
  peeled(graph, node).filter(|node| graph.objects[at(*node)].kind == Kind::Commit)
}

/// The nodes `ids` name, each once, in order; those not held left out.
fn held(graph: &TestGraph, ids: &[ObjectId]) -> Vec<u32> {
  let mut nodes = Vec::new();
  for node in ids.iter().filter_map(|id| graph.node(id)) {
    if !nodes.contains(&node) {
      nodes.push(node);
    }
  }
  nodes
}

/// The oracle's run for `want`, by §4.17.2's rule: over the greens it heads at version N, the latest version M a common
/// have headed, when M+1 to N all fast-forwarded; the shortest such run.
fn oracle_run(graph: &TestGraph, want: u32, common: &[u32]) -> Option<Run<u32>> {
  let mut best: Option<Run<u32>> = None;
  for (green, chain) in graph.greens.iter().enumerate() {
    if chain.heads.last() != Some(&want) {
      continue;
    }
    let through = u64::try_from(chain.heads.len()).unwrap();
    let after = (1..=through)
      .filter(|version| common.contains(&chain.heads[usize::try_from(*version).unwrap() - 1]))
      .max();
    let Some(after) = after else { continue };
    let green = u32::try_from(green).unwrap();
    if !(after + 1..=through).all(|version| graph.fast_forward(green, version)) {
      continue;
    }
    let run = Run {
      green,
      after,
      through,
    };
    if best.is_none_or(|best| (through - after, green) < (best.through - best.after, best.green)) {
      best = Some(run);
    }
  }
  best
}

/// The wants the store holds, each once, in order; refused at the first it does not hold.
fn oracle_wants(graph: &TestGraph, ids: &[ObjectId]) -> Result<Vec<u32>, Refusal> {
  let mut wants = Vec::new();
  for id in ids {
    let node = graph.node(id).ok_or(Refusal::NotOurs { id: *id })?;
    if !wants.contains(&node) {
      wants.push(node);
    }
  }
  Ok(wants)
}

/// The client's shallow commits the store holds, each once; refused at one that is no commit.
fn oracle_client(graph: &TestGraph, ids: &[ObjectId]) -> Result<Vec<u32>, Refusal> {
  let mut client = Vec::new();
  for (id, node) in ids
    .iter()
    .filter_map(|id| graph.node(id).map(|node| (id, node)))
  {
    if graph.objects[at(node)].kind != Kind::Commit {
      return Err(Refusal::ShallowNotCommit { id: *id });
    }
    if !client.contains(&node) {
      client.push(node);
    }
  }
  Ok(client)
}

/// The oracle's runs, and the wants they answer: none unless the request leaves the one-round path open.
fn oracle_runs(
  graph: &TestGraph,
  asks: &Asks,
  wants: &[u32],
  common: &[u32],
  client: &[u32],
) -> (Vec<Run<u32>>, Vec<u32>) {
  let open = client.is_empty()
    && asks.deepen.is_none()
    && asks.filter.as_ref().is_none_or(Filter::is_per_object);
  let mut runs = Vec::new();
  let mut answered = Vec::new();
  if open {
    for want in wants
      .iter()
      .filter(|want| graph.objects[at(**want)].kind == Kind::Commit)
    {
      if let Some(run) = oracle_run(graph, *want, common) {
        answered.push(*want);
        if run.after < run.through {
          runs.push(run);
        }
      }
    }
  }
  runs.sort();
  runs.dedup();
  (runs, answered)
}

/// Whether the oracle finds the server ready: no `wait-for-done`, a common have, and every walked want, peeled, a
/// commit that reaches a common commit or one of its parents.
fn oracle_ready(graph: &TestGraph, asks: &Asks, walk_wants: &[u32], common: &[u32]) -> bool {
  let they_have: BTreeSet<u32> = common
    .iter()
    .filter(|node| graph.objects[at(**node)].kind == Kind::Commit)
    .flat_map(|node| std::iter::once(*node).chain(graph.objects[at(*node)].parents.iter().copied()))
    .collect();
  let all_reach = walk_wants.iter().all(|want| {
    peeled_commit(graph, *want).is_none_or(|commit| {
      !graph
        .reach_commits([commit], &BTreeSet::new())
        .is_disjoint(&they_have)
    })
  });
  !asks.wait_for_done && !common.is_empty() && all_reach
}

/// What the oracle expects `asks` to be answered.
fn oracle(graph: &TestGraph, asks: &Asks) -> Result<Expected, Refusal> {
  let wants = oracle_wants(graph, &asks.wants)?;
  if wants.is_empty() && !asks.wait_for_done {
    return Ok(Expected {
      answer: Answer::Nothing,
      one_round: 0,
      walk_wants: 0,
    });
  }
  let common = held(graph, &asks.haves);
  let client = oracle_client(graph, &asks.shallow)?;
  if let Some(filter) = &asks.filter {
    super::filter::check_served(filter).map_err(Refusal::Filter)?;
  }
  let (runs, one_round) = oracle_runs(graph, asks, &wants, &common, &client);
  let walk_wants: Vec<u32> = wants
    .iter()
    .copied()
    .filter(|want| !one_round.contains(want))
    .collect();
  let answered = u64::try_from(one_round.len()).unwrap();
  let acknowledgments = if asks.done {
    None
  } else {
    let ready = oracle_ready(graph, asks, &walk_wants, &common);
    let acknowledgments = Acknowledgments {
      common: common.iter().map(|node| graph.id(*node)).collect(),
      ready,
    };
    if !ready {
      let answer = Answer::Acknowledgments(acknowledgments);
      return Ok(Expected {
        answer,
        one_round: answered,
        walk_wants: walk_wants.len(),
      });
    }
    Some(acknowledgments)
  };
  let cut = oracle_cut(graph, asks, &wants, &client)?;
  let objects = oracle_objects(graph, asks, &walk_wants, &one_round, &common, &cut, &runs);
  let pack = Pack {
    acknowledgments,
    shallow_info: cut.info.clone(),
    runs,
    objects,
  };
  Ok(Expected {
    answer: Answer::Pack(pack),
    one_round: answered,
    walk_wants: walk_wants.len(),
  })
}

/// The oracle's cut.
#[derive(Clone, Debug, Default)]
struct OracleCut {
  info: Option<ShallowInfo>,
  grafts: BTreeSet<u32>,
  wants: Vec<u32>,
  haves: Vec<u32>,
}

/// Each commit's least depth from `roots` (at depth 1), over the whole graph.
fn depths(graph: &TestGraph, roots: &[u32]) -> BTreeMap<u32, u32> {
  let mut depth = BTreeMap::new();
  let mut queue = VecDeque::new();
  for root in roots {
    if depth.insert(*root, 1).is_none() {
      queue.push_back(*root);
    }
  }
  while let Some(commit) = queue.pop_front() {
    let next = depth[&commit] + 1;
    for parent in &graph.objects[at(commit)].parents {
      if !depth.contains_key(parent) {
        depth.insert(*parent, next);
        queue.push_back(*parent);
      }
    }
  }
  depth
}

/// The oracle's cut, from the module's rules over full reachability.
fn oracle_cut(
  graph: &TestGraph,
  asks: &Asks,
  wants: &[u32],
  client: &[u32],
) -> Result<OracleCut, Refusal> {
  let cut = OracleCut {
    grafts: client.iter().copied().collect(),
    ..OracleCut::default()
  };
  let Some(deepen) = &asks.deepen else {
    return Ok(OracleCut {
      info: (!client.is_empty()).then(ShallowInfo::default),
      ..cut
    });
  };
  let (shallow, whole) = match deepen {
    OwnedDeepen::Depth(0, _) => return Err(Refusal::ZeroDepth),
    OwnedDeepen::Depth(depth, _) if *depth == INFINITE_DEPTH => {
      (BTreeSet::new(), client.iter().copied().collect())
    }
    OwnedDeepen::Depth(depth, relative) => oracle_by_depth(graph, wants, client, *depth, *relative),
    OwnedDeepen::Cut(since, not) => oracle_by_rev_list(graph, wants, *since, not)?,
  };
  Ok(oracle_finish(graph, client, cut, shallow, &whole))
}

/// `deepen` or `deepen-relative`: the commits at the depth limit, and those above it.
fn oracle_by_depth(
  graph: &TestGraph,
  wants: &[u32],
  client: &[u32],
  depth: u32,
  relative: bool,
) -> (BTreeSet<u32>, BTreeSet<u32>) {
  let (roots, limit): (Vec<u32>, u32) = if relative {
    let tips: Vec<u32> = graph
      .tips
      .iter()
      .filter_map(|tip| peeled_commit(graph, *tip))
      .collect();
    let reached = graph.reach_commits(tips, &BTreeSet::new());
    (
      client
        .iter()
        .copied()
        .filter(|node| reached.contains(node))
        .collect(),
      depth + 1,
    )
  } else {
    (
      wants
        .iter()
        .filter_map(|want| peeled_commit(graph, *want))
        .collect(),
      depth,
    )
  };
  let depth = depths(graph, &roots);
  (
    depth
      .iter()
      .filter(|(_, at)| **at == limit)
      .map(|(node, _)| *node)
      .collect(),
    depth
      .iter()
      .filter(|(_, at)| **at < limit)
      .map(|(node, _)| *node)
      .collect(),
  )
}

/// `deepen-since` and `deepen-not`: the selected commits with a parent not selected, and the rest selected.
fn oracle_by_rev_list(
  graph: &TestGraph,
  wants: &[u32],
  since: Option<u64>,
  not: &[ObjectId],
) -> Result<(BTreeSet<u32>, BTreeSet<u32>), Refusal> {
  let starts: Vec<u32> = wants
    .iter()
    .filter_map(|want| peeled_commit(graph, *want))
    .collect();
  let reached = graph.reach_commits(starts, &BTreeSet::new());
  let mut excluded_starts: Vec<u32> = Vec::new();
  for id in not {
    let node = graph.node(id).ok_or(Refusal::DeepenNotHeld { id: *id })?;
    excluded_starts.extend(peeled_commit(graph, node));
  }
  excluded_starts.extend(
    reached
      .iter()
      .copied()
      .filter(|node| since.is_some_and(|time| graph.objects[at(*node)].date < time)),
  );
  let excluded = graph.reach_commits(excluded_starts, &BTreeSet::new());
  let selected: BTreeSet<u32> = reached.difference(&excluded).copied().collect();
  if selected.is_empty() {
    return Err(Refusal::NothingSelected);
  }
  let shallow: BTreeSet<u32> = selected
    .iter()
    .copied()
    .filter(|node| {
      graph.objects[at(*node)]
        .parents
        .iter()
        .any(|parent| !selected.contains(parent))
    })
    .collect();
  let whole = selected.difference(&shallow).copied().collect();
  Ok((shallow, whole))
}

/// The cut's sections, grafts and the wants and haves a commit made whole adds.
fn oracle_finish(
  graph: &TestGraph,
  client: &[u32],
  mut cut: OracleCut,
  shallow: BTreeSet<u32>,
  whole: &BTreeSet<u32>,
) -> OracleCut {
  let made_whole: Vec<u32> = client
    .iter()
    .copied()
    .filter(|node| whole.contains(node))
    .collect();
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
  cut.info = Some(info);
  cut.grafts.extend(shallow);
  cut.wants = made_whole
    .iter()
    .flat_map(|node| graph.objects[at(*node)].parents.clone())
    .collect();
  cut.haves = made_whole;
  cut
}

/// Whether `filter` sends an object of `kind` and `size` met at `depth`.
fn admitted(filter: Option<&Filter>, kind: Kind, size: u64, depth: u64) -> bool {
  filter.is_none_or(|filter| filter.admits(kind, size, depth))
}

/// The commits the oracle sends: those the walked wants (and the parents of commits made whole) reach, grafts kept,
/// that the common haves (and the commits made whole, and the wants a run answers) do not.
fn oracle_sent(
  graph: &TestGraph,
  walk_wants: &[u32],
  one_round: &[u32],
  common: &[u32],
  cut: &OracleCut,
) -> BTreeSet<u32> {
  let wanted: Vec<u32> = walk_wants
    .iter()
    .chain(&cut.wants)
    .filter_map(|node| peeled_commit(graph, *node))
    .collect();
  let had: Vec<u32> = common
    .iter()
    .chain(&cut.haves)
    .chain(one_round)
    .filter_map(|node| peeled_commit(graph, *node))
    .collect();
  let had = graph.reach_commits(had, &cut.grafts);
  graph
    .reach_commits(wanted, &cut.grafts)
    .difference(&had)
    .copied()
    .collect()
}

/// What the oracle leaves out: everything in the trees of the sent commits' unsent parents, and in the haves that are
/// no commits; and, for a thin pack in a shallow exchange, everything in the tree of each have git lists and each
/// commit a cut made whole (git's `--objects-edge-aggressive`).
fn oracle_left_out(
  graph: &TestGraph,
  asks: &Asks,
  common: &[u32],
  cut: &OracleCut,
  sent: &BTreeSet<u32>,
) -> BTreeSet<u32> {
  let mut left_out = BTreeSet::new();
  for commit in sent.iter().filter(|commit| !cut.grafts.contains(commit)) {
    for parent in graph.objects[at(*commit)]
      .parents
      .iter()
      .filter(|parent| !sent.contains(parent))
    {
      left_out.extend(graph.reach([graph.objects[at(*parent)].tree]));
    }
  }
  left_out.extend(oracle_aggressive(graph, asks, cut));
  for have in common.iter().filter_map(|node| peeled(graph, *node)) {
    if graph.objects[at(have)].kind != Kind::Commit {
      left_out.extend(graph.reach([have]));
    }
  }
  left_out
}

/// For a thin pack in a shallow exchange, everything in the tree of each have git lists and each commit a cut made
/// whole; nothing otherwise.
fn oracle_aggressive(graph: &TestGraph, asks: &Asks, cut: &OracleCut) -> BTreeSet<u32> {
  if !asks.thin || cut.grafts.is_empty() {
    return BTreeSet::new();
  }
  oracle_listed_haves(graph, &asks.haves)
    .into_iter()
    .chain(cut.haves.iter().copied())
    .filter(|had| graph.objects[at(*had)].kind == Kind::Commit)
    .flat_map(|had| graph.reach([graph.objects[at(had)].tree]))
    .collect()
}

/// The haves git lists for `pack-objects` (`have_obj`): in the order sent, a commit only when no have before it named
/// it or named it as a parent, any other object each time.
fn oracle_listed_haves(graph: &TestGraph, haves: &[ObjectId]) -> Vec<u32> {
  let mut marked = BTreeSet::new();
  let mut listed = Vec::new();
  for node in haves.iter().filter_map(|id| graph.node(id)) {
    let object = &graph.objects[at(node)];
    let counts = if object.kind == Kind::Commit {
      let new = marked.insert(node);
      marked.extend(object.parents.iter().copied());
      new
    } else {
      true
    };
    if counts {
      listed.push(node);
    }
  }
  listed
}

/// A walked want: its chain of tags and, when it ends at a tree or blob, that object, chosen; a wanted tree's entries
/// queued at depth 0.
fn oracle_want(
  graph: &TestGraph,
  want: u32,
  chosen: &mut BTreeSet<u32>,
  roots: &mut Vec<(u32, u64)>,
) {
  let mut current = want;
  loop {
    let object = &graph.objects[at(current)];
    match object.kind {
      Kind::Tag => {
        chosen.insert(current);
        let Some(next) = object.target else { return };
        current = next;
      }
      Kind::Tree => {
        chosen.insert(current);
        roots.extend(object.entries.iter().map(|entry| (*entry, 0)));
        return;
      }
      Kind::Blob => {
        chosen.insert(current);
        return;
      }
      Kind::Commit => return,
    }
  }
}

/// Each tree and blob below `roots` (each with its depth) that nothing left out holds, at the least depth it is met at.
fn oracle_least_depths(
  graph: &TestGraph,
  roots: Vec<(u32, u64)>,
  left_out: &BTreeSet<u32>,
) -> BTreeMap<u32, u64> {
  let mut least: BTreeMap<u32, u64> = BTreeMap::new();
  let mut queue: VecDeque<(u32, u64)> = roots.into_iter().collect();
  while let Some((node, depth)) = queue.pop_front() {
    if left_out.contains(&node) || least.get(&node).is_some_and(|before| *before <= depth) {
      continue;
    }
    least.insert(node, depth);
    let object = &graph.objects[at(node)];
    if object.kind == Kind::Tree {
      queue.extend(object.entries.iter().map(|entry| (*entry, depth + 1)));
    }
  }
  least
}

/// The objects the runs carry that the filter admits.
fn oracle_carried(graph: &TestGraph, filter: Option<&Filter>, runs: &[Run<u32>]) -> BTreeSet<u32> {
  let mut carried = BTreeSet::new();
  for run in runs {
    for version in run.after + 1..=run.through {
      for node in graph.brought_set(run.green, version) {
        let object = &graph.objects[at(node)];
        if admitted(filter, object.kind, object.size, 0) {
          carried.insert(node);
        }
      }
    }
  }
  carried
}

/// `include-tag`: each annotated tag whose chain ends at an object chosen or carried, chosen with its chain.
fn oracle_tags(graph: &TestGraph, carried: &BTreeSet<u32>, chosen: &mut BTreeSet<u32>) {
  for tag in &graph.tags {
    let mut chain = vec![*tag];
    let mut current = *tag;
    let mut end = None;
    while graph.objects[at(current)].kind == Kind::Tag {
      let Some(next) = graph.objects[at(current)].target else {
        break;
      };
      if graph.objects[at(next)].kind == Kind::Tag {
        chain.push(next);
      } else {
        end = Some(next);
      }
      current = next;
    }
    if end.is_some_and(|end| chosen.contains(&end) || carried.contains(&end)) {
      chosen.extend(chain);
    }
  }
}

/// The oracle's objects, from the module's rules over full reachability.
fn oracle_objects(
  graph: &TestGraph,
  asks: &Asks,
  walk_wants: &[u32],
  one_round: &[u32],
  common: &[u32],
  cut: &OracleCut,
  runs: &[Run<u32>],
) -> Vec<u32> {
  let filter = asks.filter.as_ref();
  let sent = oracle_sent(graph, walk_wants, one_round, common, cut);
  let left_out = oracle_left_out(graph, asks, common, cut, &sent);
  let mut chosen: BTreeSet<u32> = sent
    .iter()
    .copied()
    .filter(|_| admitted(filter, Kind::Commit, 0, 0))
    .collect();
  let mut roots: Vec<(u32, u64)> = sent
    .iter()
    .map(|commit| (graph.objects[at(*commit)].tree, 0))
    .collect();
  for want in walk_wants {
    oracle_want(graph, *want, &mut chosen, &mut roots);
  }
  for (node, depth) in oracle_least_depths(graph, roots, &left_out) {
    let object = &graph.objects[at(node)];
    if admitted(filter, object.kind, object.size, depth) {
      chosen.insert(node);
    }
  }
  if asks.include_tag {
    let carried = oracle_carried(graph, filter, runs);
    oracle_tags(graph, &carried, &mut chosen);
  }
  chosen.into_iter().collect()
}

/// The plan of a generated history and request: every choice an index taken modulo what it picks from.
#[derive(Clone, Debug)]
struct Plan {
  blobs: Vec<u8>,
  trees: Vec<Vec<u8>>,
  commits: Vec<(Vec<u8>, u8, u8, bool)>,
  tags: Vec<u8>,
  greens: Vec<(u8, Vec<u8>, u8)>,
  wants: Vec<u8>,
  haves: Vec<u8>,
  shallow: Vec<u8>,
  flags: u8,
  deepen: u8,
  depth: u8,
  since: u8,
  filter: u8,
  filter_value: u8,
  mode: u8,
}

/// A generated plan: up to 6 blobs, 6 trees, 14 commits, 4 tags, 3 greens of up to 6 versions, and a request.
fn any_plan() -> impl Strategy<Value = Plan> {
  (
    (
      proptest::collection::vec(any::<u8>(), 1..6),
      proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..4), 1..6),
      proptest::collection::vec(
        (
          proptest::collection::vec(any::<u8>(), 0..3),
          any::<u8>(),
          any::<u8>(),
          any::<bool>(),
        ),
        1..14,
      ),
      proptest::collection::vec(any::<u8>(), 0..4),
      proptest::collection::vec(
        (
          any::<u8>(),
          proptest::collection::vec(any::<u8>(), 0..6),
          any::<u8>(),
        ),
        1..4,
      ),
    ),
    (
      proptest::collection::vec(any::<u8>(), 0..4),
      proptest::collection::vec(any::<u8>(), 0..5),
      proptest::collection::vec(any::<u8>(), 1..3),
      any::<u8>(),
      any::<u8>(),
      any::<u8>(),
      any::<u8>(),
      any::<u8>(),
      any::<u8>(),
      any::<u8>(),
    ),
  )
    .prop_map(
      |(
        (blobs, trees, commits, tags, greens),
        (wants, haves, shallow, flags, deepen, depth, since, filter, filter_value, mode),
      )| Plan {
        blobs,
        trees,
        commits,
        tags,
        greens,
        wants,
        haves,
        shallow,
        flags,
        deepen,
        depth,
        since,
        filter,
        filter_value,
        mode,
      },
    )
}

/// `choice` taken modulo `options.len()`.
fn pick(options: &[u32], choice: u8) -> u32 {
  options[usize::from(choice) % options.len()]
}

/// The first-parent ancestry of `head`, oldest first, ending at `head`.
fn first_parents(graph: &TestGraph, head: u32) -> Vec<u32> {
  let mut chain = vec![head];
  while let Some(parent) = graph.objects[at(*chain.last().unwrap())].parents.first() {
    chain.push(*parent);
  }
  chain.reverse();
  chain
}

/// The history and request a plan makes. Commits go oldest first, so a parent is always an earlier commit; dates rise
/// but for the skewed ones. A green's versions run along its head's first-parent ancestry, so they fast-forward, but
/// for a commit from elsewhere put among them now and then, a rewrite. Wants lean to the greens' heads and haves to their
/// versions' heads, so the one-round path is met as often as the walk; a want the store does not hold, a shallow client
/// and a shallow line naming no commit each come in some cases, not most.
fn build(plan: &Plan) -> (TestGraph, Asks) {
  let mut graph = TestGraph::default();
  let (blobs, trees, commits) = build_history(&mut graph, plan);
  build_tags(&mut graph, plan, &[&commits, &trees, &blobs]);
  let greens = build_greens(&mut graph, plan, &commits);
  let mut asks = build_asks(&graph, plan, &greens, &commits);
  asks.deepen = build_deepen(&graph, plan, &commits);
  asks.filter = build_filter(plan);
  if plan.mode % 3 == 0 {
    one_round_friendly(&graph, plan, &greens.current, &greens.chains, &mut asks);
  }
  (graph, asks)
}

/// A plan's blobs, trees and commits.
fn build_history(graph: &mut TestGraph, plan: &Plan) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
  let blobs: Vec<u32> = plan
    .blobs
    .iter()
    .map(|size| graph.new_blob(u64::from(*size)))
    .collect();
  let mut trees: Vec<u32> = Vec::new();
  for entries in &plan.trees {
    let pool: Vec<u32> = blobs.iter().chain(&trees).copied().collect();
    let mut chosen: Vec<u32> = entries.iter().map(|choice| pick(&pool, *choice)).collect();
    chosen.sort();
    chosen.dedup();
    trees.push(graph.new_tree(&chosen));
  }
  let mut commits: Vec<u32> = Vec::new();
  for (index, (parents, tree, date, skew)) in plan.commits.iter().enumerate() {
    let mut chosen: Vec<u32> = parents
      .iter()
      .filter(|_| !commits.is_empty())
      .map(|choice| pick(&commits, *choice))
      .collect();
    chosen.sort();
    chosen.dedup();
    let offset = if *skew {
      u64::from(*date)
    } else {
      u64::from(*date % 10)
    };
    let date = u64::try_from(index).unwrap() * 10 + offset;
    commits.push(graph.new_commit(&chosen, pick(&trees, *tree), date));
  }
  (blobs, trees, commits)
}

/// A plan's annotated tags, each naming a commit, tree, blob or earlier tag, all under `refs/tags/`.
fn build_tags(graph: &mut TestGraph, plan: &Plan, pools: &[&[u32]]) {
  let mut tags: Vec<u32> = Vec::new();
  for choice in &plan.tags {
    let pool: Vec<u32> = pools
      .iter()
      .flat_map(|pool| pool.iter())
      .chain(&tags)
      .copied()
      .collect();
    let tag = graph.new_tag(pick(&pool, *choice));
    tags.push(tag);
  }
  graph.tags = tags.clone();
  graph.tips.extend(tags);
}

/// A plan's greens: each one's current head, every version's head, and each one's chain of heads.
struct Greens {
  current: Vec<u32>,
  versions: Vec<u32>,
  chains: Vec<Vec<u32>>,
}

/// A plan's greens, each along its head's first-parent ancestry, a rewrite put among them in some.
fn build_greens(graph: &mut TestGraph, plan: &Plan, commits: &[u32]) -> Greens {
  let mut greens = Greens {
    current: Vec::new(),
    versions: Vec::new(),
    chains: Vec::new(),
  };
  for (head, picks, rewrite) in &plan.greens {
    let head = pick(commits, *head);
    let chain = first_parents(graph, head);
    let mut positions: Vec<usize> = picks
      .iter()
      .map(|choice| usize::from(*choice) % chain.len())
      .collect();
    positions.push(chain.len() - 1);
    positions.sort();
    positions.dedup();
    let mut heads: Vec<u32> = positions.iter().map(|index| chain[*index]).collect();
    if rewrite % 4 == 0 && heads.len() >= 2 {
      let place = usize::from(*rewrite / 4) % (heads.len() - 1);
      heads.insert(place + 1, pick(commits, rewrite.wrapping_add(7)));
    }
    greens.versions.extend(heads.iter().copied());
    greens.chains.push(heads.clone());
    greens.current.push(head);
    graph.new_green(&heads);
    graph.tips.push(head);
  }
  greens
}

/// A plan's request: its wants, haves, shallow lines and flags.
fn build_asks(graph: &TestGraph, plan: &Plan, greens: &Greens, commits: &[u32]) -> Asks {
  let everything: Vec<u32> = (0..u32::try_from(graph.objects.len()).unwrap()).collect();
  let mut wants: Vec<ObjectId> = plan
    .wants
    .iter()
    .enumerate()
    .map(|(index, choice)| {
      graph.id(pick(
        if index % 2 == 0 {
          &greens.current
        } else {
          &everything
        },
        *choice,
      ))
    })
    .collect();
  if plan.flags & 0x41 == 0x41 && !wants.is_empty() {
    wants.push(object_id(9999));
  }
  let mut haves: Vec<ObjectId> = plan
    .haves
    .iter()
    .enumerate()
    .map(|(index, choice)| {
      graph.id(pick(
        if index % 3 == 2 {
          &everything
        } else {
          &greens.versions
        },
        *choice,
      ))
    })
    .collect();
  if plan.flags & 2 != 0 {
    haves.push(object_id(8888));
  }
  let pool = if plan.flags & 0x80 != 0 {
    &everything[..]
  } else {
    commits
  };
  let shallow: Vec<ObjectId> = plan
    .shallow
    .iter()
    .filter(|_| plan.flags & 4 != 0)
    .map(|choice| graph.id(pick(pool, *choice)))
    .collect();
  Asks {
    wants,
    haves,
    done: plan.flags & 8 != 0,
    wait_for_done: plan.flags & 16 != 0,
    shallow,
    deepen: None,
    filter: None,
    include_tag: plan.flags & 32 != 0,
    thin: plan.flags & 64 != 0,
  }
}

/// A plan's deepening, if any: by depth (zero among them), relative (to infinity among them), by time, or by a rev.
fn build_deepen(graph: &TestGraph, plan: &Plan, commits: &[u32]) -> Option<OwnedDeepen> {
  match plan.deepen % 8 {
    0..=3 => None,
    4 => Some(OwnedDeepen::Depth(u32::from(plan.depth % 4), false)),
    5 => {
      let depth = if plan.depth % 5 == 0 {
        INFINITE_DEPTH
      } else {
        u32::from(plan.depth % 4) + 1
      };
      Some(OwnedDeepen::Depth(depth, true))
    }
    6 => Some(OwnedDeepen::Cut(Some(u64::from(plan.since)), vec![])),
    _ => {
      let since = (plan.since % 2 == 0).then_some(u64::from(plan.since));
      Some(OwnedDeepen::Cut(
        since,
        vec![graph.id(pick(commits, plan.depth))],
      ))
    }
  }
}

/// A plan's filter, if any: each kind, and a combination of a limit and a depth.
fn build_filter(plan: &Plan) -> Option<Filter> {
  let value = u64::from(plan.filter_value);
  match plan.filter % 9 {
    0..=2 => None,
    3 => Some(Filter::BlobNone),
    4 => Some(Filter::BlobLimit(value)),
    5 => Some(Filter::TreeDepth(value % 4)),
    6 => Some(Filter::ObjectType(
      [Kind::Commit, Kind::Tree, Kind::Blob, Kind::Tag][usize::from(plan.filter_value % 4)],
    )),
    7 => Some(Filter::Combine(vec![
      Filter::BlobLimit(value),
      Filter::TreeDepth(value % 3 + 1),
    ])),
    _ => Some(Filter::SparseOid(b"main:.sparse".to_vec())),
  }
}

/// A third of the plans: what closes the one-round path cleared (shallow lines, a deepening, a filter by depth or
/// pattern, a want the store does not hold), and a have on an earlier version of the first want's green, when the
/// first want heads one.
fn one_round_friendly(
  graph: &TestGraph,
  plan: &Plan,
  current: &[u32],
  chains: &[Vec<u32>],
  asks: &mut Asks,
) {
  asks.shallow.clear();
  asks.deepen = None;
  asks.filter = (plan.filter % 2 == 1).then_some(Filter::BlobNone);
  asks.wants.retain(|id| graph.node(id).is_some());
  let Some(first) = plan.wants.first() else {
    return;
  };
  let green = usize::from(*first) % current.len();
  let heads = &chains[green];
  if heads.len() >= 2 {
    let earlier = heads[usize::from(plan.mode / 3) % (heads.len() - 1)];
    asks.haves.push(graph.id(earlier));
  }
}

/// What the generated cases met, so a passing oracle test cannot be a vacuous one.
#[derive(Clone, Debug, Default)]
struct Census {
  nothing: u64,
  not_ready: u64,
  ready: u64,
  after_done: u64,
  runs: u64,
  walked: u64,
  runs_and_walk: u64,
  shallow: u64,
  unshallow: u64,
  shallow_section_empty: u64,
  blob_none: u64,
  blob_limit: u64,
  tree_depth: u64,
  object_type: u64,
  combine: u64,
  tags_included: u64,
  not_ours: u64,
  shallow_not_commit: u64,
  nothing_selected: u64,
  zero_depth: u64,
  sparse: u64,
  packs_checked: u64,
  runs_checked: u64,
}

impl Census {
  /// Each count, named, so a test can say which was never met.
  fn counts(&self) -> [(&'static str, u64); 23] {
    [
      ("nothing", self.nothing),
      ("not_ready", self.not_ready),
      ("ready", self.ready),
      ("after_done", self.after_done),
      ("runs", self.runs),
      ("walked", self.walked),
      ("runs_and_walk", self.runs_and_walk),
      ("shallow", self.shallow),
      ("unshallow", self.unshallow),
      ("shallow_section_empty", self.shallow_section_empty),
      ("blob_none", self.blob_none),
      ("blob_limit", self.blob_limit),
      ("tree_depth", self.tree_depth),
      ("object_type", self.object_type),
      ("combine", self.combine),
      ("tags_included", self.tags_included),
      ("not_ours", self.not_ours),
      ("shallow_not_commit", self.shallow_not_commit),
      ("nothing_selected", self.nothing_selected),
      ("zero_depth", self.zero_depth),
      ("sparse", self.sparse),
      ("packs_checked", self.packs_checked),
      ("runs_checked", self.runs_checked),
    ]
  }

  /// Counts what an answer met.
  fn answer(&mut self, graph: &TestGraph, asks: &Asks, answer: &Answer<u32, u32>, walked: u64) {
    let pack = match answer {
      Answer::Nothing => {
        self.nothing += 1;
        return;
      }
      Answer::Acknowledgments(_) => {
        self.not_ready += 1;
        return;
      }
      Answer::Pack(pack) => pack,
    };
    match pack.acknowledgments {
      Some(_) => self.ready += 1,
      None => self.after_done += 1,
    }
    let walk = walked > 0;
    self.runs += u64::from(!pack.runs.is_empty());
    self.walked += u64::from(walk);
    self.runs_and_walk += u64::from(!pack.runs.is_empty() && walk);
    if let Some(info) = &pack.shallow_info {
      self.shallow += u64::from(!info.shallow.is_empty());
      self.unshallow += u64::from(!info.unshallow.is_empty());
      self.shallow_section_empty += u64::from(info.shallow.is_empty() && info.unshallow.is_empty());
    }
    match &asks.filter {
      Some(Filter::BlobNone) => self.blob_none += 1,
      Some(Filter::BlobLimit(_)) => self.blob_limit += 1,
      Some(Filter::TreeDepth(_)) => self.tree_depth += 1,
      Some(Filter::ObjectType(_)) => self.object_type += 1,
      Some(Filter::Combine(_)) => self.combine += 1,
      _ => {}
    }
    let wanted: Vec<u32> = held(graph, &asks.wants);
    let tag_included = pack
      .objects
      .iter()
      .any(|node| graph.objects[at(*node)].kind == Kind::Tag && !wanted.contains(node));
    self.tags_included += u64::from(asks.include_tag && tag_included);
  }

  /// Counts a refusal.
  fn refusal(&mut self, refusal: &Refusal) {
    match refusal {
      Refusal::NotOurs { .. } => self.not_ours += 1,
      Refusal::ShallowNotCommit { .. } => self.shallow_not_commit += 1,
      Refusal::NothingSelected => self.nothing_selected += 1,
      Refusal::ZeroDepth => self.zero_depth += 1,
      Refusal::Filter(_) => self.sparse += 1,
      _ => {}
    }
  }
}

/// Holds one plan's answer to the oracle's, counting what it met.
fn check_against_oracle(plan: &Plan, census: &mut Census) -> Result<(), TestCaseError> {
  let (graph, asks) = build(plan);
  let got = negotiate(&graph, &asks.request(), &mut Admission::new(u64::MAX));
  let expected = oracle(&graph, &asks);
  match (got, expected) {
    (Ok(outcome), Ok(expected)) => {
      prop_assert_eq!(&outcome.answer, &expected.answer);
      prop_assert_eq!(outcome.counters.one_round, expected.one_round);
      let made_whole = match &outcome.answer {
        Answer::Pack(pack) => pack
          .shallow_info
          .as_ref()
          .is_some_and(|info| !info.unshallow.is_empty()),
        _ => false,
      };
      if expected.walk_wants == 0 && !made_whole {
        prop_assert_eq!(outcome.counters.walked, 0);
        prop_assert_eq!(outcome.counters.searched, 0);
      }
      census.answer(&graph, &asks, &outcome.answer, outcome.counters.walked);
    }
    (Err(refusal), Err(expected)) => {
      prop_assert_eq!(refusal, expected);
      census.refusal(&refusal);
    }
    (got, expected) => prop_assert!(
      false,
      "got {:?}, expected {:?}",
      got.map(|outcome| outcome.answer),
      expected.map(|expected| expected.answer)
    ),
  }
  Ok(())
}

/// A-615 part 2: do negotiate generated requests over generated histories (merges, skewed dates, tags of tags, greens
/// with fast-forwards and rewrites, haves on version heads and elsewhere, shallow clients, every deepening and filter),
/// and compute each answer by the oracle from full reachability. Expect: the same answer, refusals included; a want
/// answered in one round exactly where the oracle's rule answers it, and no commit walked or searched when every want is
/// so answered and nothing is made whole. The census shows every answer, path, section, filter and refusal was met.
#[test]
fn every_answer_is_the_oracles() {
  let census = RefCell::new(Census::default());
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config {
    cases: 4096,
    ..Config::default()
  }));
  let result = runner.run(&any_plan(), |plan| {
    check_against_oracle(&plan, &mut census.borrow_mut())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  let unmet: Vec<&str> = census
    .counts()
    .iter()
    .take(21)
    .filter(|(_, count)| *count == 0)
    .map(|(name, _)| *name)
    .collect();
  assert!(unmet.is_empty(), "never met: {unmet:?} in {census:?}");
}

/// Holds one plan's pack to what its client lacks, counting the packs and runs held.
fn check_carried(plan: &Plan, census: &mut Census) -> Result<(), TestCaseError> {
  let (graph, mut asks) = build(plan);
  asks.filter = None;
  asks.shallow.clear();
  asks.deepen = None;
  asks.include_tag = false;
  asks.done = true;
  asks.wants.retain(|id| graph.node(id).is_some());
  let Ok(outcome) = negotiate(&graph, &asks.request(), &mut Admission::new(u64::MAX)) else {
    return Ok(());
  };
  let Answer::Pack(pack) = outcome.answer else {
    return Ok(());
  };
  census.packs_checked += 1;
  let wants = held(&graph, &asks.wants);
  let common = held(&graph, &asks.haves);
  let mut carried: BTreeSet<u32> = pack.objects.iter().copied().collect();
  for run in &pack.runs {
    census.runs_checked += 1;
    let mut union = BTreeSet::new();
    for version in run.after + 1..=run.through {
      union.extend(graph.brought_set(run.green, version));
    }
    let heads = &graph.greens[at(run.green)].heads;
    let head = heads[usize::try_from(run.through).unwrap() - 1];
    let had = heads[usize::try_from(run.after).unwrap() - 1];
    let exact: BTreeSet<u32> = graph
      .reach([head])
      .difference(&graph.reach([had]))
      .copied()
      .collect();
    prop_assert_eq!(&union, &exact);
    carried.extend(union);
  }
  let reached = graph.reach(wants.iter().copied());
  let lacking: BTreeSet<u32> = reached
    .difference(&graph.reach(common.iter().copied()))
    .copied()
    .collect();
  prop_assert!(
    lacking.is_subset(&carried),
    "lacking {:?} carried {:?}",
    lacking,
    carried
  );
  prop_assert!(
    carried.is_subset(&reached),
    "carried {:?} reached {:?}",
    carried,
    reached
  );
  Ok(())
}

/// A-615 part 2 (§4.17.2 item 3): do negotiate generated requests with no filter, no shallow client and no deepening,
/// and gather what the pack carries: its objects and every object of its runs' versions. Expect: everything the wants
/// reach that the common haves do not, and nothing the wants do not reach; and each run carrying exactly what its
/// green's head reaches and the have's version head does not. The census shows packs and runs were held so.
#[test]
fn a_pack_carries_what_the_client_lacks_and_nothing_unwanted() {
  let census = RefCell::new(Census::default());
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config {
    cases: 1024,
    ..Config::default()
  }));
  let result = runner.run(&any_plan(), |plan| {
    check_carried(&plan, &mut census.borrow_mut())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  assert!(
    census.packs_checked > 0 && census.runs_checked > 0,
    "{census:?}"
  );
}

/// A straight history, `count` commits each with its own tree and blob, and one green whose versions are its commits.
fn straight(count: u64) -> (TestGraph, Vec<u32>) {
  let mut graph = TestGraph::default();
  let mut commits = Vec::new();
  for index in 0..count {
    let blob = graph.new_blob(index);
    let tree = graph.new_tree(&[blob]);
    let parents: Vec<u32> = commits.last().copied().into_iter().collect();
    commits.push(graph.new_commit(&parents, tree, index * 10));
  }
  graph.new_green(&commits);
  graph.tips.push(*commits.last().unwrap());
  (graph, commits)
}

/// A-615 part 2 (§4.17.2: "a client one or more versions behind negotiates in one request"): do fetch a green's head
/// with a have that headed version 3 of 6, without `done`. Expect: the have acknowledged and `ready` in the first
/// round; the run of versions 4 to 6; no object sent alone; no commit walked and none searched.
#[test]
fn a_client_versions_behind_is_answered_in_one_round_without_a_walk() {
  let (graph, commits) = straight(6);
  let asks = Asks {
    wants: vec![graph.id(commits[5])],
    haves: vec![graph.id(commits[2])],
    ..Asks::default()
  };
  let outcome = negotiate(&graph, &asks.request(), &mut Admission::new(u64::MAX)).unwrap();
  let Answer::Pack(pack) = &outcome.answer else {
    panic!("{:?}", outcome.answer)
  };
  assert_eq!(
    pack.acknowledgments,
    Some(Acknowledgments {
      common: vec![graph.id(commits[2])],
      ready: true
    })
  );
  assert_eq!(
    pack.runs,
    [Run {
      green: 0,
      after: 3,
      through: 6
    }]
  );
  assert!(pack.objects.is_empty());
  assert_eq!(outcome.counters.one_round, 1);
  assert_eq!(outcome.counters.walked, 0);
  assert_eq!(outcome.counters.searched, 0);
}

/// A-615 part 2 (§4.17.2 item 3: "The union rule holds within each run of fast-forwards"): do the same fetch when a
/// version between rewrote the branch. Expect: no run; the walk sends the commits the have does not reach, their trees
/// and blobs, and counts its commits.
#[test]
fn a_rewrite_between_takes_the_walk() {
  let (mut graph, commits) = straight(4);
  let blob = graph.new_blob(99);
  let tree = graph.new_tree(&[blob]);
  let side = graph.new_commit(&[commits[0]], tree, 100);
  let top = graph.new_commit(&[side], tree, 110);
  graph.greens[0].heads.extend([side, top]);
  let asks = Asks {
    wants: vec![graph.id(top)],
    haves: vec![graph.id(commits[3])],
    done: true,
    ..Asks::default()
  };
  let outcome = negotiate(&graph, &asks.request(), &mut Admission::new(u64::MAX)).unwrap();
  let Answer::Pack(pack) = &outcome.answer else {
    panic!("{:?}", outcome.answer)
  };
  assert!(pack.runs.is_empty());
  assert_eq!(pack.objects, [blob, tree, side, top]);
  assert_eq!(outcome.counters.one_round, 0);
  assert!(outcome.counters.walked >= 2);
}

/// A-615 part 2 (`wait-for-done`, `done`): do fetch with `wait-for-done` and a have the want reaches, then with `done`.
/// Expect: the acknowledgments alone, not ready, under `wait-for-done`; and after `done`, no acknowledgments section and
/// a pack.
#[test]
fn wait_for_done_never_readies_and_done_omits_the_acknowledgments() {
  let (graph, commits) = straight(3);
  let waiting = Asks {
    wants: vec![graph.id(commits[2])],
    haves: vec![graph.id(commits[0])],
    wait_for_done: true,
    ..Asks::default()
  };
  let outcome = negotiate(&graph, &waiting.request(), &mut Admission::new(u64::MAX)).unwrap();
  assert_eq!(
    outcome.answer,
    Answer::Acknowledgments(Acknowledgments {
      common: vec![graph.id(commits[0])],
      ready: false
    })
  );
  let done = Asks {
    done: true,
    ..waiting
  };
  let outcome = negotiate(&graph, &done.request(), &mut Admission::new(u64::MAX)).unwrap();
  let Answer::Pack(pack) = outcome.answer else {
    panic!()
  };
  assert_eq!(pack.acknowledgments, None);
  assert_eq!(
    pack.runs,
    [Run {
      green: 0,
      after: 1,
      through: 3
    }]
  );
}

/// A-615 part 2 (`deepen 1`, git's fixture `fetch-shallow`): do clone a straight history at depth 1. Expect: the head
/// alone shallow; its commit, tree and blob the only objects.
#[test]
fn a_clone_at_depth_one_sends_the_head_alone_and_names_it_shallow() {
  let (graph, commits) = straight(4);
  let asks = Asks {
    wants: vec![graph.id(commits[3])],
    done: true,
    deepen: Some(OwnedDeepen::Depth(1, false)),
    ..Asks::default()
  };
  let outcome = negotiate(&graph, &asks.request(), &mut Admission::new(u64::MAX)).unwrap();
  let Answer::Pack(pack) = outcome.answer else {
    panic!()
  };
  assert_eq!(
    pack.shallow_info,
    Some(ShallowInfo {
      shallow: vec![graph.id(commits[3])],
      unshallow: vec![]
    })
  );
  let head = &graph.objects[at(commits[3])];
  let mut expected = vec![
    commits[3],
    head.tree,
    graph.objects[at(head.tree)].entries[0],
  ];
  expected.sort();
  assert_eq!(pack.objects, expected);
}

/// A-615 part 2 (§4.2): do negotiate a walk with one byte of room, and with none needed by a request with no wants.
/// Expect: the walk refused `PastAdmission`; the empty request answered with nothing.
#[test]
fn a_negotiation_past_its_admission_is_refused() {
  let (graph, commits) = straight(5);
  let asks = Asks {
    wants: vec![graph.id(commits[4])],
    done: true,
    ..Asks::default()
  };
  assert!(matches!(
    negotiate(&graph, &asks.request(), &mut Admission::new(1)),
    Err(Refusal::PastAdmission(_))
  ));
  assert_eq!(
    negotiate(&graph, &Asks::default().request(), &mut Admission::new(0))
      .map(|outcome| outcome.answer),
    Ok(Answer::Nothing)
  );
}

/// A-615 part 2 (`list-objects-filter.c` `filter_trees_depth`: a tree met again shallower is walked again, "minimum
/// depth if an object is located at multiple depths"): do fetch two unrelated commits under `tree:4`, the first holding
/// a tree `x` at depth 2 and the second at depth 1, `x` holding a tree that holds a blob. Expect: the blob, at depth 4
/// below the first commit and 3 below the second, sent, which only opening `x` again at the shallower depth finds.
#[test]
fn a_tree_met_again_shallower_is_opened_again_under_a_depth_filter() {
  let mut graph = TestGraph::default();
  let blob = graph.new_blob(1);
  let inner = graph.new_tree(&[blob]);
  let x = graph.new_tree(&[inner]);
  let middle = graph.new_tree(&[x]);
  let deep_root = graph.new_tree(&[middle]);
  let shallow_root = graph.new_tree(&[x]);
  let first = graph.new_commit(&[], deep_root, 10);
  let second = graph.new_commit(&[], shallow_root, 20);
  let asks = Asks {
    wants: vec![graph.id(first), graph.id(second)],
    done: true,
    filter: Some(Filter::TreeDepth(4)),
    ..Asks::default()
  };
  let outcome = negotiate(&graph, &asks.request(), &mut Admission::new(u64::MAX)).unwrap();
  let Answer::Pack(pack) = outcome.answer else {
    panic!()
  };
  assert!(pack.objects.contains(&blob), "{:?}", pack.objects);
  let mut expected = vec![
    blob,
    inner,
    x,
    middle,
    deep_root,
    shallow_root,
    first,
    second,
  ];
  expected.sort();
  assert_eq!(pack.objects, expected);
}
