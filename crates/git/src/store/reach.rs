//! A-614's D(k) and reached sets as a type of derived data (A-647 parts 2 to 4; A-645 "Derived data, one mechanism";
//! §4.17.2 "The repository in git's terms" item 3).
//!
//! **The type** ([`ReachSets`]). The value of a ref's version (its green and its index `k`) is two sets of positions:
//! the objects its head reaches, and D(k), what the version brought. Both are what A-614's admission computes, and the
//! type computes them with the same walks over the committed index, from the version's record and its parents' values
//! alone:
//! - a fast-forward: D(k) is the walk from the head that stops at every object version `k - 1` reached, and the reached
//!   set is that set grown by D(k);
//! - a rewrite or a create: the reached set is the set of the version it was remade from and the walk beyond it; D(k)
//!   is the walk that stops at version `k - 1`'s set, or, with no head before it, everything reached;
//! - a delete: both empty.
//!
//! Which version a set was remade from is found among the greens' heads when the move is admitted (the green whose head
//! the new head descends from, at its version then), so the store records it with the version (`Followed`): a
//! derivation reads only recorded history. A version's parents in this history (the `Graph` the repository is) are
//! version `k - 1` when it had a head, then the version its set was remade from.
//!
//! **Kept with no second walk, and the store's one copy** (A-647 part 3). The store's commit puts each version's sets,
//! as its admission walked them, as the version's value, held by the version's record; the store serves every
//! version's D(k) and reached set from it ([`Repository::brought`], [`Repository::reached`]) and keeps the sets nowhere
//! else. [`Repository::reach_value`] hands the owner a ref's newest value, which it keeps under (type, version) as it
//! commits (`Derivations::keep`), and a tier that lacks a value derives it with a fill, the same value by identity. A
//! derivation's walks are the admission's own and counted as any walk is ([`super::StoreCounters`]).
//!
//! **A version stores what it adds** (A-647 part 4, format 3). Each set is a tree of derived nodes over fixed blocks of
//! positions ([`Shape`], derived from the store's window: a block is the most positions whose runs fit one window when
//! every other position is a run of its own, and an inner node names as many children as one window holds). A leaf
//! holds a block's runs, each relative to the block, so two blocks of the same runs are one node by identity; an inner
//! node of level `l` names the children of its range that hold any of the set, its payload its level and a bit for
//! each child slot; a range the set holds whole is one node a level, whatever its place, so a run over many blocks
//! costs a node a level. The root's level is the least whose range covers the set's last block. The tree is a function
//! of the set and the window alone, never of the history that made it, so the same set is the same root on every tier;
//! and the store keeps a node once, whoever puts it, so a version's value adds only the nodes no earlier set had: the
//! leaves of the blocks its D(k) touches and the inner nodes above them, never its reached set again. Format 2 (part
//! 3) kept each version's sets as one sorted sequence of runs, which the store cut into windows: a version whose D(k)
//! fell below its newest run (a merge of an older root) moved every window after it, and so stored its reached set
//! again whole.
//!
//! **A value's bytes**: a derived node naming its reached set's tree's root, then D(k)'s, each left out for an empty
//! set; its payload a tag, a byte whose bits say which it names, then its trees' shape (a block's positions, then an
//! inner node's children, eight bytes each, little-endian), so a value reads alone whatever window made it. A leaf's
//! payload is a tag and its runs, each a start and an end relative to its block (eight bytes each, little-endian),
//! ascending with a gap between each two; an inner node's, a tag, its level and its bitmap. Nodes in any other form,
//! and a child of another level than one below its parent, are refused. The window is part of the type's name, since
//! the shape is.

use std::collections::BTreeMap;
use std::mem::size_of;

use slates_vcs::content::encode_derived;
use slates_vcs::derived::{DerivedType, Graph, Mode};
use slates_vcs::{ContentId, ContentRefused, ContentStore};

use super::{Followed, Repository, StoreRefused, Version};
use crate::object_name::ObjectId;
use crate::store::{Positions, Push};

/// Format: the type's name and its format's version (module docs: 3, each set a tree its value names); the window its
/// trees are shaped by follows, eight bytes, big-endian.
const TYPE_NAME: &[u8] = b"slates-git reach sets 3 ";

