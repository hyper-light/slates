//! The indexed directory representation (D-4 `Indexed(OrderedNodes, …)`, §4.5): a copy-on-write B+-tree of slotted
//! blocks keyed by `(name hash, name)`, so the canonical order is the tree's order and a lookup is one descent. Every
//! block holds a header, its entries after it and their name bytes at the back, the way a slotted page does.
//!
//! Blocks come in size classes (A-203, [`crate::dirblocks`]): a block is the smallest class that holds its entries,
//! from 256 bytes to 4 KiB. A block that cannot take an insert grows to the class that can (one copy into a fresh
//! block); a block at the largest class splits, the half moved out going to the class that fits it; a block a removal
//! leaves holding less than half of its class shrinks to the class that fits, when the store has a block to give. So a
//! directory of three entries holds 256 bytes of block, the measured directory (36 entries of 49-byte names) one block
//! of 3 KiB, and a large directory full 4 KiB leaves. A copy of a node after a snapshot copies the blocks on one
//! root-to-leaf path and nothing else, and destroying a directory frees one slot per block.
//!
//! Why not a map from the standard library: its nodes come from the global allocator, and the Phase 1 bench measured
//! destroying a 10^6-file tree spending 2.5 ms inside `dealloc` twice (the allocator returning pages), which no slice
//! budget can hide (2026-09-05, `cargo run --release -p slates-vfs --example vfs_bench`). Block slots are never
//! returned to the allocator per item.
//!
//! The design's hash side index is not built: the leading key word is the hash, so the descent already probes by
//! hash, and the measured lookup (104 ns) is the fold and the compare, not the descent.
//!
//! Invariants: entries in a block ascend by `(hash, name bytes)`; an index block's entry `i` holds the first key of
//! its child `i`; a block born in the current epoch is mutated in place and any other is copied first (the path from
//! the root); a block is at most twice the class that fits its live bytes while the store has room; blocks the head
//! replaces or drops go to the caller as `(block, born)` pairs for the epoch rule of §4.5.

use crate::dir::Child;
pub use crate::dirblocks::ENTRY_BYTES;
use crate::dirblocks::{
  BlockRef, BlockStore, HEADER_BYTES, LARGEST_CLASS, MIN_ENTRIES_AT_NAME_MAX, SMALLEST_CLASS,
  fitted_size,
};
use crate::error::VfsError;
use crate::ids::{Epoch, InodeNo};
use crate::names::NameEquivalence;
use slates_mem::Handle;

/// Derived: the deepest tree the entry counter can need: a block at the largest class, the only one that splits,
/// holds at least [`MIN_ENTRIES_AT_NAME_MAX`] entries at the longest allowed name, and `3^21` exceeds `u32::MAX`, the
/// most entries a tree's count can address.
const MAX_HEIGHT: usize = 21;

// Every split block holds at least three entries, which the height bound counts on.
const _: () = assert!(MIN_ENTRIES_AT_NAME_MAX >= 3);

/// Derived: a block whose live bytes fall below a quarter of the largest class merges with its right sibling when
/// both fit in one largest block, which keeps the leaves from spreading after any removal sequence at amortized
/// constant cost.
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
/// Format: entry kind, FIFO inode (A-26).
const KIND_FIFO: u8 = 5;
/// Format: entry kind, socket inode (A-26).
const KIND_SOCKET: u8 = 6;

/// Format: bytes of one little-endian word of the entry layout.
const WORD_BYTES: usize = 8;
/// Format: bytes of one little-endian half-word of the header.
const HALF_BYTES: usize = 2;
/// Format: byte offset of the child word in an entry.
const CHILD_AT: usize = 8;
/// Format: byte offset of the packed word (name offset, name length, kind, and a child block's slot word) in an entry.
const PACKED_AT: usize = 16;
/// Format: the name offset occupies the low sixteen bits of the packed word.
const NAME_OFF_MASK: u64 = 0xFFFF;
/// Format: the name length sits above the name offset.
const NAME_LEN_SHIFT: u32 = 16;
/// Format: the kind sits above the name length.
const KIND_SHIFT: u32 = 24;
/// Format: a child block's slot and generation (its reference's second word) take the packed word's high half.
const AUX_SHIFT: u32 = 32;
/// Format: one byte's mask.
const BYTE_MASK: u64 = 0xFF;
/// Format: a handle word carries the slot index above its generation.
const HANDLE_INDEX_SHIFT: u32 = 32;
/// Format: the low half of a handle word, the generation.
const HANDLE_GENERATION_MASK: u64 = 0xFFFF_FFFF;
/// Format: header offset of the birth epoch.
const BORN_AT: usize = 0;
/// Format: header offset of the entry count.
const COUNT_AT: usize = 8;
/// Format: header offset of the name bytes in use at the back, live and dead.
const NAMES_USED_AT: usize = 10;
/// Format: header offset of the name bytes of removed entries still in the name area.
const NAMES_DEAD_AT: usize = 12;
/// Format: header offset of the flags byte.
const FLAGS_AT: usize = 14;
/// Format: the flag of a block whose entries point at child blocks.
const INDEX_FLAG: u8 = 1;

/// A block's bytes as a stack buffer of the largest class, for a block being built.
type Scratch = [u8; LARGEST_CLASS];

/// The little-endian word at `at` of `bytes`; zero past the end (a slot is always read inside its block).
fn read_word(bytes: &[u8], at: usize) -> u64 {
  bytes
    .get(at..)
    .and_then(<[u8]>::first_chunk::<WORD_BYTES>)
    .map_or(0, |word| u64::from_le_bytes(*word))
}

/// The little-endian half-word at `at` of `bytes`; zero past the end.
fn read_half(bytes: &[u8], at: usize) -> u16 {
  bytes
    .get(at..)
    .and_then(<[u8]>::first_chunk::<HALF_BYTES>)
    .map_or(0, |half| u16::from_le_bytes(*half))
}

fn write_bytes(bytes: &mut [u8], at: usize, value: &[u8]) {
  if let Some(field) = bytes.get_mut(at..at.saturating_add(value.len())) {
    field.copy_from_slice(value);
  }
}

/// The offset of entry `at`.
fn entry_at(at: usize) -> usize {
  HEADER_BYTES.saturating_add(at.saturating_mul(ENTRY_BYTES))
}

/// One decoded entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Slot {
  hash: u64,
  child: u64,
  name_off: u16,
  name_len: u8,
  kind: u8,
  /// A child block's second reference word; zero for every other kind.
  aux: u32,
}

impl Slot {
  /// Decodes the entry at the start of `bytes`; a slot is always read inside its block, so the zero arm (a
  /// short tail) never runs.
  fn read(bytes: &[u8]) -> Self {
    let packed = read_word(bytes, PACKED_AT);
    Self {
      hash: read_word(bytes, 0),
      child: read_word(bytes, CHILD_AT),
      name_off: u16::try_from(packed & NAME_OFF_MASK).unwrap_or(0),
      name_len: u8::try_from((packed >> NAME_LEN_SHIFT) & BYTE_MASK).unwrap_or(0),
      kind: u8::try_from((packed >> KIND_SHIFT) & BYTE_MASK).unwrap_or(0),
      aux: u32::try_from(packed >> AUX_SHIFT).unwrap_or(0),
    }
  }

  /// Encodes the entry at the start of `bytes`; a slot is always written inside its block.
  fn write(self, bytes: &mut [u8]) {
    let packed = u64::from(self.name_off)
      | (u64::from(self.name_len) << NAME_LEN_SHIFT)
      | (u64::from(self.kind) << KIND_SHIFT)
      | (u64::from(self.aux) << AUX_SHIFT);
    for (at, word) in [(0, self.hash), (CHILD_AT, self.child), (PACKED_AT, packed)] {
      write_bytes(bytes, at, &word.to_le_bytes());
    }
  }

  fn to_child(self) -> Child {
    match self.kind {
      KIND_DIR => Child::Dir(handle_from_word(self.child)),
      KIND_FILE => Child::File(InodeNo(self.child)),
      KIND_SYMLINK => Child::Symlink(InodeNo(self.child)),
      KIND_FIFO => Child::Fifo(InodeNo(self.child)),
      KIND_SOCKET => Child::Socket(InodeNo(self.child)),
      _ => Child::Whiteout,
    }
  }

  fn from_child(hash: u64, child: Child) -> Self {
    let (kind, word) = match child {
      Child::Dir(h) => (KIND_DIR, handle_word(h)),
      Child::File(no) => (KIND_FILE, no.0),
      Child::Symlink(no) => (KIND_SYMLINK, no.0),
      Child::Fifo(no) => (KIND_FIFO, no.0),
      Child::Socket(no) => (KIND_SOCKET, no.0),
      Child::Whiteout => (KIND_WHITEOUT, 0),
    };
    Self {
      hash,
      child: word,
      name_off: 0,
      name_len: 0,
      kind,
      aux: 0,
    }
  }

