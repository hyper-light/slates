//! Directory block storage (§4.5 D-4 `Indexed`, A-203): the bytes of a directory's B+-tree blocks in size classes,
//! each class a slab of pages, so a directory of three entries holds a 256-byte block and one of thirty-six a 3 KiB
//! one, where every block was 1 KiB (A-101) and before that 4 KiB.
//!
//! The classes run from [`SMALLEST_CLASS`] to [`LARGEST_CLASS`], four to a doubling (256, 320, 384, 448, 512, …,
//! 3,584, 4,096 bytes). A page is [`PAGE_BYTES`] of slots of one class; a block is named by a [`BlockRef`]: its page's
//! slab handle, its slot in the page and the slot's generation, so a block freed and its slot reused is a typed miss
//! for every reference to the old one, as a slab handle is (T-0.1). Pages come from one slab of fixed-size pages
//! ([`slates_mem::slab::Slab`]); a page whose last block is freed goes back to that slab, where any class takes it
//! again, so the classes share the memory and one class's churn does not strand another's (Bonwick's slab cache over
//! a page pool [A: Bonwick USENIX'94]). A page with a free slot is on its class's list, intrusively linked, so
//! choosing where a block goes is one look at the list's head and nothing ever reaches the global allocator per block:
//! the destroy stall `dirtree`'s module doc records (milliseconds inside `dealloc`) stays impossible.
//!
//! Bounds: the live blocks are at most the store's block cap (a count, so a refusal sweep can step it a block at a
//! time), and the pages at most [`BlockStore::max_pages`], derived from that cap at the smallest class's bytes. The
//! memory this storage can take is its pages' bytes, [`BlockStore::max_footprint_bytes`].
//!
//! Tried first and rejected (2026-10-05, BENCHMARKS "Directory blocks against real trees"): a block's bytes as a
//! global-allocator buffer of a power of two, which put destroy slices into milliseconds of `dealloc`; and power-of-two
//! classes cannot beat a 4 KiB block on the measured 36-entry directory (2,644 bytes take the 4 KiB class), which is
//! why the classes are four to a doubling.

use slates_mem::slab::Slab;
use slates_mem::{Handle, MemError};

/// Format: the bytes a block's header takes at its front: its birth epoch, entry count, name bytes used and dead,
/// and whether it is an index (`dirtree`'s layout).
pub const HEADER_BYTES: usize = 16;

/// Format: the bytes one entry takes in a block: hash, child word, packed word (`dirtree`'s layout).
pub const ENTRY_BYTES: usize = 24;

/// Measured: entries a directory of the measured `cargo build` tree holds, 35.9 (40,052 files in 1,117 directories,
/// `find target/debug -type f | wc -l` over `find target/debug -type d | wc -l`, this workspace, 2026-09-05; the
/// shape `vfs_bench` builds).
pub const MEASURED_DIR_ENTRIES: usize = 36;

/// Derived: the smallest class holds the first tree a directory becomes, the inline cut-over and one more entry at
/// the measured mean name length (`16 + 3 × (24 + 49)` = 235 bytes, rounded up to a power of two): the median real
/// directory (4 to 6 entries of shorter names in npm, pip and this repository, A-101) is one block of this class.
pub const SMALLEST_CLASS: usize = (HEADER_BYTES
  + (crate::dir::MEASURED_CUTOVER + 1) * (ENTRY_BYTES + crate::dir::MEASURED_NAME_BYTES))
  .next_power_of_two();

/// Derived: the largest class holds the measured directory whole (`16 + 36 × (24 + 49)` = 2,644 bytes, rounded up
/// to a power of two), and at least three entries at the longest name (`16 + 3 × (24 + 255)` = 853), which a B+-tree
/// block needs to split and still take an insert.
pub const LARGEST_CLASS: usize = max(
  (HEADER_BYTES + MEASURED_DIR_ENTRIES * (ENTRY_BYTES + crate::dir::MEASURED_NAME_BYTES))
    .next_power_of_two(),
  (HEADER_BYTES + MIN_ENTRIES_AT_NAME_MAX * (ENTRY_BYTES + crate::names::NAME_MAX))
    .next_power_of_two(),
);

