//! Ancestry as derived data: whether one version is an ancestor of another, two versions' merge bases, and bisect's
//! midpoint, answered from each version's value with no walk along its history's runs (A-666; A-645 "Queries by API",
//! part 1; A-647; §4.17.1 "Reads"). The evidence and what was rejected are in `research/ancestry-queries.md`.
//!
//! **The value** ([`Ancestry`]): Mononoke's commit-graph fields as one type of derived data \[C: Mononoke
//! `commit_graph_types/src/edges.rs` 23-41, 133-172\]. A version's value is a derived node naming its parents' values,
//! its first-parent skew jump, its run bottom (the nearest merge or root on its first-parent chain, for a version of one
//! parent), and its skip-tree parent and skip-tree skew jump; its payload holds its generation (git's topological level:
//! one for a root, else one more than its parents' greatest \[B: `commit-graph.adoc` 59-64\]), its first-parent depth,
//! its skip-tree depth and the bytes that name it in its history.
//! - A skew jump is Myers' \[A: Myers 1983, PUSH\]: a version jumps where its parent jumps to, or to the parent itself
//!   unless the parent's two gaps below are equal. So a child reads its parent's jump's jump, which its parent's value
//!   names, and a jump's depth is a function of the depth alone (`skew_jump_depth`).
//! - The skip-tree parent is the skip-tree lowest common ancestor of the parents \[C: Mononoke `core.rs` 79-86\]. It is
//!   a dominator: every ancestor of a version is an ancestor of its skip-tree parent or has a greater generation than
//!   it (by induction over the history), which is what makes jumping along the skip tree safe.
//! - Each field is a function of the parents' values and what they name, so the value is the same on every tier, and
//!   its identity, a hash over its parents' identities, names its whole history. A derivation reads O(1) values for a
//!   version of one parent and O(k lg n) for a merge of k parents.
//!
//! **Bounds**, in values visited (each read from the store at most once a question). A search along a skew-jumped chain
//! from depth D to a depth past the root visits at most 3⌊lg(D + 1)⌋ - 2 values \[A: Myers 1983, Corollary 5, tight\],
//! and to the root at most ⌊lg(D + 1)⌋ + 1 (a jump a unit of its skew-binary digits); with `L` = 3⌊lg(n + 1)⌋ for a
//! history of greatest generation n, a search visits at most L and a lowest common ancestor at most 3L. No index of
//! linear size answers is-ancestor in constant time on histories of two-parent merges \[A: Pǎtraşcu 2011, Theorem 2\],
//! so each bound has a part logarithmic along chains and a part linear in the merges a question must open (P: the
//! parents they take in).
//! - [`is_ancestor`]: Mononoke's lowering \[C: `lib.rs` 519-531, `frontier.rs` 120-160\]: the descendant's frontier
//!   jumps along the skip tree to the lowest version still at the ancestor's generation, and opens a merge only when
//!   its skip-tree parent lies below it. At most 2 + (1 + P)(2L + 3) visits; O(lg n) on a linear history.
//! - [`merge_bases`]: git's paint-down \[C: `commit-reach.c` `paint_down_to_common`; `paint-down-to-common.adoc`\]:
//!   sides one and two, stale below a base found, and both of git's exits (only stale entries left; one side exhausted
//!   with no pending candidate). It runs over runs, not versions: entries are grouped by run bottom, each run's entries
//!   are joined into one virtual tree over the first-parent tree (sorted in a depth-first order that a meet decides,
//!   joined where neighbours meet), so chains meet without walking; and a merge's paint jumps along the skip tree past
//!   every region no other entry reaches. For E entries joined in T runs: at most
//!   4 + T(2L + 4) + 3L·E(⌈lg E⌉ + 1) + 2P visits.
//! - [`bisect`]: the same paint with the bad version and the good ones. Every run of candidates is a segment whose
//!   weight grows by one a version; a merge's weight is the union of its parents' segments (bitsets over the K
//!   segments); the answer is the exact best of git's rule, `min(weight, count - weight)` \[C: `bisect.c` 187-207\],
//!   then the greater generation, then the lesser name. Visits as merge bases', at most P more for its merges' parents
//!   and L more for each segment; K² bits.
//!
//! **Typed refusals** ([`AncestryRefused`]): no such version; parents' values that are not the version's; a value that
//! does not read as the type's or contradicts what names it; a count past `u64::MAX`; and the store's own (a question
//! charges the store for what it keeps and is refused past its budget).
//!
//! **Evidence** (`ancestry/tests.rs`): every answer held to a naive walk over generated histories with long runs,
//! merges, octopus merges, criss-cross merges and unrelated roots; every skip-tree parent held to the immediate
//! dominator computed by its definition; every jump held to Myers' digits; every question's reads held to the bounds
//! above, and a linear history of 2^17 versions answered in a few hundred reads.
//!
//! **Owed:** corrected commit dates where a history has dates; a green's versions numbered as segmented changelog
//! numbers them; questions cut into the shard's slices; path history and blame (A-645's later parts).
//!
//! No I/O, no clock, no randomness, no panic.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;
use std::mem::size_of;

use crate::content::{ContentId, ContentRefused, ContentStore};

use super::{DerivedType, Graph, Mode};

#[cfg(test)]
mod tests;

/// Format: the type's name and its format's version. The type has no parameter: its values' shape is its history's.
const TYPE_NAME: &[u8] = b"slates-vcs ancestry 1";

/// Format: the bytes of a value's count of parents, little-endian.
const PARENTS_BYTES: usize = size_of::<u32>();

/// Format: the byte saying whether a value names a skip-tree parent and its jump.
const SKIP_BYTES: usize = size_of::<u8>();

/// Format: the bytes of each of a value's three numbers (generation, depth, skip-tree depth), little-endian.
const NUMBER_BYTES: usize = size_of::<u64>();

/// Format: the bytes before a value's name: its count of parents, its skip byte and its three numbers.
const HEAD_BYTES: usize = PARENTS_BYTES
  .saturating_add(SKIP_BYTES)
  .saturating_add(NUMBER_BYTES.saturating_mul(3));

/// Format: the skip byte of a value with no skip-tree parent.
const NO_SKIP: u8 = 0;

/// Format: the skip byte of a value with a skip-tree parent.
const HAS_SKIP: u8 = 1;

/// Format: a root's generation: "A commit with no parents (a root commit) has topological level of one" \[B:
/// `commit-graph.adoc` 61\].
const ROOT_GENERATION: u64 = 1;

/// The bits a version is painted with (module docs, [`merge_bases`] and [`bisect`]).
type Paint = u8;

/// Format: reached from the first version (git's `PARENT1`), or from the bad one.
const ONE: Paint = 1;

/// Format: reached from the second version (git's `PARENT2`), or from a good one.
const TWO: Paint = 2;

/// Format: below a merge base found, where no common ancestor can be another (git's `STALE`).
const STALE: Paint = 4;

/// Reached from both.
const BOTH: Paint = ONE | TWO;

/// Why the type refused to derive a version or to answer a question. Each leaves the store as it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AncestryRefused {
  /// The history holds no version the derivation was asked for.
  NoVersion,
  /// The version has `expected` parents and `given` values were given for them.
  Parents {
    /// The version's parents.
    expected: usize,
    /// The values given.
    given: usize,
  },
  /// The value `node` does not read as the type's, or contradicts the value that names it (a depth, a generation or a
  /// run that does not follow).
  Malformed {
    /// The value.
    node: ContentId,
  },
  /// A generation, a depth or a count would pass `u64::MAX`.
  Exhausted,
  /// A handle the question did not make: never made by this module's code; refused rather than trusted.
  Lost,
  /// The store refused: its budget, a count, a content not held, a value past the window.
  Content(ContentRefused),
}

