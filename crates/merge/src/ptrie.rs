//! A persistent radix tree over keys' bytes, whose nodes live in an arena and are shared by holder counts (§4.17.1
//! shared piece 1; A-534): the structure that lets a green be made at another green's version without copying its
//! state, and a subtree of keys be grafted under another prefix by sharing it (Subversion's copy, §4.17.4).
//!
//! **What it is.** A radix tree (Morrison, "PATRICIA — Practical Algorithm To Retrieve Information Coded in
//! Alphanumeric", JACM 1968, tier A): each edge is labelled with a run of bytes, each node's children start with
//! distinct bytes, and a key's value sits at the node where its bytes end. A walk visits a node before its children
//! and children in the order of their first bytes, so keys come out in byte order: the order of an ordered map of the
//! same strings, which every encoding the merge engine writes relies on. Each node counts its holders: the maps whose
//! root it is, the parents whose child it is and the subtrees held for a graft. Nodes belong to an arena ([`Nodes`])
//! the maps made from one another share; there is no shared-ownership pointer (CLAUDE.md R2).
//!
//! **What it costs.** A fork ([`Ptrie::fork`]) is one count: O(1). A change to a map copies the nodes on the key's
//! path that another holder still names, and changes in place the ones only it names (path copying, Driscoll, Sarnak,
//! Sleator and Tarjan, "Making data structures persistent", JCSS 1989, tier A): a map nobody forked costs what a radix
//! tree does, and a forked one at most one node per edge of the key's path, counted in [`Nodes::copied`]. A subtree is
//! one held node ([`Ptrie::subtree`]); a graft ([`Ptrie::graft`]) copies the path to its destination and the subtree's
//! top node, and shares everything beneath: O(the destination's length + the top node's fan-out), whatever the subtree
//! holds. Each node keeps the number of values beneath it, so a map's length and a graft's are known without a walk.
//!
//! **Values with a resource.** A value that names something outside the arena (a sealed block of file bytes) is told
//! when a copy of a node makes one more holder of it and when the last node holding it is freed ([`Holders`]); a value
//! without one ignores both ([`Plain`]). A value a change takes out of a map ([`Ptrie::insert`]'s replaced value,
//! [`Ptrie::remove`]'s) goes back to the caller with the hold it had in that map.
//!
//! **Walks are counted.** Every node a walk visits is counted in [`Nodes::visited`], so a test can prove an operation
//! walked no tree (A-534's non-vacuity counter).
//!
//! **No panics.** Every slot and byte is read with `get`, every count saturates, and a link that names no node (a
//! corruption no operation here makes) is answered as absent and counted in [`Nodes::missing`].

use std::cell::Cell;
use std::marker::PhantomData;

/// A node's place in its arena.
type Slot = usize;

/// One node: the edge bytes from its parent, the value whose key ends here, and its children.
#[derive(Debug)]
struct Node<V> {
  /// The edge's bytes from the parent: empty only at a map's root.
  label: Vec<u8>,
  /// The value whose key ends here.
  value: Option<V>,
  /// The children, each under the first byte of its label (distinct), ascending.
  children: Vec<(u8, Slot)>,
  /// The values in this node's subtree, its own included.
  count: usize,
  /// The maps, parents and held subtrees that name this node.
  holders: u32,
}

/// What a map's values are told as nodes holding them are copied and freed (module docs, "Values with a resource").
pub trait Holders<V> {
  /// A copy of a node made one more holder of `value`.
  fn copied(&mut self, value: &V);
  /// The last node holding `value` was freed.
  fn freed(&mut self, value: V);
}

/// The holders of values that name nothing outside the arena: both events are nothing to them.
pub struct Plain;

impl<V> Holders<V> for Plain {
  fn copied(&mut self, _value: &V) {}
  fn freed(&mut self, _value: V) {}
}

/// The arena of nodes the maps made from one another share.
#[derive(Debug)]
pub struct Nodes<V> {
  slots: Vec<Option<Node<V>>>,
  /// Freed slots, reused before the arena grows.
  free: Vec<Slot>,
  /// Nodes copied because another holder named them (module docs, "What it costs").
  copied: u64,
  /// Nodes a walk visited (module docs, "Walks are counted"); a cell, as walks borrow the arena.
  visited: Cell<u64>,
  /// Links that named no node (module docs, "No panics"): zero in every correct use.
  missing: Cell<u64>,
}

impl<V> Default for Nodes<V> {
  fn default() -> Self {
    Nodes {
      slots: Vec::new(),
      free: Vec::new(),
      copied: 0,
      visited: Cell::new(0),
      missing: Cell::new(0),
    }
  }
}

impl<V> Nodes<V> {
  /// Nodes copied because another holder named them.
  pub fn copied(&self) -> u64 {
    self.copied
  }

  /// Nodes a walk visited.
  pub fn visited(&self) -> u64 {
    self.visited.get()
  }

  /// Links that named no node: zero in every correct use.
  pub fn missing(&self) -> u64 {
    self.missing.get()
  }

  /// The nodes alive in the arena.
  pub fn live(&self) -> usize {
    self.slots.len().saturating_sub(self.free.len())
  }

  fn node(&self, slot: Slot) -> Option<&Node<V>> {
    let node = self.slots.get(slot).and_then(Option::as_ref);
    if node.is_none() {
      self.missing.set(self.missing.get().saturating_add(1));
    }
    node
  }