/// Shape: the entries a full block holds at the longest name, at the least: two to split, three so a block left by a
/// split still takes an insert.
pub const MIN_ENTRIES_AT_NAME_MAX: usize = 3;

/// Derived: four classes to a doubling, so a block's unused tail is under a fifth of it, and a block that grows an
/// entry at a time is copied a constant number of times for each byte it holds (each class a quarter larger than the
/// one below it). jemalloc's spacing [C: jemalloc `include/jemalloc/internal/sc.h`, `SC_LG_NGROUP` 2]; against powers
/// of two, the measured 36-entry directory (2,644 bytes) takes 3,072 where it took 4,096.
pub const CLASSES_PER_DOUBLING: usize = 4;

/// Derived: the classes from the smallest to the largest, four to a doubling, both ends included
/// (`4 × log2(4096 / 256) + 1` = 17).
pub const CLASSES: usize = CLASSES_PER_DOUBLING * doublings() + 1;

/// Derived: a page holds as many largest blocks as the largest class holds smallest ones (`4096 × 16` = 64 KiB), so
/// the tail of a page that no whole slot fits in, under one slot of its class, is at most a sixteenth of the page
/// (3% at worst over the 17 classes: 2,048 of 64 KiB for 3,584-byte slots), and its slots are at most 256, numbered
/// by one byte.
pub const PAGE_BYTES: usize = LARGEST_CLASS * (LARGEST_CLASS / SMALLEST_CLASS);

/// Derived: the most slots a page has, at the smallest class (`65,536 / 256` = 256).
const SLOTS_MAX: usize = PAGE_BYTES / SMALLEST_CLASS;

// A slot number is one byte of a reference: the page's slots must be numbered by it.
const _: () = assert!(SLOTS_MAX <= 1 << SLOT_BITS);

/// Format: the bits of a reference's auxiliary word that number its slot in the page.
const SLOT_BITS: u32 = 8;

/// Format: a slot's generation takes the auxiliary word above the slot number.
const GENERATION_BITS: u32 = u32::BITS - SLOT_BITS;

/// Format: the largest generation a reference can carry; a slot freed at it retires for its page's life.
const GENERATION_LIMIT: u32 = (1 << GENERATION_BITS) - 1;

/// Format: a retired slot's generation, outside every generation a reference carries, so it matches none.
const RETIRED: u32 = u32::MAX;

/// Format: no slot (the end of a page's free list).
const NO_SLOT: u16 = u16::MAX;

/// Format: no page (the end of a class's list of pages with a free slot).
const NO_PAGE: u32 = u32::MAX;

/// Format: bytes of the free-list link a vacant slot keeps at its front.
const LINK_BYTES: usize = 2;

/// Format: a handle word carries the slot index above its generation.
const HANDLE_INDEX_SHIFT: u32 = 32;

/// Format: the low half of a handle word, the generation.
const HANDLE_GENERATION_MASK: u64 = 0xFFFF_FFFF;

/// Format: one byte's mask.
const BYTE_MASK: u32 = 0xFF;

const fn max(a: usize, b: usize) -> usize {
  if a > b { a } else { b }
}

/// The doublings from the smallest class to the largest.
const fn doublings() -> usize {
  let mut count: usize = 0;
  let mut size = SMALLEST_CLASS;
  while size < LARGEST_CLASS {
    size = size.saturating_mul(2);
    count = count.saturating_add(1);
  }
  count
}

/// The bytes of class `class`; zero past the last.
pub fn class_bytes(class: usize) -> usize {
  if class >= CLASSES {
    return 0;
  }
  let doubling = u32::try_from(class / CLASSES_PER_DOUBLING).unwrap_or(u32::MAX);
  let base = SMALLEST_CLASS.checked_shl(doubling).unwrap_or(0);
  let step = base / CLASSES_PER_DOUBLING;
  base.saturating_add(step.saturating_mul(class % CLASSES_PER_DOUBLING))
}