impl From<ContentRefused> for AncestryRefused {
  fn from(refused: ContentRefused) -> AncestryRefused {
    AncestryRefused::Content(refused)
  }
}

/// What the type's derivations have done, for the tests' non-vacuity checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AncestryCounters {
  /// Values derived.
  pub derived: u64,
  /// Of them, values of merges.
  pub merges: u64,
  /// Values read by derivations: one parent and at most two jumps for a version of one parent.
  pub reads: u64,
}

/// What a question cost: the census's evidence and the owner's report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryCost {
  /// Values read from the store: each at most once a question.
  pub reads: u64,
  /// Values visited, again or for the first time: the question's work.
  pub visits: u64,
  /// Entries the paint joined into runs (merge bases, bisect), or versions the lowering held (is-ancestor).
  pub entries: u64,
  /// Runs the paint joined.
  pub runs: u64,
  /// Merges opened: their parents taken in.
  pub opened: u64,
  /// The parents the opened merges took in.
  pub parents: u64,
  /// Jumps along the skip tree past a region no other entry reaches.
  pub jumps: u64,
  /// Bisect's segments of candidates.
  pub segments: u64,
}

/// A version a question found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
  /// Its value's root.
  pub root: ContentId,
  /// Its generation.
  pub generation: u64,
  /// The bytes that name it in its history ([`Graph::name`]).
  pub name: Vec<u8>,
}

/// Bisect's answer: the version to test next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Midpoint {
  /// The version.
  pub found: Found,
  /// Its weight: the candidates it reaches, itself included.
  pub weight: u64,
  /// The candidates: the bad version's ancestors that no good one reaches.
  pub candidates: u64,
}

/// The type (module docs), declared over a history `S`.
#[derive(Debug)]
pub struct Ancestry<S> {
  mode: Mode,
  counters: AncestryCounters,
  history: PhantomData<fn(&S)>,
}

impl<S> Ancestry<S> {
  /// The type, computed by `mode`.
  pub fn declare(mode: Mode) -> Ancestry<S> {
    Ancestry {
      mode,
      counters: AncestryCounters::default(),
      history: PhantomData,
    }
  }

  /// What its derivations have done.
  pub fn counters(&self) -> AncestryCounters {
    self.counters
  }
}

impl<S: Graph> DerivedType for Ancestry<S> {
  type Source = S;
  type Refused = AncestryRefused;

  fn name(&self) -> &[u8] {
    TYPE_NAME
  }

  fn mode(&self) -> Mode {
    self.mode
  }

  fn derive(
    &mut self,
    source: &S,
    store: &mut ContentStore,
    version: S::Version,
    parents: &[ContentId],
  ) -> Result<ContentId, AncestryRefused> {
    let expected = source
      .parents(version)
      .ok_or(AncestryRefused::NoVersion)?
      .len();
    if expected != parents.len() {
      return Err(AncestryRefused::Parents {
        expected,
        given: parents.len(),
      });
    }
    let mut name = Vec::new();
    source.name(version, &mut name);
    let mut walk = Walk::new(store);
    let made = made(&mut walk, parents, name);
    let (made, cost) = walk.finish(made)?;
    let (names, payload) = made.encode().ok_or(AncestryRefused::Exhausted)?;
    let stored = store.put_derived(&names, &payload)?;
    self.counters.derived = self.counters.derived.saturating_add(1);
    self.counters.merges = self
      .counters
      .merges
      .saturating_add(u64::from(parents.len() > 1));
    self.counters.reads = self.counters.reads.saturating_add(cost.reads);
    Ok(stored.identity)
  }
}

/// Myers' J: the depth a value at `depth` jumps to, the value of `depth`'s canonical skew-binary numeral with its lowest
/// non-zero digit less one \[A: Myers 1983, p. 243\]; the root's, zero, is itself. The greedy numeral (the greatest
/// weight `2^k - 1` that fits, again and again) is the canonical one, so J subtracts the last weight taken.
fn skew_jump_depth(depth: u64) -> u64 {
  let mut rest = depth;
  let mut last = 0_u64;
  while rest > 0 {
    let Some(bits) = rest.saturating_add(1).checked_ilog2() else {
      break;
    };
    let weight = 1_u64
      .checked_shl(bits)
      .map_or(u64::MAX, |power| power.saturating_sub(1));
    rest = rest.saturating_sub(weight);
    last = weight;
  }
  depth.saturating_sub(last)
}

/// A value's skip-tree parent and skip-tree jump, when it has a skip-tree parent.
type SkipLinks = Option<(ContentId, ContentId)>;

/// A version's value as it is written: everything but its root, which its bytes decide.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Made {
  parents: Vec<ContentId>,
  jump: Option<ContentId>,
  bottom: Option<ContentId>,
  skip: SkipLinks,
  generation: u64,
  depth: u64,
  skip_depth: u64,
  name: Vec<u8>,
}

impl Made {
  /// A root's value.
  fn root(name: Vec<u8>) -> Made {
    Made {
      parents: Vec::new(),
      jump: None,
      bottom: None,
      skip: None,
      generation: ROOT_GENERATION,
      depth: 0,
      skip_depth: 0,
      name,
    }
  }

  /// The node's names and payload (module docs): the parents, the jump, the bottom, the skip-tree parent and jump, as
  /// each is present; then the count of parents, the skip byte, the three numbers and the name. `None` past `u32::MAX`
  /// parents.
  fn encode(&self) -> Option<(Vec<ContentId>, Vec<u8>)> {
    let count = u32::try_from(self.parents.len()).ok()?;
    let mut names = self.parents.clone();
    names.extend(self.jump);
    names.extend(self.bottom);
    if let Some((parent, jump)) = self.skip {
      names.extend([parent, jump]);
    }
    let mut payload = Vec::with_capacity(HEAD_BYTES.saturating_add(self.name.len()));
    payload.extend_from_slice(&count.to_le_bytes());
    payload.push(if self.skip.is_some() {
      HAS_SKIP
    } else {
      NO_SKIP
    });
    for number in [self.generation, self.depth, self.skip_depth] {
      payload.extend_from_slice(&number.to_le_bytes());
    }
    payload.extend_from_slice(&self.name);
    Some((names, payload))
  }
}

/// A version's value, read.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
  root: ContentId,
  made: Made,
}

/// The value `root` names, read and checked on its own: its links as its count and skip byte say, a root's numbers a
/// root's, a skip-tree parent exactly where its skip-tree depth says.
fn read_node(store: &ContentStore, root: ContentId) -> Result<Node, AncestryRefused> {
  let malformed = AncestryRefused::Malformed { node: root };
  let derived = store.derived(&root).ok_or(malformed)?;
  let names: Vec<ContentId> = derived.names().collect();
  let made = decode(&names, derived.payload()).ok_or(malformed)?;
  Ok(Node { root, made })
}

