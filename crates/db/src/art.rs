//! The adaptive radix tree of the partition's indexes (§4.8 `Art<Name, VolumeId>` and the
//! others; [A: Leis, Kemper, Neumann, "The Adaptive Radix Tree", ICDE'13]): a byte-keyed trie
//! whose inner nodes grow through four shapes (4, 16, 48 and 256 children) and shrink back,
//! with path compression (a node keeps the run of bytes every key beneath it shares). A lookup
//! touches one node per key byte at most and no hashing; an insert allocates one node.
//!
//! Shape here: nodes live in a vector arena and name each other by index (ownership by
//! handle, D-8); a node may carry a value (the key ends there) as well as children, so keys
//! need not be prefix-free (ids are fixed-width, names are not); the compressed prefix is
//! stored whole (pessimistic comparison), which costs one small allocation per compressed node
//! and never a wrong answer. Iteration is in key order, the order the snapshot writes.

/// Shape: the four child-array sizes of the tree's inner nodes, from the paper.
const NODE4: usize = 4;
/// Shape: see `NODE4`.
const NODE16: usize = 16;
/// Shape: see `NODE4`.
const NODE48: usize = 48;
/// Shape: see `NODE4`.
const NODE256: usize = 256;
/// Format: the "no child" marker in a Node48's index array (children are at most 47).
const NONE48: u8 = u8::MAX;

/// An index into the node arena.
type NodeIx = u32;

/// The children of a node, in one of the four shapes.
#[derive(Clone, Debug)]
enum Children {
  /// Up to four children, keys and indices in parallel, keys sorted.
  Small {
    keys: [u8; NODE4],
    nodes: [NodeIx; NODE4],
    len: u8,
  },
  /// Up to sixteen, keys sorted.
  Medium {
    keys: [u8; NODE16],
    nodes: [NodeIx; NODE16],
    len: u8,
  },
  /// Up to 48: a 256-entry index into a 48-entry node array.
  Large {
    index: Box<[u8; NODE256]>,
    nodes: Box<[NodeIx; NODE48]>,
    len: u8,
  },
  /// One slot per byte.
  Full {
    nodes: Box<[Option<NodeIx>; NODE256]>,
    len: u16,
  },
}

#[derive(Clone, Debug)]
struct Node<V> {
  prefix: Vec<u8>,
  value: Option<V>,
  children: Children,
}

/// The tree.
#[derive(Clone, Debug)]
pub struct Art<V> {
  nodes: Vec<Node<V>>,
  free: Vec<NodeIx>,
  root: Option<NodeIx>,
  len: usize,
}

impl<V> Default for Art<V> {
  fn default() -> Self {
    Self::new()
  }
}

impl Children {
  fn empty() -> Children {
    Children::Small {
      keys: [0; NODE4],
      nodes: [0; NODE4],
      len: 0,
    }
  }

  fn len(&self) -> usize {
    match self {
      Children::Small { len, .. } | Children::Medium { len, .. } | Children::Large { len, .. } => {
        usize::from(*len)
      }
      Children::Full { len, .. } => usize::from(*len),
    }
  }

  fn get(&self, byte: u8) -> Option<NodeIx> {
    match self {
      Children::Small { keys, nodes, len } => {
        sorted_position(keys, *len, byte).and_then(|at| nodes.get(at).copied())
      }
      Children::Medium { keys, nodes, len } => {
        sorted_position(keys, *len, byte).and_then(|at| nodes.get(at).copied())
      }
      Children::Large { index, nodes, .. } => {
        let slot = *index.get(usize::from(byte))?;
        if slot == NONE48 {
          None
        } else {
          nodes.get(usize::from(slot)).copied()
        }
      }
      Children::Full { nodes, .. } => nodes.get(usize::from(byte)).copied().flatten(),
    }
  }

  /// Replaces the child at `byte` (which must exist).
  fn set(&mut self, byte: u8, node: NodeIx) {
    let slot = match self {
      Children::Small { keys, nodes, len } => {
        sorted_position(keys, *len, byte).and_then(|at| nodes.get_mut(at))
      }
      Children::Medium { keys, nodes, len } => {
        sorted_position(keys, *len, byte).and_then(|at| nodes.get_mut(at))
      }
      Children::Large { index, nodes, .. } => index
        .get(usize::from(byte))
        .filter(|slot| **slot != NONE48)
        .and_then(|slot| nodes.get_mut(usize::from(*slot))),
      Children::Full { nodes, .. } => {
        if let Some(slot) = nodes.get_mut(usize::from(byte)) {
          *slot = Some(node);
        }
        return;
      }
    };
    if let Some(slot) = slot {
      *slot = node;
    }
  }