/// The smallest class that holds `bytes`; `None` past the largest.
pub fn class_for(bytes: usize) -> Option<usize> {
  (0..CLASSES).find(|class| class_bytes(*class) >= bytes)
}

/// The bytes of the smallest class that holds `bytes`, or of the largest class when none does: the size a block of
/// that many live bytes (header included) is given.
pub fn fitted_size(bytes: usize) -> usize {
  class_for(bytes).map_or(LARGEST_CLASS, class_bytes)
}

/// The class whose blocks are exactly `len` bytes long.
fn class_of_len(len: usize) -> Option<usize> {
  class_for(len).filter(|class| class_bytes(*class) == len)
}

/// The slots a page of class `class` has.
pub fn slots_per_page(class: usize) -> usize {
  PAGE_BYTES.checked_div(class_bytes(class)).unwrap_or(0)
}

/// A page of one class's slots: the slot bytes, each slot's generation, and the links of the two lists it is on — its
/// free slots, threaded through the vacant slots' first bytes, and its class's pages with a free slot.
#[derive(Debug)]
pub struct Page {
  /// The class of every slot.
  class: u8,
  /// Slots holding a block.
  live: u16,
  /// Slots issued at least once; those past it have never held a block and are not on the free list.
  fresh: u16,
  /// Slots retired at the generation limit.
  retired: u16,
  /// The first vacant slot, or [`NO_SLOT`].
  free_head: u16,
  /// Whether the page is on its class's list.
  listed: bool,
  /// The previous page on the class's list, by slab index.
  prev: u32,
  /// The next page on the class's list, by slab index.
  next: u32,
  /// Each slot's generation ([`RETIRED`] once it retires).
  generations: [u32; SLOTS_MAX],
  /// The slots, front to back.
  bytes: [u8; PAGE_BYTES],
}

impl Page {
  fn new(class: u8) -> Self {
    Self {
      class,
      live: 0,
      fresh: 0,
      retired: 0,
      free_head: NO_SLOT,
      listed: false,
      prev: NO_PAGE,
      next: NO_PAGE,
      generations: [0; SLOTS_MAX],
      bytes: [0; PAGE_BYTES],
    }
  }

  fn class_bytes(&self) -> usize {
    class_bytes(usize::from(self.class))
  }

  fn slots(&self) -> usize {
    slots_per_page(usize::from(self.class))
  }

  /// Whether a slot can still be issued: a vacant one, or one never issued.
  fn has_free(&self) -> bool {
    self.free_head != NO_SLOT || usize::from(self.fresh) < self.slots()
  }

  /// The byte range of slot `slot`.
  fn range(&self, slot: u16) -> std::ops::Range<usize> {
    let start = usize::from(slot).saturating_mul(self.class_bytes());
    start..start.saturating_add(self.class_bytes())
  }

  /// The free-list link a vacant slot keeps at its front.
  fn link(&self, slot: u16) -> u16 {
    self
      .bytes
      .get(self.range(slot))
      .and_then(<[u8]>::first_chunk::<LINK_BYTES>)
      .map_or(NO_SLOT, |link| u16::from_le_bytes(*link))
  }

  fn set_link(&mut self, slot: u16, next: u16) {
    let range = self.range(slot);
    if let Some(link) = self
      .bytes
      .get_mut(range)
      .and_then(<[u8]>::first_chunk_mut::<LINK_BYTES>)
    {
      *link = next.to_le_bytes();
    }
  }