/// Format: the bytes of a count of runs, and of a run's start and of its end, little-endian.
const WORD_BYTES: usize = size_of::<u64>();

/// Format: the bytes of one run: its start and its end.
const RUN_BYTES: usize = size_of::<(u64, u64)>();

/// Format: the first byte of a leaf's payload: the runs of one block of positions.
const LEAF: u8 = b'L';

/// Format: the first byte of an inner node's payload: its level, then a bit for each child slot.
const INNER: u8 = b'I';

/// A set's tree's shape (module docs), derived from the store's window: the positions a leaf covers and the children an
/// inner node names, each the most one window holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Shape {
  /// Positions a leaf covers: two for each run a leaf of one window holds, so a block of positions each apart fits.
  pub(crate) block: u64,
  /// Children an inner node names.
  pub(crate) fanout: u64,
}

impl Shape {
  /// The shape for a store of `window`; `None` for a window too short for a leaf of one run or an inner node of two
  /// children.
  pub(crate) fn of(window: usize) -> Option<Shape> {
    let leaf = encode_derived(&[], &[LEAF])?.len();
    let runs = window.checked_sub(leaf)?.checked_div(RUN_BYTES)?;
    let block = u64::try_from(runs.checked_mul(2)?).ok()?;
    let fanout = u64::try_from(fanout_for(window)?).ok()?;
    (block >= 2 && fanout >= 2).then_some(Shape { block, fanout })
  }

  /// The least level whose node, from block 0, covers block `block`: the root's level of a set whose last block it is.
  pub(crate) fn level_of(self, block: u64) -> u32 {
    let (mut level, mut covered) = (0_u32, 1_u64);
    while covered <= block {
      covered = covered.saturating_mul(self.fanout);
      level = level.saturating_add(1);
    }
    level
  }
}

/// An inner node's payload's length for `fanout` children: its tag, its level, and a bit for each child slot.
fn inner_payload_len(fanout: usize) -> Option<usize> {
  fanout.div_ceil(8).checked_add(2)
}

/// The most children an inner node of one `window` names, by the store's own layout of a derived node.
fn fanout_for(window: usize) -> Option<usize> {
  let count = encode_derived(&[], &[])?.len();
  let name = encode_derived(&[ContentId([0; 32])], &[])?
    .len()
    .checked_sub(count)?;
  let bytes = |fanout: usize| {
    count
      .checked_add(name.checked_mul(fanout)?)?
      .checked_add(inner_payload_len(fanout)?)
  };
  let mut fanout = window.checked_div(name)?;
  while fanout > 0 && bytes(fanout).is_none_or(|len| len > window) {
    fanout = fanout.saturating_sub(1);
  }
  Some(fanout)
}

/// Why the type refused to derive a version. Each leaves the store as it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReachRefused {
  /// The repository holds no version `version` of `green`.
  NoVersion {
    /// The green.
    green: u64,
    /// The version's index.
    version: u64,
  },
  /// The parents' values given are not the ones the version's record names: too few, or too many.
  Parents {
    /// The green.
    green: u64,
    /// The version's index.
    version: u64,
  },
  /// The value `node` does not read as the type's two sets.
  Malformed {
    /// The node.
    node: ContentId,
  },
  /// A walk met `id`, which the index does not hold: the version's objects are not all committed.
  Unindexed {
    /// The object.
    id: ObjectId,
  },
  /// The store refused a walk.
  Store(StoreRefused),
  /// The content store refused the value.
  Content(ContentRefused),
}

/// A version's two sets: what its head reaches, and D(k), what it brought.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sets {
  /// The positions its head reaches.
  pub reached: Positions,
  /// D(k): the positions it brought.
  pub brought: Positions,
}

