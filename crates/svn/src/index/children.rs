//! A directory node-revision's children (A-759): a persistent ordered map, each entry a child's name (held as the
//! number of a path the name ends) and its node-revision, kept in an arena of nodes the index owns and named by
//! handle, so a directory's successor shares every node its change does not touch and a copy shares its source's map
//! whole (main's ruling: a listing costs O(live children), not O(history beneath the directory)).
//!
//! **Shape.** A B-tree of at most [`MOST_ENTRIES`] entries a node, leaves holding (path number, node-revision) pairs
//! and inner nodes (first path number, child handle) pairs, both in the byte order of the names their path numbers
//! end with. A change copies the nodes from the root down to its entry and splits a node past [`MOST_ENTRIES`]; a
//! removal drops a node left empty and lets a root with one child go to it. Nodes are never merged, so heavy deletion
//! leaves nodes underfull, but every operation still touches one node a level. [`EMPTY`] is the empty map.
//!
//! **Copied once a revision.** A node the revision being built made itself (a staged node, `MapWrite::staged`) is
//! reachable from that revision's own directory rows alone (a copy's source is always a committed revision), so a
//! later change in the same revision edits it in place: a revision copies each node it touches once, however many of
//! the directory's children it changes. A committed node is never written again.
//!
//! **Why a name by path number.** Every path a row is made at is interned once already (`Index::intern`), so an entry
//! is two 32-bit numbers with no string of its own. A copied directory's map keeps its source's path numbers: their
//! last names are the same, and a change beneath the copy replaces the entry by name with the copy's own path.
//!
//! No I/O, no clock, no randomness, no panic.

use super::NodeRevisionId;

/// The empty map's handle.
pub(super) const EMPTY: u32 = u32::MAX;

/// The most entries a node holds; a node past it splits in two.
/// Derived: 32 entries of 8 bytes are 256 bytes, four cache lines, so a node's search stays within a few lines; a
/// change to a 10,000-entry map (three levels) copies about 768 bytes.
const MOST_ENTRIES: usize = 32;

/// The bytes one node costs besides its entries: its vector's header and its allocation's bookkeeping.
/// Derived: a `Vec` is three words (24 bytes on the 64-bit targets slates ships), and the allocator keeps about two
/// words a block.
pub(super) const NODE_BYTES: usize = 40;

/// The bytes one entry costs: two 32-bit numbers.
/// Derived: `size_of::<(u32, u32)>()`.
pub(super) const ENTRY_BYTES: usize = 8;

/// One node of a map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum MapNode {
  /// Entries: each child's path number and node-revision, in name order.
  Leaf(Vec<(u32, u32)>),
  /// Children: each subtree's first path number and handle, in name order.
  Inner(Vec<(u32, u32)>),
}

impl MapNode {
  /// Its entries or children.
  fn items(&self) -> &[(u32, u32)] {
    match self {
      Self::Leaf(items) | Self::Inner(items) => items,
    }
  }

  /// The bytes it costs: [`NODE_BYTES`] and [`ENTRY_BYTES`] an entry it can hold now.
  pub(super) fn bytes(&self) -> usize {
    let held = match self {
      Self::Leaf(items) | Self::Inner(items) => items.capacity(),
    };
    NODE_BYTES.saturating_add(held.saturating_mul(ENTRY_BYTES))
  }
}

/// Where maps are read: their nodes by handle and the paths their keys name.
pub(super) trait MapRead {
  /// The node of `handle`.
  fn map_node(&self, handle: u32) -> Option<&MapNode>;
  /// The path numbered `key`.
  fn key_path(&self, key: usize) -> Option<&str>;

  /// The name `key` ends with: its path's last component.
  fn key_name(&self, key: u32) -> Option<&str> {
    self
      .key_path(usize::try_from(key).ok()?)
      .map(|path| path.rsplit('/').next().unwrap_or(path))
  }
}

/// Where maps are made: [`MapRead`], a node pushed, and the revision's own nodes edited in place.
pub(super) trait MapWrite: MapRead {
  /// Pushes `node` and answers its handle; none past the arena's numbering.
  fn push_map_node(&mut self, node: MapNode) -> Option<u32>;
  /// Whether the node of `handle` is one the revision being built made, and so its own to edit.
  fn staged(&self, handle: u32) -> bool;
  /// The node of `handle`, a staged one, to edit.
  fn map_node_mut(&mut self, handle: u32) -> Option<&mut MapNode>;
}