  /// Takes a slot (the caller checked [`Page::has_free`]): a vacant one first, else the next never issued.
  fn take(&mut self) -> Option<u16> {
    let slot = if self.free_head != NO_SLOT {
      let slot = self.free_head;
      self.free_head = self.link(slot);
      slot
    } else if usize::from(self.fresh) < self.slots() {
      let slot = self.fresh;
      self.fresh = self.fresh.saturating_add(1);
      slot
    } else {
      return None;
    };
    self.live = self.live.saturating_add(1);
    Some(slot)
  }

  /// Whether `slot` at `generation` names a live block of this page.
  fn holds(&self, slot: u16, generation: u32) -> bool {
    slot < self.fresh
      && self
        .generations
        .get(usize::from(slot))
        .is_some_and(|current| *current == generation)
  }

  /// Vacates a live slot: its generation moves on, or it retires at the limit.
  fn vacate(&mut self, slot: u16) {
    let Some(generation) = self.generations.get_mut(usize::from(slot)) else {
      return;
    };
    match generation
      .checked_add(1)
      .filter(|next| *next <= GENERATION_LIMIT)
    {
      Some(next) => {
        *generation = next;
        let head = self.free_head;
        self.set_link(slot, head);
        self.free_head = slot;
      }
      None => {
        *generation = RETIRED;
        self.retired = self.retired.saturating_add(1);
      }
    }
    self.live = self.live.saturating_sub(1);
  }
}

/// A block's name: its page, its slot there and the slot's generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockRef {
  page: Handle<Page>,
  slot: u8,
  generation: u32,
}

impl BlockRef {
  /// The reference as the two words an index entry keeps: the page handle (index above generation) and the slot with
  /// its generation above it.
  pub fn words(self) -> (u64, u32) {
    let page =
      (u64::from(self.page.index()) << HANDLE_INDEX_SHIFT) | u64::from(self.page.generation());
    (page, u32::from(self.slot) | (self.generation << SLOT_BITS))
  }

  /// The reference two words name ([`BlockRef::words`]).
  pub fn from_words(page: u64, aux: u32) -> Self {
    Self {
      page: Handle::from_raw(
        u32::try_from(page >> HANDLE_INDEX_SHIFT).unwrap_or(0),
        u32::try_from(page & HANDLE_GENERATION_MASK).unwrap_or(0),
      ),
      slot: u8::try_from(aux & BYTE_MASK).unwrap_or(0),
      generation: aux >> SLOT_BITS,
    }
  }

  fn stale(self) -> MemError {
    MemError::StaleHandle {
      index: self.page.index(),
      generation: self.generation,
    }
  }
}

/// What the store holds of one class: its live blocks and its pages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClassCensus {
  /// Blocks of the class.
  pub blocks: usize,
  /// Pages of the class.
  pub pages: usize,
}

/// The directory blocks of a shard: one slab of pages, each page of one class.
#[derive(Debug)]
pub struct BlockStore {
  pages: Slab<Page>,
  /// Each class's first page with a free slot, by slab index, or [`NO_PAGE`].
  heads: [u32; CLASSES],
  /// Live blocks.
  live: usize,
  /// The most live blocks.
  max_blocks: usize,
}

impl BlockStore {
  /// A store whose page slab's segments hold `segment_pages` pages and whose live blocks are at most `max_blocks`.
  pub fn new(segment_pages: usize, max_blocks: usize) -> Self {
    Self {
      pages: Slab::new(segment_pages, Self::max_pages(max_blocks)),
      heads: [NO_PAGE; CLASSES],
      live: 0,
      max_blocks,
    }
  }

  /// Derived: the pages a store of `max_blocks` blocks may hold: the blocks' bytes at the smallest class (the median
  /// real directory's block), in whole pages, and a page for every class beside them, so a store holds a block of every
  /// class at once at any cap (`ceil(max_blocks × 256 / 65,536) + 17`).
  pub fn max_pages(max_blocks: usize) -> usize {
    max_blocks.div_ceil(SLOTS_MAX).saturating_add(CLASSES)
  }