impl Sets {
  /// The sets the value `root` holds, read from `store` (module docs): each set's tree by the shape the value records;
  /// `None` when it is no value of the type.
  pub fn read(store: &ContentStore, root: &ContentId) -> Option<Sets> {
    let node = store.derived(root)?;
    let (&VALUE, rest) = node.payload().split_first()? else {
      return None;
    };
    let (&present, rest) = rest.split_first()?;
    let (block, rest) = rest.split_first_chunk::<WORD_BYTES>()?;
    let fanout = <[u8; WORD_BYTES]>::try_from(rest).ok()?;
    let shape = Shape::recorded(u64::from_le_bytes(*block), u64::from_le_bytes(fanout))?;
    if present & !(REACHED | BROUGHT) != 0 {
      return None;
    }
    let mut names = node.names();
    let mut set = |bit: u8| match present & bit {
      0 => Some(Positions::new()),
      _ => read_tree(store, names.next()?, shape),
    };
    let reached = set(REACHED)?;
    let brought = set(BROUGHT)?;
    names.next().is_none().then_some(Sets { reached, brought })
  }

  /// Puts the sets as the type's value of `shape` (module docs): each set's tree, then the node naming their roots,
  /// held once by the caller. Refused, nothing is kept.
  pub(super) fn put(
    &self,
    store: &mut ContentStore,
    shape: Shape,
  ) -> Result<ContentId, ContentRefused> {
    let reached = put_tree(store, shape, &self.reached)?;
    let brought = match put_tree(store, shape, &self.brought) {
      Ok(brought) => brought,
      Err(refused) => {
        let_go(store, reached.as_slice())?;
        return Err(refused);
      }
    };
    let mut present = 0;
    if reached.is_some() {
      present |= REACHED;
    }
    if brought.is_some() {
      present |= BROUGHT;
    }
    let roots: Vec<ContentId> = reached.into_iter().chain(brought).collect();
    let mut payload = vec![VALUE, present];
    payload.extend_from_slice(&shape.block.to_le_bytes());
    payload.extend_from_slice(&shape.fanout.to_le_bytes());
    let node = store.put_derived(&roots, &payload);
    let released = let_go(store, &roots);
    match (node, released) {
      (Ok(node), Ok(())) => Ok(node.identity),
      (Ok(node), Err(refused)) => {
        store.release(&node.identity)?;
        Err(refused)
      }
      (Err(refused), _) => Err(refused),
    }
  }
}

/// Gives back one hold of each of `nodes`; past a refusal the rest still, and the first refusal is the answer.
fn let_go(store: &mut ContentStore, nodes: &[ContentId]) -> Result<(), ContentRefused> {
  let mut first = Ok(());
  for node in nodes {
    if let Err(refused) = store.release(node) {
      first = first.and(Err(refused));
    }
  }
  first
}

/// The count that would pass its width: the refusal of a tree's arithmetic past `u64`.
fn exhausted() -> ContentRefused {
  ContentRefused::CountExhausted
}

/// The shape for a store of `window`, or the store's own refusal of the least node the tree needs, an inner node of
/// two children, past that window.
pub(super) fn shape_of(window: usize) -> Result<Shape, ContentRefused> {
  Shape::of(window).ok_or_else(|| {
    let least = inner_payload_len(2)
      .and_then(|payload| encode_derived(&[ContentId([0; 32]); 2], &vec![0; payload]))
      .map_or(u64::MAX, |bytes| {
        u64::try_from(bytes.len()).unwrap_or(u64::MAX)
      });
    ContentRefused::PastWindow {
      len: least,
      window: u64::try_from(window).unwrap_or(u64::MAX),
    }
  })
}

/// Puts `set` as its tree of `shape` (module docs), made whole from its runs: the store keeps each node once whatever
/// puts it, so a version stores only the nodes no set before it had. Its root, held once by the caller; `None` for the
/// empty set.
fn put_tree(
  store: &mut ContentStore,
  shape: Shape,
  set: &Positions,
) -> Result<Option<ContentId>, ContentRefused> {
  let runs = set.run_pairs();
  let Some(&(_, end)) = runs.last() else {
    return Ok(None);
  };
  let last = end.checked_sub(1).ok_or_else(exhausted)?;
  let level = shape.level_of(last.checked_div(shape.block).ok_or_else(exhausted)?);
  let mut builder = Builder {
    store,
    shape,
    owned: Vec::new(),
    full: Vec::new(),
  };
  let made = builder.node(level, 0, &runs);
  builder.settle(made)
}

