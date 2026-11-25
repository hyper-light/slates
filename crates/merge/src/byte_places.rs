//! Where each byte of a path's history is now (A-818 piece 2a; main's ruling of 2026-11-21 10:35 PST, option (ii)):
//! the location index that lets a path's versions be persistent trees of *byte identities*
//! ([`crate::piece_tree`]) while the history keeps only what today's keeps. Its caller to come is the content history's
//! engine switch (A-818 piece 2b).
//!
//! **Why identities.** A piece tree whose runs name immutable chunks (VS Code's) never re-points a run, but it keeps a
//! path's whole earlier block the first time the path changes, beside the files arena's new block: the retention
//! A-793 removed, which refused 94,877 of 100,000 submits `BudgetExceeded` at 100k clients over the kernel (goal 86b).
//! Here a run names bytes by identity instead, and one index a path says where each identity's bytes are now, so a
//! change re-points nothing in any version and the history holds what A-793's holds: the newest block, and the bytes
//! each change removed, copied out once.
//!
//! **What it holds.**
//! - *Identities.* A path's first value's bytes are identities `0..len`; each byte a change adds gets the next unused
//!   identity, once. Kept bytes keep theirs, so an identity is in a version at most once, and an identity a change
//!   removes is in no later version: it never comes back.
//! - *The latest order* ([`Order`]): the latest version's identity runs in file order, a treap (Seidel and Aragon,
//!   "Randomized Search Trees", Algorithmica 1996, tier A) with parent links and each node's subtree length, so an
//!   identity's offset in the newest block is its node's rank, O(depth), and an insert or a delete shifts nothing
//!   stored. Priorities are a counter mixed by SplitMix64 (Steele, Lea and Flood, OOPSLA 2014), deterministic.
//! - *The index*: each identity run's place, by its first identity: [`Place::Current`] (a node of the latest order,
//!   its bytes in the newest block) or [`Place::Stored`] (bytes a change removed, copied out by the caller into its
//!   storage and named by the caller's chunk handle).
//! - *The retirement log*: the identities and chunks each change stored, by the version it made. An identity stored by
//!   the change that made version `v` is read only by versions before `v`, so once the floor reaches `v` they go
//!   ([`BytePlaces::retire_through`]).
//!
//! **A change** ([`BytePlaces::change`]) takes the engine's parts (kept ranges of the latest, in order and disjoint, and
//! added runs): it cuts the latest order at the removed ranges' ends, hands each removed range to the caller to store,
//! re-points those identities to the stored bytes, deletes their nodes, then inserts the added runs with new
//! identities. O((parts + nodes removed) × depth), plus the removed bytes the caller copies; no version is walked.
//!
//! **A read** ([`BytePlaces::locate`]) of an identity run yields its places in order: O(log entries + the entries it
//! covers × depth).
//!
//! **Bounded and charged (MEMORY SIZING).** The index holds one entry for each node of the latest order and one for each
//! run a retained change stored; a change adds at most two nodes a part and stores at most one run a node it removes,
//! and retirement drops what the floor passed. [`BytePlaces::bytes`] is what it holds, each entry, node and logged
//! identity at its size, for the history's charge.
//!
//! **No panics.** Every slot is read with `get`; a link to no node and an identity no entry holds are
//! [`Refusal::Missing`]; parts out of order are [`Refusal::Unordered`], which the caller answers as a whole replace.

use std::collections::{BTreeMap, VecDeque};

use crate::piece_tree::{Part, Refusal, Run};

/// A node's place in the latest order's arena.
type Slot = usize;

/// Format: the source a run of identities names in a version's tree ([`Run::source`]): identities are one space a path.
pub(crate) const IDENTITIES: usize = 0;

/// Where an identity run's bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Place {
  /// `len` bytes at `at` of the newest block.
  Current {
    /// Their offset in the newest block.
    at: u64,
    /// Their length.
    len: u64,
  },
  /// `len` bytes at `at` of the caller's chunk `chunk`.
  Stored {
    /// The caller's chunk.
    chunk: usize,
    /// Their offset in it.
    at: u64,
    /// Their length.
    len: u64,
  },
}

