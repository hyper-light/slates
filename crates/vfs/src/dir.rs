//! Directories (D-4): a node with a birth epoch and entries in one of two representations, a
//! sorted array inline in the node for the small directories that are the common case and a
//! copy-on-write tree of slotted blocks ([`crate::dirtree`]) beyond a measured cut-over
//! [A: Agrawal FAST'07: median 2 entries, p99 under 200, max 61,067]. Both keep entries in the
//! same canonical order, `(name hash, folded name)`, so `readdir` is stable across the switch;
//! a lookup hashes the folded name and compares folded names on a hash hit.
//!
//! Nothing here takes memory from the global allocator per entry: the small form's entries and
//! name bytes are inline in the node's slab slot, and the indexed form's live in block slots.
//! A node copy after a snapshot is therefore one slot copy; the tree beneath is shared and
//! copied one root-to-leaf path at a time as it is written (§4.5, D-5).

use slates_mem::Handle;
use slates_mem::slab::Slab;

use crate::dirtree::{DirBlock, Retired, Tree};
use crate::error::VfsError;
use crate::ids::{Epoch, InodeNo};
use crate::names::NameEquivalence;

/// Measured: the entry count up to which the sorted array beats the map. The bench probes
/// insert-and-remove in both representations at 4, 8, 16, 32, 64 and 128 entries; the map's
/// insert first won at 4 entries (49 ns against 59 ns, Apple M5 Max, 2026-09-05, `cargo run
/// --release -p slates-vfs --example bench`), so the array is kept up to 2, which is also the
/// median directory size of the cited study.
pub const MEASURED_CUTOVER: usize = 2;

/// Measured: the mean file-name length of a `cargo build` output tree, 49.2 bytes over 42,155
/// files (`find target/debug -type f | awk -F/ '{s+=length($NF); n++} END {print s/n}'`,
/// this workspace, 2026-09-05).
pub const MEASURED_NAME_BYTES: usize = 49;

/// Derived: entries the inline array holds, the measured cut-over; a store's cut-over is
/// clamped to it.
pub const SMALL_ENTRIES: usize = MEASURED_CUTOVER;

/// Derived: inline name bytes, the inline entry count times the measured mean name length;
/// longer names send the directory to the tree early.
pub const SMALL_NAME_BYTES: usize = SMALL_ENTRIES * MEASURED_NAME_BYTES;

/// What an entry points to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Child {
  /// A subdirectory node.
  Dir(Handle<DirNode>),
  /// A file inode.
  File(InodeNo),
  /// A symlink inode.
  Symlink(InodeNo),
  /// A whiteout over a base-backed name (§4.5).
  Whiteout,
}

/// An inline entry: the hash, the child, and where the name sits in the inline bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmallEntry {
  /// The hash of the folded name.
  pub hash: u64,
  /// The child.
  pub child: Child,
  /// Name offset in the inline bytes.
  off: u8,
  /// Name length.
  len: u8,
}

/// A view of an entry in either representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryRef<'a> {
  /// The hash of the folded name.
  pub hash: u64,
  /// The name as created.
  pub name: &'a str,
  /// The child.
  pub child: Child,
}

/// The base-plane state of a directory (§4.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaseDirState {
  /// No base beneath.
  None,
  /// Base entries show through where the overlay has none.
  Merged,
  /// Only overlay entries show (created by the volume, or deleted and recreated).
  Opaque,
}

/// The small representation: entries sorted by `(hash, folded name)` with their names inline.
#[derive(Clone, Copy, Debug)]
pub struct Small {
  count: u8,
  used: u8,
  entries: [SmallEntry; SMALL_ENTRIES],
  names: [u8; SMALL_NAME_BYTES],
}

impl Small {
  const EMPTY: SmallEntry = SmallEntry {
    hash: 0,
    child: Child::Whiteout,
    off: 0,
    len: 0,
  };

