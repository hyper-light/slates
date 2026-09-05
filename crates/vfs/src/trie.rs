//! The inode table: a copy-on-write radix trie from inode number to inode handle, sixteen ways
//! per level over the number's counter bits, path-copied by birth epoch like the directory tree
//! (D-5). Hard links need this indirection: two directory entries name one number, and a
//! snapshot must keep the number's old inode while the head replaces it.
//!
//! Counters are dense and monotonic, so the trie is compact: a million inodes occupy about five
//! levels of mostly full nodes. A lookup is one slab access per level.

use slates_mem::{Handle, Slab};

use crate::error::VfsError;
use crate::ids::{Epoch, InodeNo};
use crate::inode::Inode;
use crate::snapshot::{Dead, Deadlist};

/// Format: children per node (one hex digit of the counter per level).
pub const FANOUT: usize = 16;
/// Format: bits per level.
const BITS: u32 = 4;
/// Format: levels needed for the counter's width.
pub const LEVELS: u32 = InodeNo::COUNTER_BITS / BITS;
/// Format: the digit mask.
const DIGIT_MASK: u64 = (FANOUT as u64) - 1;

/// One slot of a node: nothing, a child node, or (at the last level) an inode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
  /// Empty.
  Empty,
  /// A child node.
  Node(Handle<TrieNode>),
  /// An inode, at the last level.
  Inode(Handle<Inode>),
}

/// A trie node.
#[derive(Clone, Debug)]
pub struct TrieNode {
  /// The birth epoch.
  pub born: Epoch,
  /// The slots.
  pub slots: [Slot; FANOUT],
}

impl TrieNode {
  fn empty(born: Epoch) -> Self {
    Self {
      born,
      slots: [Slot::Empty; FANOUT],
    }
  }
}

/// The digit of `no` at `level` (0 = the top level).
fn digit(no: InodeNo, level: u32) -> usize {
  let shift = (LEVELS - 1 - level) * BITS;
  usize::try_from((no.counter() >> shift) & DIGIT_MASK).unwrap_or(0)
}

/// A new, empty root.
pub fn new_root(nodes: &mut Slab<TrieNode>, epoch: Epoch) -> Result<Handle<TrieNode>, VfsError> {
  Ok(nodes.insert(TrieNode::empty(epoch))?)
}

/// The inode handle for `no`, if present.
pub fn get(nodes: &Slab<TrieNode>, root: Handle<TrieNode>, no: InodeNo) -> Option<Handle<Inode>> {
  let mut node = root;
  for level in 0..LEVELS {
    let slot = nodes.get(node).ok()?.slots[digit(no, level)];
    match slot {
      Slot::Empty => return None,
      Slot::Node(next) => node = next,
      Slot::Inode(handle) => return Some(handle),
    }
  }
  None
}

/// Sets `no` to `handle` (inserting or replacing), copying every node on the path born before
/// `epoch` and reporting the replaced nodes to `dead`. Returns the (possibly new) root and the
/// previous handle, if any.
pub fn set(
  nodes: &mut Slab<TrieNode>,
  root: Handle<TrieNode>,
  no: InodeNo,
  handle: Handle<Inode>,
  epoch: Epoch,
  dead: &mut Deadlist,
) -> Result<(Handle<TrieNode>, Option<Handle<Inode>>), VfsError> {
  let mut path: Vec<(Handle<TrieNode>, usize)> =
    Vec::with_capacity(usize::try_from(LEVELS).unwrap_or(0));
  let mut node = ensure_current(nodes, root, epoch, dead)?;
  let new_root = node;
  let mut previous = None;
  for level in 0..LEVELS {
    let d = digit(no, level);
    let last = level == LEVELS - 1;
    let current = nodes.get(node)?.slots[d];
    if last {
      if let Slot::Inode(old) = current {
        previous = Some(old);
      }
      nodes.get_mut(node)?.slots[d] = Slot::Inode(handle);
      break;
    }
    let child = match current {
      Slot::Node(child) => ensure_current(nodes, child, epoch, dead)?,
      _ => nodes.insert(TrieNode::empty(epoch))?,
    };
    nodes.get_mut(node)?.slots[d] = Slot::Node(child);
    path.push((node, d));
    node = child;
  }
  Ok((new_root, previous))
}