/// An index entry: the place of the identity run starting at its key.
#[derive(Clone, Copy, Debug)]
enum Entry {
  /// In the latest order, at this node (which holds the run's length).
  Current(Slot),
  /// Stored by the caller.
  Stored {
    /// The caller's chunk.
    chunk: usize,
    /// The offset in it.
    at: u64,
    /// The run's length.
    len: u64,
  },
}

/// A node of the latest order: an identity run, its links and its subtree's length.
#[derive(Clone, Copy, Debug)]
struct OrderNode {
  identity: u64,
  len: u64,
  /// The bytes in this subtree, its own included.
  total: u64,
  priority: u64,
  parent: Option<Slot>,
  left: Option<Slot>,
  right: Option<Slot>,
}

/// The latest version's identity runs in file order (module docs, "The latest order").
#[derive(Debug, Default)]
struct Order {
  slots: Vec<Option<OrderNode>>,
  free: Vec<Slot>,
  root: Option<Slot>,
  /// Priorities minted: the counter SplitMix64 mixes.
  minted: u64,
}

/// SplitMix64's finalizer (Steele, Lea and Flood 2014): a counter to a well-mixed priority.
fn mix(counter: u64) -> u64 {
  // Format: SplitMix64's increment, multipliers and shifts, as Steele, Lea and Flood give them.
  let mut z = counter.wrapping_add(0x9e37_79b9_7f4a_7c15);
  z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
  z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
  z ^ (z >> 31)
}

impl Order {
  fn node(&self, slot: Slot) -> Result<&OrderNode, Refusal> {
    self
      .slots
      .get(slot)
      .and_then(Option::as_ref)
      .ok_or(Refusal::Missing)
  }

  fn node_mut(&mut self, slot: Slot) -> Result<&mut OrderNode, Refusal> {
    self
      .slots
      .get_mut(slot)
      .and_then(Option::as_mut)
      .ok_or(Refusal::Missing)
  }

  fn total(&self, tree: Option<Slot>) -> Result<u64, Refusal> {
    tree.map_or(Ok(0), |slot| self.node(slot).map(|node| node.total))
  }

  fn len(&self) -> Result<u64, Refusal> {
    self.total(self.root)
  }

  fn nodes(&self) -> usize {
    self.slots.len().saturating_sub(self.free.len())
  }

  /// A new detached node for `len` identities from `identity`.
  fn alloc(&mut self, identity: u64, len: u64) -> Slot {
    self.minted = self.minted.wrapping_add(1);
    let node = OrderNode {
      identity,
      len,
      total: len,
      priority: mix(self.minted),
      parent: None,
      left: None,
      right: None,
    };
    if let Some(slot) = self.free.pop()
      && let Some(place) = self.slots.get_mut(slot)
    {
      *place = Some(node);
      return slot;
    }
    self.slots.push(Some(node));
    self.slots.len().saturating_sub(1)
  }

  /// Recomputes `slot`'s subtree length from its children.
  fn update(&mut self, slot: Slot) -> Result<(), Refusal> {
    let node = *self.node(slot)?;
    let total = node
      .len
      .checked_add(self.total(node.left)?)
      .and_then(|sum| sum.checked_add(self.total(node.right).ok()?))
      .ok_or(Refusal::Overflow)?;
    self.node_mut(slot)?.total = total;
    Ok(())
  }

  /// Recomputes the lengths from `slot` to the root.
  fn fix_up(&mut self, slot: Option<Slot>) -> Result<(), Refusal> {
    let mut at = slot;
    while let Some(slot) = at {
      self.update(slot)?;
      at = self.node(slot)?.parent;
    }
    Ok(())
  }

  /// Makes `parent`'s link to `old` name `new` (the root's when `parent` is none).
  fn relink(&mut self, parent: Option<Slot>, old: Slot, new: Option<Slot>) -> Result<(), Refusal> {
    let Some(parent) = parent else {
      self.root = new;
      return Ok(());
    };
    let node = self.node_mut(parent)?;
    if node.left == Some(old) {
      node.left = new;
    } else {
      node.right = new;
    }
    Ok(())
  }

  fn set_parent(&mut self, child: Option<Slot>, parent: Option<Slot>) -> Result<(), Refusal> {
    if let Some(child) = child {
      self.node_mut(child)?.parent = parent;
    }
    Ok(())
  }