/// A tree being put: the store, the shape, the holds its puts took (each given back as the tree settles, but for its
/// root's), and each level's node of a range the set covers whole, put once.
struct Builder<'s> {
  store: &'s mut ContentStore,
  shape: Shape,
  owned: Vec<ContentId>,
  full: Vec<ContentId>,
}

impl Builder<'_> {
  /// Puts a node, held once by the builder.
  fn put(&mut self, names: &[ContentId], payload: &[u8]) -> Result<ContentId, ContentRefused> {
    let identity = self.store.put_derived(names, payload)?.identity;
    self.owned.push(identity);
    Ok(identity)
  }

  /// The node of `level` whose first block is `first`, holding `runs`, the set's runs within its range; `None` for
  /// none.
  fn node(
    &mut self,
    level: u32,
    first: u64,
    runs: &[(u64, u64)],
  ) -> Result<Option<ContentId>, ContentRefused> {
    if runs.is_empty() {
      return Ok(None);
    }
    let (start, end) = self.shape.range(level, first).ok_or_else(exhausted)?;
    if runs == [(start, end)] {
      return self.full(level).map(Some);
    }
    let Some(below) = level.checked_sub(1) else {
      return self.leaf(start, runs).map(Some);
    };
    let span = self.shape.span(below).ok_or_else(exhausted)?;
    let blocks = self.shape.blocks(below).ok_or_else(exhausted)?;
    let mut names = Vec::new();
    let mut bitmap = vec![0_u8; self.shape.bitmap_len().ok_or_else(exhausted)?];
    for (slot, pieces) in children(runs, start, span).ok_or_else(exhausted)? {
      let child_first = slot
        .checked_mul(blocks)
        .and_then(|offset| first.checked_add(offset))
        .ok_or_else(exhausted)?;
      if let Some(child) = self.node(below, child_first, &pieces)? {
        names.push(child);
        set_bit(&mut bitmap, slot).ok_or_else(exhausted)?;
      }
    }
    self.inner(level, &names, bitmap).map(Some)
  }

  /// A leaf of the block from `start`: its runs, each relative to the block (module docs).
  fn leaf(&mut self, start: u64, runs: &[(u64, u64)]) -> Result<ContentId, ContentRefused> {
    let mut payload = Vec::with_capacity(runs.len().saturating_mul(RUN_BYTES).saturating_add(1));
    payload.push(LEAF);
    for (run_start, run_end) in runs {
      for at in [run_start, run_end] {
        let relative = at.checked_sub(start).ok_or_else(exhausted)?;
        payload.extend_from_slice(&relative.to_le_bytes());
      }
    }
    self.put(&[], &payload)
  }

  /// An inner node of `level` naming `names`, the children its `bitmap` marks.
  fn inner(
    &mut self,
    level: u32,
    names: &[ContentId],
    bitmap: Vec<u8>,
  ) -> Result<ContentId, ContentRefused> {
    let level = u8::try_from(level).map_err(|_| exhausted())?;
    let mut payload = vec![INNER, level];
    payload.extend(bitmap);
    self.put(names, &payload)
  }

  /// The node of `level` whose whole range the set holds: one a level, whatever its place (module docs), put once.
  fn full(&mut self, level: u32) -> Result<ContentId, ContentRefused> {
    let at = usize::try_from(level).map_err(|_| exhausted())?;
    if let Some(node) = self.full.get(at) {
      return Ok(*node);
    }
    let node = match level.checked_sub(1) {
      None => self.leaf(0, &[(0, self.shape.block)])?,
      Some(below) => {
        let child = self.full(below)?;
        let fanout = usize::try_from(self.shape.fanout).map_err(|_| exhausted())?;
        let mut bitmap = vec![0_u8; self.shape.bitmap_len().ok_or_else(exhausted)?];
        for slot in 0..self.shape.fanout {
          set_bit(&mut bitmap, slot).ok_or_else(exhausted)?;
        }
        self.inner(level, &vec![child; fanout], bitmap)?
      }
    };
    self.full.push(node);
    Ok(node)
  }

  /// Ends the build: one hold kept on the root for the caller, every other hold the puts took given back; refused, all
  /// of them. Past a refusal the rest is given back still, and the first refusal is the answer.
  fn settle(
    self,
    made: Result<Option<ContentId>, ContentRefused>,
  ) -> Result<Option<ContentId>, ContentRefused> {
    let Builder { store, owned, .. } = self;
    let root = made.as_ref().ok().copied().flatten();
    let mut kept = false;
    let mut released = Ok(());
    for node in owned {
      if !kept && Some(node) == root {
        kept = true;
        continue;
      }
      if let Err(refused) = store.release(&node) {
        released = released.and(Err(refused));
      }
    }
    let root = made?;
    released?;
    Ok(root)
  }
}