  fn node_mut(&mut self, slot: Slot) -> Option<&mut Node<V>> {
    let node = self.slots.get_mut(slot).and_then(Option::as_mut);
    if node.is_none() {
      self.missing.set(self.missing.get().saturating_add(1));
    }
    node
  }

  /// A new node, in a freed slot when there is one.
  fn alloc(&mut self, node: Node<V>) -> Slot {
    if let Some(slot) = self.free.pop()
      && let Some(place) = self.slots.get_mut(slot)
    {
      *place = Some(node);
      return slot;
    }
    self.slots.push(Some(node));
    self.slots.len().saturating_sub(1)
  }

  /// Takes a node out of its slot, which is freed.
  fn take(&mut self, slot: Slot) -> Option<Node<V>> {
    let node = self.slots.get_mut(slot).and_then(Option::take);
    match node {
      Some(_) => self.free.push(slot),
      None => self.missing.set(self.missing.get().saturating_add(1)),
    }
    node
  }

  /// One more holder of the node at `slot`.
  fn hold(&mut self, slot: Slot) {
    if let Some(node) = self.node_mut(slot) {
      node.holders = node.holders.saturating_add(1);
    }
  }

  /// The values in the subtree at `slot`.
  fn count(&self, slot: Slot) -> usize {
    self.node(slot).map_or(0, |node| node.count)
  }

  /// Gives one hold on the subtree at `slot` back: each node no holder names any more is freed, its value told.
  fn release(&mut self, slot: Slot, holders: &mut impl Holders<V>) {
    let mut releasing = Releasing {
      pending: vec![slot],
    };
    while self.release_step(&mut releasing, holders) {}
  }

  /// One step of a release ([`Releasing`]): one hold given back on the next node, which is freed, its value told and
  /// its children queued, when no other holder names it. False once nothing is left to give back.
  pub fn release_step(&mut self, releasing: &mut Releasing, holders: &mut impl Holders<V>) -> bool {
    let Some(slot) = releasing.pending.pop() else {
      return false;
    };
    let Some(node) = self.node_mut(slot) else {
      return true;
    };
    node.holders = node.holders.saturating_sub(1);
    if node.holders > 0 {
      return true;
    }
    if let Some(node) = self.take(slot) {
      releasing
        .pending
        .extend(node.children.iter().map(|(_, child)| *child));
      if let Some(value) = node.value {
        holders.freed(value);
      }
    }
    true
  }
}

/// A map's hold on its nodes being given back a node a step ([`Ptrie::releasing`], [`Nodes::release_step`]), so a
/// release as large as the map runs in a shard's slices (A-575: a green's retire from its lineage). The nodes still to
/// visit: each a node whose hold is still to give back; bounded by the nodes of the map.
#[derive(Debug, Default)]
pub struct Releasing {
  pending: Vec<Slot>,
}

impl Releasing {
  /// Whether every hold has been given back.
  pub fn is_done(&self) -> bool {
    self.pending.is_empty()
  }
}

/// The child of `node` whose label starts with `byte`: its index in the children and its slot.
fn child_of<V>(node: &Node<V>, byte: u8) -> Option<(usize, Slot)> {
  node
    .children
    .binary_search_by_key(&byte, |(first, _)| *first)
    .ok()
    .and_then(|index| node.children.get(index).map(|(_, slot)| (index, *slot)))
}

/// Puts `child` among `children`, keeping them ordered by first byte.
fn add_child(children: &mut Vec<(u8, Slot)>, first: u8, child: Slot) {
  let at = children.partition_point(|(byte, _)| *byte < first);
  children.insert(at, (first, child));
}

/// The bytes `a` and `b` begin with alike.
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
  a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// The node at `slot` with its referrer as its only holder: itself when it is, otherwise a copy that takes the
/// referrer's hold from it, holding the same children once more and telling its value it has one more holder.
fn unique<V: Clone>(nodes: &mut Nodes<V>, slot: Slot, holders: &mut impl Holders<V>) -> Slot {
  let Some(node) = nodes.node_mut(slot) else {
    return slot;
  };
  if node.holders <= 1 {
    return slot;
  }
  node.holders = node.holders.saturating_sub(1);
  let copy = Node {
    label: node.label.clone(),
    value: node.value.clone(),
    children: node.children.clone(),
    count: node.count,
    holders: 1,
  };
  if let Some(value) = &copy.value {
    holders.copied(value);
  }
  for (_, child) in &copy.children {
    nodes.hold(*child);
  }
  nodes.copied = nodes.copied.saturating_add(1);
  nodes.alloc(copy)
}

/// A persistent map from byte strings to values: a root in its arena.
#[derive(Debug)]
pub struct Ptrie<V> {
  root: Option<Slot>,
  marker: PhantomData<V>,
}

impl<V> Default for Ptrie<V> {
  fn default() -> Self {
    Ptrie {
      root: None,
      marker: PhantomData,
    }
  }
}

/// A subtree held for a graft ([`Ptrie::subtree`]): its top node, and how many leading bytes of that node's label lie
/// above the prefix it was taken under, so its keys below that prefix are the rest of the label and what hangs from it.
#[derive(Debug)]
pub struct Shared {
  slot: Slot,
  skip: usize,
}

/// Why a graft was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraftRefused {
  /// A key already starts with the destination prefix: a graft never mixes with what is there.
  Occupied,
}

impl<V: Clone> Ptrie<V> {
  /// An empty map.
  pub fn new() -> Self {
    Ptrie::default()
  }