  /// Rotates `slot` above its parent, keeping the order.
  fn rotate_up(&mut self, slot: Slot) -> Result<(), Refusal> {
    let node = *self.node(slot)?;
    let parent_slot = node.parent.ok_or(Refusal::Missing)?;
    let parent = *self.node(parent_slot)?;
    let moved = if parent.left == Some(slot) {
      self.node_mut(parent_slot)?.left = node.right;
      self.node_mut(slot)?.right = Some(parent_slot);
      node.right
    } else {
      self.node_mut(parent_slot)?.right = node.left;
      self.node_mut(slot)?.left = Some(parent_slot);
      node.left
    };
    self.set_parent(moved, Some(parent_slot))?;
    self.relink(parent.parent, parent_slot, Some(slot))?;
    self.node_mut(slot)?.parent = parent.parent;
    self.node_mut(parent_slot)?.parent = Some(slot);
    self.update(parent_slot)?;
    self.update(slot)
  }

  /// Rotates a new leaf up while its priority is above its parent's, then fixes the lengths above it.
  fn settle(&mut self, slot: Slot) -> Result<(), Refusal> {
    self.fix_up(self.node(slot)?.parent)?;
    loop {
      let node = *self.node(slot)?;
      let Some(parent) = node.parent else { break };
      if self.node(parent)?.priority >= node.priority {
        break;
      }
      self.rotate_up(slot)?;
    }
    self.fix_up(Some(slot))
  }

  /// The node holding offset `at` (< the length) and the offset within it.
  fn find(&self, at: u64) -> Result<(Slot, u64), Refusal> {
    let mut slot = self.root.ok_or(Refusal::OutOfRange)?;
    let mut at = at;
    loop {
      let node = self.node(slot)?;
      let left = self.total(node.left)?;
      if at < left {
        slot = node.left.ok_or(Refusal::Missing)?;
        continue;
      }
      let past = at.saturating_sub(left);
      if past < node.len {
        return Ok((slot, past));
      }
      at = past.saturating_sub(node.len);
      slot = node.right.ok_or(Refusal::OutOfRange)?;
    }
  }

  /// `slot`'s offset in the file: O(depth).
  fn rank(&self, slot: Slot) -> Result<u64, Refusal> {
    let mut node = *self.node(slot)?;
    let mut rank = self.total(node.left)?;
    let mut child = slot;
    while let Some(parent_slot) = node.parent {
      let parent = *self.node(parent_slot)?;
      if parent.right == Some(child) {
        rank = rank
          .saturating_add(self.total(parent.left)?)
          .saturating_add(parent.len);
      }
      child = parent_slot;
      node = parent;
    }
    Ok(rank)
  }

  /// The leftmost node of the subtree `slot`.
  fn leftmost(&self, slot: Slot) -> Result<Slot, Refusal> {
    let mut slot = slot;
    while let Some(left) = self.node(slot)?.left {
      slot = left;
    }
    Ok(slot)
  }

  /// The rightmost node of the subtree `slot`.
  fn rightmost(&self, slot: Slot) -> Result<Slot, Refusal> {
    let mut slot = slot;
    while let Some(right) = self.node(slot)?.right {
      slot = right;
    }
    Ok(slot)
  }

  /// The node after `slot` in file order.
  fn successor(&self, slot: Slot) -> Result<Option<Slot>, Refusal> {
    if let Some(right) = self.node(slot)?.right {
      return self.leftmost(right).map(Some);
    }
    let mut child = slot;
    let mut parent = self.node(slot)?.parent;
    while let Some(up) = parent {
      if self.node(up)?.left == Some(child) {
        return Ok(Some(up));
      }
      child = up;
      parent = self.node(up)?.parent;
    }
    Ok(None)
  }

  /// A new node for `len` identities from `identity`, placed just before `before` (at the end when none).
  fn insert_before(
    &mut self,
    before: Option<Slot>,
    identity: u64,
    len: u64,
  ) -> Result<Slot, Refusal> {
    let slot = self.alloc(identity, len);
    let (parent, on_left) = match before {
      None => match self.root {
        None => {
          self.root = Some(slot);
          return Ok(slot);
        }
        Some(root) => (self.rightmost(root)?, false),
      },
      Some(before) => match self.node(before)?.left {
        None => (before, true),
        Some(left) => (self.rightmost(left)?, false),
      },
    };
    if on_left {
      self.node_mut(parent)?.left = Some(slot);
    } else {
      self.node_mut(parent)?.right = Some(slot);
    }
    self.node_mut(slot)?.parent = Some(parent);
    self.settle(slot)?;
    Ok(slot)
  }

