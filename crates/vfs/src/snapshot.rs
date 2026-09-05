//! Snapshots by birth epoch with deadlists (D-5): a snapshot is one record naming the roots of
//! the directory tree and the inode table at its epoch; the objects the head replaces or frees
//! after that go on the newest snapshot's deadlist instead of being released, because the
//! snapshot still reaches them; destroying a snapshot releases what nobody else reaches and
//! hands the rest to the previous snapshot [A: Hitz USENIX'94; C: OpenZFS `dsl_deadlist.c`].

use slates_machine::derived;
use slates_machine::derived::Derived;
use slates_mem::Handle;

use crate::content::Chunk;
use crate::dir::DirNode;
use crate::dirtree::DirBlock;
use crate::ids::{Epoch, SnapshotId};
use crate::inode::Inode;
use crate::trie::TrieNode;

/// An object the head no longer reaches, with its birth epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dead {
  /// A directory node.
  Dir(Handle<DirNode>, Epoch),
  /// A block of an indexed directory.
  DirBlock(Handle<DirBlock>, Epoch),
  /// An inode version.
  Inode(Handle<Inode>, Epoch),
  /// An inode-table node.
  Trie(Handle<TrieNode>, Epoch),
  /// A content chunk.
  Chunk(Handle<Chunk>, Epoch),
}

impl Dead {
  /// The birth epoch.
  pub const fn born(&self) -> Epoch {
    match self {
      Dead::Dir(_, e)
      | Dead::DirBlock(_, e)
      | Dead::Inode(_, e)
      | Dead::Trie(_, e)
      | Dead::Chunk(_, e) => *e,
    }
  }
}

/// The list of objects killed after a snapshot that the snapshot still reaches.
#[derive(Clone, Debug, Default)]
pub struct Deadlist {
  items: Vec<Dead>,
}

impl Deadlist {
  /// Records a killed object.
  pub fn push(&mut self, dead: Dead) {
    self.items.push(dead);
  }

  /// Items.
  pub fn len(&self) -> usize {
    self.items.len()
  }

  /// Whether empty.
  pub fn is_empty(&self) -> bool {
    self.items.is_empty()
  }

  /// Takes the items out, leaving the list empty.
  pub fn take(&mut self) -> Vec<Dead> {
    std::mem::take(&mut self.items)
  }

  /// The items.
  pub fn items(&self) -> &[Dead] {
    &self.items
  }
}

/// A snapshot record: O(1) to take.
#[derive(Clone, Debug)]
pub struct Snapshot {
  /// The epoch the snapshot froze (the head moved to the next).
  pub epoch: Epoch,
  /// The directory root at that epoch.
  pub root: Handle<DirNode>,
  /// The inode-table root at that epoch.
  pub inode_root: Handle<TrieNode>,
  /// Objects the head killed after this snapshot that it still reaches.
  pub deadlist: Deadlist,
  /// Clones that pin this snapshot as their origin.
  pub clone_refs: u32,
  /// The previous snapshot of the volume, if any.
  pub previous: Option<SnapshotId>,
  /// The next snapshot of the volume, if any (kept so removing one relinks in constant time).
  pub next: Option<SnapshotId>,
  /// `referenced_bytes` at the snapshot.
  pub referenced_bytes: u64,
  /// The op log's head sequence at the snapshot: the deriver reads the records after it.
  pub seq: u64,
  /// The Merkle identity, computed lazily (Phase 7) and `None` until then.
  pub identity: Option<[u8; 32]>,
}

/// Derived: the initial capacity of a volume's snapshot slab, one page of records
/// (`page / size_of::<Snapshot>()`); the slab grows by segments past it.
pub fn initial_capacity(page: usize) -> Derived<usize> {
  derived!(
    (page / size_of::<Snapshot>()).max(1),
    "page / size_of::<Snapshot>()",
    ["machine page", "size_of::<Snapshot>()"]
  )
}