/// `runs`, within a node's range from `start`, cut at its children's bounds, `span` positions each, by slot.
fn children(runs: &[(u64, u64)], start: u64, span: u64) -> Option<BTreeMap<u64, Vec<(u64, u64)>>> {
  let mut children: BTreeMap<u64, Vec<(u64, u64)>> = BTreeMap::new();
  for &(run_start, run_end) in runs {
    let mut from = run_start;
    while from < run_end {
      let slot = from.checked_sub(start)?.checked_div(span)?;
      let bound = slot.checked_add(1)?.checked_mul(span)?.checked_add(start)?;
      let to = run_end.min(bound);
      children.entry(slot).or_default().push((from, to));
      from = to;
    }
  }
  Some(children)
}

/// Marks child slot `slot` in `bitmap`.
fn set_bit(bitmap: &mut [u8], slot: u64) -> Option<()> {
  let byte = bitmap.get_mut(usize::try_from(slot / 8).ok()?)?;
  *byte |= 1 << (slot % 8);
  Some(())
}

/// Whether `bitmap` marks child slot `slot`.
fn bit(bitmap: &[u8], slot: u64) -> Option<bool> {
  let byte = bitmap.get(usize::try_from(slot / 8).ok()?)?;
  Some(byte & (1 << (slot % 8)) != 0)
}

/// The set the tree `root` of `shape` holds (module docs); `None` for nodes in any other form. A node met again is read
/// once.
fn read_tree(store: &ContentStore, root: ContentId, shape: Shape) -> Option<Positions> {
  let mut seen = BTreeMap::new();
  let (_, runs) = relative(store, root, shape, &mut seen)?;
  Positions::from_runs(runs)
}

/// A node's level and runs (each relative to the node's first position), read once a node.
type Read = (u32, Vec<(u64, u64)>);

/// A node's [`Read`], from the store or from `seen`.
fn relative(
  store: &ContentStore,
  id: ContentId,
  shape: Shape,
  seen: &mut BTreeMap<ContentId, Read>,
) -> Option<Read> {
  if let Some(read) = seen.get(&id) {
    return Some(read.clone());
  }
  let node = store.derived(&id)?;
  let read = match node.payload().split_first()? {
    (&LEAF, runs) => (
      0,
      leaf_runs(runs, shape.block, node.names().next().is_none())?,
    ),
    (&INNER, rest) => {
      let (&level, bitmap) = rest.split_first()?;
      let names: Vec<ContentId> = node.names().collect();
      inner_runs(store, (u32::from(level), bitmap, &names), shape, seen)?
    }
    _ => return None,
  };
  seen.insert(id, read.clone());
  Some(read)
}

/// A leaf's runs: pairs of a start and an end, ascending with a gap between each two, none empty, within its block; a
/// leaf names nothing.
fn leaf_runs(bytes: &[u8], block: u64, names_nothing: bool) -> Option<Vec<(u64, u64)>> {
  let (pairs, left) = bytes.as_chunks::<RUN_BYTES>();
  if !names_nothing || pairs.is_empty() || !left.is_empty() {
    return None;
  }
  let mut runs: Vec<(u64, u64)> = Vec::with_capacity(pairs.len());
  for pair in pairs {
    let (start, end) = pair.split_first_chunk::<WORD_BYTES>()?;
    let (start, end) = (
      u64::from_le_bytes(*start),
      u64::from_le_bytes(<[u8; WORD_BYTES]>::try_from(end).ok()?),
    );
    let after = runs.last().is_none_or(|(_, previous)| start > *previous);
    if start >= end || end > block || !after {
      return None;
    }
    runs.push((start, end));
  }
  Some(runs)
}