  /// An index entry for `block` under the key hash `hash`.
  fn for_block(hash: u64, block: BlockRef) -> Self {
    let (child, aux) = block.words();
    Self {
      hash,
      child,
      name_off: 0,
      name_len: 0,
      kind: KIND_BLOCK,
      aux,
    }
  }

  /// The block an index entry names.
  fn block(self) -> BlockRef {
    BlockRef::from_words(self.child, self.aux)
  }

  /// This entry pointed at `block` instead.
  fn pointing_at(self, block: BlockRef) -> Self {
    let (child, aux) = block.words();
    Self { child, aux, ..self }
  }
}

/// The order of two names inside one hash under `policy`: by folded characters, so a lookup
/// by any spelling of a folded name descends the same way the entry was inserted. Distinct
/// entries never compare equal here (equal folded names are one entry).
fn cmp_names(policy: NameEquivalence, a: &str, b: &str) -> std::cmp::Ordering {
  policy.compare(a, b)
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

/// A block's bytes, read: a leaf of entries or an index of children.
#[derive(Clone, Copy)]
struct Block<'a> {
  bytes: &'a [u8],
}

impl<'a> Block<'a> {
  fn born(self) -> Epoch {
    Epoch(read_word(self.bytes, BORN_AT))
  }

  fn is_index(self) -> bool {
    self
      .bytes
      .get(FLAGS_AT)
      .is_some_and(|flags| flags & INDEX_FLAG != 0)
  }

  fn count(self) -> usize {
    usize::from(read_half(self.bytes, COUNT_AT))
  }

  fn names_used(self) -> usize {
    usize::from(read_half(self.bytes, NAMES_USED_AT))
  }

  fn names_dead(self) -> usize {
    usize::from(read_half(self.bytes, NAMES_DEAD_AT))
  }

  /// The block's class size.
  fn size(self) -> usize {
    self.bytes.len()
  }

  /// The hash of entry `at` alone: what a search compares before it needs the rest of the slot.
  fn hash_at(self, at: usize) -> u64 {
    read_word(self.bytes, entry_at(at))
  }

  fn slot(self, at: usize) -> Slot {
    Slot::read(self.bytes.get(entry_at(at)..).unwrap_or_default())
  }

  fn name(self, slot: Slot) -> &'a str {
    let start = usize::from(slot.name_off);
    let end = start.saturating_add(usize::from(slot.name_len));
    std::str::from_utf8(self.bytes.get(start..end).unwrap_or_default()).unwrap_or("")
  }

  /// The name of entry `at`.
  fn name_at(self, at: usize) -> &'a str {
    self.name(self.slot(at))
  }

  /// The block index entry `at` points at.
  fn child_block(self, at: usize) -> BlockRef {
    self.slot(at).block()
  }

  fn free_bytes(self) -> usize {
    self
      .size()
      .saturating_sub(entry_at(self.count()))
      .saturating_sub(self.names_used())
  }

  /// The bytes the entries take, their live names included, the header not.
  fn live_bytes(self) -> usize {
    self
      .count()
      .saturating_mul(ENTRY_BYTES)
      .saturating_add(self.names_used().saturating_sub(self.names_dead()))
  }

  /// The size of the class that fits this block's live bytes and header.
  fn fitted(self) -> usize {
    fitted_size(HEADER_BYTES.saturating_add(self.live_bytes()))
  }

  /// Where `key` sits or would go: `Ok(at)` for the entry equal under `policy`, `Err(at)` for
  /// the insertion point in `(hash, name bytes)` order.
  fn find(self, policy: NameEquivalence, hash: u64, name: &str) -> Result<usize, usize> {
    let count = self.count();
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
      let mid = lo.midpoint(hi);
      let mid_hash = self.hash_at(mid);
      let less = mid_hash < hash
        || (mid_hash == hash
          && cmp_names(policy, self.name_at(mid), name) == std::cmp::Ordering::Less);
      if less {
        lo = mid.saturating_add(1);
      } else {
        hi = mid;
      }
    }
    // Among equal hashes, equality is the policy's, which may differ from byte order.
    let mut at = lo;
    while at > 0 && self.hash_at(at.saturating_sub(1)) == hash {
      at = at.saturating_sub(1);
    }
    while at < count && self.hash_at(at) == hash {
      if policy.same(self.name_at(at), name) {
        return Ok(at);
      }
      at = at.saturating_add(1);
    }
    Err(lo)
  }

  /// The child position for `key` in an index block: the last entry whose key is not greater.
  fn child_for(self, policy: NameEquivalence, hash: u64, name: &str) -> usize {
    let count = self.count();
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
      let mid = lo.midpoint(hi);
      let mid_hash = self.hash_at(mid);
      let greater = mid_hash > hash
        || (mid_hash == hash
          && cmp_names(policy, self.name_at(mid), name) == std::cmp::Ordering::Greater);
      if greater {
        hi = mid;
      } else {
        lo = mid.saturating_add(1);
      }
    }
    lo.saturating_sub(1)
  }

  /// The first entry whose hash is at least `hash` (the count when none is): where a listing resumes from a
  /// cookie, whether or not the entry it last returned is still present.
  fn first_hash_at_least(self, hash: u64) -> usize {
    let (mut lo, mut hi) = (0usize, self.count());
    while lo < hi {
      let mid = lo.midpoint(hi);
      if self.hash_at(mid) < hash {
        lo = mid.saturating_add(1);
      } else {
        hi = mid;
      }
    }
    lo
  }

  fn fits(self, name_len: usize) -> bool {
    self.free_bytes() >= ENTRY_BYTES.saturating_add(name_len)
  }

  fn first_key(self) -> Option<(u64, &'a str)> {
    (self.count() > 0).then(|| {
      let s = self.slot(0);
      (s.hash, self.name(s))
    })
  }

  /// The first key, owned, for a separator the caller writes elsewhere.
  fn first_key_owned(self) -> Result<(u64, String), VfsError> {
    self
      .first_key()
      .map(|(hash, name)| (hash, name.to_owned()))
      .ok_or(VfsError::Invalid)
  }
}

/// A block's bytes, written.
struct BlockMut<'a> {
  bytes: &'a mut [u8],
}

