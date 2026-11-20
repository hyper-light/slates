//! Tests of `serve` (A-619): every answer held to a naive oracle (whole ancestor sets, every child listed) on generated
//! histories; 7.2's set discovery (`setdiscovery.findcommonheads`, sampling in revision order) driven against `heads`
//! and `known` until it agrees with the naive common set, its rounds counted; hostile lists, unknown nodes, a short
//! admission and a graph that breaks its own order refused by kind.

use super::discovery::{self, branch_head_revs, head_revs};
use super::pull::{self, Outgoing};
use super::{Refusal, Repository, Rev};
use crate::phases::{Phase, PhaseHead};
use crate::quote::{self, PATH_SAFE};
use crate::{Admission, NULL_NODE, Node, pushkey};
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

/// One changeset of a test's history.
#[derive(Clone, Debug)]
pub(super) struct Changeset {
  pub(super) node: Node,
  pub(super) parents: [Option<Rev>; 2],
  pub(super) branch: Vec<u8>,
  pub(super) closes: bool,
  pub(super) phase: Phase,
}

/// A served view held in memory: the repository every oracle reads.
#[derive(Clone, Debug, Default)]
pub(super) struct History {
  pub(super) revs: Vec<Changeset>,
  pub(super) bookmarks: Vec<(Vec<u8>, Node)>,
  pub(super) publishing: bool,
  pub(super) by_node: HashMap<Node, Rev>,
  /// Each revision's `.hgtags` file revision, where its manifest names one: its node and bytes.
  pub(super) hgtags: BTreeMap<Rev, (Node, Vec<u8>)>,
}

pub(super) fn at(rev: Rev) -> usize {
  usize::try_from(rev).unwrap()
}

impl History {
  pub(super) fn push(&mut self, changeset: Changeset) -> Rev {
    let rev = Rev::try_from(self.revs.len()).unwrap();
    self.by_node.insert(changeset.node, rev);
    self.revs.push(changeset);
    rev
  }

  /// The history of `heads` alone: their ancestors, in this history's order, parents renumbered.
  fn restrict(&self, heads: &[Rev]) -> History {
    let keep = ancestors(self, heads);
    let mut out = History {
      publishing: self.publishing,
      ..History::default()
    };
    let mut renumbered = HashMap::new();
    for rev in &keep {
      let mut changeset = self.revs[at(*rev)].clone();
      changeset.parents = changeset
        .parents
        .map(|parent| parent.map(|parent| renumbered[&parent]));
      renumbered.insert(*rev, out.push(changeset));
    }
    out.bookmarks = self
      .bookmarks
      .iter()
      .filter(|(_, node)| out.by_node.contains_key(node))
      .cloned()
      .collect();
    out
  }
}

impl Repository for History {
  fn count(&self) -> Rev {
    Rev::try_from(self.revs.len()).unwrap()
  }
  fn node(&self, rev: Rev) -> Option<Node> {
    self.revs.get(at(rev)).map(|changeset| changeset.node)
  }
  fn rev(&self, node: &Node) -> Option<Rev> {
    self.by_node.get(node).copied()
  }
  fn parents(&self, rev: Rev) -> [Option<Rev>; 2] {
    self
      .revs
      .get(at(rev))
      .map_or([None, None], |changeset| changeset.parents)
  }
  fn branch(&self, rev: Rev) -> &[u8] {
    self
      .revs
      .get(at(rev))
      .map_or(&b""[..], |changeset| &changeset.branch)
  }
  fn closes(&self, rev: Rev) -> bool {
    self
      .revs
      .get(at(rev))
      .is_some_and(|changeset| changeset.closes)
  }
  fn phase(&self, rev: Rev) -> Phase {
    self
      .revs
      .get(at(rev))
      .map_or(Phase::Public, |changeset| changeset.phase)
  }
  fn publishing(&self) -> bool {
    self.publishing
  }
  fn bookmarks(&self) -> impl Iterator<Item = (&[u8], Node)> {
    self
      .bookmarks
      .iter()
      .map(|(name, node)| (name.as_slice(), *node))
  }
  /// A history hides nothing, so the roots its register would keep are its draft changesets none of whose parents is
  /// draft.
  fn draft_roots(&self) -> impl Iterator<Item = Node> {
    self
      .revs
      .iter()
      .filter(|changeset| {
        changeset.phase == Phase::Draft
          && changeset
            .parents
            .iter()
            .flatten()
            .all(|parent| self.phase(*parent) == Phase::Public)
      })
      .map(|changeset| changeset.node)
  }
  fn tags_file(&self, rev: Rev, _admission: &mut Admission) -> Result<Option<Node>, Refusal> {
    Ok(self.hgtags.get(&rev).map(|(node, _)| *node))
  }
  fn tags_text(&self, node: &Node, admission: &mut Admission) -> Result<Vec<u8>, Refusal> {
    let (_, text) = self
      .hgtags
      .values()
      .find(|(held, _)| held == node)
      .ok_or(Refusal::Store { what: "tags" })?;
    admission.charge(u64::try_from(text.len()).unwrap())?;
    Ok(text.clone())
  }
}

/// A node no two revisions of a history share: splitmix64 of the revision's seed, so node order is not revision order.
pub(super) fn node_for(seed: u64) -> Node {
  let mut state = seed;
  let mut node = [0; 20];
  for chunk in node.chunks_mut(8) {
    state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut mixed = state;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    mixed ^= mixed >> 31;
    chunk.copy_from_slice(&mixed.to_be_bytes()[..chunk.len()]);
  }
  node
}

const BRANCHES: [&[u8]; 4] = [b"default", b"stable", b"old feature/x", b"default"];