  /// The bytes one block of the cap costs at its bound: a page slot's bytes over the smallest class's slots in it.
  pub fn unit_bytes() -> usize {
    Slab::<Page>::slot_bytes().div_ceil(SLOTS_MAX)
  }

  /// The bytes one page takes in its slab.
  pub fn page_slot_bytes() -> usize {
    Slab::<Page>::slot_bytes()
  }

  /// The most bytes the store can hold: its page bound times a page slot's bytes.
  pub const fn max_footprint_bytes(&self) -> usize {
    self.pages.max_footprint_bytes()
  }

  /// Live blocks.
  pub const fn len(&self) -> usize {
    self.live
  }

  /// Whether no block is live.
  pub const fn is_empty(&self) -> bool {
    self.live == 0
  }

  /// Pages held.
  pub const fn pages(&self) -> usize {
    self.pages.len()
  }

  /// The block cap.
  pub const fn max_slots(&self) -> usize {
    self.max_blocks
  }

  /// How many blocks of any classes can be inserted from now, at the least: under the block cap, and a page each
  /// (a block of a class with no page to spare takes a fresh one). It may refuse a few blocks early, never late.
  pub fn room(&self) -> usize {
    self
      .max_blocks
      .saturating_sub(self.live)
      .min(self.pages.room())
  }

  /// The refusal when [`BlockStore::room`] is short: the block cap, else the page slab's.
  pub fn refusal(&self) -> MemError {
    if self.live >= self.max_blocks {
      MemError::SlabFull {
        capacity: self.max_blocks,
      }
    } else {
      MemError::SlabFull {
        capacity: self.pages.max_slots(),
      }
    }
  }

  /// A new block holding `bytes`, whose length is a class's.
  pub fn insert(&mut self, bytes: &[u8]) -> Result<BlockRef, MemError> {
    let class = class_of_len(bytes.len()).ok_or(MemError::TooLarge {
      len: bytes.len(),
      max: LARGEST_CLASS,
    })?;
    if self.live >= self.max_blocks {
      return Err(self.refusal());
    }
    let page = self.page_with_room(class)?;
    let block = self.take_slot(page)?;
    let slot = self.slot_bytes_mut(block)?;
    slot.copy_from_slice(bytes);
    self.live = self.live.saturating_add(1);
    Ok(block)
  }

  /// A page of `class` with a free slot: the class list's head, else a fresh page put on the list.
  fn page_with_room(&mut self, class: usize) -> Result<Handle<Page>, MemError> {
    if let Some(page) = self.handle_at(self.heads.get(class).copied().unwrap_or(NO_PAGE)) {
      return Ok(page);
    }
    let page = self
      .pages
      .insert(Page::new(u8::try_from(class).unwrap_or(0)))?;
    self.link(page)?;
    Ok(page)
  }

  /// Takes a slot of `page`, unlinking the page from its list if that was its last free slot.
  fn take_slot(&mut self, page: Handle<Page>) -> Result<BlockRef, MemError> {
    let held = self.pages.get_mut(page)?;
    let slot = held.take().ok_or(MemError::SlabFull {
      capacity: held.slots(),
    })?;
    let generation = held
      .generations
      .get(usize::from(slot))
      .copied()
      .unwrap_or(RETIRED);
    let full = !held.has_free();
    if full {
      self.unlink(page)?;
    }
    Ok(BlockRef {
      page,
      slot: u8::try_from(slot).unwrap_or(0),
      generation,
    })
  }

  /// The handle of the page at slab index `index`, when one lives there.
  fn handle_at(&self, index: u32) -> Option<Handle<Page>> {
    if index == NO_PAGE {
      return None;
    }
    let handle = Handle::from_raw(index, self.pages.generation_at(index)?);
    self.pages.contains(handle).then_some(handle)
  }

