//! What a retired green's release takes out of its shard's arenas, moved, to be dropped on another thread (A-920, main's
//! ruling after release probe 17).
//!
//! **Why.** A release frees what the retired green held a unit at a time, each unit far under the shard's step. But
//! the allocator returns a batch of freed pages to the kernel inside whichever free crosses its threshold: in debug,
//! one free of a copy's release took 1.3-1.7 ms, nearly all of it system time, while the task's footprint dropped by
//! 39-44 MB and its reusable pages rose by 28-33 MB. Which free pays is not the walk's to choose: a node's 160-byte
//! child list paid it in one run, a block's bytes in another, a remains entry's pop in a third.
//!
//! **What.** So the release frees nothing of its own on the shard. Each unit's walk is done there (a node's hold given
//! back, its slot freed, its children queued, its block's holder let go), and what the unit frees is moved into a
//! [`Reaped`] instead: the node's label and child list, its value's heap, a block's bytes, an owned history chunk, and
//! once the holds are back, the retired green's remains whole. The caller hands the [`Reaped`] to a thread of its
//! own (the server's reaper), which drops it there; with no such thread it drops it in place, as before.
//!
//! **What it costs.** A move a thing taken, into a list that grows as a release's units do; nothing is copied. A
//! thing with no heap (an empty list, a mode, a directory's unit value) is dropped in place and not counted.

use std::any::Any;
use std::collections::BTreeSet;

/// What a release took, to be dropped wherever its holder sends it ([`crate::engine::Lineage::release_step`]): byte
/// buffers, child lists and strings each in a list of their kind, anything else whole. [`Reaped::taken`] counts them.
#[derive(Default)]
pub struct Reaped {
  /// Nodes' labels, blocks' bytes, owned history chunks and byte values.
  bytes: Vec<Vec<u8>>,
  /// Nodes' child lists.
  children: Vec<Vec<(u8, usize)>>,
  /// String values.
  strings: Vec<String>,
  /// Anything else, whole: a retired green's remains, an emptied lineage, an arena's emptied lists, rarer values.
  whole: Vec<Box<dyn Any + Send>>,
  /// The things taken, in all.
  taken: u64,
}

impl std::fmt::Debug for Reaped {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Reaped")
      .field("taken", &self.taken)
      .finish_non_exhaustive()
  }
}

impl Reaped {
  /// The things taken, in all: the non-vacuity count of what a release moved out to be dropped elsewhere.
  pub fn taken(&self) -> u64 {
    self.taken
  }

  /// Whether nothing was taken.
  pub fn is_empty(&self) -> bool {
    self.taken == 0
  }

  /// `bytes` taken, when it holds an allocation.
  pub(crate) fn bytes(&mut self, bytes: Vec<u8>) {
    if bytes.capacity() > 0 {
      self.bytes.push(bytes);
      self.taken = self.taken.saturating_add(1);
    }
  }

  /// A node's child list taken, when it holds an allocation.
  pub(crate) fn children(&mut self, children: Vec<(u8, usize)>) {
    if children.capacity() > 0 {
      self.children.push(children);
      self.taken = self.taken.saturating_add(1);
    }
  }

  /// `string` taken, when it holds an allocation.
  pub(crate) fn string(&mut self, string: String) {
    if string.capacity() > 0 {
      self.strings.push(string);
      self.taken = self.taken.saturating_add(1);
    }
  }

  /// `thing` taken whole: dropped with the rest, wherever the holder sends it.
  pub fn whole(&mut self, thing: Box<dyn Any + Send>) {
    self.whole.push(thing);
    self.taken = self.taken.saturating_add(1);
  }
}

/// A value a map's node holds, its heap moved into a [`Reaped`] when a release frees its last node.
pub(crate) trait Reapable {
  /// `self` moved into `reaped`, or dropped in place when it holds no heap.
  fn reap(self, reaped: &mut Reaped);
}

impl Reapable for () {
  fn reap(self, _reaped: &mut Reaped) {}
}

impl Reapable for u32 {
  fn reap(self, _reaped: &mut Reaped) {}
}

impl Reapable for Vec<u8> {
  fn reap(self, reaped: &mut Reaped) {
    reaped.bytes(self);
  }
}

impl Reapable for String {
  fn reap(self, reaped: &mut Reaped) {
    reaped.string(self);
  }
}

impl Reapable for BTreeSet<String> {
  fn reap(self, reaped: &mut Reaped) {
    if !self.is_empty() {
      reaped.whole(Box::new(self));
    }
  }
}

impl Reapable for crate::special::SpecialNode {
  fn reap(self, reaped: &mut Reaped) {
    reaped.whole(Box::new(self));
  }
}