/// A value's fields from its names and payload; `None` for bytes in any other form.
fn decode(names: &[ContentId], payload: &[u8]) -> Option<Made> {
  let (count, rest) = payload.split_first_chunk::<PARENTS_BYTES>()?;
  let count = usize::try_from(u32::from_le_bytes(*count)).ok()?;
  let (skip, rest) = rest.split_first()?;
  let (generation, rest) = rest.split_first_chunk::<NUMBER_BYTES>()?;
  let (depth, rest) = rest.split_first_chunk::<NUMBER_BYTES>()?;
  let (skip_depth, name) = rest.split_first_chunk::<NUMBER_BYTES>()?;
  let has_skip = match *skip {
    NO_SKIP => false,
    HAS_SKIP => true,
    _ => return None,
  };
  let mut names = names.iter().copied();
  let parents: Vec<ContentId> = names.by_ref().take(count).collect();
  let jump = if count == 0 {
    None
  } else {
    Some(names.next()?)
  };
  let bottom = if count == 1 {
    Some(names.next()?)
  } else {
    None
  };
  let skip = if has_skip {
    Some((names.next()?, names.next()?))
  } else {
    None
  };
  let made = Made {
    parents,
    jump,
    bottom,
    skip,
    generation: u64::from_le_bytes(*generation),
    depth: u64::from_le_bytes(*depth),
    skip_depth: u64::from_le_bytes(*skip_depth),
    name: name.to_vec(),
  };
  let whole = made.parents.len() == count && names.next().is_none();
  (whole && consistent(&made)).then_some(made)
}

/// Whether a value's numbers agree with its links: a root's generation is one and its depths zero; any other's
/// generation is past one and its depth past zero; a skip-tree parent exactly when the skip-tree depth is past zero,
/// and always for a version of one parent.
fn consistent(made: &Made) -> bool {
  let numbers = if made.parents.is_empty() {
    made.generation == ROOT_GENERATION && made.depth == 0
  } else {
    made.generation > ROOT_GENERATION && made.depth > 0
  };
  let skip = match made.skip {
    Some(_) => made.skip_depth > 0,
    None => made.skip_depth == 0 && made.parents.len() != 1,
  };
  numbers && skip
}

/// Shape: what one value a question or a derivation keeps charges the store, the heap behind its lists aside: derived
/// from the type.
fn item_bytes() -> u64 {
  u64::try_from(size_of::<Node>()).unwrap_or(u64::MAX)
}

/// One question's (or one derivation's) reading of the values: each value read once into an arena and named by its
/// handle after, what it keeps charged to the store, and what it cost.
struct Walk<'s> {
  store: &'s mut ContentStore,
  nodes: Vec<Node>,
  handles: BTreeMap<ContentId, usize>,
  charged: u64,
  cost: QueryCost,
}

impl<'s> Walk<'s> {
  fn new(store: &'s mut ContentStore) -> Walk<'s> {
    Walk {
      store,
      nodes: Vec::new(),
      handles: BTreeMap::new(),
      charged: 0,
      cost: QueryCost::default(),
    }
  }

  /// Charges the store for `items` more values kept.
  fn charge(&mut self, items: u64) -> Result<(), AncestryRefused> {
    self.charge_bytes(items.saturating_mul(item_bytes()))
  }

  /// Charges the store for `bytes` more kept.
  fn charge_bytes(&mut self, bytes: u64) -> Result<(), AncestryRefused> {
    self.store.charge(bytes)?;
    self.charged = self.charged.saturating_add(bytes);
    Ok(())
  }

  /// The handle of the value `root`, read from the store the first time.
  fn get(&mut self, root: ContentId) -> Result<usize, AncestryRefused> {
    self.cost.visits = self.cost.visits.saturating_add(1);
    if let Some(at) = self.handles.get(&root) {
      return Ok(*at);
    }
    let node = read_node(self.store, root)?;
    self.charge(1)?;
    self.cost.reads = self.cost.reads.saturating_add(1);
    let at = self.nodes.len();
    self.nodes.push(node);
    self.handles.insert(root, at);
    Ok(at)
  }

  /// The value a handle names.
  fn node(&self, at: usize) -> Result<&Node, AncestryRefused> {
    self.nodes.get(at).ok_or(AncestryRefused::Lost)
  }

  /// The value a handle names, as an answer names it.
  fn found(&self, at: usize) -> Result<Found, AncestryRefused> {
    let node = self.node(at)?;
    Ok(Found {
      root: node.root,
      generation: node.made.generation,
      name: node.made.name.clone(),
    })
  }

  /// Gives back what the walk charged, and its answer with its cost; past a refusal of either, the first.
  fn finish<T>(
    self,
    answer: Result<T, AncestryRefused>,
  ) -> Result<(T, QueryCost), AncestryRefused> {
    let discharged = self.store.discharge(self.charged);
    let answer = answer?;
    discharged?;
    Ok((answer, self.cost))
  }
}

/// A tree over the history's versions that a skew jump climbs: the first-parent tree, or the skip tree.
#[derive(Clone, Copy)]
struct Tree {
  depth: fn(&Made) -> u64,
  parent: fn(&Made) -> Option<ContentId>,
  jump: fn(&Made) -> Option<ContentId>,
}

/// The first-parent tree: each version's first parent.
const FIRST_PARENTS: Tree = Tree {
  depth: first_depth,
  parent: first_parent,
  jump: first_jump,
};

/// The skip tree: each version's skip-tree parent (module docs).
const SKIP_TREE: Tree = Tree {
  depth: skip_depth,
  parent: skip_parent,
  jump: skip_jump,
};

fn first_depth(made: &Made) -> u64 {
  made.depth
}

fn first_parent(made: &Made) -> Option<ContentId> {
  made.parents.first().copied()
}

fn first_jump(made: &Made) -> Option<ContentId> {
  made.jump
}

fn skip_depth(made: &Made) -> u64 {
  made.skip_depth
}

fn skip_parent(made: &Made) -> Option<ContentId> {
  made.skip.map(|(parent, _)| parent)
}

fn skip_jump(made: &Made) -> Option<ContentId> {
  made.skip.map(|(_, jump)| jump)
}

/// What a step in a tree needs of a value.
#[derive(Clone, Copy)]
struct Step {
  root: ContentId,
  depth: u64,
  parent: Option<ContentId>,
  jump: Option<ContentId>,
  generation: u64,
}

/// The step of the value `at` in `tree`.
fn step_of(walk: &Walk<'_>, tree: Tree, at: usize) -> Result<Step, AncestryRefused> {
  let node = walk.node(at)?;
  Ok(Step {
    root: node.root,
    depth: (tree.depth)(&node.made),
    parent: (tree.parent)(&node.made),
    jump: (tree.jump)(&node.made),
    generation: node.made.generation,
  })
}

/// The refusal of the value `node`.
fn malformed(node: ContentId) -> AncestryRefused {
  AncestryRefused::Malformed { node }
}

/// Reads `link`, a tree's parent or jump of a value of generation `above`: refused unless it is at `depth` in the tree
/// and its generation below.
fn follow(
  walk: &mut Walk<'_>,
  tree: Tree,
  link: ContentId,
  depth: u64,
  above: u64,
) -> Result<usize, AncestryRefused> {
  let next = walk.get(link)?;
  let step = step_of(walk, tree, next)?;
  if step.depth != depth || step.generation >= above {
    return Err(malformed(link));
  }
  Ok(next)
}

/// The ancestor of `at` at `depth` in `tree`, by Myers' FIND: a jump whenever it lands at or above the depth, else the
/// parent; from depth D at most 3⌊lg(D + 1)⌋ - 2 visits to a depth past the root \[A: Myers 1983, Corollary 5\], and to
/// the root a jump for each unit of D's skew-binary digits, at most ⌊lg(D + 1)⌋ + 1: at most L either way.
fn ancestor_at(
  walk: &mut Walk<'_>,
  tree: Tree,
  mut at: usize,
  depth: u64,
) -> Result<usize, AncestryRefused> {
  loop {
    let step = step_of(walk, tree, at)?;
    if step.depth == depth {
      return Ok(at);
    }
    let below = step
      .depth
      .checked_sub(1)
      .filter(|_| step.depth > depth)
      .ok_or(malformed(step.root))?;
    let jumped = skew_jump_depth(step.depth);
    let (link, lands) = if jumped < depth {
      (step.parent, below)
    } else {
      (step.jump, jumped)
    };
    let link = link.ok_or(malformed(step.root))?;
    at = follow(walk, tree, link, lands, step.generation)?;
  }
}

/// Where two values meet in a tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Meet {
  /// They are one value.
  Same,
  /// The first is an ancestor of the second.
  FirstBelow,
  /// The second is an ancestor of the first.
  SecondBelow,
  /// Their chains meet at `lowest`, the first's through `first` and the second's through `second`, its children.
  Branch {
    lowest: usize,
    first: usize,
    second: usize,
  },
  /// They have no common ancestor in the tree.
  Apart,
}