  /// Puts `page` at the head of its class's list.
  fn link(&mut self, page: Handle<Page>) -> Result<(), MemError> {
    let class = usize::from(self.pages.get(page)?.class);
    let old_head = self.heads.get(class).copied().unwrap_or(NO_PAGE);
    if let Some(next) = self.handle_at(old_head) {
      self.pages.get_mut(next)?.prev = page.index();
    }
    let held = self.pages.get_mut(page)?;
    held.prev = NO_PAGE;
    held.next = old_head;
    held.listed = true;
    if let Some(head) = self.heads.get_mut(class) {
      *head = page.index();
    }
    Ok(())
  }

  /// Takes `page` off its class's list.
  fn unlink(&mut self, page: Handle<Page>) -> Result<(), MemError> {
    let held = self.pages.get_mut(page)?;
    let (class, prev, next) = (usize::from(held.class), held.prev, held.next);
    held.listed = false;
    held.prev = NO_PAGE;
    held.next = NO_PAGE;
    match self.handle_at(prev) {
      Some(before) => self.pages.get_mut(before)?.next = next,
      None => {
        if let Some(head) = self.heads.get_mut(class) {
          *head = next;
        }
      }
    }
    if let Some(after) = self.handle_at(next) {
      self.pages.get_mut(after)?.prev = prev;
    }
    Ok(())
  }

  /// The bytes of a live block.
  pub fn get(&self, block: BlockRef) -> Result<&[u8], MemError> {
    let page = self.pages.get(block.page).map_err(|_| block.stale())?;
    let slot = u16::from(block.slot);
    if !page.holds(slot, block.generation) {
      return Err(block.stale());
    }
    page.bytes.get(page.range(slot)).ok_or(block.stale())
  }

  /// The bytes of a live block, mutably.
  pub fn get_mut(&mut self, block: BlockRef) -> Result<&mut [u8], MemError> {
    self.slot_bytes_mut(block)
  }

  fn slot_bytes_mut(&mut self, block: BlockRef) -> Result<&mut [u8], MemError> {
    let page = self.pages.get_mut(block.page).map_err(|_| block.stale())?;
    let slot = u16::from(block.slot);
    if !page.holds(slot, block.generation) {
      return Err(block.stale());
    }
    let range = page.range(slot);
    page.bytes.get_mut(range).ok_or(block.stale())
  }

  /// Frees a live block. Its page goes back on its class's list when this was the page's first free slot, and back to
  /// the page slab, for any class, when it was the page's last block.
  pub fn discard(&mut self, block: BlockRef) -> Result<(), MemError> {
    self.get(block)?;
    let held = self.pages.get_mut(block.page)?;
    let was_full = !held.has_free();
    held.vacate(u16::from(block.slot));
    let (empty, has_free, listed) = (held.live == 0, held.has_free(), held.listed);
    self.live = self.live.saturating_sub(1);
    if empty {
      if listed {
        self.unlink(block.page)?;
      }
      return self.pages.discard(block.page);
    }
    if was_full && has_free {
      self.link(block.page)?;
    }
    Ok(())
  }

  /// The live blocks and pages of every class, for the oracles.
  pub fn census(&self) -> [ClassCensus; CLASSES] {
    let mut census = [ClassCensus::default(); CLASSES];
    for (_, page) in self.pages.iter() {
      if let Some(class) = census.get_mut(usize::from(page.class)) {
        class.blocks = class.blocks.saturating_add(usize::from(page.live));
        class.pages = class.pages.saturating_add(1);
      }
    }
    census
  }

  /// Whether every class's list holds exactly its pages with a free slot, each once, linked both ways: the store's own
  /// invariant, for the oracles.
  pub fn lists_are_sound(&self) -> bool {
    let mut seen = 0usize;
    for class in 0..CLASSES {
      let Some(count) = self.walk_list(class) else {
        return false;
      };
      seen = seen.saturating_add(count);
    }
    let expected = self
      .pages
      .iter()
      .filter(|(_, page)| page.has_free() && page.live > 0)
      .count();
    seen == expected
  }