/// A history from per-revision choices: a first parent among the four before it (or none), a second now and then, a
/// branch, a close, a bookmark (its name divergent now and then), and draft where asked or where a parent is draft.
fn build(choices: &[(u32, u32, u8, bool, bool, bool)], seed: u64, publishing: bool) -> History {
  let mut history = History {
    publishing,
    ..History::default()
  };
  for (rev, (first, second, branch, closes, draft, mark)) in choices.iter().enumerate() {
    let rev = Rev::try_from(rev).unwrap();
    let p1 = (rev > 0 && !first.is_multiple_of(7)).then(|| rev - 1 - first % rev.min(4));
    let p2 = (rev > 1 && second.is_multiple_of(4))
      .then(|| second % rev)
      .filter(|parent| Some(*parent) != p1 && p1.is_some());
    let parents = [p1, p2];
    let inherited = parents
      .iter()
      .flatten()
      .any(|parent| history.revs[at(*parent)].phase == Phase::Draft);
    let phase = if *draft || inherited {
      Phase::Draft
    } else {
      Phase::Public
    };
    let node = node_for(seed ^ u64::from(rev).wrapping_mul(0x1000_0001));
    history.push(Changeset {
      node,
      parents,
      branch: BRANCHES[usize::from(*branch) % BRANCHES.len()].to_vec(),
      closes: *closes,
      phase,
    });
    if *mark {
      // A-690: every third name divergent (an `@` inside it), every third ending with `@` (no divergent name).
      let name = match rev % 3 {
        0 => format!("mark {rev}"),
        1 => format!("mark {rev}@1"),
        _ => format!("mark {rev}@"),
      };
      history.bookmarks.push((name.into_bytes(), node));
    }
  }
  history
    .bookmarks
    .push((b"gone".to_vec(), node_for(seed.wrapping_add(7))));
  history
}

pub(super) fn history() -> impl Strategy<Value = History> {
  (
    proptest::collection::vec(
      (
        any::<u32>(),
        any::<u32>(),
        0..4u8,
        any::<bool>(),
        proptest::bool::weighted(0.2),
        proptest::bool::weighted(0.15),
      ),
      0..48,
    ),
    any::<u64>(),
    any::<bool>(),
  )
    .prop_map(|(choices, seed, publishing)| build(&choices, seed, publishing))
}

// The naive oracles: whole sets, every child listed, nothing incremental.

pub(super) fn ancestors(history: &History, revs: &[Rev]) -> BTreeSet<Rev> {
  let mut seen = BTreeSet::new();
  let mut stack: Vec<Rev> = revs.to_vec();
  while let Some(rev) = stack.pop() {
    if seen.insert(rev) {
      stack.extend(history.parents(rev).into_iter().flatten());
    }
  }
  seen
}

fn naive_heads(history: &History) -> Vec<Rev> {
  (0..history.count())
    .filter(|rev| !(0..history.count()).any(|child| history.parents(child).contains(&Some(*rev))))
    .collect()
}

fn naive_missing(history: &History, heads: &[Rev], common: &[Rev]) -> Vec<Rev> {
  let common = ancestors(history, common);
  ancestors(history, heads)
    .difference(&common)
    .copied()
    .collect()
}

pub(super) fn hex(node: &Node) -> Vec<u8> {
  let mut out = Vec::new();
  pushkey::encode_hex(node, &mut out);
  out
}

pub(super) fn joined(nodes: impl IntoIterator<Item = Node>) -> Vec<u8> {
  nodes
    .into_iter()
    .map(|node| hex(&node))
    .collect::<Vec<_>>()
    .join(&b' ')
}

pub(super) fn room() -> Admission {
  Admission::new(u64::MAX)
}

fn answer(call: impl FnOnce(&mut Admission, &mut Vec<u8>) -> Result<(), Refusal>) -> Vec<u8> {
  let mut out = Vec::new();
  call(&mut room(), &mut out).unwrap();
  out
}

fn oracle_heads_answer(history: &History) -> Vec<u8> {
  let heads = naive_heads(history);
  let mut out = if heads.is_empty() {
    hex(&NULL_NODE)
  } else {
    joined(heads.iter().rev().map(|rev| history.revs[at(*rev)].node))
  };
  out.push(b'\n');
  out
}

fn oracle_between(history: &History, top: Node, bottom: Node) -> Vec<Node> {
  let (mut node, mut list, mut i, mut f) = (top, Vec::new(), 0, 1);
  while node != bottom && node != NULL_NODE {
    let rev = history.rev(&node).unwrap();
    let parent = history.parents(rev)[0].map_or(NULL_NODE, |parent| history.revs[at(parent)].node);
    if i == f {
      list.push(node);
      f *= 2;
    }
    node = parent;
    i += 1;
  }
  list
}

fn oracle_branches(history: &History, top: Node) -> [Node; 4] {
  let node_of = |rev: Option<Rev>| rev.map_or(NULL_NODE, |rev| history.revs[at(rev)].node);
  if top == NULL_NODE {
    return [NULL_NODE; 4];
  }
  let mut rev = history.rev(&top).unwrap();
  loop {
    let [p1, p2] = history.parents(rev);
    if p2.is_some() || p1.is_none() {
      return [top, history.revs[at(rev)].node, node_of(p1), node_of(p2)];
    }
    rev = p1.unwrap();
  }
}

/// Each branch's heads by 7.2's rule (A-667; `branchmap.py`: "A branchhead is a revision such that no descendant is on
/// the same branch"): each revision no later revision on its branch has as an ancestor.
fn oracle_branchmap(history: &History) -> BTreeMap<Vec<u8>, Vec<Rev>> {
  let mut map: BTreeMap<Vec<u8>, Vec<Rev>> = BTreeMap::new();
  for rev in 0..history.count() {
    let branch = history.branch(rev);
    let continued = (rev + 1..history.count())
      .any(|later| history.branch(later) == branch && ancestors(history, &[later]).contains(&rev));
    if !continued {
      map.entry(branch.to_vec()).or_default().push(rev);
    }
  }
  map
}

