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

  /// The slot for digit `d`; `digit` masks to the fanout, so the empty arm never runs.
  fn at(&self, d: usize) -> Slot {
    self.slots.get(d).copied().unwrap_or(Slot::Empty)
  }

  /// Sets the slot for digit `d` (inside the fanout, as `at`).
  fn put(&mut self, d: usize, slot: Slot) {
    if let Some(held) = self.slots.get_mut(d) {
      *held = slot;
    }
  }
}

/// The digit of `no` at `level` (0 = the top level).
fn digit(no: InodeNo, level: u32) -> usize {
  let shift = LEVELS
    .saturating_sub(1)
    .saturating_sub(level)
    .saturating_mul(BITS);
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
    let slot = nodes.get(node).ok()?.at(digit(no, level));
    match slot {
      Slot::Empty => return None,
      Slot::Node(next) => node = next,
      Slot::Inode(handle) => return Some(handle),
    }
  }
  None
}

/// The nodes [`set`] of `no` allocates: a copy of each node on the path born before `epoch`, and a
/// fresh node for each level below the first empty slot.
pub(crate) fn nodes_to_set(
  nodes: &Slab<TrieNode>,
  root: Handle<TrieNode>,
  no: InodeNo,
  epoch: Epoch,
) -> Result<usize, VfsError> {
  let copy = |node: Handle<TrieNode>| -> Result<usize, VfsError> {
    Ok(usize::from(nodes.get(node)?.born != epoch))
  };
  let mut needed = copy(root)?;
  let mut node = root;
  for level in 0..LEVELS.saturating_sub(1) {
    match nodes.get(node)?.at(digit(no, level)) {
      Slot::Node(child) => {
        needed = needed.saturating_add(copy(child)?);
        node = child;
      }
      Slot::Empty | Slot::Inode(_) => {
        let below = usize::try_from(LEVELS.saturating_sub(1).saturating_sub(level)).unwrap_or(0);
        return Ok(needed.saturating_add(below));
      }
    }
  }
  Ok(needed)
}

/// The nodes [`remove`] of `no` allocates: a copy of each node on its path born before `epoch` (none
/// when `no` is absent, which changes nothing).
fn nodes_to_remove(
  nodes: &Slab<TrieNode>,
  root: Handle<TrieNode>,
  no: InodeNo,
  epoch: Epoch,
) -> Result<usize, VfsError> {
  if get(nodes, root, no).is_none() {
    return Ok(0);
  }
  let copy = |node: Handle<TrieNode>| -> Result<usize, VfsError> {
    Ok(usize::from(nodes.get(node)?.born != epoch))
  };
  let mut needed = copy(root)?;
  let mut node = root;
  for level in 0..LEVELS {
    match nodes.get(node)?.at(digit(no, level)) {
      Slot::Node(child) => {
        needed = needed.saturating_add(copy(child)?);
        node = child;
      }
      Slot::Empty | Slot::Inode(_) => break,
    }
  }
  Ok(needed)
}

/// Refuses, before anything changes, a mutation that needs more nodes than the slab can issue — so a
/// set or remove either completes or leaves the trie and the slab exactly as they were (AUD-29-40: a
/// refusal halfway down the path left copies no root reached, and a lost copied root).
fn admit(nodes: &Slab<TrieNode>, needed: usize) -> Result<(), VfsError> {
  if nodes.room() < needed {
    return Err(VfsError::Memory(slates_mem::MemError::SlabFull {
      capacity: nodes.max_slots(),
    }));
  }
  Ok(())
}

/// Sets `no` to `handle` (inserting or replacing), copying every node on the path born before
/// `epoch` and reporting the replaced nodes to `dead`. Returns the (possibly new) root and the
/// previous handle, if any. All or nothing: the nodes it needs are admitted before the first changes.
pub fn set(
  nodes: &mut Slab<TrieNode>,
  root: Handle<TrieNode>,
  no: InodeNo,
  handle: Handle<Inode>,
  epoch: Epoch,
  dead: &mut Deadlist,
) -> Result<(Handle<TrieNode>, Option<Handle<Inode>>), VfsError> {
  set_reaching(nodes, root, no, handle, epoch, dead).map(|(root, previous, _)| (root, previous))
}