  /// Inserts a child at a byte not yet present, growing the shape when full.
  fn insert(&mut self, byte: u8, node: NodeIx) {
    if self.is_full() {
      self.grow();
    }
    match self {
      Children::Small { keys, nodes, len } => {
        insert_sorted(keys, nodes, len, byte, node);
      }
      Children::Medium { keys, nodes, len } => {
        insert_sorted(keys, nodes, len, byte, node);
      }
      Children::Large { index, nodes, len } => {
        // `grow` left room, so slot `len` is free.
        if let (Some(free), Some(at)) = (
          nodes.get_mut(usize::from(*len)),
          index.get_mut(usize::from(byte)),
        ) {
          *free = node;
          *at = *len;
          *len = len.saturating_add(1);
        }
      }
      Children::Full { nodes, len } => {
        if let Some(slot) = nodes.get_mut(usize::from(byte)) {
          *slot = Some(node);
          *len = len.saturating_add(1);
        }
      }
    }
  }

  fn is_full(&self) -> bool {
    match self {
      Children::Small { len, .. } => usize::from(*len) == NODE4,
      Children::Medium { len, .. } => usize::from(*len) == NODE16,
      Children::Large { len, .. } => usize::from(*len) == NODE48,
      Children::Full { .. } => false,
    }
  }

  fn grow(&mut self) {
    let pairs = self.pairs();
    *self = match self {
      Children::Small { .. } => Children::Medium {
        keys: [0; NODE16],
        nodes: [0; NODE16],
        len: 0,
      },
      Children::Medium { .. } => Children::Large {
        index: Box::new([NONE48; NODE256]),
        nodes: Box::new([0; NODE48]),
        len: 0,
      },
      Children::Large { .. } | Children::Full { .. } => Children::Full {
        nodes: Box::new([None; NODE256]),
        len: 0,
      },
    };
    for (byte, node) in pairs {
      self.insert(byte, node);
    }
  }

  /// Removes the child at `byte`, shrinking the shape when it fits a smaller one.
  fn remove(&mut self, byte: u8) -> Option<NodeIx> {
    let removed = match self {
      Children::Small { keys, nodes, len } => remove_sorted(keys, nodes, len, byte),
      Children::Medium { keys, nodes, len } => remove_sorted(keys, nodes, len, byte),
      Children::Large { index, nodes, len } => remove_large(index, nodes, len, byte),
      Children::Full { nodes, len } => {
        let removed = nodes.get_mut(usize::from(byte)).and_then(Option::take);
        if removed.is_some() {
          *len = len.saturating_sub(1);
        }
        removed
      }
    };
    self.shrink();
    removed
  }

  fn shrink(&mut self) {
    let len = self.len();
    let target = match self {
      Children::Full { .. } if len <= NODE48 => Some(NODE48),
      Children::Large { .. } if len <= NODE16 => Some(NODE16),
      Children::Medium { .. } if len <= NODE4 => Some(NODE4),
      _ => None,
    };
    let Some(target) = target else { return };
    let pairs = self.pairs();
    *self = match target {
      NODE48 => Children::Large {
        index: Box::new([NONE48; NODE256]),
        nodes: Box::new([0; NODE48]),
        len: 0,
      },
      NODE16 => Children::Medium {
        keys: [0; NODE16],
        nodes: [0; NODE16],
        len: 0,
      },
      _ => Children::empty(),
    };
    for (byte, node) in pairs {
      self.insert(byte, node);
    }
  }

  /// Every child, in key order.
  fn pairs(&self) -> Vec<(u8, NodeIx)> {
    match self {
      Children::Small { keys, nodes, len } => used(keys, *len)
        .iter()
        .copied()
        .zip(nodes.iter().copied())
        .collect(),
      Children::Medium { keys, nodes, len } => used(keys, *len)
        .iter()
        .copied()
        .zip(nodes.iter().copied())
        .collect(),
      Children::Large { index, nodes, .. } => (0..=u8::MAX)
        .zip(index.iter())
        .filter(|(_, slot)| **slot != NONE48)
        .filter_map(|(byte, slot)| nodes.get(usize::from(*slot)).map(|node| (byte, *node)))
        .collect(),
      Children::Full { nodes, .. } => (0..=u8::MAX)
        .zip(nodes.iter())
        .filter_map(|(byte, node)| node.map(|node| (byte, node)))
        .collect(),
    }
  }
}

/// The first `len` keys of a sorted shape: its children's keys.
fn used(keys: &[u8], len: u8) -> &[u8] {
  keys.get(..usize::from(len)).unwrap_or(keys)
}