/// Where `one` and `two` meet in `tree`: the deeper brought to the other's depth (at most L visits), then both climbed by
/// Myers' rule, a jump while their jumps differ, else a step: a search toward the depth under their meeting point and one
/// step more, two values each, at most 2L visits; at most 3L in all.
fn meet(walk: &mut Walk<'_>, tree: Tree, one: usize, two: usize) -> Result<Meet, AncestryRefused> {
  let (deep_one, deep_two) = (
    step_of(walk, tree, one)?.depth,
    step_of(walk, tree, two)?.depth,
  );
  let first = ancestor_at(walk, tree, one, deep_one.min(deep_two))?;
  let second = ancestor_at(walk, tree, two, deep_one.min(deep_two))?;
  if first == second {
    return Ok(match deep_one.cmp(&deep_two) {
      Ordering::Equal => Meet::Same,
      Ordering::Less => Meet::FirstBelow,
      Ordering::Greater => Meet::SecondBelow,
    });
  }
  climb(walk, tree, first, second)
}

/// Climbs two distinct values at one depth until they meet (see [`meet`]).
fn climb(
  walk: &mut Walk<'_>,
  tree: Tree,
  mut first: usize,
  mut second: usize,
) -> Result<Meet, AncestryRefused> {
  loop {
    let (one, two) = (step_of(walk, tree, first)?, step_of(walk, tree, second)?);
    let Some(below) = one.depth.checked_sub(1) else {
      return Ok(Meet::Apart);
    };
    let (links, lands) = if one.jump == two.jump {
      ((one.parent, two.parent), below)
    } else {
      ((one.jump, two.jump), skew_jump_depth(one.depth))
    };
    let (Some(link_one), Some(link_two)) = links else {
      return Err(malformed(one.root));
    };
    let next_one = follow(walk, tree, link_one, lands, one.generation)?;
    let next_two = follow(walk, tree, link_two, lands, two.generation)?;
    if next_one == next_two {
      return Ok(Meet::Branch {
        lowest: next_one,
        first,
        second,
      });
    }
    (first, second) = (next_one, next_two);
  }
}

/// The lowest common ancestor of `one` and `two` in `tree`, if they have one.
fn lowest_common(
  walk: &mut Walk<'_>,
  tree: Tree,
  one: usize,
  two: usize,
) -> Result<Option<usize>, AncestryRefused> {
  Ok(match meet(walk, tree, one, two)? {
    Meet::Same | Meet::FirstBelow => Some(one),
    Meet::SecondBelow => Some(two),
    Meet::Branch { lowest, .. } => Some(lowest),
    Meet::Apart => None,
  })
}

/// The lowest skip-tree ancestor of `at`, itself included, whose generation is at least `floor` (`at`'s is): a jump
/// when its target is at least `floor`, else the parent when it is. The moves are a search toward the answer's depth,
/// at most L, and each move and the last look read at most two values: at most 2L + 2 visits.
fn skip_lowest(walk: &mut Walk<'_>, mut at: usize, floor: u64) -> Result<usize, AncestryRefused> {
  loop {
    let step = step_of(walk, SKIP_TREE, at)?;
    let (Some(parent), Some(jump)) = (step.parent, step.jump) else {
      return Ok(at);
    };
    let jumped = follow(
      walk,
      SKIP_TREE,
      jump,
      skew_jump_depth(step.depth),
      step.generation,
    )?;
    if walk.node(jumped)?.made.generation >= floor {
      at = jumped;
      continue;
    }
    if jump == parent {
      return Ok(at);
    }
    let below = step.depth.checked_sub(1).ok_or(malformed(step.root))?;
    let stepped = follow(walk, SKIP_TREE, parent, below, step.generation)?;
    if walk.node(stepped)?.made.generation < floor {
      return Ok(at);
    }
    at = stepped;
  }
}

/// A version's value from its parents' values (module docs): its generation, its depths, its jumps by Myers' rule, its
/// run bottom and its skip-tree parent.
fn made(
  walk: &mut Walk<'_>,
  parents: &[ContentId],
  name: Vec<u8>,
) -> Result<Made, AncestryRefused> {
  let mut handles = Vec::with_capacity(parents.len());
  for parent in parents {
    handles.push(walk.get(*parent)?);
  }
  let Some(&first) = handles.first() else {
    return Ok(Made::root(name));
  };
  let mut highest = 0_u64;
  for at in &handles {
    highest = highest.max(walk.node(*at)?.made.generation);
  }
  let generation = highest.checked_add(1).ok_or(AncestryRefused::Exhausted)?;
  let depth = step_of(walk, FIRST_PARENTS, first)?
    .depth
    .checked_add(1)
    .ok_or(AncestryRefused::Exhausted)?;
  let jump = pushed(walk, FIRST_PARENTS, first)?;
  let bottom = if handles.len() == 1 {
    Some(bottom_link(walk, first)?)
  } else {
    None
  };
  let (skip, skip_depth) = skip_of(walk, &handles)?;
  Ok(Made {
    parents: parents.to_vec(),
    jump: Some(jump),
    bottom,
    skip,
    generation,
    depth,
    skip_depth,
    name,
  })
}

/// The run bottom of a child whose one parent is `parent`: the parent's own when the parent has one parent, else the
/// parent, a merge or a root.
fn bottom_link(walk: &Walk<'_>, parent: usize) -> Result<ContentId, AncestryRefused> {
  let node = walk.node(parent)?;
  match (node.made.parents.len(), node.made.bottom) {
    (1, Some(bottom)) => Ok(bottom),
    (1, None) => Err(malformed(node.root)),
    _ => Ok(node.root),
  }
}

/// The jump of a child of `parent` in `tree`, by Myers' PUSH \[A: Myers 1983, p. 244\]: the parent itself when the
/// child's jump depth is the parent's depth, else (Lemma 3) the parent's jump's jump.
fn pushed(walk: &mut Walk<'_>, tree: Tree, parent: usize) -> Result<ContentId, AncestryRefused> {
  let step = step_of(walk, tree, parent)?;
  let depth = step
    .depth
    .checked_add(1)
    .ok_or(AncestryRefused::Exhausted)?;
  let target = skew_jump_depth(depth);
  if target == step.depth {
    return Ok(step.root);
  }
  let jump = step.jump.ok_or(malformed(step.root))?;
  let then = follow(
    walk,
    tree,
    jump,
    skew_jump_depth(step.depth),
    step.generation,
  )?;
  let then = step_of(walk, tree, then)?;
  if skew_jump_depth(then.depth) != target {
    return Err(malformed(jump));
  }
  if then.depth == 0 {
    return Ok(then.root);
  }
  then.jump.ok_or(malformed(then.root))
}