fn oracle_phase_heads(history: &History, heads: &[Rev]) -> Vec<PhaseHead> {
  let mut by_phase: BTreeMap<Phase, BTreeSet<Node>> = BTreeMap::new();
  let node = |rev: Rev| history.revs[at(rev)].node;
  if history.publishing {
    by_phase
      .entry(Phase::Public)
      .or_default()
      .extend(heads.iter().map(|rev| node(*rev)));
  } else {
    let (public, draft): (Vec<Rev>, Vec<Rev>) = heads
      .iter()
      .partition(|rev| history.phase(**rev) == Phase::Public);
    by_phase
      .entry(Phase::Public)
      .or_default()
      .extend(public.iter().map(|rev| node(*rev)));
    by_phase
      .entry(Phase::Draft)
      .or_default()
      .extend(draft.iter().map(|rev| node(*rev)));
    if !draft.is_empty() {
      let only: BTreeSet<Rev> = naive_missing(history, &draft, &public)
        .into_iter()
        .filter(|rev| history.phase(*rev) == Phase::Public)
        .collect();
      let extra = only.iter().filter(|rev| {
        !only
          .iter()
          .any(|other| history.parents(*other).contains(&Some(**rev)))
      });
      by_phase
        .entry(Phase::Public)
        .or_default()
        .extend(extra.map(|rev| node(*rev)));
    }
  }
  by_phase
    .into_iter()
    .flat_map(|(phase, nodes)| nodes.into_iter().map(move |node| PhaseHead { phase, node }))
    .collect()
}

/// 7.2's `bookmarks.isdivergent`, as its source states it: `b'@' in b and not b.endswith(b'@')`.
fn oracle_divergent(name: &[u8]) -> bool {
  name.contains(&b'@') && !name.ends_with(b"@")
}

fn oracle_listkeys(history: &History, namespace: &[u8]) -> Vec<u8> {
  let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
  match namespace {
    b"namespaces" => {
      for name in ["bookmarks", "namespaces", "obsolete", "phases"] {
        entries.push((name.as_bytes().to_vec(), Vec::new()));
      }
    }
    b"bookmarks" => {
      for (name, node) in &history.bookmarks {
        if history.rev(node).is_some() && !oracle_divergent(name) {
          entries.push((name.clone(), hex(node)));
        }
      }
    }
    b"phases" => {
      for rev in 0..history.count() {
        let parents_public = history
          .parents(rev)
          .into_iter()
          .flatten()
          .all(|parent| history.phase(parent) == Phase::Public);
        if history.phase(rev) == Phase::Draft && parents_public {
          entries.push((hex(&history.revs[at(rev)].node), b"1".to_vec()));
        }
      }
      if history.publishing {
        entries.push((b"publishing".to_vec(), b"True".to_vec()));
      }
    }
    _ => {}
  }
  entries.sort();
  entries
    .iter()
    .map(|(key, value)| [key.as_slice(), b"\t", value.as_slice()].concat())
    .collect::<Vec<_>>()
    .join(&b'\n')
}

/// The served view's heads (a helper, so a property test's body holds no unwrap).
fn heads_of(history: &History) -> Vec<Rev> {
  head_revs(history, &mut room()).unwrap()
}

/// Every branch's heads.
fn branch_heads_of(history: &History) -> Vec<Rev> {
  branch_head_revs(history, &mut room()).unwrap()
}

/// A `getbundle`'s choice for these node lists.
fn choose(history: &History, heads: &[u8], common: &[u8]) -> Outgoing {
  pull::outgoing(history, heads, common, &mut room()).unwrap()
}

/// The phase heads a `getbundle` of `heads` sends.
fn phase_heads_of(history: &History, heads: &[Rev]) -> Vec<PhaseHead> {
  pull::phase_heads(history, heads, &mut room()).unwrap()
}