/// What [`set_reaching`] returns: the (possibly new) root, the replaced handle if any, and the leaf the number went to.
type Reached = (Handle<TrieNode>, Option<Handle<Inode>>, Handle<TrieNode>);

/// [`set`], also returning the leaf node `no` was placed in (the last level's node), for a [`LeafCursor`].
fn set_reaching(
  nodes: &mut Slab<TrieNode>,
  root: Handle<TrieNode>,
  no: InodeNo,
  handle: Handle<Inode>,
  epoch: Epoch,
  dead: &mut Deadlist,
) -> Result<Reached, VfsError> {
  admit(nodes, nodes_to_set(nodes, root, no, epoch)?)?;
  let mut node = ensure_current(nodes, root, epoch, dead)?;
  let new_root = node;
  let mut previous = None;
  for level in 0..LEVELS {
    let d = digit(no, level);
    let last = level == LEVELS - 1;
    let current = nodes.get(node)?.at(d);
    if last {
      if let Slot::Inode(old) = current {
        previous = Some(old);
      }
      nodes.get_mut(node)?.put(d, Slot::Inode(handle));
      break;
    }
    let child = match current {
      Slot::Node(child) => ensure_current(nodes, child, epoch, dead)?,
      _ => nodes.insert(TrieNode::empty(epoch))?,
    };
    nodes.get_mut(node)?.put(d, Slot::Node(child));
    node = child;
  }
  Ok((new_root, previous, node))
}

/// The leaf a run of [`set_in_order`] calls last placed into, so the next number that shares it is placed without a
/// walk from the root. A rebuild places a volume's inodes in number order (an image lists them so), and fifteen of
/// every sixteen numbers share the previous one's leaf: each set walked every level twice (the admission count, then
/// the descent), 30% of a restart's rebuild (2026-10-06, `recover_bench`).
#[derive(Debug, Default)]
pub struct LeafCursor {
  /// The root, the leaf, and the number's digits above the last level, as the last set left them.
  last: Option<(Handle<TrieNode>, Handle<TrieNode>, u64)>,
  /// Counter: sets placed straight into the held leaf, without a walk: the fast path's non-vacuity evidence.
  pub direct: u64,
}

/// [`set`] through `cursor`: when `no` shares the leaf the cursor holds — same root handle, same digits above the last
/// level, and the leaf still live and born in `epoch` — the slot is written there, needing no node and so no admission;
/// otherwise the full set runs and the cursor takes its leaf. Valid between calls only while nothing else changes this
/// trie, which a rebuild's placing loop guarantees; the checks keep a stale cursor from ever writing anywhere but a
/// current leaf of this root.
pub fn set_in_order(
  nodes: &mut Slab<TrieNode>,
  root: Handle<TrieNode>,
  no: InodeNo,
  handle: Handle<Inode>,
  epoch: Epoch,
  dead: &mut Deadlist,
  cursor: &mut LeafCursor,
) -> Result<(Handle<TrieNode>, Option<Handle<Inode>>), VfsError> {
  let above = no.counter() >> BITS;
  if let Some((held_root, leaf, held_above)) = cursor.last
    && held_root == root
    && held_above == above
    && let Ok(node) = nodes.get_mut(leaf)
    && node.born == epoch
  {
    let d = digit(no, LEVELS.saturating_sub(1));
    let previous = match node.at(d) {
      Slot::Inode(old) => Some(old),
      _ => None,
    };
    node.put(d, Slot::Inode(handle));
    cursor.direct = cursor.direct.saturating_add(1);
    return Ok((root, previous));
  }
  let (new_root, previous, leaf) = set_reaching(nodes, root, no, handle, epoch, dead)?;
  cursor.last = Some((new_root, leaf, above));
  Ok((new_root, previous))
}