/// Removes `no`, copying the path as `set` does; returns the (possibly new) root and the removed
/// handle, if any. Empty nodes are left in place: numbers are never reused, so a freed leaf's
/// node stays sparse and is reclaimed with its snapshot's deadlist or the volume.
pub fn remove(
  nodes: &mut Slab<TrieNode>,
  root: Handle<TrieNode>,
  no: InodeNo,
  epoch: Epoch,
  dead: &mut Deadlist,
) -> Result<(Handle<TrieNode>, Option<Handle<Inode>>), VfsError> {
  if get(nodes, root, no).is_none() {
    return Ok((root, None));
  }
  let mut node = ensure_current(nodes, root, epoch, dead)?;
  let new_root = node;
  for level in 0..LEVELS {
    let d = digit(no, level);
    let current = nodes.get(node)?.slots[d];
    match current {
      Slot::Inode(old) => {
        nodes.get_mut(node)?.slots[d] = Slot::Empty;
        return Ok((new_root, Some(old)));
      }
      Slot::Node(child) => {
        let child = ensure_current(nodes, child, epoch, dead)?;
        nodes.get_mut(node)?.slots[d] = Slot::Node(child);
        node = child;
      }
      Slot::Empty => return Ok((new_root, None)),
    }
  }
  Ok((new_root, None))
}

/// A node that may be mutated in `epoch`: the node itself if born in it, else a copy born now,
/// with the original reported to the deadlist.
fn ensure_current(
  nodes: &mut Slab<TrieNode>,
  node: Handle<TrieNode>,
  epoch: Epoch,
  dead: &mut Deadlist,
) -> Result<Handle<TrieNode>, VfsError> {
  let born = nodes.get(node)?.born;
  if born == epoch {
    return Ok(node);
  }
  let mut copy = nodes.get(node)?.clone();
  copy.born = epoch;
  let fresh = nodes.insert(copy)?;
  dead.push(Dead::Trie(node, born));
  Ok(fresh)
}

/// Walks every inode handle reachable from `root`, in number order.
pub fn walk(nodes: &Slab<TrieNode>, root: Handle<TrieNode>, out: &mut Vec<Handle<Inode>>) {
  walk_since(nodes, root, None, out);
}

/// Every inode reachable from `root` through nodes born after `since`: a node born at or
/// before `since` is shared with the origin (a copy forces its parent's copy, so no newer node
/// hides beneath an older one) and is not entered, which makes the walk proportional to the
/// volume's own nodes (the pruned traversal of ZFS's clone destroy; §4.5).
pub fn walk_since(
  nodes: &Slab<TrieNode>,
  root: Handle<TrieNode>,
  since: Option<Epoch>,
  out: &mut Vec<Handle<Inode>>,
) {
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    let Ok(n) = nodes.get(node) else { continue };
    if since.is_some_and(|s| n.born.0 <= s.0) {
      continue;
    }
    for slot in n.slots.iter().rev() {
      match slot {
        Slot::Node(child) => stack.push(*child),
        Slot::Inode(h) => out.push(*h),
        Slot::Empty => {}
      }
    }
  }
}

/// Every trie node reachable from `root`, for a destroy walk.
pub fn nodes_under(
  nodes: &Slab<TrieNode>,
  root: Handle<TrieNode>,
  out: &mut Vec<Handle<TrieNode>>,
) {
  nodes_under_since(nodes, root, None, out);
}