/// The bookmarks a `getbundle` sends, each a name and a node.
fn marks_of(history: &History) -> Vec<(&[u8], Node)> {
  pull::bookmark_entries(history, &mut room())
    .unwrap()
    .iter()
    .map(|mark| (mark.name, mark.node.unwrap()))
    .collect()
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// AC-HG.1, A-619 (oracle): do answer `heads`, `branchmap` and every `listkeys` namespace over generated histories.
  /// Expect each answer byte for byte as the naive oracle writes it: heads newest first (the null node for an empty
  /// history), each branch's heads oldest first with closed ones kept, branches by name and quoted, bookmarks naming
  /// a changeset held but a divergent one (A-690), draft roots and `publishing`.
  #[test]
  fn heads_branchmap_and_listkeys_answer_as_the_oracle(history in history()) {
    prop_assert_eq!(answer(|room, out| discovery::heads(&history, room, out)), oracle_heads_answer(&history));
    prop_assert_eq!(heads_of(&history), naive_heads(&history));
    let lines: Vec<Vec<u8>> = oracle_branchmap(&history)
      .into_iter()
      .map(|(branch, heads)| {
        let mut line = Vec::new();
        quote::quote(&branch, PATH_SAFE, &mut line);
        line.push(b' ');
        line.extend(joined(heads.iter().map(|rev| history.revs[at(*rev)].node)));
        line
      })
      .collect();
    prop_assert_eq!(answer(|room, out| discovery::branchmap(&history, room, out)), lines.join(&b'\n'));
    let mut flat: Vec<Rev> = oracle_branchmap(&history).into_values().flatten().collect();
    flat.sort_unstable();
    prop_assert_eq!(branch_heads_of(&history), flat);
    for namespace in [&b"namespaces"[..], b"bookmarks", b"phases", b"obsolete", b"unknown"] {
      prop_assert_eq!(answer(|room, out| pull::listkeys(&history, namespace, room, out)), oracle_listkeys(&history, namespace));
    }
  }

  /// AC-HG.1, A-619 (oracle): do ask `known` for every node of a generated history, unknown ones and the null node
  /// among them, and `between` and `branches` from every revision. Expect `1` exactly for the nodes held and the null
  /// node, and each line as the naive ports of `localrepo.between` and `localrepo.branches` write it.
  #[test]
  fn known_between_and_branches_answer_as_the_oracle(history in history(), stranger in any::<u64>()) {
    let unknown = node_for(stranger ^ 0xFFFF);
    prop_assume!(history.rev(&unknown).is_none());
    let mut asked: Vec<Node> = history.revs.iter().map(|changeset| changeset.node).collect();
    asked.push(unknown);
    asked.push(NULL_NODE);
    let expected: Vec<u8> = asked
      .iter()
      .map(|node| if *node == NULL_NODE || history.rev(node).is_some() { b'1' } else { b'0' })
      .collect();
    prop_assert_eq!(answer(|room, out| discovery::known(&history, &joined(asked.clone()), room, out)), expected);
    for rev in 0..history.count() {
      let top = history.revs[at(rev)].node;
      let bottom = history.revs[at(rev / 2)].node;
      let pairs = [hex(&top), b"-".to_vec(), hex(&bottom), b" ".to_vec(), hex(&top), b"-".to_vec(), hex(&NULL_NODE)].concat();
      let mut lines = joined(oracle_between(&history, top, bottom));
      lines.push(b'\n');
      lines.extend(joined(oracle_between(&history, top, NULL_NODE)));
      lines.push(b'\n');
      prop_assert_eq!(answer(|room, out| discovery::between(&history, &pairs, room, out)), lines);
      let mut line = joined(oracle_branches(&history, top));
      line.push(b'\n');
      prop_assert_eq!(answer(|room, out| discovery::branches(&history, &hex(&top), room, out)), line);
    }
    let tip = history.count().checked_sub(1).map_or(NULL_NODE, |rev| history.revs[at(rev)].node);
    let mut line = joined(oracle_branches(&history, tip));
    line.push(b'\n');
    prop_assert_eq!(answer(|room, out| discovery::branches(&history, b"", room, out)), line);
  }

  /// AC-HG.1, A-619 (oracle): do choose a `getbundle`'s changesets for generated heads and common sets, an unknown
  /// common node among them, and its phase heads. Expect the missing changesets exactly the naive difference of the
  /// two ancestor sets, oldest first; no heads asked meaning every head; the phase heads as 7.2's
  /// `_getbundlephasespart` computes them over whole sets; and the bookmarks part's held bookmarks but divergent ones.
  #[test]
  fn getbundles_choice_and_phase_heads_are_the_oracles(
    history in history(),
    heads in proptest::collection::vec(any::<u32>(), 0..4),
    common in proptest::collection::vec(any::<u32>(), 0..4),
  ) {
    prop_assume!(history.count() > 0);
    let pick = |picks: &[u32]| -> Vec<Rev> { picks.iter().map(|pick| pick % history.count()).collect() };
    let (heads, common) = (pick(&heads), pick(&common));
    let mut common_list = joined(common.iter().map(|rev| history.revs[at(*rev)].node));
    if !common_list.is_empty() {
      common_list.push(b' ');
    }
    common_list.extend(hex(&node_for(0xDEAD_BEEF)));
    let chosen = choose(&history, &joined(heads.iter().map(|rev| history.revs[at(*rev)].node)), &common_list);
    let asked: Vec<Rev> = if heads.is_empty() { naive_heads(&history) } else { heads.iter().copied().collect::<BTreeSet<_>>().into_iter().collect() };
    prop_assert_eq!(&chosen.heads, &asked);
    prop_assert_eq!(&chosen.common, &common.iter().copied().collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>());
    prop_assert_eq!(
      &chosen.missing.iter().collect::<Vec<_>>(),
      &naive_missing(&history, &asked, &common)
    );
    prop_assert_eq!(phase_heads_of(&history, &chosen.heads), oracle_phase_heads(&history, &chosen.heads));
    let marks = marks_of(&history);
    let mut expected: Vec<(&[u8], Node)> =
      history.bookmarks().filter(|(name, node)| history.rev(node).is_some() && !oracle_divergent(name)).collect();
    expected.sort();
    prop_assert_eq!(marks, expected);
  }

  /// AC-HG.9 (hostile input), A-619: do give `known`, `between`, `branches` and `getbundle` arbitrary arguments.
  /// Expect an answer or a typed refusal, never a panic, within a bounded admission.
  #[test]
  fn arbitrary_arguments_never_panic(history in history(), bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
    let mut out = Vec::new();
    if discovery::known(&history, &bytes, &mut Admission::new(1 << 16), &mut out).is_ok() {
      let items = if bytes.is_empty() { 0 } else { bytes.split(|byte| *byte == b' ').count() };
      prop_assert_eq!(out.len(), items);
    }
    for refusal in [
      discovery::between(&history, &bytes, &mut Admission::new(1 << 16), &mut out).err(),
      discovery::branches(&history, &bytes, &mut Admission::new(1 << 16), &mut out).err(),
      pull::outgoing(&history, &bytes, &bytes, &mut Admission::new(1 << 16)).err(),
    ] {
      prop_assert!(refusal.is_none_or(|refusal| typed(&refusal)));
    }
  }

  /// A-619 (set discovery): do run 7.2's `findcommonheads` from a local history against a remote one, both cut from one
  /// generated history, the remote answering through `heads` and `known`. Expect the common set it finds (every
  /// ancestor of its common heads) exactly the changesets both hold, and nothing said incoming only where the remote
  /// holds nothing the local lacks.
  #[test]
  fn set_discovery_over_these_answers_finds_the_common_set(
    universe in history(),
    local_heads in proptest::collection::vec(any::<u32>(), 1..4),
    remote_heads in proptest::collection::vec(any::<u32>(), 1..4),
  ) {
    prop_assume!(universe.count() > 0);
    let pick = |picks: &[u32]| -> Vec<Rev> { picks.iter().map(|pick| pick % universe.count()).collect() };
    let local = universe.restrict(&pick(&local_heads));
    let remote = universe.restrict(&pick(&remote_heads));
    let found = find_common_heads(&local, &remote, 4, 8);
    let both: BTreeSet<Node> = local.by_node.keys().filter(|node| remote.by_node.contains_key(*node)).copied().collect();
    let closure: BTreeSet<Node> = ancestors(&local, &found.common.iter().filter_map(|node| local.rev(node)).collect::<Vec<_>>())
      .into_iter()
      .map(|rev| local.revs[at(rev)].node)
      .collect();
    prop_assert_eq!(closure, both);
    let lacking = remote.by_node.keys().any(|node| !local.by_node.contains_key(node));
    prop_assert!(found.incoming || !lacking);
  }
}