/// The skip-tree parent and jump of a version whose parents are `parents`, and its skip-tree depth: the parents'
/// skip-tree lowest common ancestor, if they have one \[C: Mononoke `core.rs` 79-86\].
fn skip_of(walk: &mut Walk<'_>, parents: &[usize]) -> Result<(SkipLinks, u64), AncestryRefused> {
  let mut parents = parents.iter().copied();
  let Some(mut lowest) = parents.next() else {
    return Ok((None, 0));
  };
  for parent in parents {
    match lowest_common(walk, SKIP_TREE, lowest, parent)? {
      Some(at) => lowest = at,
      None => return Ok((None, 0)),
    }
  }
  let step = step_of(walk, SKIP_TREE, lowest)?;
  let depth = step
    .depth
    .checked_add(1)
    .ok_or(AncestryRefused::Exhausted)?;
  let jump = pushed(walk, SKIP_TREE, lowest)?;
  Ok((Some((step.root, jump)), depth))
}

/// The parents of `at`, read: refused unless each one's generation is below `at`'s.
fn parents_of(walk: &mut Walk<'_>, at: usize) -> Result<Vec<usize>, AncestryRefused> {
  let (links, generation) = {
    let made = &walk.node(at)?.made;
    (made.parents.clone(), made.generation)
  };
  let mut parents = Vec::with_capacity(links.len());
  for link in links {
    let parent = walk.get(link)?;
    if walk.node(parent)?.made.generation >= generation {
      return Err(malformed(link));
    }
    parents.push(parent);
  }
  walk.cost.opened = walk.cost.opened.saturating_add(1);
  let count = u64::try_from(parents.len()).unwrap_or(u64::MAX);
  walk.cost.parents = walk.cost.parents.saturating_add(count);
  Ok(parents)
}

/// Whether `ancestor` is an ancestor of `descendant`, each named by its value's root, a version its own ancestor
/// (module docs, "Bounds"; Mononoke's `is_ancestor` \[C: `lib.rs` 519-531\]). The frontier holds the versions at or
/// above the ancestor's generation still to lower, highest first; each jumps along the skip tree to the lowest version
/// still at or above it, and one whose skip-tree parent lies below it, a merge, is opened once. At most
/// 2 + (1 + P)(2L + 3) values visited, P the parents of the merges opened. Refused for a value that does not read as the
/// type's, or past the store's budget.
pub fn is_ancestor(
  store: &mut ContentStore,
  ancestor: ContentId,
  descendant: ContentId,
) -> Result<(bool, QueryCost), AncestryRefused> {
  let mut walk = Walk::new(store);
  let answer = lowered(&mut walk, ancestor, descendant);
  walk.finish(answer)
}

/// [`is_ancestor`]'s lowering.
fn lowered(
  walk: &mut Walk<'_>,
  ancestor: ContentId,
  descendant: ContentId,
) -> Result<bool, AncestryRefused> {
  let target = walk.get(ancestor)?;
  let floor = walk.node(target)?.made.generation;
  let start = walk.get(descendant)?;
  let mut frontier: BTreeSet<(u64, usize)> = BTreeSet::new();
  let mut opened: BTreeSet<usize> = BTreeSet::new();
  hold(walk, &mut frontier, start, floor)?;
  while let Some((generation, at)) = frontier.pop_last() {
    if at == target {
      return Ok(true);
    }
    if generation > floor && lower(walk, (&mut frontier, &mut opened), (at, target, floor))? {
      return Ok(true);
    }
  }
  Ok(false)
}

/// Lowers `at` to its lowest skip-tree ancestor at or above `floor`: whether that is the target; else a merge above the
/// floor is opened, once, and its parents held.
fn lower(
  walk: &mut Walk<'_>,
  (frontier, opened): (&mut BTreeSet<(u64, usize)>, &mut BTreeSet<usize>),
  (at, target, floor): (usize, usize, u64),
) -> Result<bool, AncestryRefused> {
  let lowest = skip_lowest(walk, at, floor)?;
  if lowest == target {
    return Ok(true);
  }
  if walk.node(lowest)?.made.generation > floor && opened.insert(lowest) {
    walk.charge(1)?;
    for parent in parents_of(walk, lowest)? {
      hold(walk, frontier, parent, floor)?;
    }
  }
  Ok(false)
}

/// Holds `at` on the frontier when its generation is at least `floor`; one at `floor` is held only to be compared.
fn hold(
  walk: &mut Walk<'_>,
  frontier: &mut BTreeSet<(u64, usize)>,
  at: usize,
  floor: u64,
) -> Result<(), AncestryRefused> {
  let generation = walk.node(at)?.made.generation;
  if generation >= floor && frontier.insert((generation, at)) {
    walk.charge(1)?;
    walk.cost.entries = walk.cost.entries.saturating_add(1);
  }
  Ok(())
}

/// An entry of the paint: a version reached, its generation, and its paint.
#[derive(Clone, Copy, Debug)]
struct Entry {
  generation: u64,
  paint: Paint,
}

/// A run's key: its bottom's generation, then its bottom's root.
type RunKey = (u64, ContentId);

/// A run's entries, by their versions' handles.
type Run = BTreeMap<usize, Entry>;

/// The paint's pending entries, a run each, keyed by the run's bottom (its generation, then its root), with the count
/// of entries at each generation and of those painted one side alone, the other alone, or both and not stale.
#[derive(Debug, Default)]
struct Pending {
  runs: BTreeMap<RunKey, Run>,
  generations: BTreeMap<u64, u64>,
  only_one: u64,
  only_two: u64,
  both: u64,
}

impl Pending {
  /// Counts an entry painted `paint`, or (`up` false) stops counting it.
  fn tally(&mut self, paint: Paint, up: bool) {
    let slot = match paint {
      ONE => &mut self.only_one,
      TWO => &mut self.only_two,
      BOTH => &mut self.both,
      _ => return,
    };
    *slot = if up {
      slot.saturating_add(1)
    } else {
      slot.saturating_sub(1)
    };
  }

  /// Paints `at` with `paint`: a new entry in its run, or more paint on the entry it is.
  fn add(&mut self, walk: &mut Walk<'_>, at: usize, paint: Paint) -> Result<(), AncestryRefused> {
    let bottom = bottom_of(walk, at)?;
    let key = (walk.node(bottom)?.made.generation, walk.node(bottom)?.root);
    let generation = walk.node(at)?.made.generation;
    let before = self.paint_entry(key, at, Entry { generation, paint });
    match before {
      Some(before) => self.tally(before, false),
      None => {
        walk.charge(1)?;
        let count = self.generations.entry(generation).or_insert(0);
        *count = count.saturating_add(1);
      }
    }
    self.tally(before.map_or(paint, |before| before | paint), true);
    Ok(())
  }

  /// Paints the entry `at` of the run `key` with `entry`'s paint, or makes it `entry`: the paint it had, if it was
  /// there.
  fn paint_entry(&mut self, key: RunKey, at: usize, entry: Entry) -> Option<Paint> {
    let run = self.runs.entry(key).or_default();
    match run.get_mut(&at) {
      Some(kept) => {
        let before = kept.paint;
        kept.paint |= entry.paint;
        Some(before)
      }
      None => {
        run.insert(at, entry);
        None
      }
    }
  }

  /// The run with the highest bottom, taken out of the pending entries.
  fn pop(&mut self) -> Option<(RunKey, Run)> {
    let (key, entries) = self.runs.pop_last()?;
    for entry in entries.values() {
      self.tally(entry.paint, false);
      if let Some(count) = self.generations.get_mut(&entry.generation) {
        *count = count.saturating_sub(1);
        if *count == 0 {
          self.generations.remove(&entry.generation);
        }
      }
    }
    Some((key, entries))
  }

  /// The greatest generation of a pending entry below `generation`, or zero.
  fn highest_below(&self, generation: u64) -> u64 {
    self
      .generations
      .range(..generation)
      .next_back()
      .map_or(0, |(below, _)| *below)
  }