impl BlockMut<'_> {
  fn read(&self) -> Block<'_> {
    Block { bytes: self.bytes }
  }

  /// An empty block born at `born`.
  fn init(&mut self, born: Epoch, index: bool) {
    if let Some(header) = self.bytes.get_mut(..HEADER_BYTES) {
      header.fill(0);
    }
    self.set_born(born);
    if let Some(flags) = self.bytes.get_mut(FLAGS_AT) {
      *flags = if index { INDEX_FLAG } else { 0 };
    }
  }

  fn set_born(&mut self, born: Epoch) {
    write_bytes(self.bytes, BORN_AT, &born.0.to_le_bytes());
  }

  fn set_half(&mut self, at: usize, value: usize) {
    write_bytes(
      self.bytes,
      at,
      &u16::try_from(value).unwrap_or(u16::MAX).to_le_bytes(),
    );
  }

  fn set_slot(&mut self, at: usize, slot: Slot) {
    slot.write(self.bytes.get_mut(entry_at(at)..).unwrap_or_default());
  }

  /// Inserts `slot` with `name` at `at`; the caller checked it fits.
  fn insert_at(&mut self, at: usize, mut slot: Slot, name: &str) {
    let block = self.read();
    let (count, used, len) = (block.count(), block.names_used(), name.len());
    let name_end = block.size().saturating_sub(used);
    let name_start = name_end.saturating_sub(len);
    // `fits` left room for one more slot below the names; refusing here (never) changes nothing.
    if at > count || entry_at(count.saturating_add(1)) > name_start || !block.fits(len) {
      return;
    }
    write_bytes(self.bytes, name_start, name.as_bytes());
    self.set_half(NAMES_USED_AT, used.saturating_add(len));
    slot.name_off = u16::try_from(name_start).unwrap_or(0);
    slot.name_len = u8::try_from(len).unwrap_or(u8::MAX);
    // Both ranges end at or below `entry_at(count + 1) <= name_start`, inside the block.
    self.bytes.copy_within(
      entry_at(at)..entry_at(count),
      entry_at(at.saturating_add(1)),
    );
    self.set_slot(at, slot);
    self.set_half(COUNT_AT, count.saturating_add(1));
  }

  /// Removes entry `at`: its name bytes go back at once when they are the lowest in the name area, and are otherwise
  /// left as a hole that a later compaction reclaims.
  fn remove_at(&mut self, at: usize) -> Slot {
    let block = self.read();
    let (count, slot) = (block.count(), block.slot(at));
    if at >= count {
      return slot;
    }
    let len = usize::from(slot.name_len);
    let mut used = block.names_used();
    let mut dead = block.names_dead();
    // Names are written downward from the block's end, so the name written last is the lowest: its bytes go back at
    // once, and a create and unlink of the same name never leaves a hole for a compaction (A-203).
    if usize::from(slot.name_off) == block.size().saturating_sub(used) {
      used = used.saturating_sub(len);
      self.set_half(NAMES_USED_AT, used);
    } else {
      dead = dead.saturating_add(len);
    }
    // `at < count`, so both ranges lie inside the slot area.
    self.bytes.copy_within(
      entry_at(at.saturating_add(1))..entry_at(count),
      entry_at(at),
    );
    self.set_half(COUNT_AT, count.saturating_sub(1));
    self.set_half(NAMES_DEAD_AT, dead);
    if dead > 0 && dead >= used.saturating_sub(dead) {
      self.compact_names();
    }
    slot
  }

  /// Rewrites the name area without holes.
  fn compact_names(&mut self) {
    let mut fresh: Scratch = [0u8; LARGEST_CLASS];
    let size = self.bytes.len();
    let Some(out) = fresh.get_mut(..size) else {
      return;
    };
    rebuild(self.read(), &mut BlockMut { bytes: out });
    if let Some(out) = fresh.get(..size) {
      self.bytes.copy_from_slice(out);
    }
  }

  /// Moves the upper half of the entries, by bytes, into `other` (empty), keeping at least one on each side; the
  /// caller reads `other`'s first key for the separator. Halving by bytes keeps both halves near half a block whatever
  /// the names' lengths, so the half that stays is within twice the class that fits it.
  fn split_into(&mut self, other: &mut BlockMut<'_>) {
    let block = self.read();
    let count = block.count();
    let half = split_point(block);
    for at in half..count {
      let s = block.slot(at);
      other.insert_at(other.read().count(), s, block.name(s));
    }
    let moved: usize = (half..count)
      .map(|at| usize::from(block.slot(at).name_len))
      .sum();
    let dead = block.names_dead().saturating_add(moved);
    self.set_half(COUNT_AT, half);
    self.set_half(NAMES_DEAD_AT, dead);
    self.compact_names();
  }

  /// Appends every entry of `other` after this block's (the caller checked they fit and that
  /// `other`'s keys follow this block's).
  fn absorb(&mut self, other: Block<'_>) {
    for at in 0..other.count() {
      let s = other.slot(at);
      self.insert_at(self.read().count(), s, other.name(s));
    }
  }
}

/// The first entry of a split's upper half: the first whose bytes before it reach half the block's live bytes, at
/// least one entry from either end.
fn split_point(block: Block<'_>) -> usize {
  let count = block.count();
  let half_bytes = block.live_bytes() / 2;
  let mut before = 0usize;
  for at in 0..count {
    if before >= half_bytes {
      return at.clamp(1, count.saturating_sub(1));
    }
    before = before
      .saturating_add(ENTRY_BYTES)
      .saturating_add(usize::from(block.slot(at).name_len));
  }
  count.saturating_sub(1).max(1)
}

/// Writes `src`'s entries into `dst` (any class that holds them), its names packed at the back.
fn rebuild(src: Block<'_>, dst: &mut BlockMut<'_>) {
  dst.init(src.born(), src.is_index());
  dst.absorb(src);
}

/// The size of a stored block and the size of the class that fits its live bytes, for the oracles (A-203); `None` for
/// a block the store does not hold.
pub fn block_size(blocks: &BlockStore, block: BlockRef) -> Option<(usize, usize)> {
  view(blocks, block).ok().map(|b| (b.size(), b.fitted()))
}

fn view(blocks: &BlockStore, block: BlockRef) -> Result<Block<'_>, VfsError> {
  Ok(Block {
    bytes: blocks.get(block)?,
  })
}

fn edit(blocks: &mut BlockStore, block: BlockRef) -> Result<BlockMut<'_>, VfsError> {
  Ok(BlockMut {
    bytes: blocks.get_mut(block)?,
  })
}

/// Stores `src` (a block being built) in the class that fits it.
fn store_fitted(blocks: &mut BlockStore, src: Block<'_>) -> Result<BlockRef, VfsError> {
  let mut out: Scratch = [0u8; LARGEST_CLASS];
  let bytes = out.get_mut(..src.fitted()).ok_or(VfsError::Invalid)?;
  rebuild(src, &mut BlockMut { bytes: &mut *bytes });
  Ok(blocks.insert(bytes)?)
}

/// A copy of the stored `block` in a fresh block of `size` bytes (a class that holds its live bytes).
fn resized(blocks: &mut BlockStore, block: BlockRef, size: usize) -> Result<BlockRef, VfsError> {
  let mut out: Scratch = [0u8; LARGEST_CLASS];
  let bytes = out.get_mut(..size).ok_or(VfsError::Invalid)?;
  rebuild(view(blocks, block)?, &mut BlockMut { bytes: &mut *bytes });
  Ok(blocks.insert(bytes)?)
}

/// A copy of the stored `block` born at `epoch`, in the same class.
fn copied(blocks: &mut BlockStore, block: BlockRef, epoch: Epoch) -> Result<BlockRef, VfsError> {
  let mut out: Scratch = [0u8; LARGEST_CLASS];
  let source = view(blocks, block)?.bytes;
  let bytes = out.get_mut(..source.len()).ok_or(VfsError::Invalid)?;
  bytes.copy_from_slice(source);
  BlockMut { bytes: &mut *bytes }.set_born(epoch);
  Ok(blocks.insert(bytes)?)
}

/// The indexed directory: its root block, height and entry count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tree {
  /// The root block (a leaf while the height is one).
  pub root: BlockRef,
  /// Blocks on any root-to-leaf path.
  pub height: u8,
  /// Entries in the tree.
  pub count: u32,
}

/// Blocks the head replaced or dropped, with their birth epochs, for the epoch rule.
pub type Retired = Vec<(BlockRef, Epoch)>;

/// One step of a descent: the block and the position taken in it.
type Path = [(BlockRef, usize); MAX_HEIGHT];

/// Whether every block above `depth` on `path` descended through its last slot: the block at `depth` is then the
/// rightmost of its level, so an entry past its end is past the end of the whole tree.
fn rightmost_above(blocks: &BlockStore, path: &Path, depth: usize) -> Result<bool, VfsError> {
  for level in 0..depth {
    let (block, at) = step(path, Some(level))?;
    if at.saturating_add(1) != view(blocks, block)?.count() {
      return Ok(false);
    }
  }
  Ok(true)
}

/// The block and slot a descent recorded at `level`; a level past the tree's height (never: `height <=
/// MAX_HEIGHT`, and every caller asks below it) refuses typed.
fn step(path: &Path, level: Option<usize>) -> Result<(BlockRef, usize), VfsError> {
  level
    .and_then(|level| path.get(level))
    .copied()
    .ok_or(VfsError::StaleHandle)
}

impl Tree {
  /// An empty tree with one leaf of the smallest class born at `epoch`.
  pub fn new(blocks: &mut BlockStore, epoch: Epoch) -> Result<Self, VfsError> {
    Self::holding(blocks, epoch, 0)
  }

  /// An empty tree whose one leaf, born at `epoch`, is the class that holds `entries` and `name_bytes` of names
  /// without growing: a small directory's move to a tree, whose entries are known, takes one block (A-203).
  pub fn holding_entries(
    blocks: &mut BlockStore,
    epoch: Epoch,
    entries: usize,
    name_bytes: usize,
  ) -> Result<Self, VfsError> {
    Self::holding(
      blocks,
      epoch,
      entries
        .saturating_mul(ENTRY_BYTES)
        .saturating_add(name_bytes),
    )
  }

  fn holding(blocks: &mut BlockStore, epoch: Epoch, live_bytes: usize) -> Result<Self, VfsError> {
    let mut out: Scratch = [0u8; LARGEST_CLASS];
    let size = fitted_size(HEADER_BYTES.saturating_add(live_bytes)).max(SMALLEST_CLASS);
    let bytes = out.get_mut(..size).ok_or(VfsError::Invalid)?;
    BlockMut { bytes: &mut *bytes }.init(epoch, false);
    let root = blocks.insert(bytes)?;
    Ok(Self {
      root,
      height: 1,
      count: 0,
    })
  }

