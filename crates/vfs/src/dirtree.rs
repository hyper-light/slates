//! The indexed directory representation (D-4 `Indexed(OrderedNodes, …)`, §4.5): a
//! copy-on-write B+-tree of page-sized slotted blocks that live in the store's block slab, keyed
//! by `(name hash, name)`, so the canonical order is the tree's order and a lookup is one
//! descent. Every block holds its entries at the front and their name bytes at the back, the
//! way a slotted page does, so a directory of the measured shape (36 entries of 49-byte names)
//! is one block, a copy of a node after a snapshot copies the blocks on one root-to-leaf path
//! and nothing else, and destroying a directory frees one slab slot per block instead of one
//! allocator block per name.
//!
//! Why not a map from the standard library: its nodes come from the global allocator, and the
//! Phase 1 bench measured destroying a 10^6-file tree spending 2.5 ms inside `dealloc` twice
//! (the allocator returning pages), which no slice budget can hide (2026-09-05, `cargo run
//! --release -p slates-vfs --example vfs_bench`). Slab slots are never returned per item.
//!
//! The design's hash side index is not built: the leading key word is the hash, so the descent
//! already probes by hash, and the measured lookup (104 ns) is the fold and the compare, not
//! the descent.
//!
//! Invariants: entries in a block ascend by `(hash, name bytes)`; an index block's entry `i`
//! holds the first key of its child `i`; a block born in the current epoch is mutated in place
//! and any other is copied first (the path from the root); blocks the head replaces or drops go
//! to the caller as `(handle, born)` pairs for the epoch rule of §4.5.

use slates_mem::Handle;
use slates_mem::slab::Slab;

use crate::dir::Child;
use crate::error::VfsError;
use crate::ids::{Epoch, InodeNo};
use crate::names::NameEquivalence;

/// Derived: a block is one 4 KiB page, the smallest base page of any supported target and the
/// unit a copy-on-write copy moves; a 16 KiB-page machine packs four per page. It holds about
/// 56 entries of the measured 49-byte mean name, so the measured p99 directory (181 entries)
/// is four blocks and the median (2) never reaches this representation.
pub const BLOCK_BYTES: usize = 4096;

/// Format: the bytes one entry takes in a block: hash, child word, name offset, name length,
/// kind, and padding to eight bytes.
pub const ENTRY_BYTES: usize = 24;

/// Derived: the deepest tree the entry counter can need: entries per block at the longest
/// allowed name is at least 14 (`4096 / (24 + 255)`), and `14^9` exceeds `u32::MAX`, the
/// most entries a block count can address.
const MAX_HEIGHT: usize = 9;

/// Derived: a leaf whose live bytes fall to a quarter of the block merges with its right
/// sibling when both fit in one block, which keeps blocks at least a quarter full after any
/// removal sequence at amortized constant cost.
const MERGE_BELOW_QUARTER: usize = 4;

/// Format: entry kind, a subdirectory node.
const KIND_DIR: u8 = 0;
/// Format: entry kind, a file inode.
const KIND_FILE: u8 = 1;
/// Format: entry kind, a symlink inode.
const KIND_SYMLINK: u8 = 2;
/// Format: entry kind, a whiteout.
const KIND_WHITEOUT: u8 = 3;
/// Format: entry kind in an index block, a child block.
const KIND_BLOCK: u8 = 4;

/// Format: bytes of one little-endian word of the entry layout.
const WORD_BYTES: usize = 8;
/// Format: byte offset of the child word in an entry.
const CHILD_AT: usize = 8;
/// Format: byte offset of the packed word (name offset, name length, kind) in an entry.
const PACKED_AT: usize = 16;
/// Format: the name offset occupies the low sixteen bits of the packed word.
const NAME_OFF_MASK: u64 = 0xFFFF;
/// Format: the name length sits above the name offset.
const NAME_LEN_SHIFT: u32 = 16;
/// Format: the kind sits above the name length.
const KIND_SHIFT: u32 = 24;
/// Format: one byte's mask.
const BYTE_MASK: u64 = 0xFF;
/// Format: a handle word carries the slot index above its generation.
const HANDLE_INDEX_SHIFT: u32 = 32;
/// Format: the low half of a handle word, the generation.
const HANDLE_GENERATION_MASK: u64 = 0xFFFF_FFFF;