  /// The pages on `class`'s list, when every one is of the class, has a free slot and links back to the one before.
  fn walk_list(&self, class: usize) -> Option<usize> {
    let mut count = 0usize;
    let mut prev = NO_PAGE;
    let mut at = self.heads.get(class).copied()?;
    while let Some(page) = self.handle_at(at) {
      let held = self.pages.get(page).ok()?;
      let sound =
        usize::from(held.class) == class && held.has_free() && held.listed && held.prev == prev;
      if !sound || count > self.pages.len() {
        return None;
      }
      count = count.saturating_add(1);
      prev = at;
      at = held.next;
    }
    (at == NO_PAGE).then_some(count)
  }
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeMap;

  use proptest::prelude::*;

  use super::*;

  /// Derived from the design's words: the classes ascend from 256 to 4,096, four to a doubling, each a quarter of its
  /// doubling's base above the one before.
  #[test]
  fn the_classes_run_from_256_to_4096_four_to_a_doubling() {
    assert_eq!(
      (0..CLASSES).map(class_bytes).collect::<Vec<_>>(),
      vec![
        256, 320, 384, 448, 512, 640, 768, 896, 1024, 1280, 1536, 1792, 2048, 2560, 3072, 3584,
        4096
      ]
    );
    assert_eq!(class_for(2644).map(class_bytes), Some(3072));
    assert_eq!(class_for(1).map(class_bytes), Some(256));
    assert_eq!(class_for(4097), None);
  }

  /// One operation of a generated history.
  #[derive(Clone, Debug)]
  enum Op {
    /// Insert a block of the class, filled with the byte.
    Insert(usize, u8),
    /// Discard the live block at this position of the model's order (modulo its length).
    Discard(usize),
    /// Discard a block already discarded, by position among those (modulo).
    Stale(usize),
    /// Rewrite the live block at this position with the byte.
    Rewrite(usize, u8),
  }