/// Removes `no`, copying the path as `set` does (all or nothing, admitted the same way); returns the
/// (possibly new) root and the removed handle, if any. A node the removal leaves empty is freed and its slot in the
/// parent cleared, up the path to (never including) the root (A-72): numbers are never reused, so a node left in
/// place would never fill again, and a churning volume grew by one empty node per sixteen numbers it ever held. Every
/// node on the path is current by then (born in `epoch`, copied if it was not), so no snapshot shares a freed node;
/// a snapshot's original was already given to the deadlist by the copy.
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
  admit(nodes, nodes_to_remove(nodes, root, no, epoch)?)?;
  let mut node = ensure_current(nodes, root, epoch, dead)?;
  let new_root = node;
  let mut path: Vec<(Handle<TrieNode>, usize)> =
    Vec::with_capacity(usize::try_from(LEVELS).unwrap_or(0));
  for level in 0..LEVELS {
    let d = digit(no, level);
    let current = nodes.get(node)?.at(d);
    match current {
      Slot::Inode(old) => {
        nodes.get_mut(node)?.put(d, Slot::Empty);
        prune(nodes, node, &path)?;
        return Ok((new_root, Some(old)));
      }
      Slot::Node(child) => {
        let child = ensure_current(nodes, child, epoch, dead)?;
        nodes.get_mut(node)?.put(d, Slot::Node(child));
        path.push((node, d));
        node = child;
      }
      Slot::Empty => return Ok((new_root, None)),
    }
  }
  Ok((new_root, None))
}

/// Frees `node` while it holds nothing, then its parent likewise, up `path` (each entry a parent and the slot that
/// names the next node down); the root, the path's first parent, is never freed.
fn prune(
  nodes: &mut Slab<TrieNode>,
  mut node: Handle<TrieNode>,
  path: &[(Handle<TrieNode>, usize)],
) -> Result<(), VfsError> {
  for &(parent, slot) in path.iter().rev() {
    if nodes
      .get(node)?
      .slots
      .iter()
      .any(|held| !matches!(held, Slot::Empty))
    {
      return Ok(());
    }
    nodes.remove(node)?;
    if let Some(entry) = nodes.get_mut(parent)?.slots.get_mut(slot) {
      *entry = Slot::Empty;
    }
    node = parent;
  }
  Ok(())
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
    // Children are pushed in reverse so they pop in ascending order; a leaf's inodes are emitted in slot order.
    // Together the walk yields inodes in ascending number order, the order a recovery image is documented to
    // hold (`VolumeImage`) and an applied delta keeps (A-68). Until 2026-10-04 a leaf's inodes came out in
    // reverse, so an image ran ascending across leaves and descending within each.
    for slot in n.slots.iter().rev() {
      if let Slot::Node(child) = slot {
        stack.push(*child);
      }
    }
    for slot in &n.slots {
      if let Slot::Inode(h) = slot {
        out.push(*h);
      }
    }
  }
}

/// Visits every inode handle reachable from `root` in ascending number order (the order [`walk`] yields), without
/// collecting them: a streamed checkpoint counts and then encodes a volume's inodes, where a `Vec` of every handle cost
/// eight bytes an inode for the length of a checkpoint. Stops at the first error `visit` returns.
pub fn for_each<E>(
  nodes: &Slab<TrieNode>,
  root: Handle<TrieNode>,
  mut visit: impl FnMut(Handle<Inode>) -> Result<(), E>,
) -> Result<(), E> {
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    let Ok(n) = nodes.get(node) else { continue };
    for slot in n.slots.iter().rev() {
      if let Slot::Node(child) = slot {
        stack.push(*child);
      }
    }
    for slot in &n.slots {
      if let Slot::Inode(h) = slot {
        visit(*h)?;
      }
    }
  }
  Ok(())
}