  /// Cuts `slot`'s run at `offset` (inside it): the node keeps the identities before, a new node after it the rest.
  fn split(&mut self, slot: Slot, offset: u64) -> Result<Slot, Refusal> {
    let node = *self.node(slot)?;
    self.node_mut(slot)?.len = offset;
    let rest = self.alloc(
      node.identity.saturating_add(offset),
      node.len.saturating_sub(offset),
    );
    match node.right {
      None => {
        self.node_mut(slot)?.right = Some(rest);
        self.node_mut(rest)?.parent = Some(slot);
      }
      Some(right) => {
        let parent = self.leftmost(right)?;
        self.node_mut(parent)?.left = Some(rest);
        self.node_mut(rest)?.parent = Some(parent);
      }
    }
    self.update(slot)?;
    self.settle(rest)?;
    self.fix_up(Some(slot))?;
    Ok(rest)
  }

  /// Takes `slot` out of the order and frees it.
  fn delete(&mut self, slot: Slot) -> Result<(), Refusal> {
    loop {
      let node = *self.node(slot)?;
      let child = match (node.left, node.right) {
        (None, None) => break,
        (Some(one), None) | (None, Some(one)) => one,
        (Some(left), Some(right)) => {
          if self.node(left)?.priority >= self.node(right)?.priority {
            left
          } else {
            right
          }
        }
      };
      self.rotate_up(child)?;
    }
    let parent = self.node(slot)?.parent;
    self.relink(parent, slot, None)?;
    if let Some(place) = self.slots.get_mut(slot) {
      *place = None;
      self.free.push(slot);
    }
    self.fix_up(parent)
  }
}

/// The identities and chunks one change stored, by the version it made (module docs, "The retirement log").
#[derive(Debug)]
struct Stored {
  version: u64,
  identities: Vec<u64>,
  chunks: Vec<usize>,
}

/// A path's location index (module docs).
#[derive(Debug, Default)]
pub(crate) struct BytePlaces {
  order: Order,
  index: BTreeMap<u64, Entry>,
  /// The next identity a change gives.
  next: u64,
  log: VecDeque<Stored>,
  logged: usize,
}

/// Format: the bytes an index entry is charged: its key and value (module docs, "Bounded and charged").
const ENTRY_BYTES: usize = size_of::<(u64, Entry)>();
/// Format: the bytes a node of the latest order is charged.
const NODE_BYTES: usize = size_of::<Option<OrderNode>>();
/// Format: the bytes a logged identity is charged.
const LOGGED_BYTES: usize = size_of::<u64>();

impl BytePlaces {
  /// A path's first value of `len` bytes: its places, and the run of identities its version's tree holds.
  pub(crate) fn first(len: u64) -> (BytePlaces, Option<Run>) {
    let mut places = BytePlaces {
      next: len,
      ..BytePlaces::default()
    };
    if len == 0 {
      return (places, None);
    }
    let slot = places.order.alloc(0, len);
    places.order.root = Some(slot);
    places.index.insert(0, Entry::Current(slot));
    let run = Run {
      source: IDENTITIES,
      at: 0,
      len,
    };
    (places, Some(run))
  }

  /// The bytes the index holds, for the history's charge.
  pub(crate) fn bytes(&self) -> usize {
    self
      .index
      .len()
      .saturating_mul(ENTRY_BYTES)
      .saturating_add(self.order.nodes().saturating_mul(NODE_BYTES))
      .saturating_add(self.logged.saturating_mul(LOGGED_BYTES))
  }

  /// The index's entries.
  #[cfg(test)]
  pub(crate) fn entries(&self) -> usize {
    self.index.len()
  }

  /// The latest version's length.
  #[cfg(test)]
  pub(crate) fn len(&self) -> Result<u64, Refusal> {
    self.order.len()
  }