/// Whether a refusal of arbitrary arguments is one a client's bytes can earn.
fn typed(refusal: &Refusal) -> bool {
  matches!(
    refusal,
    Refusal::NodeList { .. }
      | Refusal::Pair { .. }
      | Refusal::UnknownNode { .. }
      | Refusal::PastAdmission(_)
  )
}

/// What a discovery found, and what it took.
struct Found {
  common: BTreeSet<Node>,
  incoming: bool,
  rounds: u32,
  asked: usize,
}

/// The remote's `heads`, read from its answer.
fn remote_heads(remote: &History) -> Vec<Node> {
  let answer = answer(|room, out| discovery::heads(remote, room, out));
  answer[..answer.len() - 1]
    .split(|byte| *byte == b' ')
    .map(|digits| pushkey::hex_node(digits).unwrap().unwrap())
    .collect()
}

/// The remote's `known` for `nodes`, read from its answer.
fn remote_known(remote: &History, nodes: &[Node]) -> Vec<bool> {
  let answer =
    answer(|room, out| discovery::known(remote, &joined(nodes.iter().copied()), room, out));
  answer.iter().map(|byte| *byte == b'1').collect()
}

/// 7.2's `partialdiscovery` (the pure one), its sets whole: the bases known common, the undecided revisions, the missing.
struct Partial<'l> {
  local: &'l History,
  targets: Vec<Rev>,
  bases: BTreeSet<Rev>,
  undecided: Option<BTreeSet<Rev>>,
  missing: BTreeSet<Rev>,
}

impl Partial<'_> {
  fn undecided(&mut self) -> &mut BTreeSet<Rev> {
    if self.undecided.is_none() {
      let common = ancestors(self.local, &self.bases.iter().copied().collect::<Vec<_>>());
      self.undecided = Some(
        ancestors(self.local, &self.targets)
          .difference(&common)
          .copied()
          .collect(),
      );
    }
    self.undecided.as_mut().unwrap()
  }

  fn add_commons(&mut self, commons: &[Rev]) {
    self.bases.extend(commons);
    if self.undecided.is_some() {
      let common = ancestors(self.local, &self.bases.iter().copied().collect::<Vec<_>>());
      self.undecided().retain(|rev| !common.contains(rev));
    }
  }

  fn add_missings(&mut self, missings: &[Rev]) {
    // `%ld::%ld`: descendants of the missing that are ancestors of the undecided.
    let undecided: Vec<Rev> = self.undecided().iter().copied().collect();
    let below = ancestors(self.local, &undecided);
    let mut descendants: BTreeSet<Rev> = missings.iter().copied().collect();
    for rev in 0..self.local.count() {
      if self
        .local
        .parents(rev)
        .into_iter()
        .flatten()
        .any(|parent| descendants.contains(&parent))
      {
        descendants.insert(rev);
      }
    }
    let new: BTreeSet<Rev> = descendants.intersection(&below).copied().collect();
    self.missing.extend(&new);
    self.undecided().retain(|rev| !new.contains(rev));
  }

  fn add_info(&mut self, sample: &[Rev], known: &[bool]) {
    let common: Vec<Rev> = sample
      .iter()
      .zip(known)
      .filter(|(_, known)| **known)
      .map(|(rev, _)| *rev)
      .collect();
    let missing: Vec<Rev> = sample
      .iter()
      .zip(known)
      .filter(|(_, known)| !**known)
      .map(|(rev, _)| *rev)
      .collect();
    if !common.is_empty() {
      self.add_commons(&common);
    }
    if !missing.is_empty() {
      self.add_missings(&missing);
    }
  }

  fn heads_of(&self, revs: &BTreeSet<Rev>) -> BTreeSet<Rev> {
    let parents: BTreeSet<Rev> = revs
      .iter()
      .flat_map(|rev| self.local.parents(*rev).into_iter().flatten())
      .collect();
    revs.difference(&parents).copied().collect()
  }

  fn quick_sample(&mut self, heads: &[Rev], size: usize) -> BTreeSet<Rev> {
    let revs = self.undecided().clone();
    if revs.len() <= size {
      return revs;
    }
    let mut sample = self.heads_of(&revs);
    if sample.len() >= size {
      return limit(sample, size);
    }
    update_sample(
      None,
      heads,
      &mut sample,
      |rev| self.local.parents(rev).into_iter().flatten().collect(),
      size,
    );
    sample
  }

  fn full_sample(&mut self, size: usize) -> BTreeSet<Rev> {
    let revs = self.undecided().clone();
    if revs.len() <= size {
      return revs;
    }
    let mut sample = self.heads_of(&revs);
    let revs_heads: Vec<Rev> = sample.iter().copied().collect();
    update_sample(
      Some(&revs),
      &revs_heads,
      &mut sample,
      |rev| self.local.parents(rev).into_iter().flatten().collect(),
      0,
    );
    let roots: Vec<Rev> = revs
      .iter()
      .copied()
      .filter(|rev| {
        !self
          .local
          .parents(*rev)
          .into_iter()
          .flatten()
          .any(|parent| revs.contains(&parent))
      })
      .collect();
    let children = |rev: Rev| -> Vec<Rev> {
      revs
        .iter()
        .copied()
        .filter(|child| self.local.parents(*child).contains(&Some(rev)))
        .collect()
    };
    update_sample(Some(&revs), &roots, &mut sample, children, 0);
    let size = size.max(roots.len().min(revs_heads.len()));
    let mut sample = limit(sample, size);
    if sample.len() < size {
      let more = size - sample.len();
      let rest: Vec<Rev> = revs.difference(&sample).copied().take(more).collect();
      sample.extend(rest);
    }
    sample
  }
}