  /// The entry named `name` under `policy`, as its hash, its child and its stored name.
  pub fn lookup<'b>(
    &self,
    blocks: &'b BlockStore,
    policy: NameEquivalence,
    name: &str,
  ) -> Option<(u64, Child, &'b str)> {
    let hash = policy.hash(name);
    let mut block = self.root;
    for _ in 1..self.height {
      let b = view(blocks, block).ok()?;
      block = b.child_block(b.child_for(policy, hash, name));
    }
    let leaf = view(blocks, block).ok()?;
    let at = leaf.find(policy, hash, name).ok()?;
    let s = leaf.slot(at);
    Some((hash, s.to_child(), leaf.name(s)))
  }

  /// The name of the entry with `hash` whose child satisfies `wanted`: the entries from the first at or above
  /// `(hash, "")` in canonical order, while their hash is `hash` — one descent, then the in-order walk
  /// ([`Self::iter_from_hash`]), so an entry that opens its leaf (whose key the parent holds as a separator, so a
  /// descent by hash alone lands on the leaf before it) and a run of one hash that spans leaves are both found.
  pub fn name_of<'b>(
    &self,
    blocks: &'b BlockStore,
    hash: u64,
    wanted: &dyn Fn(Child) -> bool,
  ) -> Option<&'b str> {
    self
      .iter_from_hash(blocks, hash)
      .take_while(|(entry_hash, _, _)| *entry_hash == hash)
      .find(|(_, _, child)| wanted(*child))
      .map(|(_, name, _)| name)
  }

  /// Every block of the tree with its birth epoch, for release and destroy walks.
  pub fn blocks(&self, blocks: &BlockStore, out: &mut Vec<(BlockRef, Epoch)>) {
    self.blocks_since(blocks, None, out);
  }

  /// Every block born after `since` (a block born at or before it is shared with an origin
  /// and so is everything beneath it), for a pruned destroy walk.
  pub fn blocks_since(
    &self,
    blocks: &BlockStore,
    since: Option<Epoch>,
    out: &mut Vec<(BlockRef, Epoch)>,
  ) {
    let mut stack = vec![(self.root, 1u8)];
    while let Some((h, depth)) = stack.pop() {
      let Ok(b) = view(blocks, h) else { continue };
      if since.is_some_and(|s| b.born().0 <= s.0) {
        continue;
      }
      out.push((h, b.born()));
      if depth < self.height {
        for at in 0..b.count() {
          stack.push((b.child_block(at), depth.saturating_add(1)));
        }
      }
    }
  }

  /// Every block of the tree with its size and the size of the class that fits its live bytes, for the oracles
  /// (A-203).
  pub fn block_sizes(&self, blocks: &BlockStore) -> Vec<(BlockRef, usize, usize)> {
    let mut all = Vec::new();
    self.blocks(blocks, &mut all);
    all
      .into_iter()
      .filter_map(|(block, _)| {
        view(blocks, block)
          .ok()
          .map(|b| (block, b.size(), b.fitted()))
      })
      .collect()
  }

  /// Copies `block` (at `level` of `path`) when it was born before `epoch` and re-points its parent; returns the
  /// block to mutate.
  fn own(
    &mut self,
    blocks: &mut BlockStore,
    epoch: Epoch,
    retired: &mut Retired,
    path: &Path,
    level: usize,
    block: BlockRef,
  ) -> Result<BlockRef, VfsError> {
    let born = view(blocks, block)?.born();
    if born == epoch {
      return Ok(block);
    }
    let fresh = copied(blocks, block, epoch)?;
    retired.push((block, born));
    self.repoint(blocks, path, level, fresh)?;
    Ok(fresh)
  }

  /// Points the parent of the block at `depth` on `path` (or the root, at depth zero) at `fresh`.
  fn repoint(
    &mut self,
    blocks: &mut BlockStore,
    path: &Path,
    depth: usize,
    fresh: BlockRef,
  ) -> Result<(), VfsError> {
    if depth == 0 {
      self.root = fresh;
      return Ok(());
    }
    let (parent, at) = step(path, depth.checked_sub(1))?;
    let mut parent = edit(blocks, parent)?;
    let s = parent.read().slot(at);
    parent.set_slot(at, s.pointing_at(fresh));
    Ok(())
  }

  /// Descends to the leaf for `key`, copying every block on the path not born at `epoch`
  /// (copy-on-write) and re-pointing its parent; returns the path.
  fn descend_mut(
    &mut self,
    blocks: &mut BlockStore,
    epoch: Epoch,
    retired: &mut Retired,
    policy: NameEquivalence,
    hash: u64,
    name: &str,
  ) -> Result<Path, VfsError> {
    let mut path: Path = [(self.root, 0); MAX_HEIGHT];
    let mut block = self.root;
    let height = usize::from(self.height);
    for level in 0..height {
      block = self.own(blocks, epoch, retired, &path, level, block)?;
      let inner = level.saturating_add(1) < height;
      let at = if inner {
        view(blocks, block)?.child_for(policy, hash, name)
      } else {
        0
      };
      if let Some(recorded) = path.get_mut(level) {
        *recorded = (block, at);
      }
      if inner {
        block = view(blocks, block)?.child_block(at);
      }
    }
    Ok(path)
  }

  /// The blocks a descent to `name` copies: those on its path born before `epoch` (the read-only
  /// twin of [`Tree::descend_mut`]), and whether the leaf it reaches can take a name of `fit_len`
  /// bytes without a new block.
  pub(crate) fn descent_cost(
    &self,
    blocks: &BlockStore,
    epoch: Epoch,
    policy: NameEquivalence,
    hash: u64,
    name: &str,
    fit_len: usize,
  ) -> Result<(usize, bool), VfsError> {
    let mut copies = 0usize;
    let mut block = self.root;
    for level in 0..usize::from(self.height) {
      let node = view(blocks, block)?;
      copies = copies.saturating_add(usize::from(node.born() != epoch));
      if level.saturating_add(1) < usize::from(self.height) {
        block = node.child_block(node.child_for(policy, hash, name));
      } else {
        return Ok((copies, node.fits(fit_len)));
      }
    }
    Ok((copies, true))
  }

  /// Refuses, before anything changes, a mutation that needs more blocks than the store can issue: the
  /// path copies, and for an insert whose leaf cannot take it the worst case — two new blocks at every level (a grown
  /// one, or a split's sibling and its fitted other half) and a new root above. Reserving the worst case before the
  /// first change is how a transaction that may cascade is admitted (XFS reserves each transaction's worst-case log
  /// space before it starts [B: Linux `Documentation/filesystems/xfs/xfs-delayed-logging-design.rst`,
  /// "Transaction Reservations"; cited from memory, to verify]); it refuses at most
  /// `2 × height + 1` blocks early, where a split found the slab full partway and dropped the half it had
  /// moved (AUD-29-40).
  fn admit(blocks: &BlockStore, needed: usize) -> Result<(), VfsError> {
    if blocks.room() < needed {
      return Err(VfsError::Memory(blocks.refusal()));
    }
    Ok(())
  }

  /// The most blocks an insert of `name` can take: the path's copies, and when the leaf cannot take
  /// the name, the cascade's worst case ([`Tree::cascade`]) — what [`Tree::insert`] admits.
  pub(crate) fn insert_blocks(
    &self,
    blocks: &BlockStore,
    epoch: Epoch,
    policy: NameEquivalence,
    name: &str,
  ) -> Result<usize, VfsError> {
    let (copies, fits) =
      self.descent_cost(blocks, epoch, policy, policy.hash(name), name, name.len())?;
    Ok(copies.saturating_add(self.cascade(fits)))
  }

  /// The blocks a cascade can take past the copies: none when the leaf takes the entry, else two at every level (a
  /// grown block, or a split's sibling and the half that stayed, moved to the class that fits it) and a new root.
  fn cascade(&self, fits: bool) -> usize {
    if fits {
      0
    } else {
      usize::from(self.height).saturating_mul(2).saturating_add(1)
    }
  }

  /// The blocks a change of the entry `name` (its child set, or its removal) takes: the path's copies.
  pub(crate) fn change_blocks(
    &self,
    blocks: &BlockStore,
    epoch: Epoch,
    policy: NameEquivalence,
    name: &str,
  ) -> Result<usize, VfsError> {
    Ok(
      self
        .descent_cost(blocks, epoch, policy, policy.hash(name), name, 0)?
        .0,
    )
  }

  /// The most blocks a respelling of `old` as `new` can take, as [`Tree::respell`] admits.
  pub(crate) fn respell_blocks(
    &self,
    blocks: &BlockStore,
    epoch: Epoch,
    policy: NameEquivalence,
    old: &str,
    new: &str,
  ) -> Result<usize, VfsError> {
    let (copies, fits) =
      self.descent_cost(blocks, epoch, policy, policy.hash(old), old, new.len())?;
    Ok(copies.saturating_add(self.cascade(fits)))
  }

  /// Gives every block of a tree this mutation built and never published back to the store: the undo
  /// of a small directory's move to a tree that was refused partway (the tree's blocks are all born
  /// now and reachable only from it).
  /// Every block is tried; the first refusal is returned, for the caller's [`crate::error::VfsError::after_undo`].
  pub fn discard(self, blocks: &mut BlockStore) -> Result<(), slates_mem::MemError> {
    let mut owned = Vec::new();
    self.blocks(blocks, &mut owned);
    let mut first = Ok(());
    for (block, _) in owned {
      let removed = blocks.discard(block);
      if first.is_ok() {
        first = removed;
      }
    }
    first
  }

  /// Inserts `name → child` (the caller checked it is absent). All or nothing: the copies and any
  /// growth or split are admitted before the first block changes.
  pub fn insert(
    &mut self,
    blocks: &mut BlockStore,
    epoch: Epoch,
    retired: &mut Retired,
    policy: NameEquivalence,
    name: &str,
    child: Child,
  ) -> Result<(), VfsError> {
    let hash = policy.hash(name);
    let (copies, fits) = self.descent_cost(blocks, epoch, policy, hash, name, name.len())?;
    Self::admit(blocks, copies.saturating_add(self.cascade(fits)))?;
    let path = self.descend_mut(blocks, epoch, retired, policy, hash, name)?;
    let depth = usize::from(self.height).saturating_sub(1);
    let (leaf, _) = step(&path, Some(depth))?;
    let at = match view(blocks, leaf)?.find(policy, hash, name) {
      Ok(_) => return Err(VfsError::AlreadyExists),
      Err(at) => at,
    };
    let entry = Entry {
      at,
      slot: Slot::from_child(hash, child),
      name,
    };
    let place = Place {
      path: &path,
      depth,
      block: leaf,
    };
    self.insert_split(blocks, epoch, retired, place, entry)?;
    self.count = self.count.saturating_add(1);
    Ok(())
  }

  /// Inserts into the block at `place`, growing it, or splitting it upward at the largest class, as needed.
  fn insert_split(
    &mut self,
    blocks: &mut BlockStore,
    epoch: Epoch,
    retired: &mut Retired,
    place: Place<'_>,
    entry: Entry<'_>,
  ) -> Result<(), VfsError> {
    let b = view(blocks, place.block)?;
    if b.fits(entry.name.len()) {
      edit(blocks, place.block)?.insert_at(entry.at, entry.slot, entry.name);
      return Ok(());
    }
    let needed = HEADER_BYTES
      .saturating_add(b.live_bytes())
      .saturating_add(ENTRY_BYTES)
      .saturating_add(entry.name.len());
    if needed <= b.size() {
      // Holes of removed names: packing them makes the room.
      edit(blocks, place.block)?.compact_names();
      return self.insert_split(blocks, epoch, retired, place, entry);
    }
    if b.size() < LARGEST_CLASS {
      let born = b.born();
      let grown = resized(blocks, place.block, fitted_size(needed))?;
      self.repoint(blocks, place.path, place.depth, grown)?;
      retired.push((place.block, born));
      let place = Place {
        block: grown,
        ..place
      };
      return self.insert_split(blocks, epoch, retired, place, entry);
    }
    self.split(blocks, epoch, retired, place, entry)
  }

  /// Splits the full largest block at `place`: the upper half by bytes moves to a fresh sibling of the class that
  /// fits it; the new entry goes to whichever side its position falls in; the sibling's first key becomes a separator
  /// in the parent. An entry appended past the end of the tree's rightmost block is the exception: the full block
  /// stays full and the entry starts the sibling alone (PostgreSQL nbtree's rightmost-page split, SQLite's
  /// `balance_quick`), so ascending inserts — a directory rebuilt in key order — fill their blocks where a half/half
  /// split left every block half empty.
  fn split(
    &mut self,
    blocks: &mut BlockStore,
    epoch: Epoch,
    retired: &mut Retired,
    place: Place<'_>,
    entry: Entry<'_>,
  ) -> Result<(), VfsError> {
    let (block, depth) = (place.block, place.depth);
    let b = view(blocks, block)?;
    let appending = entry.at == b.count() && rightmost_above(blocks, place.path, depth)?;
    let mut sibling_bytes: Scratch = [0u8; LARGEST_CLASS];
    let mut sibling = BlockMut {
      bytes: &mut sibling_bytes,
    };
    sibling.init(epoch, b.is_index());
    if !appending {
      edit(blocks, block)?.split_into(&mut sibling);
    }
    let left_count = view(blocks, block)?.count();
    let len = entry.name.len();
    if appending || entry.at > left_count {
      let into = if appending {
        0
      } else {
        entry.at.saturating_sub(left_count)
      };
      if !sibling.read().fits(len) {
        return Err(VfsError::NameTooLong);
      }
      sibling.insert_at(into, entry.slot, entry.name);
    } else {
      if !view(blocks, block)?.fits(len) {
        return Err(VfsError::NameTooLong);
      }
      edit(blocks, block)?.insert_at(entry.at, entry.slot, entry.name);
    }
    let (sep_hash, sep_name) = sibling.read().first_key_owned()?;
    let sibling = store_fitted(blocks, sibling.read())?;
    let sep = Slot::for_block(sep_hash, sibling);
    // The half that stayed goes to its own class too (names of very different lengths can leave it under half).
    let block = self.shrink_if_sparse(blocks, retired, place.path, depth, block)?;
    if depth == 0 {
      return self.new_root(blocks, epoch, block, sep, &sep_name);
    }
    let (parent, parent_at) = step(place.path, depth.checked_sub(1))?;
    let up = Place {
      path: place.path,
      depth: depth.saturating_sub(1),
      block: parent,
    };
    let separator = Entry {
      at: parent_at.saturating_add(1),
      slot: sep,
      name: &sep_name,
    };
    self.insert_split(blocks, epoch, retired, up, separator)
  }

  /// A new root above the old one, `left`, and its new sibling, whose separator is `sep`.
  fn new_root(
    &mut self,
    blocks: &mut BlockStore,
    epoch: Epoch,
    left: BlockRef,
    sep: Slot,
    sep_name: &str,
  ) -> Result<(), VfsError> {
    let (first_hash, first_name) = view(blocks, left)?.first_key_owned()?;
    let mut root_bytes: Scratch = [0u8; LARGEST_CLASS];
    let mut root = BlockMut {
      bytes: &mut root_bytes,
    };
    root.init(epoch, true);
    root.insert_at(0, Slot::for_block(first_hash, left), &first_name);
    root.insert_at(1, sep, sep_name);
    self.root = store_fitted(blocks, root.read())?;
    self.height = self.height.saturating_add(1);
    Ok(())
  }

  /// Respells the entry `old` as `new`, an equivalent spelling under `policy` (same hash, same folded
  /// key, so the same position): the entry is rewritten where it is, growing or splitting the leaf only when the
  /// new spelling does not fit. All or nothing: the copies, and the cascade's worst case when the leaf
  /// cannot take the new spelling, are admitted before the first block changes. `false` when `old` is
  /// absent.
  pub fn respell(
    &mut self,
    blocks: &mut BlockStore,
    epoch: Epoch,
    retired: &mut Retired,
    policy: NameEquivalence,
    old: &str,
    new: &str,
  ) -> Result<bool, VfsError> {
    let hash = policy.hash(old);
    // A removed name stays a hole until the block compacts, so even a shorter spelling may need a
    // new block: the cascade's worst case is reserved whenever the leaf cannot take the new spelling now.
    let (copies, fits) = self.descent_cost(blocks, epoch, policy, hash, old, new.len())?;
    Self::admit(blocks, copies.saturating_add(self.cascade(fits)))?;
    let path = self.descend_mut(blocks, epoch, retired, policy, hash, old)?;
    let depth = usize::from(self.height).saturating_sub(1);
    let (leaf, _) = step(&path, Some(depth))?;
    let Ok(at) = view(blocks, leaf)?.find(policy, hash, old) else {
      return Ok(false);
    };
    let slot = edit(blocks, leaf)?.remove_at(at);
    let entry = Entry {
      at,
      slot: Slot {
        name_off: 0,
        name_len: 0,
        ..slot
      },
      name: new,
    };
    let place = Place {
      path: &path,
      depth,
      block: leaf,
    };
    self.insert_split(blocks, epoch, retired, place, entry)?;
    Ok(true)
  }

  /// Removes `name`; returns its child if it was present.
  pub fn remove(
    &mut self,
    blocks: &mut BlockStore,
    epoch: Epoch,
    retired: &mut Retired,
    policy: NameEquivalence,
    name: &str,
  ) -> Result<Option<Child>, VfsError> {
    let hash = policy.hash(name);
    let (copies, _) = self.descent_cost(blocks, epoch, policy, hash, name, 0)?;
    Self::admit(blocks, copies)?;
    let path = self.descend_mut(blocks, epoch, retired, policy, hash, name)?;
    let depth = usize::from(self.height).saturating_sub(1);
    let (leaf, _) = step(&path, Some(depth))?;
    let Ok(at) = view(blocks, leaf)?.find(policy, hash, name) else {
      return Ok(None);
    };
    let slot = edit(blocks, leaf)?.remove_at(at);
    self.count = self.count.saturating_sub(1);
    self.rebalance(blocks, retired, &path, depth)?;
    Ok(Some(slot.to_child()))
  }

  /// After a removal in the block at `depth`: an emptied block leaves the tree, a block under a quarter of the
  /// largest class merges with its right sibling when both fit in one, a block holding less than half its class
  /// shrinks to the class that fits, separators follow the first keys.
  fn rebalance(
    &mut self,
    blocks: &mut BlockStore,
    retired: &mut Retired,
    path: &Path,
    depth: usize,
  ) -> Result<(), VfsError> {
    if depth == 0 {
      self.collapse_root(blocks, retired)?;
      return self
        .shrink_if_sparse(blocks, retired, path, 0, self.root)
        .map(drop);
    }
    let (block, _) = step(path, Some(depth))?;
    let (parent, parent_at) = step(path, depth.checked_sub(1))?;
    let b = view(blocks, block)?;
    if b.count() == 0 {
      let born = b.born();
      edit(blocks, parent)?.remove_at(parent_at);
      retired.push((block, born));
      return self.rebalance(blocks, retired, path, depth.saturating_sub(1));
    }
    // The separator follows the block's first key.
    self.refresh_separator(blocks, parent, parent_at, block)?;
    if !self.merge_right(blocks, retired, path, depth)? {
      self.shrink_if_sparse(blocks, retired, path, depth, block)?;
    }
    self.rebalance(blocks, retired, path, depth.saturating_sub(1))
  }

  /// The root: a one-child index collapses into its child.
  fn collapse_root(&mut self, blocks: &BlockStore, retired: &mut Retired) -> Result<(), VfsError> {
    while self.height > 1 && view(blocks, self.root)?.count() == 1 {
      let root = view(blocks, self.root)?;
      retired.push((self.root, root.born()));
      self.root = root.child_block(0);
      self.height = self.height.saturating_sub(1);
    }
    Ok(())
  }

  /// Merges the block at `depth` of `path` with its right sibling when its live bytes are under a quarter of the
  /// largest class and both fit in one largest block: in place when it has the room, else into a fresh block of the
  /// class that fits both, when the store has one to give (a removal never needs room). Whether it merged.
  fn merge_right(
    &mut self,
    blocks: &mut BlockStore,
    retired: &mut Retired,
    path: &Path,
    depth: usize,
  ) -> Result<bool, VfsError> {
    let (block, _) = step(path, Some(depth))?;
    let (parent, parent_at) = step(path, depth.checked_sub(1))?;
    let p = view(blocks, parent)?;
    let b = view(blocks, block)?;
    let right_at = parent_at.saturating_add(1);
    if b.live_bytes() >= LARGEST_CLASS / MERGE_BELOW_QUARTER || right_at >= p.count() {
      return Ok(false);
    }
    let right = p.child_block(right_at);
    let r = view(blocks, right)?;
    let (right_born, right_live) = (r.born(), r.live_bytes());
    let together = HEADER_BYTES
      .saturating_add(b.live_bytes())
      .saturating_add(right_live);
    if b.free_bytes() >= right_live {
      let mut copy: Scratch = [0u8; LARGEST_CLASS];
      let source = view(blocks, right)?.bytes;
      let bytes = copy.get_mut(..source.len()).ok_or(VfsError::Invalid)?;
      bytes.copy_from_slice(source);
      edit(blocks, block)?.absorb(Block { bytes: &*bytes });
    } else if together <= LARGEST_CLASS && blocks.room() > 0 {
      self.merged_anew(blocks, retired, path, depth, right)?;
    } else {
      return Ok(false);
    }
    edit(blocks, parent)?.remove_at(right_at);
    retired.push((right, right_born));
    Ok(true)
  }

  /// The block at `depth` and `right` copied into one fresh block of the class that fits both, put where the block
  /// was; the old block is retired here, `right` by the caller.
  fn merged_anew(
    &mut self,
    blocks: &mut BlockStore,
    retired: &mut Retired,
    path: &Path,
    depth: usize,
    right: BlockRef,
  ) -> Result<(), VfsError> {
    let (block, _) = step(path, Some(depth))?;
    let born = view(blocks, block)?.born();
    let mut both: Scratch = [0u8; LARGEST_CLASS];
    let mut out = BlockMut { bytes: &mut both };
    rebuild(view(blocks, block)?, &mut out);
    out.absorb(view(blocks, right)?);
    let merged = store_fitted(blocks, out.read())?;
    self.repoint(blocks, path, depth, merged)?;
    retired.push((block, born));
    Ok(())
  }

  /// Shrinks `block` (at `depth` of `path`) to the class that fits it when that is under half its class and the store
  /// has a block to give: a removal never needs room, and a block twice its fitted class at most gives back what a
  /// shrinking directory frees while an insert and a removal at a class's edge cannot copy it back and forth.
  fn shrink_if_sparse(
    &mut self,
    blocks: &mut BlockStore,
    retired: &mut Retired,
    path: &Path,
    depth: usize,
    block: BlockRef,
  ) -> Result<BlockRef, VfsError> {
    let b = view(blocks, block)?;
    let fitted = b.fitted();
    if fitted.saturating_mul(2) >= b.size() || blocks.room() == 0 {
      return Ok(block);
    }
    let born = b.born();
    let small = resized(blocks, block, fitted)?;
    self.repoint(blocks, path, depth, small)?;
    retired.push((block, born));
    Ok(small)
  }

  fn refresh_separator(
    &self,
    blocks: &mut BlockStore,
    parent: BlockRef,
    parent_at: usize,
    block: BlockRef,
  ) -> Result<(), VfsError> {
    let (hash, name) = view(blocks, block)?.first_key_owned()?;
    let p = view(blocks, parent)?;
    let current = p.slot(parent_at);
    if current.hash == hash && p.name(current) == name {
      return Ok(());
    }
    let mut p = edit(blocks, parent)?;
    let mut s = p.remove_at(parent_at);
    s.hash = hash;
    // The separator's name must fit; a block that cannot hold it keeps the old separator,
    // which still orders correctly because it is not greater than the child's first key.
    if p.read().fits(name.len()) {
      p.insert_at(parent_at, s, &name);
    } else {
      p.insert_at(parent_at, s, "");
    }
    Ok(())
  }

  /// Replaces the child of an existing entry.
  pub fn set_child(
    &mut self,
    blocks: &mut BlockStore,
    epoch: Epoch,
    retired: &mut Retired,
    policy: NameEquivalence,
    name: &str,
    child: Child,
  ) -> Result<bool, VfsError> {
    let hash = policy.hash(name);
    let (copies, _) = self.descent_cost(blocks, epoch, policy, hash, name, 0)?;
    Self::admit(blocks, copies)?;
    let path = self.descend_mut(blocks, epoch, retired, policy, hash, name)?;
    let (leaf, _) = step(&path, usize::from(self.height).checked_sub(1))?;
    let Ok(at) = view(blocks, leaf)?.find(policy, hash, name) else {
      return Ok(false);
    };
    let mut leaf = edit(blocks, leaf)?;
    let old = leaf.read().slot(at);
    let mut s = Slot::from_child(hash, child);
    s.name_off = old.name_off;
    s.name_len = old.name_len;
    leaf.set_slot(at, s);
    Ok(true)
  }

  /// The entries in canonical order whose hash is at least `hash` — one descent, then the walk — so a listing
  /// resumed from a hash cookie does no work for the entries before it, and an entry removed or added
  /// meanwhile shifts nothing (§4.5; AUD-29-86).
  pub fn iter_from_hash<'b>(&self, blocks: &'b BlockStore, hash: u64) -> TreeIter<'b> {
    let height = usize::from(self.height);
    let mut it = TreeIter {
      blocks,
      stack: [(self.root, 0); MAX_HEIGHT],
      depth: 0,
      height,
    };
    let mut block = self.root;
    for level in 0..height {
      let Ok(b) = view(blocks, block) else {
        // A block that cannot be read ends the walk: an empty listing, never a wrong one.
        it.depth = 0;
        return it;
      };
      let leaf = level.saturating_add(1) == height;
      // An index entry's key is its child's first key; the child holding the first key at or above
      // `(hash, "")` is the last whose first key is at most that — every name sorts after the empty one.
      let at = if leaf {
        b.first_hash_at_least(hash)
      } else {
        b.child_for(NameEquivalence::Exact, hash, "")
      };
      let Some(entry) = it.stack.get_mut(level) else {
        it.depth = 0;
        return it;
      };
      *entry = (block, at);
      it.depth = level.saturating_add(1);
      if leaf {
        break;
      }
      block = b.child_block(at);
    }
    it
  }

  /// The entries in canonical order.
  pub fn iter<'b>(&self, blocks: &'b BlockStore) -> TreeIter<'b> {
    let mut it = TreeIter {
      blocks,
      stack: [(self.root, 0); MAX_HEIGHT],
      depth: 1,
      height: usize::from(self.height),
    };
    it.settle();
    it
  }
}