  #[allow(clippy::disallowed_types)] // proptest's `prop_oneof!` holds its arms in an `Arc` (D-8's test-harness exception).
  fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
      4 => (0..CLASSES, any::<u8>()).prop_map(|(c, b)| Op::Insert(c, b)),
      3 => any::<usize>().prop_map(Op::Discard),
      1 => any::<usize>().prop_map(Op::Stale),
      1 => (any::<usize>(), any::<u8>()).prop_map(|(at, b)| Op::Rewrite(at, b)),
    ]
  }

  /// The model: every live block's class and fill, and every discarded reference.
  #[derive(Default)]
  struct Model {
    live: BTreeMap<(u64, u32), (BlockRef, usize, u8)>,
    dead: Vec<BlockRef>,
  }

  fn nth(model: &Model, at: usize) -> Option<BlockRef> {
    let len = model.live.len();
    (len > 0).then(|| model.live.values().nth(at % len).unwrap().0)
  }

  fn insert(store: &mut BlockStore, model: &mut Model, class: usize, fill: u8) {
    let bytes = vec![fill; class_bytes(class)];
    let room = store.room();
    match store.insert(&bytes) {
      Ok(block) => {
        let earlier = model.live.insert(block.words(), (block, class, fill));
        assert!(earlier.is_none(), "a live reference issued twice");
      }
      Err(refusal) => {
        assert!(matches!(refusal, MemError::SlabFull { .. }), "{refusal:?}");
        // `room` is a floor: an insert it promised may never be refused.
        assert_eq!(room, 0, "refused with room {room}");
      }
    }
  }

  fn apply(store: &mut BlockStore, model: &mut Model, op: &Op) {
    match *op {
      Op::Insert(class, fill) => insert(store, model, class, fill),
      Op::Discard(at) => {
        if let Some(block) = nth(model, at) {
          store.discard(block).unwrap();
          model.live.remove(&block.words());
          model.dead.push(block);
        }
      }
      Op::Stale(at) => {
        if let Some(block) = model.dead.get(at % model.dead.len().max(1)) {
          assert!(store.get(*block).is_err(), "a freed block still resolves");
          assert!(
            store.discard(*block).is_err(),
            "a freed block discards twice"
          );
        }
      }
      Op::Rewrite(at, fill) => {
        if let Some(block) = nth(model, at) {
          store.get_mut(block).unwrap().fill(fill);
          model.live.get_mut(&block.words()).unwrap().2 = fill;
        }
      }
    }
  }

  fn check(store: &BlockStore, model: &Model) {
    assert_eq!(store.len(), model.live.len());
    for (block, class, fill) in model.live.values() {
      let bytes = store.get(*block).unwrap();
      assert_eq!(bytes.len(), class_bytes(*class));
      assert!(
        bytes.iter().all(|b| b == fill),
        "a block's bytes were overwritten"
      );
    }
    let census = store.census();
    for (class, counted) in census.iter().enumerate() {
      let expected = model.live.values().filter(|(_, c, _)| *c == class).count();
      assert_eq!(counted.blocks, expected, "class {class}'s blocks");
      // An emptied page goes back to the slab, so every page held holds a block.
      assert!(
        counted.pages <= counted.blocks,
        "class {class}: an empty page kept"
      );
    }
    assert!(
      store.lists_are_sound(),
      "the class lists drifted from the pages"
    );
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(200), .. ProptestConfig::default() }))]

    /// A-203. Do: run generated histories of inserts of every class, discards, stale uses and rewrites against a map
    /// model, on a store capped at 300 blocks. Expect: every live block reads back its class's length and its own
    /// bytes after every step, a freed reference is refused by `get` and `discard`, the census counts the model's
    /// blocks per class, no page is held empty, and the class lists hold exactly the pages with a free slot.
    #[test]
    fn the_store_equals_its_model_on_every_history(ops in prop::collection::vec(op(), 1..400)) {
      let mut store = BlockStore::new(4, 300);
      let mut model = Model::default();
      for op in &ops {
        apply(&mut store, &mut model, op);
        check(&store, &model);
      }
    }
  }

  /// A-203. Do: fill a page of 256-byte blocks and part of a second, free every block of the first, then insert
  /// 4,096-byte blocks. Expect: the emptied page went back to the slab (two pages, then one, then the 4 KiB class's
  /// page), so one class's freed memory serves another.
  #[test]
  fn an_emptied_page_serves_another_class() {
    let mut store = BlockStore::new(4, 1 << 12);
    let small = vec![1u8; class_bytes(0)];
    let first: Vec<BlockRef> = (0..slots_per_page(0))
      .map(|_| store.insert(&small).unwrap())
      .collect();
    let second = store.insert(&small).unwrap();
    assert_eq!(store.pages(), 2);
    for block in first {
      store.discard(block).unwrap();
    }
    assert_eq!(store.pages(), 1, "the emptied page went back");
    let large = vec![2u8; LARGEST_CLASS];
    let big = store.insert(&large).unwrap();
    assert_eq!(store.pages(), 2);
    assert_eq!(store.get(second).unwrap(), small.as_slice());
    assert_eq!(store.get(big).unwrap(), large.as_slice());
  }

  /// A-203. Do: insert and free one block 2^24 + 1 times on a page kept alive by a neighbour. Expect: the slot retires
  /// at the generation limit rather than wrapping, so the first reference never resolves again, and later inserts take
  /// another slot.
  #[test]
  #[ignore = "2^24 rounds: run with --ignored"]
  fn a_slot_retires_at_its_generation_limit() {
    let mut store = BlockStore::new(4, 8);
    let bytes = vec![0u8; LARGEST_CLASS];
    let keeper = store.insert(&bytes).unwrap();
    let first = store.insert(&bytes).unwrap();
    store.discard(first).unwrap();
    for _ in 0..GENERATION_LIMIT {
      let again = store.insert(&bytes).unwrap();
      store.discard(again).unwrap();
    }
    assert!(store.get(first).is_err());
    assert!(store.get(keeper).is_ok());
  }
}