/// `_limitsample` with `randomize` off: the first `size` revisions.
fn limit(sample: BTreeSet<Rev>, size: usize) -> BTreeSet<Rev> {
  sample.into_iter().take(size).collect()
}

/// `_updatesample`: from `heads`, every revision at distance 1, 2, 4, 8 and on, within `revs` when given, until `quick`
/// revisions are sampled when it is not 0.
fn update_sample(
  revs: Option<&BTreeSet<Rev>>,
  heads: &[Rev],
  sample: &mut BTreeSet<Rev>,
  next: impl Fn(Rev) -> Vec<Rev>,
  quick: usize,
) {
  let mut distance: HashMap<Rev, u32> = HashMap::new();
  let mut visit: VecDeque<Rev> = heads.iter().copied().collect();
  let mut seen = BTreeSet::new();
  let mut factor = 1;
  while let Some(current) = visit.pop_front() {
    if seen.contains(&current) {
      continue;
    }
    let d = *distance.entry(current).or_insert(1);
    if d > factor {
      factor *= 2;
    }
    if d == factor {
      sample.insert(current);
      if quick > 0 && sample.len() >= quick {
        return;
      }
    }
    seen.insert(current);
    for parent in next(current) {
      if revs.is_none_or(|revs| revs.contains(&parent)) {
        distance.entry(parent).or_insert(d + 1);
        visit.push_back(parent);
      }
    }
  }
}

/// 7.2's `findcommonheads` (heads exchanged first, samples in revision order) against `remote`'s answers.
fn find_common_heads(local: &History, remote: &History, initial: usize, full: usize) -> Found {
  let own_heads = naive_heads(local);
  let node = |rev: Rev| local.revs[at(rev)].node;
  let sample: Vec<Rev> = own_heads.clone();
  let server_heads = remote_heads(remote);
  let known = remote_known(
    remote,
    &sample.iter().map(|rev| node(*rev)).collect::<Vec<_>>(),
  );
  let mut found = Found {
    common: BTreeSet::new(),
    incoming: server_heads != [NULL_NODE],
    rounds: 1,
    asked: sample.len(),
  };
  if local.count() == 0 {
    found.common.insert(NULL_NODE);
    return found;
  }
  let known_server_heads: Vec<Rev> = server_heads
    .iter()
    .filter(|head| **head != NULL_NODE)
    .filter_map(|head| local.rev(head))
    .collect();
  if known_server_heads.len() == server_heads.len() {
    found.common = server_heads.iter().copied().collect();
    found.incoming = false;
    return found;
  }
  if known.iter().all(|known| *known) {
    found.common = own_heads.iter().map(|rev| node(*rev)).collect();
    return found;
  }
  let mut partial = Partial {
    local,
    targets: own_heads.clone(),
    bases: BTreeSet::new(),
    undecided: None,
    missing: BTreeSet::new(),
  };
  partial.add_commons(&known_server_heads);
  partial.add_info(&sample, &known);
  let mut full_sample = false;
  while !partial.undecided().is_empty() {
    let sample: Vec<Rev> = if full_sample || !partial.bases.is_empty() {
      partial.full_sample(full).into_iter().collect()
    } else {
      partial
        .quick_sample(&own_heads, initial)
        .into_iter()
        .collect()
    };
    found.rounds += 1;
    found.asked += sample.len();
    let known = remote_known(
      remote,
      &sample.iter().map(|rev| node(*rev)).collect::<Vec<_>>(),
    );
    full_sample = true;
    partial.add_info(&sample, &known);
  }
  let heads = partial.heads_of(&partial.bases.clone());
  found.common = heads.iter().map(|rev| node(*rev)).collect();
  if found.common.is_empty() && server_heads != [NULL_NODE] {
    found.common.insert(NULL_NODE);
  }
  found
}