/// A block of the tree: a leaf of entries or an index of children.
#[derive(Clone, Debug)]
pub struct DirBlock {
  /// The birth epoch.
  pub born: Epoch,
  /// Whether this block's entries point at child blocks.
  index: bool,
  /// Entries in the block.
  count: u16,
  /// Name bytes in use at the back of the block, live and dead.
  names_used: u16,
  /// Name bytes of removed entries still in the name area.
  names_dead: u16,
  /// The bytes: entries from the front, names from the back; inline in the slab slot, so no
  /// block ever comes from the global allocator.
  bytes: [u8; BLOCK_BYTES],
}

/// One decoded entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Slot {
  hash: u64,
  child: u64,
  name_off: u16,
  name_len: u8,
  kind: u8,
}

impl Slot {
  fn read(bytes: &[u8]) -> Self {
    let word = |at: usize| {
      let mut b = [0u8; WORD_BYTES];
      b.copy_from_slice(&bytes[at..at + WORD_BYTES]);
      u64::from_le_bytes(b)
    };
    let packed = word(PACKED_AT);
    Self {
      hash: word(0),
      child: word(CHILD_AT),
      name_off: u16::try_from(packed & NAME_OFF_MASK).unwrap_or(0),
      name_len: u8::try_from((packed >> NAME_LEN_SHIFT) & BYTE_MASK).unwrap_or(0),
      kind: u8::try_from((packed >> KIND_SHIFT) & BYTE_MASK).unwrap_or(0),
    }
  }

  fn write(self, bytes: &mut [u8]) {
    bytes[..WORD_BYTES].copy_from_slice(&self.hash.to_le_bytes());
    bytes[CHILD_AT..CHILD_AT + WORD_BYTES].copy_from_slice(&self.child.to_le_bytes());
    let packed = u64::from(self.name_off)
      | (u64::from(self.name_len) << NAME_LEN_SHIFT)
      | (u64::from(self.kind) << KIND_SHIFT);
    bytes[PACKED_AT..PACKED_AT + WORD_BYTES].copy_from_slice(&packed.to_le_bytes());
  }

  fn to_child(self) -> Child {
    match self.kind {
      KIND_DIR => Child::Dir(handle_from_word(self.child)),
      KIND_FILE => Child::File(InodeNo(self.child)),
      KIND_SYMLINK => Child::Symlink(InodeNo(self.child)),
      _ => Child::Whiteout,
    }
  }

  fn from_child(hash: u64, child: Child) -> Self {
    let (kind, word) = match child {
      Child::Dir(h) => (KIND_DIR, handle_word(h)),
      Child::File(no) => (KIND_FILE, no.0),
      Child::Symlink(no) => (KIND_SYMLINK, no.0),
      Child::Whiteout => (KIND_WHITEOUT, 0),
    };
    Self {
      hash,
      child: word,
      name_off: 0,
      name_len: 0,
      kind,
    }
  }
}

/// The order of two names inside one hash under `policy`: by folded characters, so a lookup
/// by any spelling of a folded name descends the same way the entry was inserted. Distinct
/// entries never compare equal here (equal folded names are one entry).
fn cmp_names(policy: NameEquivalence, a: &str, b: &str) -> std::cmp::Ordering {
  policy.folded(a).cmp(policy.folded(b))
}

/// A handle as one word: index in the high half, generation in the low.
fn handle_word<T>(h: Handle<T>) -> u64 {
  (u64::from(h.index()) << HANDLE_INDEX_SHIFT) | u64::from(h.generation())
}

fn handle_from_word<T>(word: u64) -> Handle<T> {
  Handle::from_raw(
    u32::try_from(word >> HANDLE_INDEX_SHIFT).unwrap_or(0),
    u32::try_from(word & HANDLE_GENERATION_MASK).unwrap_or(0),
  )
}

impl DirBlock {
  fn new(born: Epoch, index: bool) -> Self {
    Self {
      born,
      index,
      count: 0,
      names_used: 0,
      names_dead: 0,
      bytes: [0u8; BLOCK_BYTES],
    }
  }

  /// A copy of this block born at `epoch`.
  fn copy(&self, epoch: Epoch) -> Self {
    let mut c = self.clone();
    c.born = epoch;
    c
  }

  fn count(&self) -> usize {
    usize::from(self.count)
  }

  fn slot(&self, at: usize) -> Slot {
    Slot::read(&self.bytes[at * ENTRY_BYTES..])
  }

  fn set_slot(&mut self, at: usize, slot: Slot) {
    slot.write(&mut self.bytes[at * ENTRY_BYTES..]);
  }