  /// The values the map holds: its root's count, no walk.
  pub fn len(&self, nodes: &Nodes<V>) -> usize {
    self.root.map_or(0, |root| nodes.count(root))
  }

  /// Whether the map holds none.
  pub fn is_empty(&self, nodes: &Nodes<V>) -> bool {
    self.len(nodes) == 0
  }

  /// The value under `key`.
  pub fn get<'a>(&self, nodes: &'a Nodes<V>, key: &[u8]) -> Option<&'a V> {
    let mut at = self.root?;
    let mut rest = key;
    loop {
      let node = nodes.node(at)?;
      let Some(first) = rest.first() else {
        return node.value.as_ref();
      };
      let (_, child) = child_of(node, *first)?;
      let label = &nodes.node(child)?.label;
      rest = rest.strip_prefix(label.as_slice())?;
      at = child;
    }
  }

  /// Whether the map holds `key`.
  pub fn contains_key(&self, nodes: &Nodes<V>, key: &[u8]) -> bool {
    self.get(nodes, key).is_some()
  }

  /// Another map holding exactly these values, sharing every node: O(1).
  pub fn fork(&self, nodes: &mut Nodes<V>) -> Self {
    if let Some(root) = self.root {
      nodes.hold(root);
    }
    Ptrie {
      root: self.root,
      marker: PhantomData,
    }
  }

  /// The root, made if the map has none.
  fn root_slot(&mut self, nodes: &mut Nodes<V>) -> Slot {
    match self.root {
      Some(root) => root,
      None => {
        let root = nodes.alloc(Node {
          label: Vec::new(),
          value: None,
          children: Vec::new(),
          count: 0,
          holders: 1,
        });
        self.root = Some(root);
        root
      }
    }
  }

  /// Sets `key` to `value`, the replaced value back to the caller with its hold.
  pub fn insert(
    &mut self,
    nodes: &mut Nodes<V>,
    key: &[u8],
    value: V,
    holders: &mut impl Holders<V>,
  ) -> Option<V> {
    let root = self.root_slot(nodes);
    let (root, replaced) = insert_at(nodes, root, key, value, holders);
    self.root = Some(root);
    replaced
  }

  /// Takes `key` out, its value back to the caller with its hold. A key the map does not hold copies nothing.
  pub fn remove(
    &mut self,
    nodes: &mut Nodes<V>,
    key: &[u8],
    holders: &mut impl Holders<V>,
  ) -> Option<V> {
    let root = self.root?;
    if !self.contains_key(nodes, key) {
      return None;
    }
    let (root, removed) = remove_at(nodes, root, key, true, holders);
    self.root = root;
    removed
  }

  /// Gives this map's hold on its nodes back: every node no other holder names is freed, its value told
  /// ([`Holders::freed`]). The map is empty after.
  pub fn release(&mut self, nodes: &mut Nodes<V>, holders: &mut impl Holders<V>) {
    if let Some(root) = self.root.take() {
      nodes.release(root, holders);
    }
  }

  /// This map's hold on its nodes, to give back a node a step ([`Nodes::release_step`]): the map is empty after, and
  /// no node is touched until the steps run.
  pub fn releasing(&mut self) -> Releasing {
    Releasing {
      pending: self.root.take().into_iter().collect(),
    }
  }

  /// Every key and value, in byte order.
  pub fn iter<'a>(&self, nodes: &'a Nodes<V>) -> Iter<'a, V> {
    Iter::start(nodes, self.root, Vec::new())
  }

  /// The top node of the keys that start with `prefix`, and the key bytes above that node's label: `None` when no key
  /// starts with it. One descent.
  fn under(&self, nodes: &Nodes<V>, prefix: &[u8]) -> Option<(Slot, Vec<u8>, usize)> {
    let found = self.under_any(nodes, prefix)?;
    (nodes.count(found.0) > 0).then_some(found)
  }

  /// [`Ptrie::under`] before the empty check: an empty map's root answers the empty prefix.
  fn under_any(&self, nodes: &Nodes<V>, prefix: &[u8]) -> Option<(Slot, Vec<u8>, usize)> {
    let mut at = self.root?;
    let mut above: Vec<u8> = Vec::new();
    let mut rest = prefix;
    loop {
      let node = nodes.node(at)?;
      let Some(first) = rest.first() else {
        let label = node.label.len();
        let before = above.len().saturating_sub(label);
        above.truncate(before);
        return Some((at, above, label));
      };
      let (_, child) = child_of(node, *first)?;
      let label = &nodes.node(child)?.label;
      let common = common_prefix(label, rest);
      if common == rest.len() {
        return Some((child, above, common));
      }
      if common < label.len() {
        return None;
      }
      above.extend_from_slice(label);
      rest = rest.get(label.len()..)?;
      at = child;
    }
  }

  /// The keys that start with `prefix` and their values, in byte order: one descent, then the keys it answers.
  pub fn prefix<'a>(&self, nodes: &'a Nodes<V>, prefix: &[u8]) -> Iter<'a, V> {
    match self.under(nodes, prefix) {
      Some((slot, above, _)) => Iter::start(nodes, Some(slot), above),
      None => Iter::start(nodes, None, Vec::new()),
    }
  }

  /// The keys that start with `prefix`, held as one subtree for a graft: one descent and one count, whatever the
  /// subtree holds. `None` when no key starts with it.
  pub fn subtree(&self, nodes: &mut Nodes<V>, prefix: &[u8]) -> Option<Shared> {
    let (slot, _, skip) = self.under(nodes, prefix)?;
    nodes.hold(slot);
    Some(Shared { slot, skip })
  }

  /// Gives a held subtree back without grafting it.
  pub fn drop_subtree(nodes: &mut Nodes<V>, shared: Shared, holders: &mut impl Holders<V>) {
    nodes.release(shared.slot, holders);
  }

  /// Makes the subtree's keys reappear under `prefix` in this map, sharing its nodes: the path to `prefix` and the
  /// subtree's top node are copied, everything beneath is shared. Refused, the subtree given back, when a key already
  /// starts with `prefix`.
  pub fn graft(
    &mut self,
    nodes: &mut Nodes<V>,
    prefix: &[u8],
    shared: Shared,
    holders: &mut impl Holders<V>,
  ) -> Result<(), GraftRefused> {
    if self.under(nodes, prefix).is_some() {
      Ptrie::drop_subtree(nodes, shared, holders);
      return Err(GraftRefused::Occupied);
    }
    let top = graft_top(nodes, shared, holders);
    let root = self.root_slot(nodes);
    let root = graft_at(nodes, root, prefix, top, holders);
    self.root = Some(root);
    Ok(())
  }

  /// A copy of this map whose nodes are all `to`'s own: O(values), for a map that will not share `from` (a replica on
  /// another shard). Each value is told it has one more holder.
  pub fn copy_into(
    &self,
    from: &Nodes<V>,
    to: &mut Nodes<V>,
    holders: &mut impl Holders<V>,
  ) -> Self {
    let mut copy = Ptrie::new();
    for (key, value) in self.iter(from) {
      holders.copied(value);
      copy.insert(to, &key, value.clone(), &mut Plain);
    }
    copy
  }
}