/// Visits every inode handle reachable from `root` whose number's counter is past `after`'s, in ascending number order
/// (the order [`walk`] yields), until `visit` breaks; `Break` when it broke, so a caller resumes after the last inode
/// it was given (A-548's slices of a takeover round walk the whole table this way). A walk resumed after an inode
/// enters only the nodes on that inode's path and the subtrees past it, never the inodes before it. `after` is compared
/// by its counter, the trie's key, so it is a number of the table's own volume.
pub fn visit_after(
  nodes: &Slab<TrieNode>,
  root: Handle<TrieNode>,
  after: Option<InodeNo>,
  visit: &mut dyn FnMut(Handle<Inode>) -> std::ops::ControlFlow<()>,
) -> std::ops::ControlFlow<()> {
  visit_node_after(nodes, root, 0, after, visit)
}

/// One node of [`visit_after`]'s walk, at `level`. `after` is `Some` only for a node on its path: the slots before its
/// digit hold numbers below it and are skipped, the slot at its digit is entered bounded by it, and every slot past it
/// is walked whole. An inode in `after`'s own slot is `after` itself, which the walk resumes past. The recursion is as
/// deep as the trie, [`LEVELS`].
fn visit_node_after(
  nodes: &Slab<TrieNode>,
  node: Handle<TrieNode>,
  level: u32,
  after: Option<InodeNo>,
  visit: &mut dyn FnMut(Handle<Inode>) -> std::ops::ControlFlow<()>,
) -> std::ops::ControlFlow<()> {
  let Ok(n) = nodes.get(node) else {
    return std::ops::ControlFlow::Continue(());
  };
  let from = after.map_or(0, |after| digit(after, level));
  for (d, slot) in n.slots.iter().enumerate().skip(from) {
    let bound = after.filter(|_| d == from);
    match *slot {
      Slot::Empty => {}
      Slot::Node(child) => {
        visit_node_after(nodes, child, level.saturating_add(1), bound, visit)?;
      }
      Slot::Inode(handle) => {
        if bound.is_none() {
          visit(handle)?;
        }
      }
    }
  }
  std::ops::ControlFlow::Continue(())
}

/// One inode's record before and after a span: `None` on a side whose table has no such number.
pub type ChangedRecord = (Option<Handle<Inode>>, Option<Handle<Inode>>);

/// The inodes whose records differ between the tables under `before` and `after`, as pairs of the record each
/// holds (`None` where one has no such number): added, removed or changed. A node both tables share is the
/// same handle (path copying by birth epoch, D-5), so it is never entered: the walk is proportional to the
/// nodes the span copied, not to the volume. A slot that is a node on one side and empty (or an inode) on the
/// other is expanded against nothing, so every record under it is reported one-sided.
pub fn changed(
  nodes: &Slab<TrieNode>,
  before: Handle<TrieNode>,
  after: Handle<TrieNode>,
  out: &mut Vec<ChangedRecord>,
) {
  let mut stack = vec![(Slot::Node(before), Slot::Node(after))];
  while let Some((left, right)) = stack.pop() {
    if left == right {
      continue;
    }
    let left_inode = match left {
      Slot::Inode(handle) => Some(handle),
      _ => None,
    };
    let right_inode = match right {
      Slot::Inode(handle) => Some(handle),
      _ => None,
    };
    if left_inode.is_some() || right_inode.is_some() {
      out.push((left_inode, right_inode));
    }
    let left_children = children_of(nodes, left);
    let right_children = children_of(nodes, right);
    if left_children.is_none() && right_children.is_none() {
      continue;
    }
    let empty = [Slot::Empty; FANOUT];
    let left_children = left_children.unwrap_or(empty);
    let right_children = right_children.unwrap_or(empty);
    for (l, r) in left_children.into_iter().zip(right_children) {
      if l != r {
        stack.push((l, r));
      }
    }
  }
}