/// An inner node's runs: each child's, of the level below, shifted to its slot, a run met at a slot's bound joined; the
/// node names one child for each slot its bitmap marks, and at least one.
fn inner_runs(
  store: &ContentStore,
  (level, bitmap, names): (u32, &[u8], &[ContentId]),
  shape: Shape,
  seen: &mut BTreeMap<ContentId, Read>,
) -> Option<Read> {
  let below = level.checked_sub(1)?;
  let span = shape.span(below)?;
  if bitmap.len() != shape.bitmap_len()? || stray_bits(bitmap, shape.fanout)? {
    return None;
  }
  let mut names = names.iter();
  let mut runs: Vec<(u64, u64)> = Vec::new();
  for slot in 0..shape.fanout {
    if !bit(bitmap, slot)? {
      continue;
    }
    let (child_level, child) = relative(store, *names.next()?, shape, seen)?;
    if child_level != below {
      return None;
    }
    let offset = slot.checked_mul(span)?;
    for (start, end) in child {
      let (start, end) = (start.checked_add(offset)?, end.checked_add(offset)?);
      match runs.last_mut() {
        Some(last) if last.1 == start => last.1 = end,
        _ => runs.push((start, end)),
      }
    }
  }
  (names.next().is_none() && !runs.is_empty()).then_some((level, runs))
}

/// Whether `bitmap` marks a slot past `fanout`.
fn stray_bits(bitmap: &[u8], fanout: u64) -> Option<bool> {
  let slots = u64::try_from(bitmap.len()).ok()?.checked_mul(8)?;
  let mut slot = fanout;
  while slot < slots {
    if bit(bitmap, slot)? {
      return Some(true);
    }
    slot = slot.checked_add(1)?;
  }
  Some(false)
}

/// The type (module docs), declared over the store's window, eager or lazy by its owner.
#[derive(Clone, Debug)]
pub struct ReachSets {
  name: Vec<u8>,
  window: usize,
  mode: Mode,
}

impl ReachSets {
  /// The type whose trees are shaped by `window`, the store's (the window is part of its name, since the shape is),
  /// computed by `mode`.
  pub fn declare(window: usize, mode: Mode) -> ReachSets {
    let mut name = TYPE_NAME.to_vec();
    name.extend_from_slice(&u64::try_from(window).unwrap_or(u64::MAX).to_be_bytes());
    ReachSets { name, window, mode }
  }
}

impl Graph for Repository {
  type Version = (u64, u64);

  fn parents(&self, (green, version): (u64, u64)) -> Option<Vec<(u64, u64)>> {
    let (record, before) = self.reach_record(green, version)?;
    Some(parents_of((green, version), record, before))
  }

  fn name(&self, (green, version): (u64, u64), out: &mut Vec<u8>) {
    out.extend_from_slice(&green.to_be_bytes());
    out.extend_from_slice(&version.to_be_bytes());
  }
}

/// A version's parents in the type's history (module docs): version `k - 1` when `before` (it had a head) and the
/// version's set follows from it, then the version its set was remade from. A delete has none.
fn parents_of((green, version): (u64, u64), record: &Version, before: bool) -> Vec<(u64, u64)> {
  let previous = version
    .checked_sub(1)
    .filter(|_| before)
    .map(|previous| (green, previous));
  match (record.followed, record.head) {
    (Followed::Emptied, _) | (_, None) => Vec::new(),
    (Followed::Grown, Some(_)) => previous.into_iter().collect(),
    (Followed::Remade { base }, Some(_)) => {
      let base = base.and_then(|(green, at)| Some((green, u64::try_from(at).ok()?)));
      previous.into_iter().chain(base).collect()
    }
  }
}

impl DerivedType for ReachSets {
  type Source = Repository;
  type Refused = ReachRefused;

  fn name(&self) -> &[u8] {
    &self.name
  }

  fn mode(&self) -> Mode {
    self.mode
  }