  fn name(&self, slot: Slot) -> &str {
    let start = usize::from(slot.name_off);
    let end = start + usize::from(slot.name_len);
    std::str::from_utf8(&self.bytes[start..end]).unwrap_or("")
  }

  /// The name of entry `at`.
  fn name_at(&self, at: usize) -> &str {
    self.name(self.slot(at))
  }

  fn free_bytes(&self) -> usize {
    BLOCK_BYTES - self.count() * ENTRY_BYTES - usize::from(self.names_used)
  }

  fn live_bytes(&self) -> usize {
    self.count() * ENTRY_BYTES + usize::from(self.names_used - self.names_dead)
  }

  /// Where `key` sits or would go: `Ok(at)` for the entry equal under `policy`, `Err(at)` for
  /// the insertion point in `(hash, name bytes)` order.
  fn find(&self, policy: NameEquivalence, hash: u64, name: &str) -> Result<usize, usize> {
    let count = self.count();
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
      let mid = lo + (hi - lo) / 2;
      let s = self.slot(mid);
      let less = s.hash < hash
        || (s.hash == hash && cmp_names(policy, self.name(s), name) == std::cmp::Ordering::Less);
      if less {
        lo = mid + 1;
      } else {
        hi = mid;
      }
    }
    // Among equal hashes, equality is the policy's, which may differ from byte order.
    let mut at = lo;
    while at > 0 && self.slot(at - 1).hash == hash {
      at -= 1;
    }
    while at < count && self.slot(at).hash == hash {
      if policy.same(self.name_at(at), name) {
        return Ok(at);
      }
      at += 1;
    }
    Err(lo)
  }

  /// The child position for `key` in an index block: the last entry whose key is not greater.
  fn child_for(&self, policy: NameEquivalence, hash: u64, name: &str) -> usize {
    let count = self.count();
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
      let mid = lo + (hi - lo) / 2;
      let s = self.slot(mid);
      let greater = s.hash > hash
        || (s.hash == hash && cmp_names(policy, self.name(s), name) == std::cmp::Ordering::Greater);
      if greater {
        hi = mid;
      } else {
        lo = mid + 1;
      }
    }
    lo.saturating_sub(1)
  }

  fn fits(&self, name_len: usize) -> bool {
    self.free_bytes() >= ENTRY_BYTES + name_len
  }

  /// Inserts `slot` with `name` at `at`; the caller checked it fits.
  fn insert_at(&mut self, at: usize, mut slot: Slot, name: &str) {
    let count = self.count();
    let len = name.len();
    let name_end = BLOCK_BYTES - usize::from(self.names_used);
    let name_start = name_end - len;
    self.bytes[name_start..name_end].copy_from_slice(name.as_bytes());
    self.names_used += u16::try_from(len).unwrap_or(u16::MAX);
    slot.name_off = u16::try_from(name_start).unwrap_or(0);
    slot.name_len = u8::try_from(len).unwrap_or(u8::MAX);
    self.bytes.copy_within(
      at * ENTRY_BYTES..count * ENTRY_BYTES,
      (at + 1) * ENTRY_BYTES,
    );
    self.set_slot(at, slot);
    self.count += 1;
  }

  /// Removes entry `at`, leaving its name bytes as a hole that a later compaction reclaims.
  fn remove_at(&mut self, at: usize) -> Slot {
    let count = self.count();
    let slot = self.slot(at);
    self.bytes.copy_within(
      (at + 1) * ENTRY_BYTES..count * ENTRY_BYTES,
      at * ENTRY_BYTES,
    );
    self.count -= 1;
    self.names_dead += u16::from(slot.name_len);
    if usize::from(self.names_dead) >= usize::from(self.names_used - self.names_dead) {
      self.compact_names();
    }
    slot
  }

  /// Rewrites the name area without holes.
  fn compact_names(&mut self) {
    let mut fresh = [0u8; BLOCK_BYTES];
    let mut used = 0usize;
    for at in 0..self.count() {
      let mut s = self.slot(at);
      let name = self.name(s);
      let len = name.len();
      let end = BLOCK_BYTES - used;
      fresh[end - len..end].copy_from_slice(name.as_bytes());
      used += len;
      s.name_off = u16::try_from(end - len).unwrap_or(0);
      s.write(&mut fresh[at * ENTRY_BYTES..]);
    }
    self.bytes.copy_from_slice(&fresh);
    self.names_used = u16::try_from(used).unwrap_or(u16::MAX);
    self.names_dead = 0;
  }

  /// Moves the upper half of the entries into `other` (empty); returns nothing, the caller
  /// reads `other`'s first key for the separator.
  fn split_into(&mut self, other: &mut DirBlock) {
    let count = self.count();
    let half = count / 2;
    for at in half..count {
      let s = self.slot(at);
      let name = self.name(s).to_owned();
      other.insert_at(other.count(), s, &name);
    }
    for at in (half..count).rev() {
      let s = self.slot(at);
      self.count -= 1;
      self.names_dead += u16::from(s.name_len);
    }
    self.compact_names();
  }

  /// Appends every entry of `other` after this block's (the caller checked they fit and that
  /// `other`'s keys follow this block's).
  fn absorb(&mut self, other: &DirBlock) {
    for at in 0..other.count() {
      let s = other.slot(at);
      self.insert_at(self.count(), s, other.name(s));
    }
  }

  fn first_key(&self) -> Option<(u64, &str)> {
    (self.count > 0).then(|| {
      let s = self.slot(0);
      (s.hash, self.name(s))
    })
  }
}