  fn new() -> Self {
    Self {
      count: 0,
      used: 0,
      entries: [Self::EMPTY; SMALL_ENTRIES],
      names: [0; SMALL_NAME_BYTES],
    }
  }

  fn name(&self, e: SmallEntry) -> &str {
    let start = usize::from(e.off);
    std::str::from_utf8(&self.names[start..start + usize::from(e.len)]).unwrap_or("")
  }

  fn entries(&self) -> &[SmallEntry] {
    &self.entries[..usize::from(self.count)]
  }

  fn position(&self, policy: NameEquivalence, hash: u64, name: &str) -> Result<usize, usize> {
    for (at, e) in self.entries().iter().enumerate() {
      if e.hash > hash {
        return Err(at);
      }
      if e.hash == hash {
        let stored = self.name(*e);
        if policy.same(stored, name) {
          return Ok(at);
        }
        if policy.folded(stored).gt(policy.folded(name)) {
          return Err(at);
        }
      }
    }
    Err(usize::from(self.count))
  }

  fn fits(&self, name_len: usize) -> bool {
    usize::from(self.count) < SMALL_ENTRIES && usize::from(self.used) + name_len <= SMALL_NAME_BYTES
  }

  fn insert_at(&mut self, at: usize, hash: u64, name: &str, child: Child) {
    let count = usize::from(self.count);
    let off = self.used;
    let start = usize::from(off);
    self.names[start..start + name.len()].copy_from_slice(name.as_bytes());
    self.used += u8::try_from(name.len()).unwrap_or(u8::MAX);
    self.entries.copy_within(at..count, at + 1);
    self.entries[at] = SmallEntry {
      hash,
      child,
      off,
      len: u8::try_from(name.len()).unwrap_or(u8::MAX),
    };
    self.count += 1;
  }

  fn remove_at(&mut self, at: usize) -> SmallEntry {
    let count = usize::from(self.count);
    let removed = self.entries[at];
    self.entries.copy_within(at + 1..count, at);
    self.count -= 1;
    // Rebuild the name bytes without the hole (at most two names).
    let mut names = [0u8; SMALL_NAME_BYTES];
    let mut used = 0usize;
    for e in self.entries[..usize::from(self.count)].iter_mut() {
      let start = usize::from(e.off);
      let len = usize::from(e.len);
      names[used..used + len].copy_from_slice(&self.names[start..start + len]);
      e.off = u8::try_from(used).unwrap_or(u8::MAX);
      used += len;
    }
    self.names = names;
    self.used = u8::try_from(used).unwrap_or(u8::MAX);
    removed
  }
}

/// The entries.
#[derive(Clone, Copy, Debug)]
pub enum DirEntries {
  /// Inline, sorted by `(hash, folded name)`.
  Small(Small),
  /// A copy-on-write tree of slotted blocks in the store's block slab.
  Indexed(Tree),
}

/// A directory node.
#[derive(Clone, Debug)]
pub struct DirNode {
  /// The birth epoch.
  pub born: Epoch,
  /// The parent directory's inode number (none for the root). An inode number, not a node
  /// handle: nodes are copied on write and shared with snapshots and clones, so a handle held by
  /// a shared node goes stale, while the number resolves to the head's current node through the
  /// volume's inode table.
  pub parent: Option<InodeNo>,
  /// The directory's own inode number.
  pub inode: InodeNo,
  /// The directory's own name in its parent (empty for the root), kept here so building a
  /// path and re-pointing a parent after a copy never scan the parent's entries.
  pub name: Box<str>,
  /// The entries.
  pub entries: DirEntries,
  /// The base-plane state.
  pub base: BaseDirState,
  /// For a renamed base directory, the base path it came from (a redirect).
  pub origin: Option<Box<str>>,
}

impl DirNode {
  /// An empty directory named `name` in its parent.
  pub fn new(born: Epoch, parent: Option<InodeNo>, inode: InodeNo, name: &str) -> Self {
    Self {
      born,
      parent,
      inode,
      name: name.into(),
      entries: DirEntries::Small(Small::new()),
      base: BaseDirState::None,
      origin: None,
    }
  }