/// A long line of history the local and the remote share up to a fork deep down, each going on its own way after it.
fn forked(shared: u32, local_only: u32, remote_only: u32) -> (History, History) {
  let mut universe = History::default();
  let mut previous = None;
  for rev in 0..shared + local_only + remote_only {
    let parent = if rev == shared + local_only {
      Some(shared - 1)
    } else {
      previous
    };
    universe.push(Changeset {
      node: node_for(u64::from(rev) + 1),
      parents: [parent, None],
      branch: b"default".to_vec(),
      closes: false,
      phase: Phase::Public,
    });
    previous = Some(rev);
  }
  let local = universe.restrict(&[shared + local_only - 1]);
  let remote = universe.restrict(&[shared + local_only + remote_only - 1]);
  (local, remote)
}

/// A-619 (set discovery, non-vacuity): do run 7.2's discovery from a local history of 300 changesets whose last 100
/// the remote lacks, against a remote that holds 60 the local lacks, sampling 4 nodes at first and 8 after. Expect it to
/// find exactly the 200 shared changesets (its common heads hold the shared line's last, and, as 7.2's `basesheads`
/// removes only direct parents, may hold earlier common bases beside it), and the counters to show the sampled rounds
/// were taken: more than the head exchange, more nodes asked than the heads.
#[test]
fn set_discovery_takes_its_sampled_rounds() {
  let (local, remote) = forked(200, 100, 60);
  let found = find_common_heads(&local, &remote, 4, 8);
  let revs: Vec<Rev> = found
    .common
    .iter()
    .filter_map(|node| local.rev(node))
    .collect();
  assert_eq!(
    ancestors(&local, &revs),
    (0..200).collect::<BTreeSet<Rev>>()
  );
  assert!(found.common.contains(&node_for(200)));
  assert!(found.incoming);
  assert!(found.rounds >= 3, "rounds: {}", found.rounds);
  assert!(found.asked > 1, "asked: {}", found.asked);
}

/// A line of `count` public changesets on `default`.
fn chain(count: u32) -> History {
  let mut history = History {
    publishing: true,
    ..History::default()
  };
  for rev in 0..count {
    history.push(Changeset {
      node: node_for(u64::from(rev) + 1),
      parents: [rev.checked_sub(1), None],
      branch: b"default".to_vec(),
      closes: false,
      phase: Phase::Public,
    });
  }
  history
}

/// A line whose third revision names a parent after itself, against the rule every revlog keeps.
fn broken() -> History {
  let mut history = chain(4);
  history.revs[2].parents = [Some(3), None];
  history
}

/// AC-HG.9 (hostile input), A-619: do give node lists with an item of 39 and 41 digits, a non-hexadecimal one and an
/// empty one between two spaces, and `between` pairs of one node and of three. Expect each refused by kind, naming the
/// list's item or the pair.
#[test]
fn malformed_lists_and_pairs_are_refused_by_kind() {
  let history = chain(6);
  let known_node = hex(&history.revs[5].node);
  let mut out = Vec::new();
  for (list, at) in [
    ([&known_node[..], b" ", &known_node[..39]].concat(), 1),
    ([&known_node[..], b"0"].concat(), 0),
    ([&b"zz"[..], &known_node[2..]].concat(), 0),
    ([&known_node[..], b"  ", &known_node[..]].concat(), 1),
  ] {
    assert_eq!(
      discovery::known(&history, &list, &mut room(), &mut out),
      Err(Refusal::NodeList { at })
    );
  }
  for (pairs, at) in [
    (known_node.clone(), 0),
    (
      [
        &known_node[..],
        b"-",
        &known_node[..],
        b"-",
        &known_node[..],
      ]
      .concat(),
      0,
    ),
  ] {
    assert_eq!(
      discovery::between(&history, &pairs, &mut room(), &mut out),
      Err(Refusal::Pair { at })
    );
  }
}

/// AC-HG.9 (hostile input), A-619: do give an unknown node as a `between` pair's top, as a `branches` node, as a
/// `getbundle` head and as a common node. Expect the first three refused naming it, and the common node dropped, as
/// 7.2 drops it, every head then sent whole.
#[test]
fn unknown_nodes_are_refused_except_common_ones() {
  let history = chain(6);
  let unknown_node = node_for(1 << 40);
  let unknown = hex(&unknown_node);
  let refused = Err(Refusal::UnknownNode { node: unknown_node });
  let mut out = Vec::new();
  let pair = [&unknown[..], b"-", &hex(&NULL_NODE)[..]].concat();
  assert_eq!(
    discovery::between(&history, &pair, &mut room(), &mut out),
    refused
  );
  assert_eq!(
    discovery::branches(&history, &unknown, &mut room(), &mut out),
    refused
  );
  assert_eq!(
    pull::outgoing(&history, &unknown, b"", &mut room()).err(),
    refused.err()
  );
  let dropped = pull::outgoing(&history, b"", &unknown, &mut room()).unwrap();
  assert_eq!(
    dropped,
    Outgoing {
      heads: vec![5],
      common: Vec::new(),
      missing: std::iter::once(0..6).collect()
    }
  );
}

/// AC-HG.9 (hostile input), A-619: do give `known` a list longer than its admission, and walk a repository whose graph
/// names a parent after its child. Expect the list refused past the admission, and each walk refused naming the
/// revision that broke the rule, none looping.
#[test]
fn a_short_admission_and_a_broken_graph_are_refused_by_kind() {
  let history = chain(6);
  let long = vec![hex(&history.revs[5].node); 100].join(&b' ');
  let mut out = Vec::new();
  assert!(matches!(
    discovery::known(&history, &long, &mut Admission::new(100), &mut out),
    Err(Refusal::PastAdmission(_))
  ));
  let broken = broken();
  assert_eq!(
    head_revs(&broken, &mut room()),
    Err(Refusal::Graph { rev: 2 })
  );
  let node = hex(&broken.revs[2].node);
  assert_eq!(
    discovery::branches(&broken, &node, &mut room(), &mut out),
    Err(Refusal::Graph { rev: 2 })
  );
  let tip = hex(&broken.revs[3].node);
  assert_eq!(
    pull::outgoing(&broken, &tip, b"", &mut room()),
    Err(Refusal::Graph { rev: 2 })
  );
}