  fn derive(
    &mut self,
    repository: &Repository,
    store: &mut ContentStore,
    (green, version): (u64, u64),
    parents: &[ContentId],
  ) -> Result<ContentId, ReachRefused> {
    let (record, before) = repository
      .reach_record(green, version)
      .ok_or(ReachRefused::NoVersion { green, version })?;
    let mut values = Vec::with_capacity(parents.len());
    for root in parents {
      values.push(Sets::read(store, root).ok_or(ReachRefused::Malformed { node: *root })?);
    }
    let sets = repository.follow(store, (green, version), (record, before), &values)?;
    let shape = shape_of(self.window).map_err(ReachRefused::Content)?;
    sets.put(store, shape).map_err(ReachRefused::Content)
  }
}

impl Repository {
  /// Version `version` of `green`, and whether the version before it had a head.
  fn reach_record(&self, green: u64, version: u64) -> Option<(&Version, bool)> {
    let chain = self.chains.get(&green)?;
    let at = usize::try_from(version).ok()?;
    let record = chain.versions.get(at)?;
    let before = at
      .checked_sub(1)
      .and_then(|previous| chain.versions.get(previous))
      .is_some_and(|previous| previous.head.is_some());
    Some((record, before))
  }

  /// A version's sets from its record and its parents' values, in its parents' order (module docs).
  fn follow(
    &self,
    store: &ContentStore,
    (green, version): (u64, u64),
    (record, before): (&Version, bool),
    values: &[Sets],
  ) -> Result<Sets, ReachRefused> {
    let wrong = ReachRefused::Parents { green, version };
    let head = match (record.followed, record.head) {
      (Followed::Emptied, _) | (_, None) => {
        return if values.is_empty() {
          Ok(Sets::default())
        } else {
          Err(wrong)
        };
      }
      (_, Some(position)) => self
        .id_at(store, position)
        .ok_or(ReachRefused::NoVersion { green, version })?,
    };
    let mut values = values.iter();
    let previous = if before {
      Some(values.next().ok_or(wrong)?)
    } else {
      None
    };
    let sets = match record.followed {
      Followed::Grown => {
        let previous = previous.ok_or(wrong)?;
        let brought = self.reach_walk(store, head, &previous.reached)?;
        Sets {
          reached: previous.reached.union(&brought),
          brought,
        }
      }
      Followed::Remade { base } => {
        let base = match base {
          Some(_) => values.next().ok_or(wrong)?.reached.clone(),
          None => Positions::new(),
        };
        let reached = base.union(&self.reach_walk(store, head, &base)?);
        let brought = match previous {
          Some(previous) => self.reach_walk(store, head, &previous.reached)?,
          None => reached.clone(),
        };
        Sets { reached, brought }
      }
      Followed::Emptied => Sets::default(),
    };
    if values.next().is_some() {
      return Err(wrong);
    }
    Ok(sets)
  }

  /// What the committed index reaches from `head`, stopping at every position `stop` holds: the admission's own walk,
  /// over a push that holds nothing.
  fn reach_walk(
    &self,
    store: &ContentStore,
    head: ObjectId,
    stop: &Positions,
  ) -> Result<Positions, ReachRefused> {
    let members = self
      .walk(store, &Push::new(self.format), head, stop)
      .map_err(ReachRefused::Store)?;
    if let Some(id) = members.new.first() {
      return Err(ReachRefused::Unindexed { id: *id });
    }
    Ok(members.indexed)
  }

  /// `green`'s newest version and its value of [`ReachSets`]: the very value the store serves the version's sets from,
  /// put as it committed with the sets its admission computed, held once more by the caller: what the owner keeps under
  /// (type, version) as it commits, with no second walk (module docs).
  pub fn reach_value(
    &self,
    store: &mut ContentStore,
    green: u64,
  ) -> Result<(u64, ContentId), StoreRefused> {
    let chain = self
      .chains
      .get(&green)
      .ok_or(StoreRefused::NoVersion { green, version: 0 })?;
    let newest = chain
      .versions
      .last()
      .ok_or(StoreRefused::NoVersion { green, version: 0 })?;
    store.hold(&newest.value)?;
    let version = u64::try_from(chain.versions.len().saturating_sub(1)).unwrap_or(u64::MAX);
    Ok((version, newest.value))
  }
}