/// Where an insert lands: the descent's path, the level and the block there.
#[derive(Clone, Copy)]
struct Place<'p> {
  path: &'p Path,
  depth: usize,
  block: BlockRef,
}

/// What an insert puts: its position in the block, its slot and its name.
#[derive(Clone, Copy)]
struct Entry<'n> {
  at: usize,
  slot: Slot,
  name: &'n str,
}

/// An in-order walk over a tree's leaves, one block per level on its stack.
pub struct TreeIter<'b> {
  blocks: &'b BlockStore,
  stack: [(BlockRef, usize); MAX_HEIGHT],
  /// Levels on the stack (the last is a leaf when settled).
  depth: usize,
  height: usize,
}

/// Where a [`TreeIter`] stands, held apart from the blocks it walks, so a caller that holds the store a step at a time
/// can read a directory an entry a step: [`TreeCursor::resume`] over the same blocks goes on from there. The blocks a
/// snapshot reaches never change while it lives (copy-on-write, §4.5), so a cursor over a snapshot's directory stays
/// good from one step to the next; a block that can no longer be read ends the walk, as it ends an iterator's.
#[derive(Clone, Copy, Debug)]
pub struct TreeCursor {
  stack: [(BlockRef, usize); MAX_HEIGHT],
  depth: usize,
  height: usize,
}