/// The indexed directory: its root block, height and entry count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tree {
  /// The root block (a leaf while the height is one).
  pub root: Handle<DirBlock>,
  /// Blocks on any root-to-leaf path.
  pub height: u8,
  /// Entries in the tree.
  pub count: u32,
}

/// Blocks the head replaced or dropped, with their birth epochs, for the epoch rule.
pub type Retired = Vec<(Handle<DirBlock>, Epoch)>;

/// One step of a descent: the block and the position taken in it.
type Path = [(Handle<DirBlock>, usize); MAX_HEIGHT];

impl Tree {
  /// An empty tree with one leaf born at `epoch`.
  pub fn new(blocks: &mut Slab<DirBlock>, epoch: Epoch) -> Result<Self, VfsError> {
    let root = blocks.insert(DirBlock::new(epoch, false))?;
    Ok(Self {
      root,
      height: 1,
      count: 0,
    })
  }

  /// The entry named `name` under `policy`, as its hash, its child and its stored name.
  pub fn lookup<'b>(
    &self,
    blocks: &'b Slab<DirBlock>,
    policy: NameEquivalence,
    name: &str,
  ) -> Option<(u64, Child, &'b str)> {
    let hash = policy.hash(name);
    let mut block = self.root;
    for _ in 1..self.height {
      let b = blocks.get(block).ok()?;
      let at = b.child_for(policy, hash, name);
      block = handle_from_word(b.slot(at).child);
    }
    let leaf = blocks.get(block).ok()?;
    let at = leaf.find(policy, hash, name).ok()?;
    let s = leaf.slot(at);
    Some((hash, s.to_child(), leaf.name(s)))
  }

  /// The name of the entry with `hash` whose child satisfies `wanted`, searching the leaf the
  /// hash descends to (a run of one hash that spans two leaves is not followed; the caller
  /// falls back to a walk).
  pub fn name_of<'b>(
    &self,
    blocks: &'b Slab<DirBlock>,
    hash: u64,
    wanted: &dyn Fn(Child) -> bool,
  ) -> Option<&'b str> {
    let mut block = self.root;
    for _ in 1..self.height {
      let b = blocks.get(block).ok()?;
      let at = b.child_for(NameEquivalence::Exact, hash, "");
      block = handle_from_word(b.slot(at).child);
    }
    let leaf = blocks.get(block).ok()?;
    let count = leaf.count();
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
      let mid = lo + (hi - lo) / 2;
      if leaf.slot(mid).hash < hash {
        lo = mid + 1;
      } else {
        hi = mid;
      }
    }
    let mut at = lo;
    while at < count {
      let s = leaf.slot(at);
      if s.hash != hash {
        break;
      }
      if wanted(s.to_child()) {
        return Some(leaf.name(s));
      }
      at += 1;
    }
    None
  }

  /// Every block of the tree with its birth epoch, for release and destroy walks.
  pub fn blocks(&self, blocks: &Slab<DirBlock>, out: &mut Vec<(Handle<DirBlock>, Epoch)>) {
    let mut stack = vec![(self.root, 1u8)];
    while let Some((h, depth)) = stack.pop() {
      let Ok(b) = blocks.get(h) else { continue };
      out.push((h, b.born));
      if depth < self.height {
        for at in 0..b.count() {
          stack.push((handle_from_word(b.slot(at).child), depth + 1));
        }
      }
    }
  }

  /// Every block born after `since` (a block born at or before it is shared with an origin
  /// and so is everything beneath it), for a pruned destroy walk.
  pub fn blocks_since(
    &self,
    blocks: &Slab<DirBlock>,
    since: Option<Epoch>,
    out: &mut Vec<(Handle<DirBlock>, Epoch)>,
  ) {
    let mut stack = vec![(self.root, 1u8)];
    while let Some((h, depth)) = stack.pop() {
      let Ok(b) = blocks.get(h) else { continue };
      if since.is_some_and(|s| b.born.0 <= s.0) {
        continue;
      }
      out.push((h, b.born));
      if depth < self.height {
        for at in 0..b.count() {
          stack.push((handle_from_word(b.slot(at).child), depth + 1));
        }
      }
    }
  }

  /// Descends to the leaf for `key`, copying every block on the path not born at `epoch`
  /// (copy-on-write) and re-pointing its parent; returns the path.
  fn descend_mut(
    &mut self,
    blocks: &mut Slab<DirBlock>,
    epoch: Epoch,
    retired: &mut Retired,
    policy: NameEquivalence,
    hash: u64,
    name: &str,
  ) -> Result<Path, VfsError> {
    let mut path: Path = [(self.root, 0); MAX_HEIGHT];
    let mut block = self.root;
    for level in 0..usize::from(self.height) {
      let born = blocks.get(block)?.born;
      if born != epoch {
        let copy = blocks.get(block)?.copy(epoch);
        let fresh = blocks.insert(copy)?;
        retired.push((block, born));
        if level == 0 {
          self.root = fresh;
        } else {
          let (parent, at) = path[level - 1];
          let mut s = blocks.get(parent)?.slot(at);
          s.child = handle_word(fresh);
          blocks.get_mut(parent)?.set_slot(at, s);
        }
        block = fresh;
      }
      let at = if level + 1 < usize::from(self.height) {
        blocks.get(block)?.child_for(policy, hash, name)
      } else {
        0
      };
      path[level] = (block, at);
      if level + 1 < usize::from(self.height) {
        block = handle_from_word(blocks.get(block)?.slot(at).child);
      }
    }
    Ok(path)
  }

  /// Inserts `name → child` (the caller checked it is absent).
  pub fn insert(
    &mut self,
    blocks: &mut Slab<DirBlock>,
    epoch: Epoch,
    retired: &mut Retired,
    policy: NameEquivalence,
    name: &str,
    child: Child,
  ) -> Result<(), VfsError> {
    let hash = policy.hash(name);
    let path = self.descend_mut(blocks, epoch, retired, policy, hash, name)?;
    let depth = usize::from(self.height) - 1;
    let (leaf, _) = path[depth];
    let at = match blocks.get(leaf)?.find(policy, hash, name) {
      Ok(_) => return Err(VfsError::AlreadyExists),
      Err(at) => at,
    };
    let slot = Slot::from_child(hash, child);
    self.insert_split(blocks, epoch, &path, depth, leaf, at, slot, name)?;
    self.count += 1;
    Ok(())
  }

  /// Inserts into `block` at `at`, splitting upward as needed.
  #[allow(clippy::too_many_arguments)]
  fn insert_split(
    &mut self,
    blocks: &mut Slab<DirBlock>,
    epoch: Epoch,
    path: &Path,
    depth: usize,
    block: Handle<DirBlock>,
    at: usize,
    slot: Slot,
    name: &str,
  ) -> Result<(), VfsError> {
    if blocks.get(block)?.fits(name.len()) {
      blocks.get_mut(block)?.insert_at(at, slot, name);
      return Ok(());
    }
    // Split: the upper half moves to a fresh sibling; the new entry goes to whichever side
    // its position falls in; the sibling's first key becomes a separator in the parent.
    let index = blocks.get(block)?.index;
    let mut sibling = DirBlock::new(epoch, index);
    blocks.get_mut(block)?.split_into(&mut sibling);
    let left_count = blocks.get(block)?.count();
    if at <= left_count {
      if !blocks.get(block)?.fits(name.len()) {
        return Err(VfsError::InvalidName);
      }
      blocks.get_mut(block)?.insert_at(at, slot, name);
    } else {
      if !sibling.fits(name.len()) {
        return Err(VfsError::InvalidName);
      }
      sibling.insert_at(at - left_count, slot, name);
    }
    let (sep_hash, sep_name) = sibling
      .first_key()
      .map(|(h, n)| (h, n.to_owned()))
      .ok_or(VfsError::Invalid)?;
    let sibling_handle = blocks.insert(sibling)?;
    let sep = Slot {
      hash: sep_hash,
      child: handle_word(sibling_handle),
      name_off: 0,
      name_len: 0,
      kind: KIND_BLOCK,
    };
    if depth == 0 {
      // A new root above the old one and its sibling.
      let (first_hash, first_name) = blocks
        .get(block)?
        .first_key()
        .map(|(h, n)| (h, n.to_owned()))
        .ok_or(VfsError::Invalid)?;
      let mut root = DirBlock::new(epoch, true);
      root.insert_at(
        0,
        Slot {
          hash: first_hash,
          child: handle_word(block),
          name_off: 0,
          name_len: 0,
          kind: KIND_BLOCK,
        },
        &first_name,
      );
      root.insert_at(1, sep, &sep_name);
      self.root = blocks.insert(root)?;
      self.height += 1;
      return Ok(());
    }
    let (parent, parent_at) = path[depth - 1];
    self.insert_split(
      blocks,
      epoch,
      path,
      depth - 1,
      parent,
      parent_at + 1,
      sep,
      &sep_name,
    )
  }

  /// Removes `name`; returns its child if it was present.
  pub fn remove(
    &mut self,
    blocks: &mut Slab<DirBlock>,
    epoch: Epoch,
    retired: &mut Retired,
    policy: NameEquivalence,
    name: &str,
  ) -> Result<Option<Child>, VfsError> {
    let hash = policy.hash(name);
    let path = self.descend_mut(blocks, epoch, retired, policy, hash, name)?;
    let depth = usize::from(self.height) - 1;
    let (leaf, _) = path[depth];
    let Ok(at) = blocks.get(leaf)?.find(policy, hash, name) else {
      return Ok(None);
    };
    let slot = blocks.get_mut(leaf)?.remove_at(at);
    self.count -= 1;
    self.rebalance(blocks, retired, &path, depth)?;
    Ok(Some(slot.to_child()))
  }

  /// After a removal in the block at `depth`: an emptied block leaves the tree, a block at a
  /// quarter merges with its right sibling when both fit, separators follow the first keys.
  fn rebalance(
    &mut self,
    blocks: &mut Slab<DirBlock>,
    retired: &mut Retired,
    path: &Path,
    depth: usize,
  ) -> Result<(), VfsError> {
    let (block, _) = path[depth];
    if depth == 0 {
      // The root: collapse a one-child index into its child.
      while self.height > 1 && blocks.get(self.root)?.count() == 1 {
        let child = handle_from_word(blocks.get(self.root)?.slot(0).child);
        let born = blocks.get(self.root)?.born;
        retired.push((self.root, born));
        self.root = child;
        self.height -= 1;
      }
      return Ok(());
    }
    let (parent, parent_at) = path[depth - 1];
    let count = blocks.get(block)?.count();
    if count == 0 {
      blocks.get_mut(parent)?.remove_at(parent_at);
      let born = blocks.get(block)?.born;
      retired.push((block, born));
      return self.rebalance(blocks, retired, path, depth - 1);
    }
    // The separator follows the block's first key.
    self.refresh_separator(blocks, parent, parent_at, block)?;
    let parent_count = blocks.get(parent)?.count();
    let quarter = BLOCK_BYTES / MERGE_BELOW_QUARTER;
    if blocks.get(block)?.live_bytes() < quarter && parent_at + 1 < parent_count {
      let right = handle_from_word(blocks.get(parent)?.slot(parent_at + 1).child);
      let right_live = blocks.get(right)?.live_bytes();
      if blocks.get(block)?.free_bytes() >= right_live {
        let right_block = blocks.get(right)?.clone();
        blocks.get_mut(block)?.absorb(&right_block);
        blocks.get_mut(parent)?.remove_at(parent_at + 1);
        retired.push((right, right_block.born));
        return self.rebalance(blocks, retired, path, depth - 1);
      }
    }
    self.rebalance(blocks, retired, path, depth - 1)
  }

  fn refresh_separator(
    &self,
    blocks: &mut Slab<DirBlock>,
    parent: Handle<DirBlock>,
    parent_at: usize,
    block: Handle<DirBlock>,
  ) -> Result<(), VfsError> {
    let (hash, name) = blocks
      .get(block)?
      .first_key()
      .map(|(h, n)| (h, n.to_owned()))
      .ok_or(VfsError::Invalid)?;
    let current = blocks.get(parent)?.slot(parent_at);
    if current.hash == hash && blocks.get(parent)?.name(current) == name {
      return Ok(());
    }
    let p = blocks.get_mut(parent)?;
    let mut s = p.remove_at(parent_at);
    s.hash = hash;
    // The separator's name must fit; a block that cannot hold it keeps the old separator,
    // which still orders correctly because it is not greater than the child's first key.
    if p.fits(name.len()) {
      p.insert_at(parent_at, s, &name);
    } else {
      p.insert_at(parent_at, s, "");
    }
    Ok(())
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
    let hash = policy.hash(name);
    let path = self.descend_mut(blocks, epoch, retired, policy, hash, name)?;
    let (leaf, _) = path[usize::from(self.height) - 1];
    let Ok(at) = blocks.get(leaf)?.find(policy, hash, name) else {
      return Ok(false);
    };
    let old = blocks.get(leaf)?.slot(at);
    let mut s = Slot::from_child(hash, child);
    s.name_off = old.name_off;
    s.name_len = old.name_len;
    blocks.get_mut(leaf)?.set_slot(at, s);
    Ok(true)
  }

  /// The entries in canonical order.
  pub fn iter<'b>(&self, blocks: &'b Slab<DirBlock>) -> TreeIter<'b> {
    let mut it = TreeIter {
      blocks,
      stack: [(self.root, 0); MAX_HEIGHT],
      depth: 0,
      height: usize::from(self.height),
    };
    it.stack[0] = (self.root, 0);
    it.depth = 1;
    it.settle();
    it
  }
}