/// The probe of stock `hg serve --stdio` 7.2 (`fixtures/serve/make-serve-fixtures.sh`): the served view it answered
/// from, oldest first, its bookmarks, what the probe wrote, and what the server answered.
const PROBE_STATE: &str = include_str!("../../fixtures/serve/state-probe.txt");
const PROBE_BOOKMARKS: &str = include_str!("../../fixtures/serve/bookmarks-probe.txt");
const PROBE_CLIENT: &[u8] = include_bytes!("../../fixtures/serve/probe.client");
const PROBE_SERVER: &[u8] = include_bytes!("../../fixtures/serve/probe.server");

/// The repository a capture asked (`make-serve-fixtures.sh`'s `dump`): each state line a node, its two parents, its
/// phase, `1` where it closes its branch, then `|` and its branch; each bookmarks line a node and a name;
/// non-publishing, as the script's hgrc says.
pub(super) fn captured(state: &str, bookmarks: &str) -> History {
  let node = |hex: &str| pushkey::hex_node(hex.as_bytes()).unwrap().unwrap();
  let mut history = History::default();
  for line in state.lines() {
    let (fields, branch) = line.split_once('|').unwrap();
    let fields: Vec<&str> = fields.split(' ').collect();
    let parent = |hex: &str| (node(hex) != NULL_NODE).then(|| history.rev(&node(hex)).unwrap());
    let parents = [parent(fields[1]), parent(fields[2])];
    let phase = if fields[3] == "public" {
      Phase::Public
    } else {
      Phase::Draft
    };
    history.push(Changeset {
      node: node(fields[0]),
      parents,
      branch: branch.as_bytes().to_vec(),
      closes: fields[4] == "1",
      phase,
    });
  }
  for line in bookmarks.lines() {
    let (hex, name) = line.split_once(' ').unwrap();
    history
      .bookmarks
      .push((name.as_bytes().to_vec(), node(hex)));
  }
  history
}

/// The repository the probe asked.
fn probed() -> History {
  captured(PROBE_STATE, PROBE_BOOKMARKS)
}

/// A line and the bytes after it.
pub(super) fn split_line(bytes: &[u8]) -> (&[u8], &[u8]) {
  let at = bytes.iter().position(|byte| *byte == b'\n').unwrap();
  (&bytes[..at], &bytes[at + 1..])
}

/// The probe's requests as SSH version 1 carries them: a command's name on its line, then each argument its key, its
/// length and its bytes, and `known`'s empty dictionary (`* 0`).
fn probe_requests(mut client: &[u8]) -> Vec<(String, Vec<u8>)> {
  let mut requests = Vec::new();
  while !client.is_empty() {
    let (name, rest) = split_line(client);
    let name = String::from_utf8(name.to_vec()).unwrap();
    client = rest;
    let mut argument = Vec::new();
    if ["known", "between", "branches", "listkeys"].contains(&name.as_str()) {
      let (header, rest) = split_line(client);
      let len: usize = std::str::from_utf8(header)
        .unwrap()
        .rsplit(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
      argument = rest[..len].to_vec();
      client = &rest[len..];
      if name == "known" {
        let (dictionary, rest) = split_line(client);
        assert_eq!(dictionary, b"* 0");
        client = rest;
      }
    }
    requests.push((name, argument));
  }
  requests
}

/// The server's answers: each its length on a line, then its bytes.
fn probe_answers(mut server: &[u8]) -> Vec<Vec<u8>> {
  let mut answers = Vec::new();
  while !server.is_empty() {
    let (len, rest) = split_line(server);
    let len: usize = std::str::from_utf8(len).unwrap().parse().unwrap();
    answers.push(rest[..len].to_vec());
    server = &rest[len..];
  }
  answers
}

/// AC-HG.1 (golden), A-619: do answer, from the repository stock `hg serve --stdio` 7.2 served (non-publishing; two
/// named branches, a closed head on a branch whose name needs quoting, a merge, drafts, an amended draft whose
/// predecessor is hidden, two bookmarks), each request its probe sent: `heads`; `known` of held, unknown and null
/// nodes; `between` from the tip to the null node, to an ancestor, and the null pair; `branches`; `branchmap`; and
/// `listkeys` of `namespaces`, `bookmarks` and `phases`. Expect eight answers, each byte for byte as 7.2's (its branch
/// cache's order here the names' order, which this answer keeps).
#[test]
fn every_probe_request_is_answered_as_mercurial_answered_it() {
  let history = probed();
  let requests = probe_requests(PROBE_CLIENT);
  let answers = probe_answers(PROBE_SERVER);
  assert_eq!(requests.len(), 8);
  assert_eq!(answers.len(), requests.len());
  for ((command, argument), expected) in requests.iter().zip(&answers) {
    let got = answer(|room, out| match command.as_str() {
      "heads" => discovery::heads(&history, room, out),
      "known" => discovery::known(&history, argument, room, out),
      "between" => discovery::between(&history, argument, room, out),
      "branches" => discovery::branches(&history, argument, room, out),
      "branchmap" => discovery::branchmap(&history, room, out),
      "listkeys" => pull::listkeys(&history, argument, room, out),
      other => panic!("the probe sent no {other}"),
    });
    assert_eq!(
      String::from_utf8_lossy(&got),
      String::from_utf8_lossy(expected),
      "{command}"
    );
  }
}