impl TreeCursor {
  /// The walk this cursor marks, over `blocks`.
  pub fn resume(self, blocks: &BlockStore) -> TreeIter<'_> {
    TreeIter {
      blocks,
      stack: self.stack,
      depth: self.depth,
      height: self.height,
    }
  }
}

impl<'b> TreeIter<'b> {
  /// Where this walk stands, to [`TreeCursor::resume`] it later over the same blocks.
  pub fn cursor(&self) -> TreeCursor {
    TreeCursor {
      stack: self.stack,
      depth: self.depth,
      height: self.height,
    }
  }

  /// The block and slot on top of the stack.
  fn top(&self) -> Option<(BlockRef, usize)> {
    self
      .depth
      .checked_sub(1)
      .and_then(|level| self.stack.get(level))
      .copied()
  }

  /// Moves the top of the stack to its next slot.
  fn advance_top(&mut self) {
    if let Some((_, at)) = self
      .depth
      .checked_sub(1)
      .and_then(|level| self.stack.get_mut(level))
    {
      *at = at.saturating_add(1);
    }
  }

  /// Leaves the top block: back to its parent, which moves on to its next child.
  fn pop(&mut self) {
    self.depth = self.depth.saturating_sub(1);
    self.advance_top();
  }

  /// Descends leftmost from the top of the stack until a leaf is on top.
  fn settle(&mut self) {
    while self.depth < self.height {
      let Some((h, at)) = self.top() else {
        return;
      };
      let Ok(b) = view(self.blocks, h) else {
        self.depth = 0;
        return;
      };
      if at >= b.count() {
        self.pop();
        continue;
      }
      let child = b.child_block(at);
      let Some(below) = self.stack.get_mut(self.depth) else {
        // Deeper than `MAX_HEIGHT` (never: `height` is at most it): the walk ends rather than misreads.
        self.depth = 0;
        return;
      };
      *below = (child, 0);
      self.depth = self.depth.saturating_add(1);
    }
  }
}