/// Every trie node reachable from `root` and born after `since` (see [`walk_since`]).
pub fn nodes_under_since(
  nodes: &Slab<TrieNode>,
  root: Handle<TrieNode>,
  since: Option<Epoch>,
  out: &mut Vec<Handle<TrieNode>>,
) {
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    let Ok(n) = nodes.get(node) else { continue };
    if since.is_some_and(|s| n.born.0 <= s.0) {
      continue;
    }
    out.push(node);
    for slot in &n.slots {
      if let Slot::Node(child) = slot {
        stack.push(*child);
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::inode::{Body, Inode, Kind};

  fn inode(store: &mut Slab<Inode>, no: u64) -> Handle<Inode> {
    store
      .insert(Inode::new(
        InodeNo::compose(1, no),
        Epoch(0),
        Kind::File,
        0o644,
        Body::Inline(Vec::new()),
      ))
      .unwrap()
  }

  struct Fixture {
    nodes: Slab<TrieNode>,
    inodes: Slab<Inode>,
    root: Handle<TrieNode>,
    a: Handle<Inode>,
    b: Handle<Inode>,
  }

  fn two_inodes() -> Fixture {
    let mut nodes: Slab<TrieNode> = Slab::new(64, 1 << 16);
    let mut inodes: Slab<Inode> = Slab::new(64, 1 << 16);
    let mut dead = Deadlist::default();
    let root0 = new_root(&mut nodes, Epoch(0)).unwrap();
    let a = inode(&mut inodes, 1);
    let b = inode(&mut inodes, 4097);
    let (root, _) = set(
      &mut nodes,
      root0,
      InodeNo::compose(1, 1),
      a,
      Epoch(0),
      &mut dead,
    )
    .unwrap();
    assert_eq!(root, root0, "same epoch mutates in place");
    let (root, _) = set(
      &mut nodes,
      root,
      InodeNo::compose(1, 4097),
      b,
      Epoch(0),
      &mut dead,
    )
    .unwrap();
    assert!(dead.is_empty());
    Fixture {
      nodes,
      inodes,
      root,
      a,
      b,
    }
  }

  #[test]
  fn set_and_get_in_one_epoch() {
    let f = two_inodes();
    assert_eq!(get(&f.nodes, f.root, InodeNo::compose(1, 1)), Some(f.a));
    assert_eq!(get(&f.nodes, f.root, InodeNo::compose(1, 4097)), Some(f.b));
    assert_eq!(get(&f.nodes, f.root, InodeNo::compose(1, 2)), None);
  }

  #[test]
  fn a_later_epoch_copies_the_path_and_leaves_the_old_root_intact() {
    let mut f = two_inodes();
    let mut dead = Deadlist::default();
    let c = inode(&mut f.inodes, 1);
    let (root2, prev) = set(
      &mut f.nodes,
      f.root,
      InodeNo::compose(1, 1),
      c,
      Epoch(1),
      &mut dead,
    )
    .unwrap();
    assert_eq!(prev, Some(f.a));
    assert_ne!(root2, f.root);
    assert_eq!(
      get(&f.nodes, f.root, InodeNo::compose(1, 1)),
      Some(f.a),
      "the old root still sees the old inode"
    );
    assert_eq!(get(&f.nodes, root2, InodeNo::compose(1, 1)), Some(c));
    assert_eq!(
      get(&f.nodes, root2, InodeNo::compose(1, 4097)),
      Some(f.b),
      "untouched subtrees are shared"
    );
    assert_eq!(
      dead.len(),
      usize::try_from(LEVELS).unwrap(),
      "every node on the path was copied once"
    );
  }

  #[test]
  fn removal_copies_the_path_too_and_the_walk_lists_what_remains() {
    let mut f = two_inodes();
    let mut dead = Deadlist::default();
    let (root2, removed) = remove(
      &mut f.nodes,
      f.root,
      InodeNo::compose(1, 4097),
      Epoch(1),
      &mut dead,
    )
    .unwrap();
    assert_eq!(removed, Some(f.b));
    assert_eq!(get(&f.nodes, root2, InodeNo::compose(1, 4097)), None);
    assert_eq!(get(&f.nodes, f.root, InodeNo::compose(1, 4097)), Some(f.b));
    let mut all = Vec::new();
    walk(&f.nodes, root2, &mut all);
    assert_eq!(all, vec![f.a]);
  }
}