  /// git's two exits \[C: `paint-down-to-common.adoc`, "Termination" 2 and 5\]: no new merge base can come once no entry
  /// is a pending candidate and one side has no entry of its own (only stale entries left is the case of both sides).
  fn exhausted(&self) -> bool {
    self.both == 0 && (self.only_one == 0 || self.only_two == 0)
  }

  /// The bisect paint's exit: no entry is painted bad alone, so every candidate has been joined.
  fn candidates_joined(&self) -> bool {
    self.only_one == 0
  }
}

/// The run bottom of `at`: the nearest merge or root on its first-parent chain, itself if it is one; refused unless the
/// run between them climbs one generation and one depth a version.
fn bottom_of(walk: &mut Walk<'_>, at: usize) -> Result<usize, AncestryRefused> {
  let (root, generation, depth, link) = {
    let node = walk.node(at)?;
    (
      node.root,
      node.made.generation,
      node.made.depth,
      node.made.bottom,
    )
  };
  let Some(link) = link else {
    return Ok(at);
  };
  let bottom = walk.get(link)?;
  let made = &walk.node(bottom)?.made;
  let rise = generation.checked_sub(made.generation);
  let follows = made.parents.len() != 1
    && rise.is_some_and(|rise| rise > 0)
    && rise == depth.checked_sub(made.depth);
  if follows {
    Ok(bottom)
  } else {
    Err(malformed(root))
  }
}

/// One run's entries joined into a tree over the first-parent tree: the run's bottom, the entries and the versions
/// where their chains meet, each with its parent among them and its paint (its own and every one's above it).
#[derive(Debug, Default)]
struct Joined {
  bottom: usize,
  parents: BTreeMap<usize, usize>,
  painted: BTreeMap<usize, Paint>,
}

impl Joined {
  /// The paint of `at`, a version of the tree.
  fn paint(&self, at: usize) -> Paint {
    self.painted.get(&at).copied().unwrap_or(0)
  }
}

/// A top-down merge sort under a comparison that may be refused: at most k⌈lg k⌉ comparisons for k items.
fn sorted<T, E>(
  mut items: Vec<T>,
  compare: &mut impl FnMut(&T, &T) -> Result<Ordering, E>,
) -> Result<Vec<T>, E> {
  if items.len() < 2 {
    return Ok(items);
  }
  let half = items.len().checked_div(2).unwrap_or(0);
  let right = sorted(items.split_off(half), compare)?;
  let left = sorted(items, compare)?;
  let mut merged = Vec::with_capacity(left.len().saturating_add(right.len()));
  let mut left = left.into_iter().peekable();
  let mut right = right.into_iter().peekable();
  loop {
    let take_left = match (left.peek(), right.peek()) {
      (Some(one), Some(two)) => compare(one, two)? != Ordering::Greater,
      (Some(_), None) => true,
      (None, Some(_)) => false,
      (None, None) => return Ok(merged),
    };
    merged.extend(if take_left { left.next() } else { right.next() });
  }
}

/// The meets a joining has found, by pair: each pair's lowest common ancestor in the first-parent tree.
type Meets = BTreeMap<(usize, usize), usize>;

/// Two versions of one run in a depth-first order from its bottom: an ancestor first, else the one whose chain leaves
/// their meeting point by the lesser child. Records the meeting point.
fn preorder(
  walk: &mut Walk<'_>,
  meets: &mut Meets,
  one: usize,
  two: usize,
) -> Result<Ordering, AncestryRefused> {
  let pair = (one.min(two), one.max(two));
  let (lowest, order) = match meet(walk, FIRST_PARENTS, one, two)? {
    Meet::Same => return Ok(Ordering::Equal),
    Meet::FirstBelow => (one, Ordering::Less),
    Meet::SecondBelow => (two, Ordering::Greater),
    Meet::Branch {
      lowest,
      first,
      second,
    } => (lowest, walk.node(first)?.root.cmp(&walk.node(second)?.root)),
    Meet::Apart => return Err(malformed(walk.node(one)?.root)),
  };
  meets.insert(pair, lowest);
  Ok(order)
}

/// The meeting point of `top`, a version of the run whose bottom is `bottom`, with `last`: the bottom itself when
/// `last` is the bottom (every version of a run is above it), else found again or computed.
fn met(
  walk: &mut Walk<'_>,
  meets: &mut Meets,
  (bottom, top, last): (usize, usize, usize),
) -> Result<usize, AncestryRefused> {
  if last == bottom {
    return Ok(bottom);
  }
  let pair = (top.min(last), top.max(last));
  if let Some(at) = meets.get(&pair) {
    return Ok(*at);
  }
  let at = lowest_common(walk, FIRST_PARENTS, top, last)?.ok_or(AncestryRefused::Lost)?;
  meets.insert(pair, at);
  Ok(at)
}

/// Joins one run's entries (module docs, [`merge_bases`]): sorted in depth-first order, then the standard stack
/// construction over the sorted entries, which meets each with the stack's top and closes the chains below the meeting
/// point: a virtual tree, every meeting point of two entries among its versions.
fn joined(walk: &mut Walk<'_>, bottom: usize, entries: &Run) -> Result<Joined, AncestryRefused> {
  let mut meets = Meets::new();
  let tops: Vec<usize> = entries.keys().copied().filter(|at| *at != bottom).collect();
  let tops = sorted(tops, &mut |one, two| preorder(walk, &mut meets, *one, *two))?;
  let mut joined = Joined {
    bottom,
    ..Joined::default()
  };
  let mut stack = vec![bottom];
  for top in tops {
    join_one(walk, &mut meets, &mut stack, &mut joined, top)?;
  }
  close_down_to(walk, &mut stack, &mut joined, 0)?;
  walk.charge(u64::try_from(joined.parents.len()).unwrap_or(u64::MAX))?;
  paint_joined(walk, &mut joined, entries)?;
  Ok(joined)
}

/// Closes the stack's top onto the version under it while that version's depth is at least `depth`.
fn close_down_to(
  walk: &Walk<'_>,
  stack: &mut Vec<usize>,
  joined: &mut Joined,
  depth: u64,
) -> Result<(), AncestryRefused> {
  while let [.., below, above] = stack.as_slice() {
    let (below, above) = (*below, *above);
    if walk.node(below)?.made.depth < depth {
      break;
    }
    joined.parents.insert(above, below);
    stack.pop();
  }
  Ok(())
}

/// One step of the stack construction: `top` meets the stack's top at `lowest`; every version on the stack above
/// `lowest` is closed onto the one under it, `lowest` takes the top's place if it is new, and `top` is pushed.
fn join_one(
  walk: &mut Walk<'_>,
  meets: &mut Meets,
  stack: &mut Vec<usize>,
  joined: &mut Joined,
  top: usize,
) -> Result<(), AncestryRefused> {
  let last = *stack.last().ok_or(AncestryRefused::Lost)?;
  let lowest = met(walk, meets, (joined.bottom, top, last))?;
  if lowest != last {
    let depth = walk.node(lowest)?.made.depth;
    close_down_to(walk, stack, joined, depth)?;
    let last = *stack.last().ok_or(AncestryRefused::Lost)?;
    if last != lowest {
      joined.parents.insert(last, lowest);
      stack.pop();
      stack.push(lowest);
    }
  }
  stack.push(top);
  Ok(())
}