impl<'b> Iterator for TreeIter<'b> {
  type Item = (u64, &'b str, Child);

  fn next(&mut self) -> Option<Self::Item> {
    loop {
      let (h, at) = self.top()?;
      let b = view(self.blocks, h).ok()?;
      if at < b.count() {
        self.advance_top();
        let s = b.slot(at);
        return Some((s.hash, b.name(s), s.to_child()));
      }
      // This leaf is done: go up one level, advance, and settle on the next leaf.
      self.pop();
      self.settle();
    }
  }
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeMap;

  use super::*;

  fn slab() -> BlockStore {
    BlockStore::new(4, 1 << 20)
  }

  fn keyed(policy: NameEquivalence, n: &str) -> (u64, String) {
    (policy.hash(n), n.to_owned())
  }

  type Model = BTreeMap<(u64, String), Child>;

  struct Fixture {
    blocks: BlockStore,
    tree: Tree,
    model: Model,
    retired: Retired,
    epoch: Epoch,
    /// Roots frozen at earlier epochs with what they listed then.
    old_roots: Vec<(Tree, Vec<String>)>,
  }

  const POLICY: NameEquivalence = NameEquivalence::Fold;

  fn listed(tree: &Tree, blocks: &BlockStore) -> Vec<(u64, String)> {
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

  /// A-203's class rule: every block is at most twice the class that fits its live bytes (the store always has room
  /// here, so no shrink was passed over).
  fn assert_classes_fit(tree: &Tree, blocks: &BlockStore) {
    for (block, size, fitted) in tree.block_sizes(blocks) {
      assert!(
        size <= 2 * fitted,
        "{block:?}: a {size}-byte block whose entries fit {fitted}"
      );
    }
  }

  fn assert_equal_to_model(f: &Fixture) {
    assert_classes_fit(&f.tree, &f.blocks);
    for (old, _) in &f.old_roots {
      assert_classes_fit(old, &f.blocks);
    }
    let expected: Vec<(u64, String)> = f.model.keys().cloned().collect();
    assert_eq!(listed(&f.tree, &f.blocks), expected);
    assert_eq!(usize::try_from(f.tree.count).unwrap(), f.model.len());
    for (key, child) in &f.model {
      let found = f.tree.lookup(&f.blocks, POLICY, &key.1.to_uppercase());
      assert_eq!(found.map(|(_, c, _)| c), Some(*child), "{}", key.1);
      // The reverse lookup a path report takes (an inode's home names its hash and its number): every entry,
      // the ones that open a leaf included, is found by its hash and child (AUD-29-76's large-span diff).
      let named = f.tree.name_of(&f.blocks, key.0, &|c| c == *child);
      assert_eq!(named, Some(key.1.as_str()), "name_of by hash {}", key.0);
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
    let held: usize = f
      .tree
      .block_sizes(&f.blocks)
      .iter()
      .map(|(_, size, _)| size)
      .sum();
    assert!(
      held < 4 * f.model.len() * (ENTRY_BYTES + 60),
      "merges keep the blocks from spreading: {held} bytes of block for {} entries",
      f.model.len()
    );
    // Old roots still list what they listed, untouched by every later insert and removal.
    for (old, names) in &f.old_roots {
      let now: Vec<String> = listed(old, &f.blocks).into_iter().map(|(_, n)| n).collect();
      assert_eq!(&now, names);
    }
    assert!(!f.retired.is_empty(), "copies and merges retired blocks");
  }

  /// AUD-29-86. Do: over the 3,000-entry tree (three levels of splits) and again after removing two thirds,
  /// resume a listing from the hash of every present entry, from each such hash plus one, and from zero and the
  /// largest hash. Expect: each continuation equals the ordered map's range from `(hash, "")` — the oracle —
  /// so a listing resumed from a hash neither repeats nor skips an entry.
  #[test]
  fn a_listing_resumed_from_a_hash_equals_the_maps_range_from_it() {
    let mut f = fill();
    let check = |f: &Fixture| {
      let mut probes: Vec<u64> = f.model.keys().map(|(h, _)| *h).collect();
      let after: Vec<u64> = probes.iter().map(|h| h.saturating_add(1)).collect();
      probes.extend(after);
      probes.push(0);
      probes.push(u64::MAX);
      for hash in probes {
        let expected: Vec<(u64, String)> = f
          .model
          .range((hash, String::new())..)
          .map(|(k, _)| k.clone())
          .collect();
        let resumed: Vec<(u64, String)> = f
          .tree
          .iter_from_hash(&f.blocks, hash)
          .map(|(h, n, _)| (h, n.to_owned()))
          .collect();
        assert_eq!(resumed, expected, "from {hash}");
      }
    };
    assert!(f.tree.height >= 2);
    check(&f);
    let names: Vec<String> = f.model.keys().map(|k| k.1.clone()).collect();
    for name in names
      .iter()
      .enumerate()
      .filter(|(i, _)| i % 3 != 0)
      .map(|(_, n)| n)
    {
      f.tree
        .remove(&mut f.blocks, f.epoch, &mut f.retired, POLICY, name)
        .unwrap();
      f.model.remove(&keyed(POLICY, name));
    }
    check(&f);
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
    // The removals merged blocks, which retires the merged-away ones; what follows must retire nothing.
    retired.clear();
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

  /// §4.8 rebuild (the rightmost split). Do: insert 4,000 names in the tree's own key order (as a rebuild does), and
  /// the same names in a scattered order into a second tree. Expect: both list exactly the model; the ascending tree's
  /// leaves are full but for its last, so it holds far fewer blocks than the scattered one (whose splits leave halves).
  #[test]
  fn ascending_inserts_fill_their_blocks() {
    let mut names: Vec<(u64, String)> = (0..4000u64)
      .map(|n| {
        keyed(
          POLICY,
          &format!(
            "name-{:05}-{}",
            (n * 7919) % 4001,
            "y".repeat(usize::try_from(n % 30).unwrap_or(0))
          ),
        )
      })
      .collect();
    let scattered = names.clone();
    names.sort();
    let fill = |order: &[(u64, String)]| {
      let mut blocks = slab();
      let mut tree = Tree::new(&mut blocks, Epoch(0)).unwrap();
      let mut retired = Retired::new();
      for (index, (_, name)) in order.iter().enumerate() {
        let child = Child::File(InodeNo(u64::try_from(index).unwrap_or(0)));
        tree
          .insert(&mut blocks, Epoch(0), &mut retired, POLICY, name, child)
          .unwrap();
      }
      let listed = listed(&tree, &blocks);
      (tree.block_sizes(&blocks).len(), listed)
    };
    let (ascending_blocks, ascending_listed) = fill(&names);
    let (scattered_blocks, scattered_listed) = fill(&scattered);
    assert_eq!(
      ascending_listed, names,
      "the ascending tree lists the model"
    );
    assert_eq!(
      scattered_listed, names,
      "the scattered tree lists the model"
    );
    let bytes: usize = names.iter().map(|(_, n)| ENTRY_BYTES + n.len()).sum();
    let full_leaves = bytes.div_ceil(LARGEST_CLASS - HEADER_BYTES);
    assert!(
      ascending_blocks <= full_leaves + full_leaves / 8 + 2,
      "ascending: {ascending_blocks} blocks for {full_leaves} blocks' worth of entries"
    );
    assert!(
      ascending_blocks * 5 < scattered_blocks * 4,
      "ascending {ascending_blocks} blocks against scattered {scattered_blocks}"
    );
  }

  /// A-203. Do: insert the measured directory, 36 names of 49 bytes, one at a time. Expect: it is one block of
  /// 3,072 bytes (the class that fits its 2,644), where a 4 KiB block held it before A-101 and four 1 KiB blocks after;
  /// on the way it passed through every class from 256 bytes up, each growth retiring the block it replaced.
  #[test]
  fn the_measured_directory_is_one_block_of_the_class_that_fits_it() {
    let mut blocks = slab();
    let mut tree = Tree::new(&mut blocks, Epoch(1)).unwrap();
    let mut retired = Retired::new();
    let mut sizes = std::collections::BTreeSet::new();
    for n in 0..crate::dirblocks::MEASURED_DIR_ENTRIES {
      let name = format!("{n:0>42}.rlib.d");
      assert_eq!(name.len(), crate::dir::MEASURED_NAME_BYTES);
      tree
        .insert(
          &mut blocks,
          Epoch(1),
          &mut retired,
          POLICY,
          &name,
          Child::File(InodeNo(u64::try_from(n).unwrap())),
        )
        .unwrap();
      sizes.extend(
        tree
          .block_sizes(&blocks)
          .into_iter()
          .map(|(_, size, _)| size),
      );
    }
    let held = tree.block_sizes(&blocks);
    assert_eq!(held.len(), 1, "one block: {held:?}");
    assert_eq!(held[0].1, 3072);
    assert_eq!(tree.height, 1);
    assert!(sizes.len() >= 10, "grew through the classes: {sizes:?}");
    assert_eq!(
      retired.len(),
      sizes.len() - 1,
      "each growth retired the block it replaced"
    );
  }

  /// A-203. Do: insert 600 names, then remove all but ten, at one epoch. Expect: the tree lists the model at every
  /// step; it grew past one block, split and merged; and once nearly empty it holds a few hundred bytes of block, not
  /// the kilobytes it held full (the shrink gives the classes back).
  #[test]
  fn a_shrinking_directory_gives_its_classes_back() {
    let mut blocks = slab();
    let mut tree = Tree::new(&mut blocks, Epoch(1)).unwrap();
    let mut retired = Retired::new();
    let mut model = Model::new();
    let names: Vec<String> = (0..600)
      .map(|n| format!("file-{n}-{}", "z".repeat(n % 23)))
      .collect();
    for (n, name) in names.iter().enumerate() {
      let child = Child::File(InodeNo(u64::try_from(n).unwrap()));
      tree
        .insert(&mut blocks, Epoch(1), &mut retired, POLICY, name, child)
        .unwrap();
      model.insert(keyed(POLICY, name), child);
    }
    let full: usize = tree
      .block_sizes(&blocks)
      .iter()
      .map(|(_, size, _)| size)
      .sum();
    assert!(tree.height >= 2, "split");
    for name in names.iter().skip(10) {
      tree
        .remove(&mut blocks, Epoch(1), &mut retired, POLICY, name)
        .unwrap();
      model.remove(&keyed(POLICY, name));
      assert_classes_fit(&tree, &blocks);
    }
    let expected: Vec<(u64, String)> = model.keys().cloned().collect();
    assert_eq!(listed(&tree, &blocks), expected);
    let left: usize = tree
      .block_sizes(&blocks)
      .iter()
      .map(|(_, size, _)| size)
      .sum();
    assert!(
      left <= 1024,
      "{left} bytes of block left for ten entries (full: {full})"
    );
    // Every block the tree replaced or dropped is retired, and the store holds exactly the tree's own.
    for (block, _) in retired.drain(..) {
      blocks.discard(block).unwrap();
    }
    assert_eq!(blocks.len(), tree.block_sizes(&blocks).len());
  }

  /// A-203 (the measured insert-and-remove regression). Do: fill a tree's one leaf with 20 names, then insert and
  /// remove one more name 1,000 times, as a create and unlink of a temporary file does. Expect: the leaf is the same
  /// block throughout and holds no dead name bytes afterwards: the name written last is the lowest in the name area,
  /// so its removal hands its bytes back at once, and the round trip never compacts the block (before: every few
  /// rounds, since a fitted block's slack is a few dozen bytes).
  #[test]
  fn a_name_removed_after_it_was_written_leaves_no_hole() {
    let mut blocks = slab();
    let mut tree = Tree::new(&mut blocks, Epoch(1)).unwrap();
    let mut retired = Retired::new();
    for n in 0..20u64 {
      let name = format!("resident-{n}");
      tree.insert(&mut blocks, Epoch(1), &mut retired, POLICY, &name, Child::File(InodeNo(n))).unwrap();
    }
    let leaf = tree.root;
    for _ in 0..1000 {
      tree.insert(&mut blocks, Epoch(1), &mut retired, POLICY, "temporary", Child::File(InodeNo(99))).unwrap();
      assert!(tree.remove(&mut blocks, Epoch(1), &mut retired, POLICY, "temporary").unwrap().is_some());
      assert_eq!(tree.root, leaf, "the round trip kept its block");
      assert_eq!(view(&blocks, leaf).unwrap().names_dead(), 0, "a hole was left");
    }
  }
}