  /// The change making version `version` from the latest by `parts` (kept ranges in order and disjoint, and added
  /// runs, whose lengths alone count): each removed range of the latest handed to `store` (its offset and length in the
  /// newest block before the change), which copies its bytes out and answers where (its chunk and offset). Answers
  /// the parts with the added runs as new identities, for the version's tree.
  pub(crate) fn change(
    &mut self,
    version: u64,
    parts: &[Part],
    store: &mut impl FnMut(u64, u64) -> Result<(usize, u64), Refusal>,
  ) -> Result<Vec<Part>, Refusal> {
    let removed = self.removed_ranges(parts)?;
    let mut stored = Stored {
      version,
      identities: Vec::new(),
      chunks: Vec::new(),
    };
    for &(from, end) in removed.iter().rev() {
      self.remove_range((from, end), &mut stored, store)?;
    }
    let made = self.insert_added(parts)?;
    self.logged = self.logged.saturating_add(stored.identities.len());
    if !stored.identities.is_empty() {
      self.log.push_back(stored);
    }
    Ok(made)
  }

  /// The ranges of the latest the kept parts leave out, in order; refused when the kept parts are not in order,
  /// overlap or pass the end.
  fn removed_ranges(&self, parts: &[Part]) -> Result<Vec<(u64, u64)>, Refusal> {
    let len = self.order.len()?;
    let mut removed = Vec::new();
    let mut cursor = 0u64;
    for part in parts {
      let Part::Kept { from, len: kept } = *part else {
        continue;
      };
      let end = from.checked_add(kept).ok_or(Refusal::Overflow)?;
      if from < cursor || end > len {
        return Err(Refusal::Unordered);
      }
      if from > cursor {
        removed.push((cursor, from));
      }
      cursor = end;
    }
    if cursor < len {
      removed.push((cursor, len));
    }
    Ok(removed)
  }

  /// Makes offset `at` of the latest a node boundary.
  fn cut(&mut self, at: u64) -> Result<(), Refusal> {
    if at >= self.order.len()? {
      return Ok(());
    }
    let (slot, offset) = self.order.find(at)?;
    if offset > 0 {
      let node = *self.order.node(slot)?;
      let rest = self.order.split(slot, offset)?;
      self
        .index
        .insert(node.identity.saturating_add(offset), Entry::Current(rest));
    }
    Ok(())
  }

  /// Stores the latest's bytes in `[from, end)` through `store` and deletes their nodes.
  fn remove_range(
    &mut self,
    (from, end): (u64, u64),
    stored: &mut Stored,
    store: &mut impl FnMut(u64, u64) -> Result<(usize, u64), Refusal>,
  ) -> Result<(), Refusal> {
    self.cut(end)?;
    self.cut(from)?;
    let (chunk, at) = store(from, end.saturating_sub(from))?;
    stored.chunks.push(chunk);
    let (mut slot, _) = self.order.find(from)?;
    let mut offset = 0u64;
    while offset < end.saturating_sub(from) {
      let node = *self.order.node(slot)?;
      let next = self.order.successor(slot)?;
      let place = Entry::Stored {
        chunk,
        at: at.saturating_add(offset),
        len: node.len,
      };
      self.index.insert(node.identity, place);
      stored.identities.push(node.identity);
      self.order.delete(slot)?;
      offset = offset.saturating_add(node.len);
      match next {
        Some(next) => slot = next,
        None => break,
      }
    }
    Ok(())
  }

  /// Inserts the added parts' runs at their offsets in the new latest, each with new identities; answers the parts
  /// for the version's tree.
  fn insert_added(&mut self, parts: &[Part]) -> Result<Vec<Part>, Refusal> {
    let mut made = Vec::with_capacity(parts.len());
    let mut at = 0u64;
    for part in parts {
      match *part {
        Part::Kept { len, .. } => {
          at = at.checked_add(len).ok_or(Refusal::Overflow)?;
          made.push(*part);
        }
        Part::Added(run) if run.len > 0 => {
          let identity = self.next;
          self.next = identity.checked_add(run.len).ok_or(Refusal::Overflow)?;
          self.cut(at)?;
          let before = if at < self.order.len()? {
            Some(self.order.find(at)?.0)
          } else {
            None
          };
          let slot = self.order.insert_before(before, identity, run.len)?;
          self.index.insert(identity, Entry::Current(slot));
          at = at.checked_add(run.len).ok_or(Refusal::Overflow)?;
          made.push(Part::Added(Run {
            source: IDENTITIES,
            at: identity,
            len: run.len,
          }));
        }
        Part::Added(_) => {}
      }
    }
    Ok(made)
  }