/// The map of `root` with `key`'s name set to `value`, replacing an entry of that name.
pub(super) fn set(
  store: &mut impl MapWrite,
  root: u32,
  key: usize,
  value: NodeRevisionId,
) -> Option<u32> {
  let entry = (u32::try_from(key).ok()?, u32::try_from(value.0).ok()?);
  if root == EMPTY {
    return store.push_map_node(MapNode::Leaf(vec![entry]));
  }
  let name = store.key_name(entry.0)?.to_owned();
  let (left, right) = insert(store, root, entry, &name)?;
  match right {
    None => Some(left),
    Some(right) => {
      let children = vec![
        (first_of(store, left)?, left),
        (first_of(store, right)?, right),
      ];
      store.push_map_node(MapNode::Inner(children))
    }
  }
}

/// The map of `root` with the entry named `name` removed; the same map when it holds none.
pub(super) fn remove(store: &mut impl MapWrite, root: u32, name: &str) -> Option<u32> {
  if root == EMPTY {
    return Some(EMPTY);
  }
  let mut handle = match removed(store, root, name)? {
    Removed::Unchanged => return Some(root),
    Removed::Empty => return Some(EMPTY),
    Removed::Node(handle) => handle,
  };
  // A root left with one child gives way to it.
  while let Some(MapNode::Inner(children)) = store.map_node(handle) {
    match children.as_slice() {
      [(_, only)] => handle = *only,
      _ => break,
    }
  }
  Some(handle)
}

/// Every entry of the map of `root` in name order: each child's path number and node-revision (the tests' read).
#[cfg(test)]
pub(super) fn entries(store: &impl MapRead, root: u32) -> Option<Vec<(usize, NodeRevisionId)>> {
  let mut out = Vec::new();
  if root == EMPTY {
    return Some(out);
  }
  let mut stack = vec![root];
  while let Some(handle) = stack.pop() {
    match store.map_node(handle)? {
      MapNode::Leaf(entries) => {
        for &(key, value) in entries {
          out.push((
            usize::try_from(key).ok()?,
            NodeRevisionId(usize::try_from(value).ok()?),
          ));
        }
      }
      MapNode::Inner(children) => stack.extend(children.iter().rev().map(|(_, child)| *child)),
    }
  }
  Some(out)
}

/// Up to `most` entries of the map of `root` in name order, those whose names come after `after` (all, without one),
/// and whether more follow: a cursor's read, which descends past the subtrees at or before `after` and reads only the
/// entries it gives, and one more.
pub(super) fn entries_after(
  store: &impl MapRead,
  root: u32,
  after: Option<&str>,
  most: usize,
) -> Option<(Vec<(usize, NodeRevisionId)>, bool)> {
  let mut out = Vec::new();
  if root != EMPTY {
    collect_after(store, root, after, most.saturating_add(1), &mut out)?;
  }
  let more = out.len() > most;
  out.truncate(most);
  Some((out, more))
}

/// The entries of the node of `handle` after `after` into `out`, until it holds `most`.
fn collect_after(
  store: &impl MapRead,
  handle: u32,
  after: Option<&str>,
  most: usize,
  out: &mut Vec<(usize, NodeRevisionId)>,
) -> Option<()> {
  match store.map_node(handle)? {
    MapNode::Leaf(entries) => collect_leaf(store, entries, after, most, out)?,
    MapNode::Inner(children) => {
      let start = match after {
        Some(after) => child_for(store, children, after)?,
        None => 0,
      };
      for &(_, child) in children.get(start..)? {
        if out.len() >= most {
          break;
        }
        collect_after(store, child, after, most, out)?;
      }
    }
  }
  Some(())
}

/// A leaf's entries after `after` into `out`, until it holds `most`.
fn collect_leaf(
  store: &impl MapRead,
  entries: &[(u32, u32)],
  after: Option<&str>,
  most: usize,
  out: &mut Vec<(usize, NodeRevisionId)>,
) -> Option<()> {
  for &(key, value) in entries {
    if out.len() >= most {
      break;
    }
    if let Some(after) = after
      && store.key_name(key)? <= after
    {
      continue;
    }
    out.push((
      usize::try_from(key).ok()?,
      NodeRevisionId(usize::try_from(value).ok()?),
    ));
  }
  Some(())
}

/// What a removal left of a node.
enum Removed {
  /// The name was not there: the node as it was.
  Unchanged,
  /// The node is left empty.
  Empty,
  /// The node with the name removed.
  Node(u32),
}

/// The first path number of the node of `handle`.
fn first_of(store: &impl MapRead, handle: u32) -> Option<u32> {
  store.map_node(handle)?.items().first().map(|(key, _)| *key)
}

/// Where `name` goes among `items` (by their keys' names): its place and whether an item there holds it.
fn place_of(store: &impl MapRead, items: &[(u32, u32)], name: &str) -> Option<(usize, bool)> {
  let mut place = 0usize;
  for (key, _) in items {
    match store.key_name(*key)?.cmp(name) {
      std::cmp::Ordering::Less => place = place.saturating_add(1),
      std::cmp::Ordering::Equal => return Some((place, true)),
      std::cmp::Ordering::Greater => break,
    }
  }
  Some((place, false))
}