/// An in-order walk over a tree's leaves, one block per level on its stack.
pub struct TreeIter<'b> {
  blocks: &'b Slab<DirBlock>,
  stack: [(Handle<DirBlock>, usize); MAX_HEIGHT],
  /// Levels on the stack (the last is a leaf when settled).
  depth: usize,
  height: usize,
}

impl<'b> TreeIter<'b> {
  /// Descends leftmost from the top of the stack until a leaf is on top.
  fn settle(&mut self) {
    while self.depth > 0 && self.depth < self.height {
      let (h, at) = self.stack[self.depth - 1];
      let Ok(b) = self.blocks.get(h) else {
        self.depth = 0;
        return;
      };
      if at >= b.count() {
        self.depth -= 1;
        if self.depth > 0 {
          self.stack[self.depth - 1].1 += 1;
        }
        continue;
      }
      let child = handle_from_word(b.slot(at).child);
      self.stack[self.depth] = (child, 0);
      self.depth += 1;
    }
  }
}

impl<'b> Iterator for TreeIter<'b> {
  type Item = (u64, &'b str, Child);

  fn next(&mut self) -> Option<Self::Item> {
    loop {
      if self.depth == 0 {
        return None;
      }
      let (h, at) = self.stack[self.depth - 1];
      let b = self.blocks.get(h).ok()?;
      if at < b.count() {
        self.stack[self.depth - 1].1 += 1;
        let s = b.slot(at);
        return Some((s.hash, b.name(s), s.to_child()));
      }
      // This leaf is done: go up one level, advance, and settle on the next leaf.
      self.depth -= 1;
      if self.depth > 0 {
        self.stack[self.depth - 1].1 += 1;
      }
      self.settle();
    }
  }
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeMap;

  use super::*;

  fn slab() -> Slab<DirBlock> {
    Slab::new(64, 1 << 20)
  }

  fn keyed(policy: NameEquivalence, n: &str) -> (u64, String) {
    (policy.hash(n), n.to_owned())
  }

  type Model = BTreeMap<(u64, String), Child>;

  struct Fixture {
    blocks: Slab<DirBlock>,
    tree: Tree,
    model: Model,
    retired: Retired,
    epoch: Epoch,
    /// Roots frozen at earlier epochs with what they listed then.
    old_roots: Vec<(Tree, Vec<String>)>,
  }

  const POLICY: NameEquivalence = NameEquivalence::Fold;

  fn listed(tree: &Tree, blocks: &Slab<DirBlock>) -> Vec<(u64, String)> {
    tree
      .iter(blocks)
      .map(|(h, n, _)| (h, n.to_owned()))
      .collect()
  }

  /// Inserts 3,000 names of varying length, freezing the root every 500 like a snapshot.
  fn fill() -> Fixture {
    let mut f = Fixture {
      blocks: slab(),
      tree: Tree::new(&mut slab(), Epoch(0)).unwrap(),
      model: Model::new(),
      retired: Retired::new(),
      epoch: Epoch(0),
      old_roots: Vec::new(),
    };
    f.tree = Tree::new(&mut f.blocks, Epoch(0)).unwrap();
    for round in 0..3000u64 {
      let pad = usize::try_from(round % 40).unwrap_or(0);
      let name = format!("entry-{:05}-{}", (round * 7919) % 3001, "x".repeat(pad));
      let child = Child::File(InodeNo(round));
      f.tree
        .insert(&mut f.blocks, f.epoch, &mut f.retired, POLICY, &name, child)
        .unwrap();
      f.model.insert(keyed(POLICY, &name), child);
      if round % 500 == 499 {
        let names = listed(&f.tree, &f.blocks)
          .into_iter()
          .map(|(_, n)| n)
          .collect();
        f.old_roots.push((f.tree, names));
        f.epoch = Epoch(f.epoch.0 + 1);
      }
    }
    f
  }

  fn assert_equal_to_model(f: &Fixture) {
    let expected: Vec<(u64, String)> = f.model.keys().cloned().collect();
    assert_eq!(listed(&f.tree, &f.blocks), expected);
    assert_eq!(usize::try_from(f.tree.count).unwrap(), f.model.len());
    for (key, child) in &f.model {
      let found = f.tree.lookup(&f.blocks, POLICY, &key.1.to_uppercase());
      assert_eq!(found.map(|(_, c, _)| c), Some(*child), "{}", key.1);
    }
  }

  /// The oracle: a map ordered by `(hash, name)`; the tree must list and answer like it after
  /// every operation, across splits, merges and copy-on-write epochs.
  #[test]
  fn the_tree_equals_the_ordered_map_across_splits_merges_and_epochs() {
    let mut f = fill();
    assert!(
      f.tree.height >= 2,
      "splits happened: height {}",
      f.tree.height
    );
    assert_equal_to_model(&f);
    assert!(f.tree.lookup(&f.blocks, POLICY, "absent").is_none());
    // Removals, with merges.
    let names: Vec<String> = f.model.keys().map(|k| k.1.clone()).collect();
    for name in names
      .iter()
      .enumerate()
      .filter(|(i, _)| i % 3 != 0)
      .map(|(_, n)| n)
    {
      let removed = f
        .tree
        .remove(&mut f.blocks, f.epoch, &mut f.retired, POLICY, name)
        .unwrap();
      assert_eq!(removed, f.model.remove(&keyed(POLICY, name)));
    }
    assert_equal_to_model(&f);
    let mut live = Vec::new();
    f.tree.blocks(&f.blocks, &mut live);
    assert!(
      live.len() * BLOCK_BYTES < 4 * f.model.len() * (ENTRY_BYTES + 60),
      "merges keep the blocks from spreading: {} blocks for {} entries",
      live.len(),
      f.model.len()
    );
    // Old roots still list what they listed, untouched by every later insert and removal.
    for (old, names) in &f.old_roots {
      let now: Vec<String> = listed(old, &f.blocks).into_iter().map(|(_, n)| n).collect();
      assert_eq!(&now, names);
    }
    assert!(!f.retired.is_empty(), "copies and merges retired blocks");
  }

  #[test]
  fn set_child_replaces_in_place_and_names_survive_compaction() {
    let policy = NameEquivalence::Exact;
    let mut blocks = slab();
    let mut tree = Tree::new(&mut blocks, Epoch(3)).unwrap();
    let mut retired = Retired::new();
    for i in 0..100u64 {
      tree
        .insert(
          &mut blocks,
          Epoch(3),
          &mut retired,
          policy,
          &format!("n{i}"),
          Child::File(InodeNo(i)),
        )
        .unwrap();
    }
    for i in 0..70u64 {
      assert!(
        tree
          .remove(
            &mut blocks,
            Epoch(3),
            &mut retired,
            policy,
            &format!("n{i}")
          )
          .unwrap()
          .is_some()
      );
    }
    assert!(
      tree
        .set_child(
          &mut blocks,
          Epoch(3),
          &mut retired,
          policy,
          "n80",
          Child::Whiteout
        )
        .unwrap()
    );
    assert_eq!(
      tree
        .lookup(&blocks, policy, "n80")
        .map(|(_, c, n)| (c, n.to_owned())),
      Some((Child::Whiteout, "n80".to_owned()))
    );
    assert!(
      !tree
        .set_child(
          &mut blocks,
          Epoch(3),
          &mut retired,
          policy,
          "gone",
          Child::Whiteout
        )
        .unwrap()
    );
    assert!(retired.is_empty(), "same epoch: nothing copied");
    assert_eq!(tree.count, 30);
  }
}