  /// Yields the places of the `len` identities from `identity`, in order.
  pub(crate) fn locate(
    &self,
    (identity, len): (u64, u64),
    yield_place: &mut impl FnMut(Place),
  ) -> Result<(), Refusal> {
    let end = identity.checked_add(len).ok_or(Refusal::Overflow)?;
    let first = self
      .index
      .range(..=identity)
      .next_back()
      .map(|(start, _)| *start)
      .ok_or(Refusal::Missing)?;
    let mut cursor = identity;
    for (&start, entry) in self.index.range(first..end) {
      let (place_len, place) = self.place_of(*entry)?;
      let stop = start.saturating_add(place_len).min(end);
      if start > cursor || stop <= cursor {
        return Err(Refusal::Missing);
      }
      let skip = cursor.saturating_sub(start);
      let take = stop.saturating_sub(cursor);
      yield_place(match place {
        Place::Current { at, .. } => Place::Current {
          at: at.saturating_add(skip),
          len: take,
        },
        Place::Stored { chunk, at, .. } => Place::Stored {
          chunk,
          at: at.saturating_add(skip),
          len: take,
        },
      });
      cursor = stop;
    }
    if cursor < end {
      return Err(Refusal::Missing);
    }
    Ok(())
  }

  /// An entry's length and place.
  fn place_of(&self, entry: Entry) -> Result<(u64, Place), Refusal> {
    match entry {
      Entry::Current(slot) => {
        let len = self.order.node(slot)?.len;
        let at = self.order.rank(slot)?;
        Ok((len, Place::Current { at, len }))
      }
      Entry::Stored { chunk, at, len } => Ok((len, Place::Stored { chunk, at, len })),
    }
  }

  /// Every chunk handle currently referenced by unretired stored runs (for balanced-accounting recounts).
  pub(crate) fn stored_chunks(&self) -> impl Iterator<Item = usize> + '_ {
    self
      .log
      .iter()
      .flat_map(|stored| stored.chunks.iter().copied())
  }

  /// Retires what the changes making versions up to `floor` stored: no retained version (each at `floor` or later)
  /// reads it. Each chunk is handed to `free`.
  pub(crate) fn retire_through(&mut self, floor: u64, free: &mut impl FnMut(usize)) {
    while self
      .log
      .front()
      .is_some_and(|stored| stored.version <= floor)
    {
      let Some(stored) = self.log.pop_front() else {
        break;
      };
      for identity in &stored.identities {
        self.index.remove(identity);
      }
      self.logged = self.logged.saturating_sub(stored.identities.len());
      for chunk in stored.chunks {
        free(chunk);
      }
    }
  }
}