/// The node a held subtree grafts as: a copy of its top node, its label's first `skip` bytes dropped, holding the same
/// children once more. The subtree's hold on its top node is given back.
fn graft_top<V: Clone>(
  nodes: &mut Nodes<V>,
  shared: Shared,
  holders: &mut impl Holders<V>,
) -> Node<V> {
  let mut top = copy_of(nodes, shared.slot, shared.skip, holders);
  nodes.release(shared.slot, holders);
  // A map's root may be a bare edge (no value, one child); a grafted node may not, so the child takes its place, the
  // edge's bytes before its own.
  if top.value.is_none()
    && let [(_, only)] = top.children.as_slice()
  {
    let only = *only;
    let mut below = copy_of(nodes, only, 0, holders);
    nodes.release(only, holders);
    let mut label = std::mem::take(&mut top.label);
    label.append(&mut below.label);
    below.label = label;
    top = below;
  }
  top
}

/// A new node like the one at `slot`, its label's first `skip` bytes dropped: its value told it has one more holder
/// and each child held once more.
fn copy_of<V: Clone>(
  nodes: &mut Nodes<V>,
  slot: Slot,
  skip: usize,
  holders: &mut impl Holders<V>,
) -> Node<V> {
  let copy = nodes.node(slot).map(|node| Node {
    label: node
      .label
      .get(skip..)
      .map(<[u8]>::to_vec)
      .unwrap_or_default(),
    value: node.value.clone(),
    children: node.children.clone(),
    count: node.count,
    holders: 1,
  });
  let Some(copy) = copy else {
    return Node {
      label: Vec::new(),
      value: None,
      children: Vec::new(),
      count: 0,
      holders: 1,
    };
  };
  if let Some(value) = &copy.value {
    holders.copied(value);
  }
  for (_, child) in &copy.children {
    nodes.hold(*child);
  }
  copy
}

/// Hangs `top` (a subtree's top node, its label relative to the graft point) under `rest` below the unique-made node
/// at `slot`, where no key starts with `rest`: the node's new slot.
fn graft_at<V: Clone>(
  nodes: &mut Nodes<V>,
  slot: Slot,
  rest: &[u8],
  mut top: Node<V>,
  holders: &mut impl Holders<V>,
) -> Slot {
  let slot = unique(nodes, slot, holders);
  let added = top.count;
  if let Some(node) = nodes.node_mut(slot) {
    node.count = node.count.saturating_add(added);
  }
  let Some(first) = rest.first().copied() else {
    // Only an empty map's root stands where `rest` is empty and nothing starts with it: the subtree's top takes the
    // root's place, or hangs from it under its own label's bytes.
    match top.label.first().copied() {
      None => {
        if let Some(node) = nodes.node_mut(slot) {
          node.value = top.value;
          node.children = top.children;
        }
      }
      Some(first) => {
        let child = nodes.alloc(top);
        if let Some(node) = nodes.node_mut(slot) {
          add_child(&mut node.children, first, child);
        }
      }
    }
    return slot;
  };
  let found = nodes.node(slot).and_then(|node| child_of(node, first));
  match found {
    None => {
      let mut label = rest.to_vec();
      label.append(&mut top.label);
      top.label = label;
      let child = nodes.alloc(top);
      if let Some(node) = nodes.node_mut(slot) {
        add_child(&mut node.children, first, child);
      }
    }
    Some((index, child)) => {
      let label = nodes
        .node(child)
        .map(|node| node.label.clone())
        .unwrap_or_default();
      let common = common_prefix(&label, rest);
      if common == label.len() {
        let below = rest.get(common..).unwrap_or_default();
        let child = graft_at(nodes, child, below, top, holders);
        if let Some(node) = nodes.node_mut(slot)
          && let Some(entry) = node.children.get_mut(index)
        {
          entry.1 = child;
        }
      } else {
        // The destination and the child part within the child's label: split it there.
        let child = unique(nodes, child, holders);
        let tail = label.get(common..).map(<[u8]>::to_vec).unwrap_or_default();
        let child_count = nodes.count(child);
        let tail_first = tail.first().copied().unwrap_or_default();
        if let Some(node) = nodes.node_mut(child) {
          node.label = tail;
        }
        let mut grafted_label = rest.get(common..).map(<[u8]>::to_vec).unwrap_or_default();
        let grafted_first = grafted_label.first().copied().unwrap_or_default();
        grafted_label.append(&mut top.label);
        top.label = grafted_label;
        let grafted = nodes.alloc(top);
        let mut children = vec![(tail_first, child)];
        add_child(&mut children, grafted_first, grafted);
        let middle = nodes.alloc(Node {
          label: label.get(..common).map(<[u8]>::to_vec).unwrap_or_default(),
          value: None,
          children,
          count: child_count.saturating_add(added),
          holders: 1,
        });
        if let Some(node) = nodes.node_mut(slot)
          && let Some(entry) = node.children.get_mut(index)
        {
          entry.1 = middle;
        }
      }
    }
  }
  slot
}