/// Where `byte` sits among a sorted shape's first `len` keys.
fn sorted_position(keys: &[u8], len: u8, byte: u8) -> Option<usize> {
  used(keys, len).iter().position(|key| *key == byte)
}

/// Inserts into a sorted shape with room (`insert` grows a full one first).
fn insert_sorted<const N: usize>(
  keys: &mut [u8; N],
  nodes: &mut [NodeIx; N],
  len: &mut u8,
  byte: u8,
  node: NodeIx,
) {
  let n = usize::from(*len);
  if n >= N {
    return;
  }
  let at = used(keys, *len).iter().position(|k| *k > byte).unwrap_or(n);
  // `at <= n < N`, so both ranges lie inside the arrays.
  keys.copy_within(at..n, at.saturating_add(1));
  nodes.copy_within(at..n, at.saturating_add(1));
  if let (Some(key), Some(child)) = (keys.get_mut(at), nodes.get_mut(at)) {
    *key = byte;
    *child = node;
    *len = len.saturating_add(1);
  }
}

fn remove_sorted<const N: usize>(
  keys: &mut [u8; N],
  nodes: &mut [NodeIx; N],
  len: &mut u8,
  byte: u8,
) -> Option<NodeIx> {
  let n = usize::from(*len).min(N);
  let at = sorted_position(keys, *len, byte)?;
  let removed = *nodes.get(at)?;
  // `at < n <= N`, so both ranges lie inside the arrays.
  keys.copy_within(at.saturating_add(1)..n, at);
  nodes.copy_within(at.saturating_add(1)..n, at);
  *len = len.saturating_sub(1);
  Some(removed)
}

/// Removes from a Node48, moving the last child into the freed slot so the array stays dense.
fn remove_large(
  index: &mut [u8; NODE256],
  nodes: &mut [NodeIx; NODE48],
  len: &mut u8,
  byte: u8,
) -> Option<NodeIx> {
  let at = index.get_mut(usize::from(byte))?;
  let slot = *at;
  if slot == NONE48 {
    return None;
  }
  *at = NONE48;
  let removed = *nodes.get(usize::from(slot))?;
  let last = len.saturating_sub(1);
  if slot != last {
    if let Some(&moved_node) = nodes.get(usize::from(last))
      && let Some(freed) = nodes.get_mut(usize::from(slot))
    {
      *freed = moved_node;
    }
    if let Some(moved) = index.iter_mut().find(|s| **s == last) {
      *moved = slot;
    }
  }
  *len = last;
  Some(removed)
}

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
  a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

impl<V> Art<V> {
  /// An empty tree.
  pub fn new() -> Self {
    Art {
      nodes: Vec::new(),
      free: Vec::new(),
      root: None,
      len: 0,
    }
  }

  /// Keys stored.
  pub fn len(&self) -> usize {
    self.len
  }

  /// Whether no key is stored.
  pub fn is_empty(&self) -> bool {
    self.len == 0
  }

  fn node(&self, ix: NodeIx) -> Option<&Node<V>> {
    self.nodes.get(usize::try_from(ix).ok()?)
  }

  fn node_mut(&mut self, ix: NodeIx) -> Option<&mut Node<V>> {
    self.nodes.get_mut(usize::try_from(ix).ok()?)
  }

  fn alloc(&mut self, node: Node<V>) -> NodeIx {
    if let Some(ix) = self.free.pop()
      && let Some(slot) = self.node_mut(ix)
    {
      *slot = node;
      return ix;
    }
    self.nodes.push(node);
    u32::try_from(self.nodes.len().saturating_sub(1)).unwrap_or(u32::MAX)
  }

  fn release(&mut self, ix: NodeIx) {
    if let Some(node) = self.node_mut(ix) {
      node.value = None;
      node.children = Children::empty();
      node.prefix.clear();
      self.free.push(ix);
    }
  }

  /// The node `key` ends at, if it is in the tree.
  fn find(&self, key: &[u8]) -> Option<NodeIx> {
    let mut ix = self.root?;
    let mut rest = key;
    loop {
      let node = self.node(ix)?;
      rest = rest.strip_prefix(node.prefix.as_slice())?;
      let Some((&byte, tail)) = rest.split_first() else {
        return Some(ix);
      };
      ix = node.children.get(byte)?;
      rest = tail;
    }
  }

  /// The value at `key`.
  pub fn get(&self, key: &[u8]) -> Option<&V> {
    self.node(self.find(key)?)?.value.as_ref()
  }

  /// The value at `key`, mutably.
  pub fn get_mut(&mut self, key: &[u8]) -> Option<&mut V> {
    let ix = self.find(key)?;
    self.node_mut(ix)?.value.as_mut()
  }