/// The child of an inner node `name` goes beneath: the last whose first name is at or before it, else the first.
fn child_for(store: &impl MapRead, children: &[(u32, u32)], name: &str) -> Option<usize> {
  let (place, found) = place_of(store, children, name)?;
  Some(if found {
    place
  } else {
    place.saturating_sub(1)
  })
}

/// The node of `handle` to change: taken out of a staged node (to be put back in place), else a copy.
fn taken(store: &mut impl MapWrite, handle: u32) -> Option<MapNode> {
  if store.staged(handle) {
    let node = store.map_node_mut(handle)?;
    return Some(std::mem::replace(node, MapNode::Leaf(Vec::new())));
  }
  store.map_node(handle).cloned()
}

/// `node`, changed from the node of `handle`, put back: in place where that node is staged, else pushed.
fn put(store: &mut impl MapWrite, handle: u32, node: MapNode) -> Option<u32> {
  if store.staged(handle) {
    *store.map_node_mut(handle)? = node;
    return Some(handle);
  }
  store.push_map_node(node)
}

/// The node of `handle` with `entry` (named `name`) set beneath it, and the right half where it split.
fn insert(
  store: &mut impl MapWrite,
  handle: u32,
  entry: (u32, u32),
  name: &str,
) -> Option<(u32, Option<u32>)> {
  match taken(store, handle)? {
    MapNode::Leaf(mut entries) => {
      let (place, found) = place_of(store, &entries, name)?;
      if found {
        *entries.get_mut(place)? = entry;
      } else {
        entries.insert(place, entry);
      }
      split(store, handle, MapNode::Leaf, entries)
    }
    MapNode::Inner(mut children) => {
      let at = child_for(store, &children, name)?;
      let (_, child) = *children.get(at)?;
      let (left, right) = insert(store, child, entry, name)?;
      *children.get_mut(at)? = (first_of(store, left)?, left);
      if let Some(right) = right {
        children.insert(at.saturating_add(1), (first_of(store, right)?, right));
      }
      split(store, handle, MapNode::Inner, children)
    }
  }
}

/// A node of `items` put back for the node of `handle`, split in two halves first when past [`MOST_ENTRIES`].
fn split(
  store: &mut impl MapWrite,
  handle: u32,
  make: fn(Vec<(u32, u32)>) -> MapNode,
  mut items: Vec<(u32, u32)>,
) -> Option<(u32, Option<u32>)> {
  if items.len() <= MOST_ENTRIES {
    return Some((put(store, handle, make(items))?, None));
  }
  let right = items.split_off(items.len() / 2);
  let left = put(store, handle, make(items))?;
  let right = store.push_map_node(make(right))?;
  Some((left, Some(right)))
}

/// The node of `handle` with the entry named `name` removed beneath it; taken to change only once a change is due.
fn removed(store: &mut impl MapWrite, handle: u32, name: &str) -> Option<Removed> {
  let below = match store.map_node(handle)? {
    MapNode::Leaf(entries) => {
      if !place_of(store, entries, name)?.1 {
        return Some(Removed::Unchanged);
      }
      None
    }
    MapNode::Inner(children) => {
      let at = child_for(store, children, name)?;
      Some((at, children.get(at)?.1))
    }
  };
  let below = match below {
    None => None,
    Some((at, child)) => match removed(store, child, name)? {
      Removed::Unchanged => return Some(Removed::Unchanged),
      changed => Some((at, changed)),
    },
  };
  removed_here(store, handle, name, below)
}

/// The node of `handle`, taken, with the removal applied: a leaf's entry named `name` removed, or an inner node's child
/// at `below`'s place replaced by what the removal beneath left (dropped when it left nothing).
fn removed_here(
  store: &mut impl MapWrite,
  handle: u32,
  name: &str,
  below: Option<(usize, Removed)>,
) -> Option<Removed> {
  match (taken(store, handle)?, below) {
    (MapNode::Leaf(mut entries), None) => {
      let (place, _) = place_of(store, &entries, name)?;
      entries.remove(place);
      if entries.is_empty() {
        return Some(Removed::Empty);
      }
      Some(Removed::Node(put(store, handle, MapNode::Leaf(entries))?))
    }
    (MapNode::Inner(mut children), Some((at, changed))) => {
      match changed {
        Removed::Empty => {
          children.remove(at);
        }
        Removed::Node(left) => *children.get_mut(at)? = (first_of(store, left)?, left),
        Removed::Unchanged => {}
      }
      if children.is_empty() {
        return Some(Removed::Empty);
      }
      Some(Removed::Node(put(store, handle, MapNode::Inner(children))?))
    }
    _ => None,
  }
}

#[cfg(test)]
mod tests;