#[cfg(test)]
mod tests {
  //! The index against a serial model: each version's bytes kept whole, made by the same parts.
  #![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
  )]

  use super::*;
  use crate::piece_tree::PieceTrees;
  use proptest::prelude::*;

  /// The model of a change: the parts over `before`, added bytes from `post` at the run's offset.
  fn applied(before: &[u8], parts: &[Part], post: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
      match *part {
        Part::Kept { from, len } => {
          out.extend_from_slice(&before[from as usize..(from + len) as usize])
        }
        Part::Added(run) => {
          out.extend_from_slice(&post[run.at as usize..(run.at + run.len) as usize])
        }
      }
    }
    out
  }

  /// Generated parts over a value of `len` bytes: kept ranges in order and disjoint, and added runs.
  fn parts_over(len: u64, raw: &[(bool, u16, u16)]) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut cursor = 0u64;
    for &(kept, a, b) in raw {
      if kept && cursor < len {
        let from = cursor + u64::from(a) % (len - cursor);
        let take = u64::from(b) % (len - from + 1);
        parts.push(Part::Kept { from, len: take });
        cursor = from + take;
      } else {
        parts.push(Part::Added(Run {
          source: 1,
          at: u64::from(a) % 64,
          len: u64::from(b) % 32,
        }));
      }
    }
    parts
  }

  /// A version's bytes: its tree's identity runs located, read from the newest bytes or the stored chunks.
  fn read(
    (trees, places): (&PieceTrees, &BytePlaces),
    version: &crate::piece_tree::Version,
    (current, chunks): (&[u8], &[Vec<u8>]),
  ) -> Vec<u8> {
    let mut runs = Vec::new();
    trees
      .read(version, (0, trees.len(version).unwrap()), &mut |run| {
        runs.push(run)
      })
      .unwrap();
    let mut out = Vec::new();
    for run in runs {
      places
        .locate((run.at, run.len), &mut |place| match place {
          Place::Current { at, len } => {
            out.extend_from_slice(&current[at as usize..(at + len) as usize])
          }
          Place::Stored { chunk, at, len } => {
            out.extend_from_slice(&chunks[chunk][at as usize..(at + len) as usize]);
          }
        })
        .unwrap();
    }
    out
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

    /// A-818 piece 2a. Do: from a generated first value, make a chain of changes by generated parts (kept ranges in
    /// order, added runs), each version a piece tree of identities and the index re-pointing what each change removed
    /// to bytes copied out; read every retained version after every change; at a generated point retire the versions
    /// before a generated floor. Expect: every retained version reads as the model's bytes; the latest order's length is
    /// the newest value's; the index holds a bounded number of entries (two a part for each retained change, and the
    /// first value's one); after the release and retirement only the latest's entries remain.
    #[test]
    fn every_version_reads_its_bytes_through_the_places(
      first in prop::collection::vec(any::<u8>(), 0..200),
      chain in prop::collection::vec(prop::collection::vec((any::<bool>(), any::<u16>(), any::<u16>()), 0..6), 1..20),
      post in prop::collection::vec(any::<u8>(), 96),
      floor_at in any::<u8>(),
    ) {
      let mut trees = PieceTrees::default();
      let (mut places, run) = BytePlaces::first(first.len() as u64);
      let mut versions = vec![trees.of_runs(&run.into_iter().collect::<Vec<_>>()).unwrap()];
      let mut models = vec![first.clone()];
      let mut chunks: Vec<Vec<u8>> = Vec::new();
      let mut freed = 0usize;
      let mut floor = 0usize;
      let mut parts_made = 0usize;
      for (step, raw) in chain.iter().enumerate() {
        let current = models.last().unwrap().clone();
        let parts = parts_over(current.len() as u64, raw);
        let next = applied(&current, &parts, &post);
        let made = places.change((step + 1) as u64, &parts, &mut |at, len| {
          chunks.push(current[at as usize..(at + len) as usize].to_vec());
          Ok((chunks.len() - 1, 0))
        }).unwrap();
        parts_made += parts.len();
        let version = trees.rebuild(versions.last().unwrap(), &made).unwrap();
        versions.push(version);
        models.push(next);
        prop_assert_eq!(places.len().unwrap(), models.last().unwrap().len() as u64);
        prop_assert!(places.entries() <= 1 + 2 * parts_made + 2 * (step + 1));
        if step == usize::from(floor_at) % chain.len() {
          floor = step;
          places.retire_through(floor as u64, &mut |chunk| { chunks[chunk].clear(); freed += 1; });
        }
        let newest = models.last().unwrap();
        for (number, (version, model)) in versions.iter().zip(&models).enumerate().skip(floor) {
          prop_assert_eq!(&read((&trees, &places), version, (newest, &chunks)), model, "version {}", number);
        }
      }
      for version in versions {
        trees.release(version);
      }
      places.retire_through(u64::MAX, &mut |_| freed += 1);
      prop_assert_eq!(trees.live(), 0);
      prop_assert_eq!(places.entries(), places.order.nodes());
      prop_assert_eq!(places.logged, 0);
      prop_assert_eq!(places.bytes(), places.entries() * ENTRY_BYTES + places.order.nodes() * NODE_BYTES);
      prop_assert!(freed > 0 || chunks.is_empty());
    }
  }

  /// A-818 piece 2a. Do: parts whose kept ranges go backwards. Expect: refused unordered, the index as it was.
  #[test]
  fn kept_parts_out_of_order_are_refused() {
    let (mut places, _) = BytePlaces::first(10);
    let parts = [
      Part::Kept { from: 5, len: 5 },
      Part::Kept { from: 0, len: 5 },
    ];
    let answer = places.change(1, &parts, &mut |_, _| Ok((0, 0)));
    assert_eq!(answer, Err(Refusal::Unordered));
    assert_eq!((places.entries(), places.len()), (1, Ok(10)));
  }
}