  /// Inserts `value` at `key`, returning the previous value.
  pub fn insert(&mut self, key: &[u8], value: V) -> Option<V> {
    let Some(root) = self.root else {
      let ix = self.alloc(Node {
        prefix: key.to_vec(),
        value: Some(value),
        children: Children::empty(),
      });
      self.root = Some(ix);
      self.len = self.len.saturating_add(1);
      return None;
    };
    let (ix, previous) = self.insert_at(root, key, value);
    self.root = Some(ix);
    if previous.is_none() {
      self.len = self.len.saturating_add(1);
    }
    previous
  }

  /// Inserts beneath `ix`; returns the node now standing where `ix` stood (a split may put a
  /// new parent above it) and the previous value. Every `ix` here came from `alloc`, so the node is
  /// always present; the miss arm keeps the tree as it was.
  fn insert_at(&mut self, ix: NodeIx, key: &[u8], value: V) -> (NodeIx, Option<V>) {
    let Some(node) = self.node_mut(ix) else {
      return (ix, None);
    };
    let shared = common_prefix(&node.prefix, key);
    if let Some((&old_byte, old_tail)) = node.prefix.get(shared..).and_then(<[u8]>::split_first) {
      // Split: a new parent with the shared run; the old node keeps the rest of its prefix.
      node.prefix = old_tail.to_vec();
      let mut parent = Node {
        prefix: key.get(..shared).unwrap_or_default().to_vec(),
        value: None,
        children: Children::empty(),
      };
      parent.children.insert(old_byte, ix);
      let parent_ix = self.alloc(parent);
      match key.get(shared..).unwrap_or_default().split_first() {
        None => {
          if let Some(parent) = self.node_mut(parent_ix) {
            parent.value = Some(value);
          }
        }
        Some((&byte, tail)) => {
          let leaf = self.alloc(Node {
            prefix: tail.to_vec(),
            value: Some(value),
            children: Children::empty(),
          });
          if let Some(parent) = self.node_mut(parent_ix) {
            parent.children.insert(byte, leaf);
          }
        }
      }
      return (parent_ix, None);
    }
    // No split: the whole prefix is shared, so `key` continues past it.
    let rest = key.get(shared..).unwrap_or_default();
    let Some((&byte, tail)) = rest.split_first() else {
      return (ix, node.value.replace(value));
    };
    match node.children.get(byte) {
      Some(child) => {
        let (new_child, previous) = self.insert_at(child, tail, value);
        if new_child != child
          && let Some(node) = self.node_mut(ix)
        {
          node.children.set(byte, new_child);
        }
        (ix, previous)
      }
      None => {
        let leaf = self.alloc(Node {
          prefix: tail.to_vec(),
          value: Some(value),
          children: Children::empty(),
        });
        if let Some(node) = self.node_mut(ix) {
          node.children.insert(byte, leaf);
        }
        (ix, None)
      }
    }
  }

  /// Removes the value at `key`.
  pub fn remove(&mut self, key: &[u8]) -> Option<V> {
    let root = self.root?;
    let (keep, removed) = self.remove_at(root, key);
    self.root = keep;
    if removed.is_some() {
      self.len = self.len.saturating_sub(1);
    }
    removed
  }

  /// Removes beneath `ix`; returns the node that should stand where `ix` stood (`None` when
  /// the subtree became empty) and the removed value. A node left with no value and one child
  /// merges with that child (path compression restored).
  fn remove_at(&mut self, ix: NodeIx, key: &[u8]) -> (Option<NodeIx>, Option<V>) {
    let Some(node) = self.node(ix) else {
      return (Some(ix), None);
    };
    let Some(rest) = key.strip_prefix(node.prefix.as_slice()) else {
      return (Some(ix), None);
    };
    let removed = match rest.split_first() {
      None => self.node_mut(ix).and_then(|node| node.value.take()),
      Some((&byte, tail)) => {
        let Some(child) = node.children.get(byte) else {
          return (Some(ix), None);
        };
        let (keep, removed) = self.remove_at(child, tail);
        if let Some(node) = self.node_mut(ix) {
          match keep {
            Some(k) if k != child => node.children.set(byte, k),
            Some(_) => {}
            None => {
              node.children.remove(byte);
            }
          }
        }
        removed
      }
    };
    if removed.is_none() {
      return (Some(ix), None);
    }
    (self.compact(ix), removed)
  }