  /// The number of entries.
  pub fn len(&self) -> usize {
    match &self.entries {
      DirEntries::Small(s) => usize::from(s.count),
      DirEntries::Indexed(t) => usize::try_from(t.count).unwrap_or(usize::MAX),
    }
  }

  /// Whether the directory has no entries (whiteouts included; see `live_len`).
  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }

  /// The entries that are not whiteouts.
  pub fn live_len(&self, blocks: &Slab<DirBlock>) -> usize {
    self
      .iter(blocks)
      .filter(|e| e.child != Child::Whiteout)
      .count()
  }

  /// Whether the entries are in the indexed representation.
  pub fn is_indexed(&self) -> bool {
    matches!(self.entries, DirEntries::Indexed(_))
  }

  /// The entry for `name` under `policy`.
  pub fn lookup<'a>(
    &'a self,
    blocks: &'a Slab<DirBlock>,
    policy: NameEquivalence,
    name: &str,
  ) -> Option<EntryRef<'a>> {
    match &self.entries {
      DirEntries::Small(s) => {
        let hash = policy.hash(name);
        let at = s.position(policy, hash, name).ok()?;
        let e = s.entries[at];
        Some(EntryRef {
          hash,
          name: s.name(e),
          child: e.child,
        })
      }
      DirEntries::Indexed(t) => {
        t.lookup(blocks, policy, name)
          .map(|(hash, child, stored)| EntryRef {
            hash,
            name: stored,
            child,
          })
      }
    }
  }

  /// Inserts an entry (the caller checked it is absent), moving to the tree past the store's
  /// cut-over (at most the inline capacity) or when the names no longer fit inline. Blocks the
  /// head replaced go to `retired` for the epoch rule.
  #[allow(clippy::too_many_arguments)]
  pub fn insert(
    &mut self,
    blocks: &mut Slab<DirBlock>,
    epoch: Epoch,
    retired: &mut Retired,
    policy: NameEquivalence,
    name: &str,
    child: Child,
    cutover: usize,
  ) -> Result<(), VfsError> {
    let cutover = cutover.clamp(1, SMALL_ENTRIES);
    if let DirEntries::Small(s) = &mut self.entries {
      let hash = policy.hash(name);
      let at = match s.position(policy, hash, name) {
        Ok(_) => return Err(VfsError::AlreadyExists),
        Err(at) => at,
      };
      if usize::from(s.count) < cutover && s.fits(name.len()) {
        s.insert_at(at, hash, name, child);
        return Ok(());
      }
      // Move to the tree, then insert there.
      let mut tree = Tree::new(blocks, epoch)?;
      for e in s.entries() {
        tree.insert(blocks, epoch, retired, policy, s.name(*e), e.child)?;
      }
      self.entries = DirEntries::Indexed(tree);
    }
    match &mut self.entries {
      DirEntries::Indexed(t) => t.insert(blocks, epoch, retired, policy, name, child),
      DirEntries::Small(_) => Err(VfsError::Invalid),
    }
  }

  /// Removes the entry for `name` and returns its child, moving back inline once the count
  /// is at or below half the cut-over and the names fit (the hysteresis of §4.5).
  pub fn remove(
    &mut self,
    blocks: &mut Slab<DirBlock>,
    epoch: Epoch,
    retired: &mut Retired,
    policy: NameEquivalence,
    name: &str,
    cutover: usize,
  ) -> Result<Option<Child>, VfsError> {
    let cutover = cutover.clamp(1, SMALL_ENTRIES);
    match &mut self.entries {
      DirEntries::Small(s) => {
        let hash = policy.hash(name);
        let Ok(at) = s.position(policy, hash, name) else {
          return Ok(None);
        };
        Ok(Some(s.remove_at(at).child))
      }
      DirEntries::Indexed(t) => {
        let removed = t.remove(blocks, epoch, retired, policy, name)?;
        if removed.is_some() && usize::try_from(t.count).unwrap_or(usize::MAX) <= cutover / 2 {
          let mut small = Small::new();
          let mut fits = true;
          for (hash, stored, child) in t.iter(blocks) {
            if !small.fits(stored.len()) {
              fits = false;
              break;
            }
            let at = usize::from(small.count);
            small.insert_at(at, hash, stored, child);
          }
          if fits {
            t.blocks(blocks, retired);
            self.entries = DirEntries::Small(small);
          }
        }
        Ok(removed)
      }
    }
  }

  /// Replaces the child of an existing entry.
  pub fn set_child(
    &mut self,
    blocks: &mut Slab<DirBlock>,
    epoch: Epoch,
    retired: &mut Retired,
    policy: NameEquivalence,
    name: &str,
    child: Child,
  ) -> Result<bool, VfsError> {
    match &mut self.entries {
      DirEntries::Small(s) => {
        let hash = policy.hash(name);
        let Ok(at) = s.position(policy, hash, name) else {
          return Ok(false);
        };
        s.entries[at].child = child;
        Ok(true)
      }
      DirEntries::Indexed(t) => t.set_child(blocks, epoch, retired, policy, name, child),
    }
  }

  /// The entries in canonical `(hash, folded name)` order.
  pub fn iter<'a>(
    &'a self,
    blocks: &'a Slab<DirBlock>,
  ) -> Box<dyn Iterator<Item = EntryRef<'a>> + 'a> {
    match &self.entries {
      DirEntries::Small(s) => Box::new(s.entries().iter().map(move |e| EntryRef {
        hash: e.hash,
        name: s.name(*e),
        child: e.child,
      })),
      DirEntries::Indexed(t) => {
        Box::new(
          t.iter(blocks)
            .map(|(hash, name, child)| EntryRef { hash, name, child }),
        )
      }
    }
  }

  /// The name of the entry with hash `hash` whose child is inode `no`, for a file's home.
  pub fn name_of<'a>(
    &'a self,
    blocks: &'a Slab<DirBlock>,
    hash: u64,
    no: InodeNo,
  ) -> Option<&'a str> {
    let wanted = |c: Child| matches!(c, Child::File(n) | Child::Symlink(n) if n == no);
    match &self.entries {
      DirEntries::Small(s) => s
        .entries()
        .iter()
        .find(|e| e.hash == hash && wanted(e.child))
        .map(|e| s.name(*e)),
      DirEntries::Indexed(t) => t.name_of(blocks, hash, &wanted),
    }
  }

  /// The tree blocks this node reaches, with their birth epochs (none for the small form).
  pub fn blocks(&self, blocks: &Slab<DirBlock>, out: &mut Vec<(Handle<DirBlock>, Epoch)>) {
    if let DirEntries::Indexed(t) = &self.entries {
      t.blocks(blocks, out);
    }
  }

  /// The tree blocks born after `since` (a pruned walk for a clone's destroy).
  pub fn blocks_since(
    &self,
    blocks: &Slab<DirBlock>,
    since: Option<Epoch>,
    out: &mut Vec<(Handle<DirBlock>, Epoch)>,
  ) {
    if let DirEntries::Indexed(t) = &self.entries {
      t.blocks_since(blocks, since, out);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn blocks() -> Slab<DirBlock> {
    Slab::new(8, 1024)
  }

  fn ten_entries(blocks: &mut Slab<DirBlock>, policy: NameEquivalence) -> DirNode {
    let mut d = DirNode::new(Epoch(0), None, InodeNo(1), "ten");
    let mut retired = Retired::new();
    for i in 0..10u64 {
      d.insert(
        blocks,
        Epoch(0),
        &mut retired,
        policy,
        &format!("f{i}"),
        Child::File(InodeNo(i)),
        8,
      )
      .unwrap();
    }
    d
  }

  #[test]
  fn lookup_folds_and_the_cut_over_switches_the_representation() {
    let policy = NameEquivalence::Fold;
    let mut blocks = blocks();
    let d = ten_entries(&mut blocks, policy);
    assert!(d.is_indexed());
    assert_eq!(
      d.lookup(&blocks, policy, "F3").map(|e| e.child),
      Some(Child::File(InodeNo(3)))
    );
    assert_eq!(d.lookup(&blocks, policy, "F3").map(|e| e.name), Some("f3"));
    assert!(d.lookup(&blocks, policy, "missing").is_none());
    assert_eq!(d.len(), 10);
  }

  #[test]
  fn removal_shrinks_back_and_keeps_the_canonical_order() {
    let policy = NameEquivalence::Fold;
    let mut blocks = blocks();
    let mut d = ten_entries(&mut blocks, policy);
    let mut retired = Retired::new();
    let order_indexed: Vec<String> = d.iter(&blocks).map(|e| e.name.to_string()).collect();
    for i in 0..9u64 {
      assert!(
        d.remove(
          &mut blocks,
          Epoch(0),
          &mut retired,
          policy,
          &format!("f{i}"),
          8
        )
        .unwrap()
        .is_some()
      );
    }
    assert!(!d.is_indexed(), "back inline at or below half the cut-over");
    assert!(!retired.is_empty(), "the tree's blocks were released");
    let order_small: Vec<String> = d.iter(&blocks).map(|e| e.name.to_string()).collect();
    let expected: Vec<&String> = order_indexed
      .iter()
      .filter(|n| n.as_str() == "f9")
      .collect();
    assert_eq!(order_small.iter().collect::<Vec<_>>(), expected);
    assert!(
      d.set_child(
        &mut blocks,
        Epoch(0),
        &mut retired,
        policy,
        "f9",
        Child::Whiteout
      )
      .unwrap()
    );
    assert_eq!(
      d.lookup(&blocks, policy, "F9").map(|e| e.child),
      Some(Child::Whiteout)
    );
    assert!(
      d.remove(&mut blocks, Epoch(0), &mut retired, policy, "missing", 8)
        .unwrap()
        .is_none()
    );
    assert_eq!(d.len(), 1);
  }

  #[test]
  fn the_inline_form_orders_two_entries_and_spills_on_a_long_name() {
    let policy = NameEquivalence::Exact;
    let mut blocks = blocks();
    let mut retired = Retired::new();
    let mut d = DirNode::new(Epoch(0), None, InodeNo(1), "two");
    d.insert(
      &mut blocks,
      Epoch(0),
      &mut retired,
      policy,
      "zeta",
      Child::File(InodeNo(1)),
      2,
    )
    .unwrap();
    d.insert(
      &mut blocks,
      Epoch(0),
      &mut retired,
      policy,
      "alpha",
      Child::File(InodeNo(2)),
      2,
    )
    .unwrap();
    assert!(!d.is_indexed());
    let listed: Vec<String> = d.iter(&blocks).map(|e| e.name.to_string()).collect();
    let mut expected = vec!["zeta".to_owned(), "alpha".to_owned()];
    expected.sort_by_key(|n| (policy.hash(n), n.clone()));
    assert_eq!(listed, expected);
    assert_eq!(
      d.insert(
        &mut blocks,
        Epoch(0),
        &mut retired,
        policy,
        "alpha",
        Child::Whiteout,
        2
      ),
      Err(VfsError::AlreadyExists)
    );
    let mut long = DirNode::new(Epoch(0), None, InodeNo(2), "long");
    long
      .insert(
        &mut blocks,
        Epoch(0),
        &mut retired,
        policy,
        &"n".repeat(60),
        Child::File(InodeNo(3)),
        2,
      )
      .unwrap();
    long
      .insert(
        &mut blocks,
        Epoch(0),
        &mut retired,
        policy,
        &"m".repeat(60),
        Child::File(InodeNo(4)),
        2,
      )
      .unwrap();
    assert!(long.is_indexed(), "120 name bytes do not fit inline");
    assert_eq!(long.len(), 2);
  }
}