/// Inserts `rest` below the node at `slot` (whose own label is matched already): the node's new slot and the value the
/// key held before, if any.
fn insert_at<V: Clone>(
  nodes: &mut Nodes<V>,
  slot: Slot,
  rest: &[u8],
  value: V,
  holders: &mut impl Holders<V>,
) -> (Slot, Option<V>) {
  let slot = unique(nodes, slot, holders);
  let Some(first) = rest.first().copied() else {
    let replaced = nodes
      .node_mut(slot)
      .and_then(|node| node.value.replace(value));
    if replaced.is_none()
      && let Some(node) = nodes.node_mut(slot)
    {
      node.count = node.count.saturating_add(1);
    }
    return (slot, replaced);
  };
  let found = nodes.node(slot).and_then(|node| child_of(node, first));
  let replaced = match found {
    None => {
      let leaf = nodes.alloc(Node {
        label: rest.to_vec(),
        value: Some(value),
        children: Vec::new(),
        count: 1,
        holders: 1,
      });
      if let Some(node) = nodes.node_mut(slot) {
        add_child(&mut node.children, first, leaf);
      }
      None
    }
    Some((index, child)) => {
      let label = nodes
        .node(child)
        .map(|node| node.label.clone())
        .unwrap_or_default();
      let common = common_prefix(&label, rest);
      if common == label.len() {
        let below = rest.get(common..).unwrap_or_default();
        let (child, replaced) = insert_at(nodes, child, below, value, holders);
        if let Some(node) = nodes.node_mut(slot)
          && let Some(entry) = node.children.get_mut(index)
        {
          entry.1 = child;
        }
        replaced
      } else {
        // The key leaves the child's label partway: split the label there.
        let child = unique(nodes, child, holders);
        let tail = label.get(common..).map(<[u8]>::to_vec).unwrap_or_default();
        let tail_first = tail.first().copied().unwrap_or_default();
        let child_count = nodes.count(child);
        if let Some(node) = nodes.node_mut(child) {
          node.label = tail;
        }
        let mut middle = Node {
          label: label.get(..common).map(<[u8]>::to_vec).unwrap_or_default(),
          value: None,
          children: vec![(tail_first, child)],
          count: child_count.saturating_add(1),
          holders: 1,
        };
        match rest.get(common..) {
          Some(below) if !below.is_empty() => {
            let leaf = nodes.alloc(Node {
              label: below.to_vec(),
              value: Some(value),
              children: Vec::new(),
              count: 1,
              holders: 1,
            });
            add_child(
              &mut middle.children,
              below.first().copied().unwrap_or_default(),
              leaf,
            );
          }
          _ => middle.value = Some(value),
        }
        let middle = nodes.alloc(middle);
        if let Some(node) = nodes.node_mut(slot)
          && let Some(entry) = node.children.get_mut(index)
        {
          entry.1 = middle;
        }
        None
      }
    }
  };
  if replaced.is_none()
    && let Some(node) = nodes.node_mut(slot)
  {
    node.count = node.count.saturating_add(1);
  }
  (slot, replaced)
}