/// Paints a joined run: each version its entry's paint, then each its children's, deepest first.
fn paint_joined(
  walk: &Walk<'_>,
  joined: &mut Joined,
  entries: &Run,
) -> Result<(), AncestryRefused> {
  let mut members: Vec<(u64, usize)> = Vec::with_capacity(joined.parents.len().saturating_add(1));
  members.push((walk.node(joined.bottom)?.made.depth, joined.bottom));
  for child in joined.parents.keys() {
    members.push((walk.node(*child)?.made.depth, *child));
  }
  for (_, at) in &members {
    let own = entries.get(at).map_or(0, |entry| entry.paint);
    joined.painted.insert(*at, own);
  }
  members.sort_unstable();
  for (_, at) in members.iter().rev() {
    if let Some(parent) = joined.parents.get(at).copied() {
      let paint = joined.paint(*at) | joined.paint(parent);
      joined.painted.insert(parent, paint);
    }
  }
  Ok(())
}

/// The run's paint carried past its bottom (module docs): a merge's paint jumps, when `may_jump`, to its lowest
/// skip-tree ancestor at or above the greatest generation still pending below it, past a region no other entry
/// reaches; when no jump is left, its parents are painted.
fn carry(
  walk: &mut Walk<'_>,
  pending: &mut Pending,
  bottom: usize,
  paint: Paint,
  may_jump: bool,
) -> Result<(), AncestryRefused> {
  let (is_root, generation) = {
    let node = walk.node(bottom)?;
    (node.made.parents.is_empty(), node.made.generation)
  };
  if is_root {
    return Ok(());
  }
  if may_jump {
    let lowest = skip_lowest(walk, bottom, pending.highest_below(generation))?;
    if lowest != bottom {
      walk.cost.jumps = walk.cost.jumps.saturating_add(1);
      return pending.add(walk, lowest, paint);
    }
  }
  for parent in parents_of(walk, bottom)? {
    pending.add(walk, parent, paint)?;
  }
  Ok(())
}

/// The next run of the paint, joined, unless `done` says the paint is over.
fn next_run(
  walk: &mut Walk<'_>,
  pending: &mut Pending,
  done: fn(&Pending) -> bool,
) -> Result<Option<Joined>, AncestryRefused> {
  if done(pending) {
    return Ok(None);
  }
  let Some(((_, bottom), entries)) = pending.pop() else {
    return Ok(None);
  };
  let bottom = walk.get(bottom)?;
  walk.cost.runs = walk.cost.runs.saturating_add(1);
  let count = u64::try_from(entries.len()).unwrap_or(u64::MAX);
  walk.cost.entries = walk.cost.entries.saturating_add(count);
  joined(walk, bottom, &entries).map(Some)
}

/// The merge bases of `one` and `two`, each named by its value's root: every common ancestor that is no ancestor of
/// another, as `git merge-base --all` answers \[C: `paint-down-to-common.adoc` 14-15\], the greatest generation first,
/// then by name; empty when they have none. The first is git's single answer, "necessarily the highest-generation
/// common ancestor". The paint is git's, over runs (module docs, "Bounds"): with E entries joined in T runs and P
/// parents taken in, at most 4 + T(2L + 4) + 3L·E(⌈lg E⌉ + 1) + 2P values visited.
pub fn merge_bases(
  store: &mut ContentStore,
  one: ContentId,
  two: ContentId,
) -> Result<(Vec<Found>, QueryCost), AncestryRefused> {
  let mut walk = Walk::new(store);
  let answer = bases(&mut walk, one, two);
  walk.finish(answer)
}

/// [`merge_bases`]'s paint.
fn bases(
  walk: &mut Walk<'_>,
  one: ContentId,
  two: ContentId,
) -> Result<Vec<Found>, AncestryRefused> {
  let mut pending = Pending::default();
  let first = walk.get(one)?;
  pending.add(walk, first, ONE)?;
  let second = walk.get(two)?;
  pending.add(walk, second, TWO)?;
  let mut found = Vec::new();
  while let Some(joined) = next_run(walk, &mut pending, Pending::exhausted)? {
    bases_in(walk, &joined, &mut found)?;
    let paint = joined.paint(joined.bottom);
    let paint = if paint & BOTH == BOTH {
      paint | STALE
    } else {
      paint
    };
    carry(walk, &mut pending, joined.bottom, paint, true)?;
  }
  found.sort_by(|one, two| {
    two
      .generation
      .cmp(&one.generation)
      .then_with(|| one.name.cmp(&two.name))
  });
  Ok(found)
}

/// The merge bases a joined run holds: its versions painted both sides and not stale, with no child painted both.
fn bases_in(
  walk: &Walk<'_>,
  joined: &Joined,
  found: &mut Vec<Found>,
) -> Result<(), AncestryRefused> {
  let mut under_both = BTreeSet::new();
  for (child, parent) in &joined.parents {
    if joined.paint(*child) & BOTH == BOTH {
      under_both.insert(*parent);
    }
  }
  for (at, paint) in &joined.painted {
    if *paint & BOTH == BOTH && *paint & STALE == 0 && !under_both.contains(at) {
      found.push(walk.found(*at)?);
    }
  }
  Ok(())
}

/// Bisect's midpoint (module docs, "Bounds"): among the candidates, the versions `bad` reaches and none of `goods`
/// does (each named by its value's root), the one maximizing `min(weight, candidates - weight)`, its weight the
/// candidates it reaches, itself included \[C: `bisect.c` 187-207, 269-270\]; then the greater generation, then the
/// lesser name. `None` when there are no candidates. git's own stops early within a thousandth of the half and takes
/// the first in its listing among equals; this answer is exact and the same on every tier. Its paint visits what
/// [`merge_bases`]' does, its weights at most P more (its merges' parents), the answer at most L more for each segment
/// tied with it; it charges the store K² bits for K segments.
pub fn bisect(
  store: &mut ContentStore,
  bad: ContentId,
  goods: &[ContentId],
) -> Result<(Option<Midpoint>, QueryCost), AncestryRefused> {
  let mut walk = Walk::new(store);
  let answer = midpoint(&mut walk, bad, goods);
  walk.finish(answer)
}

/// [`bisect`]'s paint, then its weights.
fn midpoint(
  walk: &mut Walk<'_>,
  bad: ContentId,
  goods: &[ContentId],
) -> Result<Option<Midpoint>, AncestryRefused> {
  let mut pending = Pending::default();
  let start = walk.get(bad)?;
  pending.add(walk, start, ONE)?;
  for good in goods {
    let at = walk.get(*good)?;
    pending.add(walk, at, TWO)?;
  }
  let mut runs = Vec::new();
  while let Some(joined) = next_run(walk, &mut pending, Pending::candidates_joined)? {
    let paint = joined.paint(joined.bottom);
    carry(walk, &mut pending, joined.bottom, paint, paint & TWO != 0)?;
    runs.push(joined);
  }
  Segments::of(walk, &runs)?.best(walk)
}

/// A run of candidates: the versions above `below` (a version of its run, or nothing for a bottom) up to `top`, each
/// reaching one more candidate than the one under it.
#[derive(Clone, Copy, Debug)]
struct Segment {
  top: usize,
  below: Option<usize>,
  length: u64,
}

/// Bisect's candidates as segments, each with its top's weight: the candidates the top reaches.
struct Segments {
  segments: Vec<Segment>,
  weights: Vec<u64>,
  total: u64,
}