/// The slots beneath `slot` when it is a node.
fn children_of(nodes: &Slab<TrieNode>, slot: Slot) -> Option<[Slot; FANOUT]> {
  match slot {
    Slot::Node(handle) => nodes.get(handle).ok().map(|node| node.slots),
    _ => None,
  }
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

  /// A-548 (a takeover round walks the whole table in slices). Do: put inodes at counters spread over the trie's levels
  /// (runs, digit boundaries at several levels, the derived bit); walk from no bound, from every counter, from each
  /// counter's neighbours and from past them all; then walk in slices that break after k inodes for k from 1 to 4, each
  /// resuming after the last inode the slice before was given. Expect each walk to give exactly the inodes past its
  /// bound, in ascending order, and the slices joined to be the whole walk.
  #[test]
  fn a_walk_after_any_number_gives_the_inodes_past_it_in_order_and_slices_join_whole() {
    let counters: Vec<u64> = vec![
      1,
      2,
      3,
      15,
      16,
      17,
      255,
      256,
      4_095,
      4_096,
      4_097,
      65_535,
      65_536,
      1 << 20,
      (1 << 20) + 1,
      1 << 40,
      (1 << 47) + 5,
    ];
    let mut nodes: Slab<TrieNode> = Slab::new(64, 1 << 16);
    let mut inodes: Slab<Inode> = Slab::new(64, 1 << 16);
    let mut dead = Deadlist::default();
    let mut root = new_root(&mut nodes, Epoch(0)).unwrap();
    for &counter in &counters {
      let handle = inode(&mut inodes, counter);
      root = set(
        &mut nodes,
        root,
        InodeNo::compose(1, counter),
        handle,
        Epoch(0),
        &mut dead,
      )
      .unwrap()
      .0;
    }
    let counter_of = |handle: Handle<Inode>| inodes.get(handle).unwrap().no.counter();
    let walked = |after: Option<u64>| -> Vec<u64> {
      let mut seen = Vec::new();
      let flow = visit_after(
        &nodes,
        root,
        after.map(|after| InodeNo::compose(1, after)),
        &mut |handle| {
          seen.push(counter_of(handle));
          std::ops::ControlFlow::Continue(())
        },
      );
      assert!(flow.is_continue(), "a walk that never breaks ends whole");
      seen
    };
    assert_eq!(walked(None), counters, "no bound: every inode, ascending");
    let mut bounds: Vec<u64> = counters
      .iter()
      .flat_map(|&counter| [counter - 1, counter, counter + 1])
      .collect();
    bounds.push((1 << 47) + 6);
    for after in bounds {
      let past: Vec<u64> = counters
        .iter()
        .copied()
        .filter(|&counter| counter > after)
        .collect();
      assert_eq!(walked(Some(after)), past, "the inodes past {after}");
    }
    for per_slice in 1..=4 {
      let mut joined = Vec::new();
      let mut after: Option<u64> = None;
      loop {
        let mut taken = 0;
        let bound = after.map(|after| InodeNo::compose(1, after));
        let flow = visit_after(&nodes, root, bound, &mut |handle| {
          let counter = counter_of(handle);
          joined.push(counter);
          after = Some(counter);
          taken += 1;
          if taken == per_slice {
            std::ops::ControlFlow::Break(())
          } else {
            std::ops::ControlFlow::Continue(())
          }
        });
        if flow.is_continue() {
          break;
        }
      }
      assert_eq!(
        joined, counters,
        "slices of {per_slice} join to the whole walk"
      );
    }
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

  /// D-5, A-72: do create and remove many numbers in turn, as a churning volume does (numbers are never reused); expect
  /// the trie to hold only its root again, and a removal past a snapshot's epoch to leave the snapshot's view intact.
  /// Empty nodes left in place grew a daemon by about 100 KB per Docker round, without bound (2026-10-04).
  #[test]
  fn removal_frees_the_nodes_it_empties_so_churn_leaves_only_the_root() {
    let mut nodes: Slab<TrieNode> = Slab::new(64, 1 << 16);
    let mut inodes: Slab<Inode> = Slab::new(64, 1 << 16);
    let mut dead = Deadlist::default();
    let mut root = new_root(&mut nodes, Epoch(0)).unwrap();
    for round in 0..8u64 {
      let numbers: Vec<u64> = (round * 1000 + 1..=round * 1000 + 1000).collect();
      for &no in &numbers {
        let handle = inode(&mut inodes, no);
        root = set(
          &mut nodes,
          root,
          InodeNo::compose(1, no),
          handle,
          Epoch(0),
          &mut dead,
        )
        .unwrap()
        .0;
      }
      for &no in &numbers {
        root = remove(
          &mut nodes,
          root,
          InodeNo::compose(1, no),
          Epoch(0),
          &mut dead,
        )
        .unwrap()
        .0;
      }
      assert_eq!(nodes.len(), 1, "round {round}: only the root remains");
    }
    let kept = inode(&mut inodes, 9001);
    root = set(
      &mut nodes,
      root,
      InodeNo::compose(1, 9001),
      kept,
      Epoch(0),
      &mut dead,
    )
    .unwrap()
    .0;
    let frozen = root;
    let (later, removed) = remove(
      &mut nodes,
      root,
      InodeNo::compose(1, 9001),
      Epoch(1),
      &mut dead,
    )
    .unwrap();
    assert_eq!(removed, Some(kept));
    assert_eq!(get(&nodes, later, InodeNo::compose(1, 9001)), None);
    assert_eq!(
      get(&nodes, frozen, InodeNo::compose(1, 9001)),
      Some(kept),
      "the snapshot still sees it"
    );
  }

  /// §4.8 rebuild (the leaf cursor). Do: place 5,000 numbers in increasing order through one [`LeafCursor`] in one
  /// trie and through plain [`set`] in another; then replace one already placed through the cursor. Expect: every
  /// number reads back the same handle from both, the cursor took its direct path for most of them (the non-vacuity
  /// counter), the trees hold the same number of nodes, and the replacement returns the handle it replaced.
  #[test]
  fn a_cursor_run_places_what_plain_sets_place() {
    let mut cursor_nodes: Slab<TrieNode> = Slab::new(64, 1 << 16);
    let mut plain_nodes: Slab<TrieNode> = Slab::new(64, 1 << 16);
    let mut inodes: Slab<Inode> = Slab::new(64, 1 << 16);
    let mut dead = Deadlist::default();
    let mut cursor_root = new_root(&mut cursor_nodes, Epoch(0)).unwrap();
    let mut plain_root = new_root(&mut plain_nodes, Epoch(0)).unwrap();
    let mut cursor = LeafCursor::default();
    let numbers: Vec<u64> = (1..=5000).map(|n| n * 3).collect();
    let mut handles = Vec::new();
    for &n in &numbers {
      let handle = inode(&mut inodes, n);
      let no = InodeNo::compose(1, n);
      cursor_root = set_in_order(
        &mut cursor_nodes,
        cursor_root,
        no,
        handle,
        Epoch(0),
        &mut dead,
        &mut cursor,
      )
      .unwrap()
      .0;
      plain_root = set(
        &mut plain_nodes,
        plain_root,
        no,
        handle,
        Epoch(0),
        &mut dead,
      )
      .unwrap()
      .0;
      handles.push(handle);
    }
    for (&n, &handle) in numbers.iter().zip(&handles) {
      let no = InodeNo::compose(1, n);
      assert_eq!(
        get(&cursor_nodes, cursor_root, no),
        Some(handle),
        "{n} through the cursor"
      );
      assert_eq!(
        get(&plain_nodes, plain_root, no),
        Some(handle),
        "{n} through plain sets"
      );
    }
    assert!(
      cursor.direct > 4000,
      "the direct path ran ({} of 5000)",
      cursor.direct
    );
    assert_eq!(
      cursor_nodes.len(),
      plain_nodes.len(),
      "the same nodes either way"
    );
    let replacement = inode(&mut inodes, 99_999);
    let no = InodeNo::compose(1, numbers[numbers.len() - 1]);
    let (_, previous) = set_in_order(
      &mut cursor_nodes,
      cursor_root,
      no,
      replacement,
      Epoch(0),
      &mut dead,
      &mut cursor,
    )
    .unwrap();
    assert_eq!(
      previous,
      handles.last().copied(),
      "a replacement returns what it replaced"
    );
    assert!(dead.is_empty(), "one epoch copies nothing");
  }
}