/// Removes `rest` below the node at `slot`, which holds it: the node's replacement (none when it is left empty, its
/// only child when it is left a bare edge) and the value. The root is never merged or dropped.
fn remove_at<V: Clone>(
  nodes: &mut Nodes<V>,
  slot: Slot,
  rest: &[u8],
  is_root: bool,
  holders: &mut impl Holders<V>,
) -> (Option<Slot>, Option<V>) {
  let slot = unique(nodes, slot, holders);
  let removed = match rest.first().copied() {
    None => nodes.node_mut(slot).and_then(|node| node.value.take()),
    Some(first) => {
      let found = nodes.node(slot).and_then(|node| child_of(node, first));
      let Some((index, child)) = found else {
        return (Some(slot), None);
      };
      let label = nodes.node(child).map_or(0, |node| node.label.len());
      let below = rest.get(label..).unwrap_or_default();
      let (child, removed) = remove_at(nodes, child, below, false, holders);
      if let Some(node) = nodes.node_mut(slot) {
        match child {
          Some(child) => {
            if let Some(entry) = node.children.get_mut(index) {
              entry.1 = child;
            }
          }
          None => {
            if index < node.children.len() {
              node.children.remove(index);
            }
          }
        }
      }
      removed
    }
  };
  if removed.is_some()
    && let Some(node) = nodes.node_mut(slot)
  {
    node.count = node.count.saturating_sub(1);
  }
  if is_root {
    return (Some(slot), removed);
  }
  let (bare, children) = match nodes.node(slot) {
    Some(node) => (node.value.is_none(), node.children.clone()),
    None => return (None, removed),
  };
  if !bare {
    return (Some(slot), removed);
  }
  match children.as_slice() {
    [] => {
      nodes.take(slot);
      (None, removed)
    }
    [(_, only)] => {
      // A bare edge with one child: the child takes the edge's bytes before its own, and the edge goes.
      let only = unique(nodes, *only, holders);
      let mut label = nodes
        .node(slot)
        .map(|node| node.label.clone())
        .unwrap_or_default();
      if let Some(node) = nodes.node_mut(only) {
        label.append(&mut node.label);
        node.label = label;
      }
      nodes.take(slot);
      (Some(only), removed)
    }
    _ => (Some(slot), removed),
  }
}

/// A walk of a map in byte order, each node it visits counted.
pub struct Iter<'a, V> {
  nodes: &'a Nodes<V>,
  /// The key bytes down to the current node.
  key: Vec<u8>,
  /// Each node being walked: its slot, its next child, the key's length above its label, whether it was visited.
  stack: Vec<(Slot, usize, usize, bool)>,
}

impl<'a, V> Iter<'a, V> {
  /// A walk of the subtree at `top`, whose label starts after the key bytes `above`.
  fn start(nodes: &'a Nodes<V>, top: Option<Slot>, above: Vec<u8>) -> Self {
    let base = above.len();
    Iter {
      nodes,
      key: above,
      stack: top.map(|slot| (slot, 0, base, false)).into_iter().collect(),
    }
  }
}

impl<'a, V> Iterator for Iter<'a, V> {
  type Item = (Vec<u8>, &'a V);

  fn next(&mut self) -> Option<Self::Item> {
    let nodes: &'a Nodes<V> = self.nodes;
    loop {
      let (slot, next, base, visited) = *self.stack.last()?;
      let Some(node) = nodes.node(slot) else {
        self.stack.pop();
        continue;
      };
      if !visited {
        if let Some(top) = self.stack.last_mut() {
          top.3 = true;
        }
        nodes.visited.set(nodes.visited.get().saturating_add(1));
        self.key.truncate(base);
        self.key.extend_from_slice(&node.label);
        if let Some(value) = &node.value {
          return Some((self.key.clone(), value));
        }
        continue;
      }
      match node.children.get(next) {
        Some((_, child)) => {
          if let Some(top) = self.stack.last_mut() {
            top.1 = next.saturating_add(1);
          }
          let below = base.saturating_add(node.label.len());
          self.key.truncate(below);
          self.stack.push((*child, 0, below, false));
        }
        None => {
          self.stack.pop();
        }
      }
    }
  }
}

#[cfg(test)]
// proptest's strategy types carry `Arc` (D-8's harness exception).
#[allow(clippy::disallowed_types)]
mod tests {
  use std::collections::BTreeMap;

  use proptest::prelude::*;

  use super::*;

  /// The holds each value has, as a resource would count them: one per insert, one per copy, less one per free and per
  /// value handed back.
  #[derive(Default)]
  struct Counted {
    holds: BTreeMap<u32, i64>,
  }

  impl Holders<u32> for Counted {
    fn copied(&mut self, value: &u32) {
      *self.holds.entry(*value).or_default() += 1;
    }
    fn freed(&mut self, value: u32) {
      *self.holds.entry(value).or_default() -= 1;
    }
  }

  impl Counted {
    fn put(&mut self, value: u32) {
      *self.holds.entry(value).or_default() += 1;
    }
  }

  #[derive(Clone, Debug)]
  enum Step {
    Insert {
      map: usize,
      key: Vec<u8>,
      value: u32,
    },
    Remove {
      map: usize,
      key: Vec<u8>,
    },
    Fork {
      map: usize,
    },
    Release {
      map: usize,
    },
    Graft {
      from: usize,
      src: Vec<u8>,
      to: usize,
      dst: Vec<u8>,
    },
  }

  /// Shape: the most maps a history keeps live, so forks of forks happen.
  const MAPS: usize = 6;

  /// Keys over a small alphabet with shared prefixes, so edges split and merge often, the separator among them.
  fn key() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(
      prop_oneof![Just(b'a'), Just(b'b'), Just(b'/'), Just(b'.')],
      0..6,
    )
  }

  fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
      5 => (0..MAPS, key(), 0u32..64).prop_map(|(map, key, value)| Step::Insert { map, key, value }),
      2 => (0..MAPS, key()).prop_map(|(map, key)| Step::Remove { map, key }),
      1 => (0..MAPS).prop_map(|map| Step::Fork { map }),
      1 => (0..MAPS).prop_map(|map| Step::Release { map }),
      1 => (0..MAPS, key(), 0..MAPS, key()).prop_map(|(from, src, to, dst)| Step::Graft { from, src, to, dst }),
    ]
  }

  /// Every node's count is its subtree's values, and every non-root node has a label and is not a bare single edge.
  fn checked(nodes: &Nodes<u32>, slot: Slot, root: bool) -> usize {
    let node = nodes.node(slot).expect("a live node");
    let below: usize = node
      .children
      .iter()
      .map(|(_, child)| checked(nodes, *child, false))
      .sum();
    let count = below + usize::from(node.value.is_some());
    assert_eq!(node.count, count, "a stale count");
    if !root {
      assert!(!node.label.is_empty(), "an empty label below the root");
      assert!(
        node.value.is_some() || node.children.len() > 1,
        "a bare edge left in place"
      );
    }
    for pair in node.children.windows(2) {
      assert!(pair[0].0 < pair[1].0, "children out of order");
    }
    for (first, child) in &node.children {
      assert_eq!(
        nodes.node(*child).expect("a child").label.first(),
        Some(first)
      );
    }
    count
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(400), ..ProptestConfig::default() }))]

    /// A-534. Do: run generated histories of inserts, removes, forks, releases and grafts over several maps sharing one
    /// arena. Expect: every map reads as its own copied ordered map would after every step (each key, the byte order,
    /// every prefix, the length), the tree's shape holds, a change to one map never shows in another, a graft into an
    /// occupied prefix is refused and changes nothing, and once every map is released the arena holds no node and every
    /// value has been freed as many times as it was held.
    #[test]
    fn every_fork_and_graft_reads_as_its_own_copy_over_every_history(
      steps in proptest::collection::vec(step(), 0..200),
    ) {
      let mut nodes: Nodes<u32> = Nodes::default();
      let mut counted = Counted::default();
      let mut maps: Vec<Option<(Ptrie<u32>, BTreeMap<Vec<u8>, u32>)>> = (0..MAPS).map(|_| None).collect();
      maps[0] = Some((Ptrie::new(), BTreeMap::new()));
      for step in &steps {
        match step.clone() {
          Step::Insert { map, key, value } => {
            if let Some((trie, model)) = maps[map].as_mut() {
              counted.put(value);
              let replaced = trie.insert(&mut nodes, &key, value, &mut counted);
              prop_assert_eq!(replaced, model.insert(key, value));
              if let Some(replaced) = replaced {
                counted.freed(replaced);
              }
            }
          }
          Step::Remove { map, key } => {
            if let Some((trie, model)) = maps[map].as_mut() {
              let removed = trie.remove(&mut nodes, &key, &mut counted);
              prop_assert_eq!(removed, model.remove(&key));
              if let Some(removed) = removed {
                counted.freed(removed);
              }
            }
          }
          Step::Fork { map } => {
            let forked = maps[map].as_ref().map(|(trie, model)| (trie.fork(&mut nodes), model.clone()));
            if let Some(forked) = forked {
              match maps.iter().position(Option::is_none) {
                Some(slot) => maps[slot] = Some(forked),
                None => {
                  let (mut forked, _) = forked;
                  forked.release(&mut nodes, &mut counted);
                }
              }
            }
          }
          Step::Release { map } => {
            if let Some((mut trie, _)) = maps[map].take() {
              trie.release(&mut nodes, &mut counted);
            }
          }
          Step::Graft { from, src, to, dst } => {
            let shared = maps[from].as_ref().and_then(|(trie, _)| trie.subtree(&mut nodes, &src));
            let moved: Vec<(Vec<u8>, u32)> = maps[from].as_ref().map(|(_, model)| {
              model.iter().filter(|(key, _)| key.starts_with(&src))
                .map(|(key, value)| ([dst.as_slice(), &key[src.len()..]].concat(), *value)).collect()
            }).unwrap_or_default();
            if let Some(shared) = shared {
              match maps[to].as_mut() {
                Some((trie, model)) => {
                  let occupied = model.keys().any(|key| key.starts_with(&dst));
                  let result = trie.graft(&mut nodes, &dst, shared, &mut counted);
                  prop_assert_eq!(result.is_err(), occupied);
                  if !occupied {
                    for (key, value) in moved {
                      model.insert(key, value);
                    }
                  }
                }
                None => Ptrie::drop_subtree(&mut nodes, shared, &mut counted),
              }
            }
          }
        }
        for (trie, model) in maps.iter().flatten() {
          prop_assert_eq!(trie.len(&nodes), model.len());
          let listed: Vec<(Vec<u8>, u32)> = trie.iter(&nodes).map(|(key, value)| (key, *value)).collect();
          let expected: Vec<(Vec<u8>, u32)> = model.iter().map(|(key, value)| (key.clone(), *value)).collect();
          prop_assert_eq!(listed, expected);
          for probe in [&b""[..], b"a", b"a/", b"ab", b"b.", b"a/b"] {
            prop_assert_eq!(trie.get(&nodes, probe), model.get(probe));
            let prefixed: Vec<Vec<u8>> = trie.prefix(&nodes, probe).map(|(key, _)| key).collect();
            let expected: Vec<Vec<u8>> = model.keys().filter(|key| key.starts_with(probe)).cloned().collect();
            prop_assert_eq!(prefixed, expected);
          }
          if let Some(root) = trie.root {
            checked(&nodes, root, true);
          }
        }
        prop_assert_eq!(nodes.missing(), 0);
      }
      for (mut trie, _) in maps.into_iter().flatten() {
        trie.release(&mut nodes, &mut counted);
      }
      prop_assert_eq!(nodes.live(), 0);
      for (value, held) in &counted.holds {
        prop_assert_eq!(*held, 0, "value {} was freed {} times fewer than it was held", value, held);
      }
    }
  }

  /// A-575. Do: fill a map, fork it, change a tenth of the fork's keys, then give the fork's hold back a step at a time
  /// and the original's after it. Expect: no step frees more than one node; the original reads the same after the
  /// fork's release; and once both are released the arena holds no node and every value was freed as often as it was
  /// held.
  #[test]
  fn a_release_a_node_a_step_frees_what_its_map_alone_held() {
    let mut nodes: Nodes<u32> = Nodes::default();
    let mut counted = Counted::default();
    let mut map: Ptrie<u32> = Ptrie::new();
    for n in 0..500u32 {
      counted.put(n);
      map.insert(&mut nodes, format!("d{}/f{n}", n % 7).as_bytes(), n, &mut counted);
    }
    let alone = nodes.live();
    let mut fork = map.fork(&mut nodes);
    for n in (0..500u32).step_by(10) {
      let value = n + 1_000;
      counted.put(value);
      if let Some(replaced) = fork.insert(&mut nodes, format!("d{}/f{n}", n % 7).as_bytes(), value, &mut counted) {
        counted.freed(replaced);
      }
    }
    let step = |nodes: &mut Nodes<u32>, releasing: &mut Releasing, counted: &mut Counted| {
      let before = nodes.live();
      let more = nodes.release_step(releasing, counted);
      assert!(before - nodes.live() <= 1, "a step freed more than one node");
      more
    };
    let mut releasing = fork.releasing();
    while step(&mut nodes, &mut releasing, &mut counted) {}
    assert_eq!(nodes.live(), alone, "the fork's release freed other than what it alone held");
    for n in 0..500u32 {
      assert_eq!(map.get(&nodes, format!("d{}/f{n}", n % 7).as_bytes()), Some(&n));
    }
    let mut releasing = map.releasing();
    while step(&mut nodes, &mut releasing, &mut counted) {}
    assert_eq!(nodes.live(), 0);
    assert_eq!(nodes.missing(), 0);
    for (value, held) in &counted.holds {
      assert_eq!(*held, 0, "value {value} was freed {held} times fewer than it was held");
    }
  }

  /// A-534. Do: fill a map with ten thousand path-like keys, fork it, and change one key of the fork. Expect: the fork
  /// makes no node, the change copies no more nodes than the key has edges, no node is visited by a walk, and the
  /// original reads unchanged.
  #[test]
  fn a_fork_costs_nothing_and_a_change_copies_one_path() {
    let mut nodes: Nodes<u32> = Nodes::default();
    let mut map: Ptrie<u32> = Ptrie::new();
    for n in 0..10_000u32 {
      let key = format!("src/dir{}/file{}.rs", n % 97, n);
      map.insert(&mut nodes, key.as_bytes(), n, &mut Plain);
    }
    let live = nodes.live();
    let visited = nodes.visited();
    let mut fork = map.fork(&mut nodes);
    assert_eq!(nodes.live(), live, "a fork made a node");
    let key = b"src/dir5/file5.rs";
    fork.insert(&mut nodes, key, 0, &mut Plain);
    let copied = nodes.copied();
    assert!(
      copied > 0 && copied <= key.len() as u64,
      "{copied} nodes copied for one change"
    );
    assert_eq!(
      nodes.visited(),
      visited,
      "a fork or a change walked the tree"
    );
    assert_eq!(map.get(&nodes, key), Some(&5));
    assert_eq!(fork.get(&nodes, key), Some(&0));
    assert_eq!(fork.len(&nodes), map.len(&nodes));
  }

  /// A-534 (for Subversion's copy, §4.17.4). Do: hold the subtree under `trunk/` of a map of ten thousand keys and
  /// graft it under `branches/x/` of the same map. Expect: every key under `trunk/` also reads under `branches/x/`, the
  /// graft made at most a handful of nodes (the destination's path and the subtree's top), and no node was walked.
  #[test]
  fn a_graft_shares_its_subtree_and_walks_nothing() {
    let mut nodes: Nodes<u32> = Nodes::default();
    let mut map: Ptrie<u32> = Ptrie::new();
    for n in 0..10_000u32 {
      let key = format!("trunk/dir{}/file{}", n % 31, n);
      map.insert(&mut nodes, key.as_bytes(), n, &mut Plain);
    }
    let live = nodes.live();
    let visited = nodes.visited();
    let shared = map.subtree(&mut nodes, b"trunk/").expect("a subtree");
    map
      .graft(&mut nodes, b"branches/x/", shared, &mut Plain)
      .unwrap();
    assert_eq!(nodes.visited(), visited, "the graft walked the subtree");
    assert!(
      nodes.live() - live <= 4,
      "the graft made {} nodes",
      nodes.live() - live
    );
    assert_eq!(map.len(&nodes), 20_000);
    assert_eq!(map.get(&nodes, b"branches/x/dir3/file3"), Some(&3));
    assert_eq!(map.get(&nodes, b"trunk/dir3/file3"), Some(&3));
    map.insert(&mut nodes, b"branches/x/dir3/file3", 7, &mut Plain);
    assert_eq!(
      map.get(&nodes, b"trunk/dir3/file3"),
      Some(&3),
      "a change under the graft showed in its source"
    );
  }
}