  /// After a removal: an empty node goes away; a valueless node with one child merges into it.
  fn compact(&mut self, ix: NodeIx) -> Option<NodeIx> {
    let node = self.node(ix)?;
    let children = node.children.len();
    if node.value.is_some() || children > 1 {
      return Some(ix);
    }
    let only = if children == 1 {
      node.children.pairs().first().copied()
    } else {
      None
    };
    let Some((byte, child)) = only else {
      self.release(ix);
      return None;
    };
    let mut merged = node.prefix.clone();
    merged.push(byte);
    let Some(child_node) = self.node_mut(child) else {
      return Some(ix);
    };
    merged.append(&mut child_node.prefix);
    child_node.prefix = merged;
    self.release(ix);
    Some(child)
  }

  /// Every key and value, in key order.
  pub fn iter(&self) -> impl Iterator<Item = (Vec<u8>, &V)> {
    let mut out = Vec::with_capacity(self.len);
    if let Some(root) = self.root {
      self.collect(root, Vec::new(), &mut out);
    }
    out.into_iter()
  }

  fn collect<'a>(&'a self, ix: NodeIx, mut so_far: Vec<u8>, out: &mut Vec<(Vec<u8>, &'a V)>) {
    let Some(node) = self.node(ix) else {
      return;
    };
    so_far.extend_from_slice(&node.prefix);
    if let Some(v) = &node.value {
      out.push((so_far.clone(), v));
    }
    for (byte, child) in node.children.pairs() {
      let mut key = so_far.clone();
      key.push(byte);
      self.collect(child, key, out);
    }
  }

  /// Every value whose key starts with `prefix`, in key order.
  pub fn scan_prefix(&self, prefix: &[u8]) -> Vec<(Vec<u8>, &V)> {
    self.iter().filter(|(k, _)| k.starts_with(prefix)).collect()
  }
}

#[cfg(test)]
// proptest's strategy types carry `Arc` (D-8's harness exception).
#[allow(clippy::disallowed_types)]
mod tests {
  use std::collections::BTreeMap;

  use proptest::prelude::*;

  use super::*;

  #[derive(Clone, Debug)]
  enum Step {
    Insert(Vec<u8>, u32),
    Remove(Vec<u8>),
  }

  fn key() -> impl Strategy<Value = Vec<u8>> {
    // Keys from a small alphabet with shared prefixes, so splits and merges happen often, plus
    // fixed-width ids and prefixes of other keys.
    prop_oneof![
      proptest::collection::vec(0u8..4, 0..6),
      proptest::collection::vec(any::<u8>(), 8..=8),
      Just(Vec::new()),
    ]
  }

  fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
      (key(), any::<u32>()).prop_map(|(k, v)| Step::Insert(k, v)),
      key().prop_map(Step::Remove),
    ]
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: 400, ..ProptestConfig::default() }))]

    /// The tree equals an ordered map on every generated history: insert, remove, get, and
    /// iteration order, with prefixes of other keys and empty keys included.
    #[test]
    fn the_tree_equals_an_ordered_map(steps in proptest::collection::vec(step(), 0..300)) {
      let mut art: Art<u32> = Art::new();
      let mut model: BTreeMap<Vec<u8>, u32> = BTreeMap::new();
      for s in steps {
        match s {
          Step::Insert(k, v) => {
            prop_assert_eq!(art.insert(&k, v), model.insert(k, v));
          }
          Step::Remove(k) => {
            prop_assert_eq!(art.remove(&k), model.remove(&k));
          }
        }
        prop_assert_eq!(art.len(), model.len());
        for (k, v) in &model {
          prop_assert_eq!(art.get(k), Some(v));
        }
        let listed: Vec<(Vec<u8>, u32)> = art.iter().map(|(k, v)| (k, *v)).collect();
        let expected: Vec<(Vec<u8>, u32)> = model.iter().map(|(k, v)| (k.clone(), *v)).collect();
        prop_assert_eq!(listed, expected);
      }
    }
  }

  /// Every node shape is reached and left: 300 keys under one byte fill a Node256 and empty it.
  #[test]
  fn nodes_grow_through_every_shape_and_shrink_back() {
    let mut art: Art<u32> = Art::new();
    for b in 0..=u8::MAX {
      art.insert(&[7, b], u32::from(b));
    }
    assert_eq!(art.len(), 256);
    for b in 0..=u8::MAX {
      assert_eq!(art.get(&[7, b]), Some(&u32::from(b)));
    }
    for b in 0..=u8::MAX {
      assert_eq!(art.remove(&[7, b]), Some(u32::from(b)));
    }
    assert!(art.is_empty());
    assert!(art.get(&[7]).is_none());
    assert_eq!(art.scan_prefix(&[7]).len(), 0);
  }
}
