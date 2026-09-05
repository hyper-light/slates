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
      Children::Small { keys, nodes, len } => keys[..usize::from(*len)]
        .iter()
        .position(|k| *k == byte)
        .map(|i| nodes[i]),
      Children::Medium { keys, nodes, len } => keys[..usize::from(*len)]
        .iter()
        .position(|k| *k == byte)
        .map(|i| nodes[i]),
      Children::Large { index, nodes, .. } => {
        let slot = index[usize::from(byte)];
        (slot != NONE48).then(|| nodes[usize::from(slot)])
      }
      Children::Full { nodes, .. } => nodes[usize::from(byte)],
    }
  }

  /// Replaces the child at `byte` (which must exist).
  fn set(&mut self, byte: u8, node: NodeIx) {
    match self {
      Children::Small { keys, nodes, len } => {
        if let Some(i) = keys[..usize::from(*len)].iter().position(|k| *k == byte) {
          nodes[i] = node;
        }
      }
      Children::Medium { keys, nodes, len } => {
        if let Some(i) = keys[..usize::from(*len)].iter().position(|k| *k == byte) {
          nodes[i] = node;
        }
      }
      Children::Large { index, nodes, .. } => {
        let slot = index[usize::from(byte)];
        if slot != NONE48 {
          nodes[usize::from(slot)] = node;
        }
      }
      Children::Full { nodes, .. } => nodes[usize::from(byte)] = Some(node),
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
        let slot = *len;
        nodes[usize::from(slot)] = node;
        index[usize::from(byte)] = slot;
        *len += 1;
      }
      Children::Full { nodes, len } => {
        nodes[usize::from(byte)] = Some(node);
        *len += 1;
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
      Children::Large { index, nodes, len } => {
        let slot = index[usize::from(byte)];
        if slot == NONE48 {
          return None;
        }
        index[usize::from(byte)] = NONE48;
        let removed = nodes[usize::from(slot)];
        // Move the last node into the freed slot so the array stays dense.
        let last = *len - 1;
        if slot != last {
          nodes[usize::from(slot)] = nodes[usize::from(last)];
          if let Some(moved) = index.iter().position(|s| *s == last) {
            index[moved] = slot;
          }
        }
        *len -= 1;
        Some(removed)
      }
      Children::Full { nodes, len } => {
        let removed = nodes[usize::from(byte)].take();
        if removed.is_some() {
          *len -= 1;
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
      Children::Small { keys, nodes, len } => keys[..usize::from(*len)]
        .iter()
        .copied()
        .zip(nodes[..usize::from(*len)].iter().copied())
        .collect(),
      Children::Medium { keys, nodes, len } => keys[..usize::from(*len)]
        .iter()
        .copied()
        .zip(nodes[..usize::from(*len)].iter().copied())
        .collect(),
      Children::Large { index, nodes, .. } => (0..=u8::MAX)
        .filter_map(|b| {
          let slot = index[usize::from(b)];
          (slot != NONE48).then(|| (b, nodes[usize::from(slot)]))
        })
        .collect(),
      Children::Full { nodes, .. } => (0..=u8::MAX)
        .filter_map(|b| nodes[usize::from(b)].map(|n| (b, n)))
        .collect(),
    }
  }
}

fn insert_sorted<const N: usize>(
  keys: &mut [u8; N],
  nodes: &mut [NodeIx; N],
  len: &mut u8,
  byte: u8,
  node: NodeIx,
) {
  let n = usize::from(*len);
  let at = keys[..n].iter().position(|k| *k > byte).unwrap_or(n);
  keys.copy_within(at..n, at + 1);
  nodes.copy_within(at..n, at + 1);
  keys[at] = byte;
  nodes[at] = node;
  *len += 1;
}

fn remove_sorted<const N: usize>(
  keys: &mut [u8; N],
  nodes: &mut [NodeIx; N],
  len: &mut u8,
  byte: u8,
) -> Option<NodeIx> {
  let n = usize::from(*len);
  let at = keys[..n].iter().position(|k| *k == byte)?;
  let removed = nodes[at];
  keys.copy_within(at + 1..n, at);
  nodes.copy_within(at + 1..n, at);
  *len -= 1;
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

  fn alloc(&mut self, node: Node<V>) -> NodeIx {
    if let Some(ix) = self.free.pop() {
      self.nodes[ix as usize] = node;
      ix
    } else {
      self.nodes.push(node);
      u32::try_from(self.nodes.len() - 1).unwrap_or(u32::MAX)
    }
  }

  fn release(&mut self, ix: NodeIx) {
    self.nodes[ix as usize].value = None;
    self.nodes[ix as usize].children = Children::empty();
    self.nodes[ix as usize].prefix.clear();
    self.free.push(ix);
  }

  /// The value at `key`.
  pub fn get(&self, key: &[u8]) -> Option<&V> {
    let mut ix = self.root?;
    let mut rest = key;
    loop {
      let node = &self.nodes[ix as usize];
      if !rest.starts_with(&node.prefix) {
        return None;
      }
      rest = &rest[node.prefix.len()..];
      let Some((&byte, tail)) = rest.split_first() else {
        return node.value.as_ref();
      };
      ix = node.children.get(byte)?;
      rest = tail;
    }
  }

  /// The value at `key`, mutably.
  pub fn get_mut(&mut self, key: &[u8]) -> Option<&mut V> {
    let mut ix = self.root?;
    let mut rest = key;
    loop {
      let node = &self.nodes[ix as usize];
      if !rest.starts_with(&node.prefix) {
        return None;
      }
      rest = &rest[node.prefix.len()..];
      let Some((&byte, tail)) = rest.split_first() else {
        return self.nodes[ix as usize].value.as_mut();
      };
      ix = node.children.get(byte)?;
      rest = tail;
    }
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
      self.len += 1;
      return None;
    };
    let (ix, previous) = self.insert_at(root, key, value);
    self.root = Some(ix);
    if previous.is_none() {
      self.len += 1;
    }
    previous
  }

  /// Inserts beneath `ix`; returns the node now standing where `ix` stood (a split may put a
  /// new parent above it) and the previous value.
  fn insert_at(&mut self, ix: NodeIx, key: &[u8], value: V) -> (NodeIx, Option<V>) {
    let shared = common_prefix(&self.nodes[ix as usize].prefix, key);
    let prefix_len = self.nodes[ix as usize].prefix.len();
    if shared < prefix_len {
      // Split: a new parent with the shared run; the old node keeps the rest of its prefix.
      let parent_prefix = key[..shared].to_vec();
      let old_rest = self.nodes[ix as usize].prefix[shared..].to_vec();
      let old_byte = old_rest[0];
      self.nodes[ix as usize].prefix = old_rest[1..].to_vec();
      let mut parent = Node {
        prefix: parent_prefix,
        value: None,
        children: Children::empty(),
      };
      parent.children.insert(old_byte, ix);
      let parent_ix = self.alloc(parent);
      let rest = &key[shared..];
      match rest.split_first() {
        None => self.nodes[parent_ix as usize].value = Some(value),
        Some((&byte, tail)) => {
          let leaf = self.alloc(Node {
            prefix: tail.to_vec(),
            value: Some(value),
            children: Children::empty(),
          });
          self.nodes[parent_ix as usize].children.insert(byte, leaf);
        }
      }
      return (parent_ix, None);
    }
    let rest = &key[prefix_len..];
    let Some((&byte, tail)) = rest.split_first() else {
      return (ix, self.nodes[ix as usize].value.replace(value));
    };
    match self.nodes[ix as usize].children.get(byte) {
      Some(child) => {
        let (new_child, previous) = self.insert_at(child, tail, value);
        if new_child != child {
          self.nodes[ix as usize].children.set(byte, new_child);
        }
        (ix, previous)
      }
      None => {
        let leaf = self.alloc(Node {
          prefix: tail.to_vec(),
          value: Some(value),
          children: Children::empty(),
        });
        self.nodes[ix as usize].children.insert(byte, leaf);
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
      self.len -= 1;
    }
    removed
  }

  /// Removes beneath `ix`; returns the node that should stand where `ix` stood (`None` when
  /// the subtree became empty) and the removed value. A node left with no value and one child
  /// merges with that child (path compression restored).
  fn remove_at(&mut self, ix: NodeIx, key: &[u8]) -> (Option<NodeIx>, Option<V>) {
    let prefix_len = self.nodes[ix as usize].prefix.len();
    if !key.starts_with(&self.nodes[ix as usize].prefix) {
      return (Some(ix), None);
    }
    let rest = &key[prefix_len..];
    let removed = match rest.split_first() {
      None => self.nodes[ix as usize].value.take(),
      Some((&byte, tail)) => {
        let Some(child) = self.nodes[ix as usize].children.get(byte) else {
          return (Some(ix), None);
        };
        let (keep, removed) = self.remove_at(child, tail);
        match keep {
          Some(k) if k != child => self.nodes[ix as usize].children.set(byte, k),
          Some(_) => {}
          None => {
            self.nodes[ix as usize].children.remove(byte);
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
    let node = &self.nodes[ix as usize];
    let children = node.children.len();
    if node.value.is_some() || children > 1 {
      return Some(ix);
    }
    if children == 0 {
      self.release(ix);
      return None;
    }
    let (byte, child) = self.nodes[ix as usize].children.pairs()[0];
    let mut merged = self.nodes[ix as usize].prefix.clone();
    merged.push(byte);
    merged.extend_from_slice(&self.nodes[child as usize].prefix);
    self.nodes[child as usize].prefix = merged;
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
    let node = &self.nodes[ix as usize];
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
    #![proptest_config(ProptestConfig { cases: 400, failure_persistence: None, ..ProptestConfig::default() })]

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