impl Segments {
  /// The segments of the joined runs and their tops' weights: each top's set of segments is its own with its parent's
  /// in its run, or for a bottom with its parents', as bitsets over the segments, from the lowest runs up.
  fn of(walk: &mut Walk<'_>, runs: &[Joined]) -> Result<Segments, AncestryRefused> {
    let mut segments = Vec::new();
    let mut tops = BTreeMap::new();
    for joined in runs.iter().rev() {
      segments_of(walk, joined, &mut segments, &mut tops)?;
    }
    walk.cost.segments = u64::try_from(segments.len()).unwrap_or(u64::MAX);
    let word_bits = usize::try_from(u64::BITS).unwrap_or(usize::MAX);
    let words = segments.len().div_ceil(word_bits);
    let word_bytes = u64::try_from(size_of::<u64>()).unwrap_or(u64::MAX);
    let bytes = u64::try_from(words.saturating_mul(segments.len()))
      .unwrap_or(u64::MAX)
      .saturating_mul(word_bytes);
    walk.charge_bytes(bytes)?;
    let sets = reached_sets(walk, &segments, &tops, words)?;
    let weights = sets.iter().map(|set| weight_of(set, &segments)).collect();
    let total = segments
      .iter()
      .fold(0_u64, |sum, segment| sum.saturating_add(segment.length));
    Ok(Segments {
      segments,
      weights,
      total,
    })
  }

  /// The weights segment `index` holds, its lowest version's and its top's.
  fn range(&self, index: usize) -> Result<(u64, u64), AncestryRefused> {
    let segment = self.segments.get(index).ok_or(AncestryRefused::Lost)?;
    let highest = *self.weights.get(index).ok_or(AncestryRefused::Lost)?;
    let lowest = highest.saturating_sub(segment.length.saturating_sub(1));
    Ok((lowest, highest))
  }

  /// The segments holding a version of the best rank, the score `min(weight, candidates - weight)` then the
  /// generation, each with that version's weight.
  fn ties(&self, walk: &Walk<'_>) -> Result<Vec<(usize, u64)>, AncestryRefused> {
    let mut best: Option<(u64, u64)> = None;
    let mut ties: Vec<(usize, u64)> = Vec::new();
    for index in 0..self.segments.len() {
      let (weight, generation) = self.best_on(walk, index)?;
      let rank = (weight.min(self.total.saturating_sub(weight)), generation);
      if best.is_none_or(|best| rank > best) {
        best = Some(rank);
        ties.clear();
      }
      if best == Some(rank) {
        ties.push((index, weight));
      }
    }
    Ok(ties)
  }

  /// The best candidate (see [`bisect`]): among the versions of the best rank, the least name.
  fn best(&self, walk: &mut Walk<'_>) -> Result<Option<Midpoint>, AncestryRefused> {
    let mut answer: Option<Midpoint> = None;
    for (index, weight) in self.ties(walk)? {
      let found = self.version_at(walk, index, weight)?;
      if answer
        .as_ref()
        .is_none_or(|answer| found.name < answer.found.name)
      {
        answer = Some(Midpoint {
          found,
          weight,
          candidates: self.total,
        });
      }
    }
    Ok(answer)
  }

  /// The best weight on segment `index` and its version's generation: the weight nearest half the candidates, the
  /// greater of the two nearest when they tie.
  fn best_on(&self, walk: &Walk<'_>, index: usize) -> Result<(u64, u64), AncestryRefused> {
    let (lowest, highest) = self.range(index)?;
    let half = self.total.checked_div(2).unwrap_or(0);
    let mut best = (0_u64, lowest);
    for wanted in [half, self.total.saturating_sub(half)] {
      let weight = wanted.clamp(lowest, highest);
      let score = weight.min(self.total.saturating_sub(weight));
      if (score, weight) > best {
        best = (score, weight);
      }
    }
    let segment = self.segments.get(index).ok_or(AncestryRefused::Lost)?;
    let top = walk.node(segment.top)?.made.generation;
    Ok((best.1, top.saturating_sub(highest.saturating_sub(best.1))))
  }

  /// The version on segment `index` whose weight is `weight`: its top, or the top's first-parent ancestor that many
  /// versions under it.
  fn version_at(
    &self,
    walk: &mut Walk<'_>,
    index: usize,
    weight: u64,
  ) -> Result<Found, AncestryRefused> {
    let (_, highest) = self.range(index)?;
    let segment = self.segments.get(index).ok_or(AncestryRefused::Lost)?;
    let depth = walk
      .node(segment.top)?
      .made
      .depth
      .saturating_sub(highest.saturating_sub(weight));
    let at = ancestor_at(walk, FIRST_PARENTS, segment.top, depth)?;
    walk.found(at)
  }
}

/// The segments of one joined run, appended: its bottom alone when it is a candidate, and each other version painted
/// bad alone, from the version under it in the run.
fn segments_of(
  walk: &Walk<'_>,
  joined: &Joined,
  segments: &mut Vec<Segment>,
  tops: &mut BTreeMap<usize, usize>,
) -> Result<(), AncestryRefused> {
  let mut members: Vec<(u64, usize)> = Vec::with_capacity(joined.painted.len());
  for at in joined.painted.keys() {
    members.push((walk.node(*at)?.made.depth, *at));
  }
  members.sort_unstable();
  for (depth, at) in members {
    if joined.paint(at) != ONE {
      continue;
    }
    let below = joined.parents.get(&at).copied();
    let length = match below {
      Some(below) => depth.saturating_sub(walk.node(below)?.made.depth),
      None => 1,
    };
    tops.insert(at, segments.len());
    segments.push(Segment {
      top: at,
      below,
      length,
    });
  }
  Ok(())
}

/// Each segment's set of the segments its top reaches, as bitsets of `words` words: its own, with its run parent's
/// for a segment over another version, or with its parents' for a bottom.
fn reached_sets(
  walk: &mut Walk<'_>,
  segments: &[Segment],
  tops: &BTreeMap<usize, usize>,
  words: usize,
) -> Result<Bitsets, AncestryRefused> {
  let mut sets: Bitsets = Vec::with_capacity(segments.len());
  for (index, segment) in segments.iter().enumerate() {
    let mut set = vec![0_u64; words];
    set_bit(&mut set, index);
    let under = match segment.below {
      Some(below) => vec![below],
      None => {
        let links = walk.node(segment.top)?.made.parents.clone();
        let mut parents = Vec::with_capacity(links.len());
        for link in links {
          parents.push(walk.get(link)?);
        }
        parents
      }
    };
    for at in under {
      if let Some(reached) = tops.get(&at).and_then(|index| sets.get(*index)) {
        for (word, more) in set.iter_mut().zip(reached) {
          *word |= *more;
        }
      }
    }
    sets.push(set);
  }
  Ok(sets)
}

/// One bitset over the segments for each segment.
type Bitsets = Vec<Vec<u64>>;

/// Sets bit `index` of `set`.
fn set_bit(set: &mut [u64], index: usize) {
  let word_bits = usize::try_from(u64::BITS).unwrap_or(usize::MAX);
  let word = index.checked_div(word_bits).unwrap_or(0);
  let bit = u32::try_from(index.checked_rem(word_bits).unwrap_or(0)).unwrap_or(0);
  if let Some(word) = set.get_mut(word) {
    *word |= 1_u64.checked_shl(bit).unwrap_or(0);
  }
}

/// The candidates a set of segments holds: their lengths, summed.
fn weight_of(set: &[u64], segments: &[Segment]) -> u64 {
  let word_bits = usize::try_from(u64::BITS).unwrap_or(usize::MAX);
  let mut weight = 0_u64;
  for (word_index, word) in set.iter().enumerate() {
    let mut word = *word;
    while word != 0 {
      let bit = usize::try_from(word.trailing_zeros()).unwrap_or(0);
      word &= word.saturating_sub(1);
      let index = word_index.saturating_mul(word_bits).saturating_add(bit);
      let length = segments.get(index).map_or(0, |segment| segment.length);
      weight = weight.saturating_add(length);
    }
  }
  weight
}
