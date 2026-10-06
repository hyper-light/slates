//! A binary buddy allocator over one region: blocks are power-of-two multiples of the base
//! granule; allocation splits the smallest sufficient free block, freeing coalesces with the
//! buddy while the buddy is free (§4.2; [A: Knowlton, "A fast storage allocator", CACM 1965;
//! A: Knuth, TAOCP vol. 1 §2.5]).
//!
//! The allocator's own state lives beside the region, not inside it, so it never touches
//! chunk pages that may be cold or not yet faulted: one byte of state per granule (free bit and
//! order), an intrusive doubly-linked free list per order over granule indices, and one allocation
//! incarnation per granule. Split and coalesce are O(log region); allocation is O(log region) in the
//! worst case and O(1) when a block of the right order is free.
//!
//! A [`Block`] is unforgeable outside this crate (its fields are private) and carries the incarnation
//! of the allocation that produced it. A free is accepted only when it names exactly a live block: its
//! offset on a granule and on its own size's boundary, its length exactly one block's, a head allocated
//! at that order, and the head's current incarnation. Anything else is a typed
//! [`MemError::ForeignExtent`] with every total unchanged (AUD-29-10: a free of offset 1 / length 4,095
//! was accepted, credited 4,095 bytes, and coalesced a block that was still in use). An incarnation is
//! checked, never wrapped: a head whose incarnations are spent refuses with
//! [`MemError::GenerationExhausted`] (AUD-29-11), so a stale copy of a block can never name its reuse.
//!
//! Recovery images name blocks (A-64), so the allocator also keeps three bits per granule, all sized when
//! it is built: which heads are live, which heads the committed image may name, and which of those were
//! freed since and wait. [`Buddy::free_or_defer`] defers a free of a committed head; [`Buddy::capture`]
//! records the live, undeferred heads as the set a publication may name; [`Buddy::commit_capture`] makes
//! that set the committed one and frees every deferred block, none of which the new image can name (each
//! was freed before the capture). [`Buddy::claim`] rebuilds the allocation of exactly the blocks a
//! recovered image names.

use crate::error::{ExtentRefusal, MemError};

// Every sentinel of the per-granule arrays is zero, so the arrays are zero-allocated and the operating system
// backs a page only when a block on it is first split, freed or linked (AC-0.4, A-69): at a 4 KiB granule the
// arrays are 17 bytes per granule, which eager non-zero sentinels made resident whole (measured 2026-10-04: an
// empty four-shard daemon at 99.6 MB RSS against 41 MB before the granule shrank).

/// Format: state byte layout: bit 7 = free, bit 6 = head, bits 0..5 = order of the block whose head this is.
const FREE_BIT: u8 = 0x80;
/// Format: set on every head, so a head of order zero is never the zero byte that means "inside".
const HEAD_BIT: u8 = 0x40;
/// Format: the order mask.
const ORDER_MASK: u8 = 0x3F;
/// Format: a granule that is inside a block, not its head.
const INSIDE: u8 = 0;

/// The sentinel for "no link" in the free lists, as the accessors return it. A link is stored as its index plus
/// one, so the stored "no link" is zero; an index is below `1 << 31`, so the shift never wraps a real one.
const NONE: u32 = u32::MAX;

/// A buddy allocator over `granules << max_order` bytes of address space, addressed by offset.
#[derive(Debug)]
pub struct Buddy {
  granule: usize,
  /// `granule` is `1 << granule_shift`: every offset/granule conversion is a shift and every alignment
  /// test a mask, never a division (measured 2026-09-30: the validated free's two divisions cost the split
  /// path about 6 ns of 42).
  granule_shift: u32,
  max_order: u32,
  state: Vec<u8>,
  next: Vec<u32>,
  prev: Vec<u32>,
  heads: Vec<u32>,
  /// Per granule: the incarnation of the last allocation headed there.
  incarnations: Vec<u64>,
  free_bytes: usize,
  /// The heads of live blocks, deferred ones included: kept from the first capture on (built then from `state`),
  /// so an allocator nothing publishes from maintains no bitmap.
  live: Bits,
  /// The heads the committed image may name (A-64): freed only after a newer image commits.
  committed: Bits,
  /// Committed heads freed since: still allocated, released by the next commit.
  deferred: Bits,
  /// The heads a publication in progress may name.
  capture: Bits,
  /// Whether a capture is open (taken, neither committed nor abandoned).
  capturing: bool,
  /// Whether any image may name a block: set by the first capture and kept. Until then neither an allocation nor
  /// a free touches a bitmap (an allocator nothing publishes from pays one flag test for the deferral).
  imaged: bool,
  deferred_bytes: usize,
  /// The heads of blocks locked in RAM (§4.2 D-12: a strict volume's content): set by [`Buddy::lock_block`], cleared
  /// when the block is released, whichever way, so the owner unlocks exactly the blocks that were locked.
  locked: Bits,
  /// The bytes of the locked blocks.
  locked_bytes: usize,
}

/// One bit per granule, sized once (AC-0.4: nothing reaches the system allocator after the build).
#[derive(Debug)]
struct Bits {
  words: Vec<u64>,
}

impl Bits {
  /// Format: bits in one word.
  const WORD_BITS: usize = u64::BITS as usize;

  fn new(bits: usize) -> Bits {
    Bits {
      words: vec![0; bits.div_ceil(Self::WORD_BITS)],
    }
  }

  fn word_and_mask(index: u32) -> Option<(usize, u64)> {
    let index = usize::try_from(index).ok()?;
    Some((index / Self::WORD_BITS, 1u64 << (index % Self::WORD_BITS)))
  }

  /// The granule index of bit `bit` of word `word`.
  fn index_of(word: usize, bit: u32) -> Option<u32> {
    word
      .checked_mul(Self::WORD_BITS)
      .and_then(|base| base.checked_add(usize::try_from(bit).ok()?))
      .and_then(|index| u32::try_from(index).ok())
  }

  fn get(&self, index: u32) -> bool {
    Self::word_and_mask(index)
      .and_then(|(word, mask)| self.words.get(word).map(|w| w & mask != 0))
      .unwrap_or(false)
  }

  fn set(&mut self, index: u32, on: bool) {
    if let Some((word, mask)) = Self::word_and_mask(index)
      && let Some(w) = self.words.get_mut(word)
    {
      if on {
        *w |= mask;
      } else {
        *w &= !mask;
      }
    }
  }
}

/// A block handed out by the allocator: an offset and a length in bytes, both granule multiples, and the
/// incarnation of the allocation. Only this crate constructs one.
///
/// ```compile_fail,E0451
/// let forged = slates_mem::buddy::Block { offset: 1, len: 4095, incarnation: 0 };
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Block {
  offset: usize,
  len: usize,
  incarnation: u64,
}

impl Block {
  /// Byte offset within the region.
  pub const fn offset(&self) -> usize {
    self.offset
  }

  /// Length in bytes (a power-of-two multiple of the granule).
  pub const fn len(&self) -> usize {
    self.len
  }

  /// Whether the block is empty (never: a block holds at least one granule).
  pub const fn is_empty(&self) -> bool {
    self.len == 0
  }

  /// The incarnation of the allocation that produced it.
  pub const fn incarnation(&self) -> u64 {
    self.incarnation
  }

  /// A block with the given fields, for the refusal tests (nothing outside a test can name one).
  #[cfg(test)]
  pub(crate) const fn named(offset: usize, len: usize, incarnation: u64) -> Block {
    Block {
      offset,
      len,
      incarnation,
    }
  }
}

impl Buddy {
  /// An allocator for a region of `1 << max_order` granules of `granule` bytes each; the whole
  /// region starts free as one block. The granule is a page size, so a power of two; anything else is
  /// refused ([`MemError::BadCapacity`]). A `max_order` past the address space or the state byte's order
  /// field is clamped to the largest both hold.
  pub fn new(granule: usize, max_order: u32) -> Result<Self, MemError> {
    if !granule.is_power_of_two() {
      return Err(MemError::BadCapacity { capacity: granule });
    }
    let granule_shift = granule.trailing_zeros();
    let max_order = max_order
      .min(usize::BITS.saturating_sub(1))
      .min(u32::from(ORDER_MASK))
      .min(u32::BITS.saturating_sub(1));
    let granules = 1usize << max_order;
    let mut state = vec![INSIDE; granules];
    if let Some(first) = state.first_mut() {
      *first = FREE_BIT | HEAD_BIT | u8::try_from(max_order).unwrap_or(ORDER_MASK);
    }
    let orders = usize::try_from(max_order).unwrap_or(0).saturating_add(1);
    let mut heads = vec![NONE; orders];
    if let Some(top) = heads.last_mut() {
      *top = 0;
    }
    Ok(Self {
      granule,
      granule_shift,
      max_order,
      state,
      next: vec![0; granules],
      prev: vec![0; granules],
      heads,
      incarnations: vec![0; granules],
      free_bytes: granules.saturating_mul(granule),
      live: Bits::new(granules),
      committed: Bits::new(granules),
      deferred: Bits::new(granules),
      capture: Bits::new(granules),
      capturing: false,
      imaged: false,
      deferred_bytes: 0,
      locked: Bits::new(granules),
      locked_bytes: 0,
    })
  }

  /// Every allocated block, deferred ones included, as `(offset, len)`, in address order: a walk of the heads that
  /// jumps each block whole, so it costs the blocks and free blocks, not the granules inside them. A cold path (a
  /// locked arena takes it once, when a strict volume first lives on its shard).
  pub fn allocated(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
    let granules = self.state.len();
    let mut at = 0usize;
    std::iter::from_fn(move || {
      while at < granules {
        let here = at;
        let state = self.state.get(here).copied().unwrap_or(INSIDE);
        if state & HEAD_BIT == 0 {
          at = here.saturating_add(1);
          continue;
        }
        let span = 1usize << u32::from(state & ORDER_MASK);
        at = here.saturating_add(span);
        if state & FREE_BIT == 0 {
          return Some((here << self.granule_shift, span << self.granule_shift));
        }
      }
      None
    })
  }

  /// Every free block at or after byte `from`, as `(offset, len)`, in address order: the same head walk as
  /// [`Buddy::allocated`], keeping the free heads instead (A-105: what an idle purge gives back to the OS). A cold path.
  pub fn free_blocks(&self, from: usize) -> impl Iterator<Item = (usize, usize)> + '_ {
    let granules = self.state.len();
    let mut at = from >> self.granule_shift;
    std::iter::from_fn(move || {
      while at < granules {
        let here = at;
        let state = self.state.get(here).copied().unwrap_or(INSIDE);
        if state & HEAD_BIT == 0 {
          at = here.saturating_add(1);
          continue;
        }
        let span = 1usize << u32::from(state & ORDER_MASK);
        at = here.saturating_add(span);
        if state & FREE_BIT != 0 {
          return Some((here << self.granule_shift, span << self.granule_shift));
        }
      }
      None
    })
  }

  /// Records that the live `block` is locked in RAM (the owner locked its pages). Idempotent; a block that names no
  /// live block is refused as a free would refuse it.
  pub fn lock_block(&mut self, block: Block) -> Result<(), MemError> {
    let (index, order) = self
      .validate(block)
      .map_err(|reason| MemError::ForeignExtent {
        offset: block.offset,
        len: block.len,
        reason,
      })?;
    if !self.locked.get(index) {
      self.locked.set(index, true);
      self.locked_bytes = self.locked_bytes.saturating_add(self.order_bytes(order));
    }
    Ok(())
  }

  /// Whether the live `block` is locked; false for one that names no live block.
  pub fn is_locked(&self, block: Block) -> bool {
    self
      .validate(block)
      .is_ok_and(|(index, _)| self.locked.get(index))
  }

  /// The bytes of the locked blocks.
  pub const fn locked_bytes(&self) -> usize {
    self.locked_bytes
  }

  /// Clears the lock bit of the head at `index` (a block being released), returning whether it was set.
  fn unmark_locked(&mut self, index: u32, order: u32) -> bool {
    if !self.locked.get(index) {
      return false;
    }
    self.locked.set(index, false);
    self.locked_bytes = self.locked_bytes.saturating_sub(self.order_bytes(order));
    true
  }

  /// Bytes per granule.
  pub const fn granule(&self) -> usize {
    self.granule
  }

  /// The region's size in bytes.
  pub fn region_bytes(&self) -> usize {
    (1usize << self.max_order).saturating_mul(self.granule)
  }

  /// Free bytes (fragmented or not).
  pub const fn free_bytes(&self) -> usize {
    self.free_bytes
  }

  /// The bytes of a block of `order`.
  fn order_bytes(&self, order: u32) -> usize {
    order
      .checked_add(self.granule_shift)
      .and_then(|shift| 1usize.checked_shl(shift))
      .unwrap_or(usize::MAX)
  }

  /// The free list head of `order`, or [`NONE`].
  fn head_of(&self, order: u32) -> u32 {
    usize::try_from(order)
      .ok()
      .and_then(|o| self.heads.get(o))
      .copied()
      .unwrap_or(NONE)
  }

  /// The largest block that can be allocated right now, in bytes (0 when nothing is free).
  pub fn largest_free(&self) -> usize {
    // A plain countdown: an inclusive range's iterator carries an exhaustion check per step (measured in the
    // instruction-count gate).
    let mut order = self.max_order;
    loop {
      if self.head_of(order) != NONE {
        return self.order_bytes(order);
      }
      if order == 0 {
        return 0;
      }
      order = order.saturating_sub(1);
    }
  }

  /// The order (block size exponent in granules) that fits `len` bytes.
  pub fn order_for(&self, len: usize) -> Result<u32, MemError> {
    let order = len
      .max(1)
      .checked_add(self.granule.saturating_sub(1))
      .map(|rounded| rounded >> self.granule_shift)
      .and_then(usize::checked_next_power_of_two)
      .map(usize::trailing_zeros)
      .filter(|order| *order <= self.max_order);
    order.ok_or_else(|| MemError::TooLarge {
      len,
      max: self.region_bytes(),
    })
  }

  /// Allocates a block of at least `len` bytes.
  pub fn alloc(&mut self, len: usize) -> Result<Block, MemError> {
    let order = self.order_for(len)?;
    let mut found = order;
    while self.head_of(found) == NONE {
      if found >= self.max_order {
        return Err(MemError::ArenaExhausted {
          requested: len,
          largest_free: self.largest_free(),
        });
      }
      found = found.saturating_add(1);
    }
    let index = self.head_of(found);
    // The incarnation is taken before anything changes, so a spent head refuses with the allocator intact.
    let incarnation = self
      .incarnation_at(index)
      .checked_add(1)
      .ok_or(MemError::GenerationExhausted { index })?;
    self.unlink(index, found);
    let mut current = found;
    while current > order {
      current = current.saturating_sub(1);
      let buddy = index.saturating_add(1u32 << current);
      self.mark(buddy, current, true);
      self.link(buddy, current);
    }
    Ok(self.take(index, order, incarnation))
  }

  /// Marks the block of `order` at `index` allocated under `incarnation` and returns it.
  #[inline]
  fn take(&mut self, index: u32, order: u32, incarnation: u64) -> Block {
    self.mark(index, order, false);
    if let Some(slot) = self.slot_mut(index) {
      *slot = incarnation;
    }
    if self.imaged {
      self.live.set(index, true);
    }
    let block_len = self.order_bytes(order);
    self.free_bytes = self.free_bytes.saturating_sub(block_len);
    Block {
      offset: usize::try_from(index).unwrap_or(0) << self.granule_shift,
      len: block_len,
      incarnation,
    }
  }

  /// Allocates exactly the block of `len` bytes at `offset` (A-64: rebuilding the allocation a recovered
  /// image names). The block must be one this allocator could hand out (offset on a granule and on its
  /// size's boundary, length a power-of-two number of granules, inside the region) and lie wholly in one
  /// free block, which is split down to it. Anything else is refused [`MemError::ForeignExtent`] with
  /// nothing changed: `Claimed` when any of the span is already allocated.
  pub fn claim(&mut self, offset: usize, len: usize) -> Result<Block, MemError> {
    let refused = |reason| MemError::ForeignExtent {
      offset,
      len,
      reason,
    };
    if offset & self.granule.saturating_sub(1) != 0 {
      return Err(refused(ExtentRefusal::Misaligned));
    }
    let order = self
      .order_for(len)
      .map_err(|_| refused(ExtentRefusal::WrongLength))?;
    if self.order_bytes(order) != len {
      return Err(refused(ExtentRefusal::WrongLength));
    }
    let index = u32::try_from(offset >> self.granule_shift)
      .ok()
      .filter(|index| usize::try_from(*index).is_ok_and(|i| i < self.state.len()))
      .ok_or(refused(ExtentRefusal::OutOfRange))?;
    if index & ((1u32 << order).saturating_sub(1)) != 0 {
      return Err(refused(ExtentRefusal::Misaligned));
    }
    let (head, head_order) = self
      .covering_free(index, order)
      .ok_or(refused(ExtentRefusal::Claimed))?;
    let incarnation = self
      .incarnation_at(index)
      .checked_add(1)
      .ok_or(MemError::GenerationExhausted { index })?;
    self.unlink(head, head_order);
    let (mut at, mut current) = (head, head_order);
    while current > order {
      current = current.saturating_sub(1);
      let half = 1u32 << current;
      let upper = at.saturating_add(half);
      let (kept, freed) = if index >= upper {
        (upper, at)
      } else {
        (at, upper)
      };
      self.mark(freed, current, true);
      self.link(freed, current);
      at = kept;
    }
    Ok(self.take(index, order, incarnation))
  }

  /// The free block, as (head, order), that wholly holds the `order` block at `index`; `None` when any of
  /// that span is allocated. A head covering `index` is found at its own alignment, so climbing the orders
  /// from `order` meets it; buddies of a free block are never both free, so a free block smaller than the
  /// span means part of the span is allocated.
  fn covering_free(&self, index: u32, order: u32) -> Option<(u32, u32)> {
    for level in order..=self.max_order {
      let head = index & !((1u32 << level).saturating_sub(1));
      let byte = self.state_at(head);
      if byte == INSIDE {
        continue;
      }
      let head_order = u32::from(byte & ORDER_MASK);
      let end = u64::from(head).saturating_add(1u64 << head_order);
      if end <= u64::from(index) {
        continue;
      }
      return (byte & FREE_BIT != 0 && head_order >= order).then_some((head, head_order));
    }
    None
  }

  /// Frees `block`, or, when the committed image may name it, defers the free to the next
  /// [`Buddy::commit_capture`] (A-64). Returns whether it was deferred. Validated as [`Buddy::free`] is.
  pub fn free_or_defer(&mut self, block: Block) -> Result<bool, MemError> {
    let (index, order) = self
      .validate(block)
      .map_err(|reason| MemError::ForeignExtent {
        offset: block.offset,
        len: block.len,
        reason,
      })?;
    if !self.named_by_an_image(index) {
      // Released now: the owner asked `is_locked` first and unlocks the pages itself.
      let _ = self.release(index, order);
      return Ok(false);
    }
    if self.deferred.get(index) {
      return Err(MemError::ForeignExtent {
        offset: block.offset,
        len: block.len,
        reason: ExtentRefusal::NotAllocated,
      });
    }
    self.deferred.set(index, true);
    self.deferred_bytes = self.deferred_bytes.saturating_add(self.order_bytes(order));
    Ok(true)
  }

  /// Whether the committed image, or the one being published, may name the block headed at `index`.
  fn named_by_an_image(&self, index: u32) -> bool {
    self.imaged && (self.committed.get(index) || (self.capturing && self.capture.get(index)))
  }

  /// [`Buddy::named_by_an_image`] once `imaged` is known to be set.
  fn named_once_imaged(&self, index: u32) -> bool {
    self.committed.get(index) || (self.capturing && self.capture.get(index))
  }

  /// How many blocks of `len` bytes could be allocated now, counted up to `cap`: each free block of at least that
  /// size holds a power of two of them. Walks the free lists of those orders until `cap` is reached, so the count
  /// costs at most `cap` steps.
  pub fn allocatable(&self, cap: usize, len: usize) -> usize {
    let Ok(order) = self.order_for(len) else {
      return 0;
    };
    let mut units = 0usize;
    for level in order..=self.max_order {
      let per = 1usize
        .checked_shl(level.saturating_sub(order))
        .unwrap_or(usize::MAX);
      let mut at = self.head_of(level);
      while at != NONE && units < cap {
        units = units.saturating_add(per);
        at = Self::link_at(&self.next, at);
      }
    }
    units.min(cap)
  }

  /// Whether `count` blocks of `len` bytes can be allocated now.
  pub fn can_allocate(&self, count: usize, len: usize) -> bool {
    self.allocatable(count, len) >= count
  }

  /// Whether a recovery image, the committed one or the one being published, may name `block` (A-64): a block whose
  /// bytes must keep the meaning the image gave them until the next commit. `false` for a block this pool did not issue.
  pub fn imaged(&self, block: Block) -> bool {
    self
      .validate(block)
      .is_ok_and(|(index, _)| self.named_by_an_image(index))
  }

  /// Whether `block` names exactly a live block whose free is not deferred.
  pub fn holds(&self, block: Block) -> bool {
    self
      .validate(block)
      .is_ok_and(|(index, _)| !self.deferred.get(index))
  }

  /// Bytes freed but deferred until the next commit: allocated, yet no live state names them.
  pub const fn deferred_bytes(&self) -> usize {
    self.deferred_bytes
  }

  /// Records the heads a publication starting now may name: every live block not deferred. Until the capture
  /// is committed or abandoned, a free of one of them is deferred too.
  pub fn capture(&mut self) {
    if !self.imaged {
      // The first capture: the live heads, read once from the state bytes, kept from now on.
      for (index, byte) in self.state.iter().enumerate() {
        if *byte != INSIDE
          && *byte & FREE_BIT == 0
          && let Ok(index) = u32::try_from(index)
        {
          self.live.set(index, true);
        }
      }
      self.imaged = true;
    }
    self.capturing = true;
    for ((capture, live), deferred) in self
      .capture
      .words
      .iter_mut()
      .zip(&self.live.words)
      .zip(&self.deferred.words)
    {
      *capture = live & !deferred;
    }
  }

  /// The publication of the last [`Buddy::capture`] committed: its heads become the committed set, and every
  /// deferred block the new image cannot name is freed. One freed while the capture was open stays deferred,
  /// since the new image may name it, and the next commit frees it.
  pub fn commit_capture(&mut self) {
    self.commit_capture_releasing(|_, _, _| {});
  }

  /// [`Buddy::commit_capture`], telling `released` the byte offset and length of every block it returns to the free
  /// lists, and whether it was locked, so the owner of the bytes can scrub them (A-99: a freed block keeps no
  /// plaintext) and unlock what was locked.
  pub fn commit_capture_releasing(&mut self, mut released: impl FnMut(usize, usize, bool)) {
    if !self.capturing {
      return;
    }
    self.capturing = false;
    let mut kept_bytes = 0usize;
    for word in 0..self.deferred.words.len() {
      let captured = self.capture.words.get(word).copied().unwrap_or(0);
      let deferred = self.deferred.words.get(word).copied().unwrap_or(0);
      if let Some(slot) = self.deferred.words.get_mut(word) {
        *slot = deferred & captured;
      }
      let mut bits = deferred & !captured;
      while bits != 0 {
        let bit = bits.trailing_zeros();
        bits &= bits.wrapping_sub(1);
        let index = Bits::index_of(word, bit);
        if let Some(index) = index {
          let order = u32::from(self.state_at(index) & ORDER_MASK);
          let locked = self.release(index, order);
          if let Some(offset) = usize::try_from(index)
            .ok()
            .and_then(|index| index.checked_shl(self.granule_shift))
          {
            released(offset, self.order_bytes(order), locked);
          }
        }
      }
      let mut kept = deferred & captured;
      while kept != 0 {
        let bit = kept.trailing_zeros();
        kept &= kept.wrapping_sub(1);
        let order =
          Bits::index_of(word, bit).map(|index| u32::from(self.state_at(index) & ORDER_MASK));
        if let Some(order) = order {
          kept_bytes = kept_bytes.saturating_add(self.order_bytes(order));
        }
      }
    }
    self.deferred_bytes = kept_bytes;
    std::mem::swap(&mut self.committed, &mut self.capture);
  }

  /// The publication of the last [`Buddy::capture`] did not commit: the committed set is unchanged, and the
  /// blocks freed while the capture was open stay deferred for the committed image's sake only if it names
  /// them; the others are freed now.
  pub fn abandon_capture(&mut self) {
    self.abandon_capture_releasing(|_, _, _| {});
  }

  /// [`Buddy::abandon_capture`], telling `released` the byte offset and length of every block it returns to the free
  /// lists (A-99, as [`Buddy::commit_capture_releasing`]).
  pub fn abandon_capture_releasing(&mut self, mut released: impl FnMut(usize, usize, bool)) {
    if !self.capturing {
      return;
    }
    self.capturing = false;
    for word in 0..self.deferred.words.len() {
      let committed = self.committed.words.get(word).copied().unwrap_or(0);
      let deferred = self.deferred.words.get(word).copied().unwrap_or(0);
      let mut bits = deferred & !committed;
      if let Some(slot) = self.deferred.words.get_mut(word) {
        *slot = deferred & committed;
      }
      while bits != 0 {
        let bit = bits.trailing_zeros();
        bits &= bits.wrapping_sub(1);
        let index = Bits::index_of(word, bit);
        if let Some(index) = index {
          let order = u32::from(self.state_at(index) & ORDER_MASK);
          self.deferred_bytes = self.deferred_bytes.saturating_sub(self.order_bytes(order));
          let locked = self.release(index, order);
          if let Some(offset) = usize::try_from(index)
            .ok()
            .and_then(|index| index.checked_shl(self.granule_shift))
          {
            released(offset, self.order_bytes(order), locked);
          }
        }
      }
    }
  }

  /// Every live block becomes committed (A-64: after a recovery's claims, the recovered image names them).
  pub fn commit_live(&mut self) {
    self.capture();
    self.commit_capture();
  }

  /// The incarnation last allocated at granule `index` (zero if none, or out of range).
  fn incarnation_at(&self, index: u32) -> u64 {
    usize::try_from(index)
      .ok()
      .and_then(|i| self.incarnations.get(i))
      .copied()
      .unwrap_or(0)
  }

  fn slot_mut(&mut self, index: u32) -> Option<&mut u64> {
    usize::try_from(index)
      .ok()
      .and_then(|i| self.incarnations.get_mut(i))
  }

  /// The granule index and order `block` names when it names exactly a live block, else why not.
  fn validate(&self, block: Block) -> Result<(u32, u32), ExtentRefusal> {
    if block.offset & self.granule.saturating_sub(1) != 0 {
      return Err(ExtentRefusal::Misaligned);
    }
    let order = self
      .order_for(block.len)
      .map_err(|_| ExtentRefusal::WrongLength)?;
    if self.order_bytes(order) != block.len {
      return Err(ExtentRefusal::WrongLength);
    }
    let index =
      u32::try_from(block.offset >> self.granule_shift).map_err(|_| ExtentRefusal::OutOfRange)?;
    if index & ((1u32 << order).saturating_sub(1)) != 0 {
      return Err(ExtentRefusal::Misaligned);
    }
    let head = usize::try_from(index)
      .ok()
      .and_then(|i| self.state.get(i))
      .copied()
      .ok_or(ExtentRefusal::OutOfRange)?;
    if head == INSIDE || head & FREE_BIT != 0 || u32::from(head & ORDER_MASK) != order {
      return Err(ExtentRefusal::NotAllocated);
    }
    if self.incarnation_at(index) != block.incarnation {
      return Err(ExtentRefusal::Stale);
    }
    Ok((index, order))
  }

  /// Frees a block previously returned by `alloc`, coalescing with free buddies. A block that names no
  /// live block of this allocator is refused ([`MemError::ForeignExtent`]) and nothing changes.
  pub fn free(&mut self, block: Block) -> Result<(), MemError> {
    let (index, order) = self
      .validate(block)
      .map_err(|reason| MemError::ForeignExtent {
        offset: block.offset,
        len: block.len,
        reason,
      })?;
    if self.imaged {
      // A committed head is freed only through the deferral, never here, or the committed image could
      // name a reused block.
      if self.named_once_imaged(index) {
        return Err(MemError::ForeignExtent {
          offset: block.offset,
          len: block.len,
          reason: ExtentRefusal::Claimed,
        });
      }
      self.live.set(index, false);
    }
    // The owner asked `is_locked` before the free and unlocks the pages itself; the bit goes with the block.
    let _ = self.unmark_locked(index, order);
    self.coalesce_free(index, order);
    Ok(())
  }

  /// Returns the allocated block of `order` at `index` to the free lists, coalescing.
  /// Never a block an image names: the deferral keeps those until a commit, and the commit replaces the committed
  /// set whole, so its bit needs no clearing here.
  fn release(&mut self, index: u32, order: u32) -> bool {
    if self.imaged {
      self.live.set(index, false);
    }
    let locked = self.unmark_locked(index, order);
    self.coalesce_free(index, order);
    locked
  }

  /// Returns the block of `order` at `index` to the free lists, coalescing; its live bit is the caller's.
  #[inline]
  fn coalesce_free(&mut self, index: u32, order: u32) {
    self.free_bytes = self.free_bytes.saturating_add(self.order_bytes(order));
    let (index, order) = self.coalesce(index, order);
    self.mark(index, order, true);
    self.link(index, order);
  }

  /// Merges the block at `index` with its free buddies upward; returns the merged block.
  fn coalesce(&mut self, mut index: u32, mut order: u32) -> (u32, u32) {
    while order < self.max_order {
      let buddy = index ^ (1u32 << order);
      let buddy_state = self.state_at(buddy);
      if buddy_state & FREE_BIT == 0 || u32::from(buddy_state & ORDER_MASK) != order {
        break;
      }
      self.unlink(buddy, order);
      self.set_state(buddy.max(index), INSIDE);
      index = index.min(buddy);
      order = order.saturating_add(1);
    }
    (index, order)
  }

  fn state_at(&self, index: u32) -> u8 {
    usize::try_from(index)
      .ok()
      .and_then(|i| self.state.get(i))
      .copied()
      .unwrap_or(INSIDE)
  }

  fn set_state(&mut self, index: u32, byte: u8) {
    if let Some(slot) = usize::try_from(index)
      .ok()
      .and_then(|i| self.state.get_mut(i))
    {
      *slot = byte;
    }
  }

  fn mark(&mut self, index: u32, order: u32, free: bool) {
    let byte =
      HEAD_BIT | u8::try_from(order).unwrap_or(ORDER_MASK) | if free { FREE_BIT } else { 0 };
    self.set_state(index, byte);
  }

  fn set_link(links: &mut [u32], index: u32, value: u32) {
    if let Some(slot) = usize::try_from(index).ok().and_then(|i| links.get_mut(i)) {
      *slot = value.wrapping_add(1);
    }
  }

  fn link_at(links: &[u32], index: u32) -> u32 {
    usize::try_from(index)
      .ok()
      .and_then(|i| links.get(i))
      .map_or(NONE, |stored| stored.wrapping_sub(1))
  }

  fn set_head(&mut self, order: u32, value: u32) {
    if let Some(slot) = usize::try_from(order)
      .ok()
      .and_then(|o| self.heads.get_mut(o))
    {
      *slot = value;
    }
  }

  fn link(&mut self, index: u32, order: u32) {
    let head = self.head_of(order);
    Self::set_link(&mut self.next, index, head);
    Self::set_link(&mut self.prev, index, NONE);
    if head != NONE {
      Self::set_link(&mut self.prev, head, index);
    }
    self.set_head(order, index);
  }

  fn unlink(&mut self, index: u32, order: u32) {
    let (next, prev) = (
      Self::link_at(&self.next, index),
      Self::link_at(&self.prev, index),
    );
    if prev == NONE {
      self.set_head(order, next);
    } else {
      Self::set_link(&mut self.next, prev, next);
    }
    if next != NONE {
      Self::set_link(&mut self.prev, next, prev);
    }
    Self::set_link(&mut self.next, index, NONE);
    Self::set_link(&mut self.prev, index, NONE);
  }

  /// Sets granule `index`'s incarnation, to start a test at the edge of the incarnation space.
  #[cfg(test)]
  fn set_incarnation_for_test(&mut self, index: u32, incarnation: u64) {
    if let Some(slot) = self.slot_mut(index) {
      *slot = incarnation;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_machine::stats::Xorshift;

  #[test]
  fn allocation_splits_and_freeing_coalesces_back_to_one_block() {
    let mut b = Buddy::new(4096, 4).unwrap(); // 16 granules = 64 KiB
    assert_eq!(b.region_bytes(), 65_536);
    let a = b.alloc(4096).unwrap();
    let c = b.alloc(10_000).unwrap();
    assert_eq!((a.offset(), a.len()), (0, 4096));
    assert_eq!(c.len(), 16_384);
    assert_eq!(b.free_bytes(), 65_536 - 4096 - 16_384);
    b.free(a).unwrap();
    b.free(c).unwrap();
    assert_eq!(b.free_bytes(), 65_536);
    assert_eq!(b.largest_free(), 65_536);
    assert!(matches!(b.alloc(65_537), Err(MemError::TooLarge { .. })));
  }

  #[test]
  fn exhaustion_is_a_typed_refusal_naming_the_largest_free_block() {
    let mut b = Buddy::new(4096, 2).unwrap();
    let x = b.alloc(4096).unwrap();
    let _y = b.alloc(8192).unwrap();
    assert!(matches!(
      b.alloc(8192),
      Err(MemError::ArenaExhausted {
        requested: 8192,
        largest_free: 4096
      })
    ));
    b.free(x).unwrap();
    assert!(b.free(x).is_err(), "double free is refused");
    assert!(
      b.free(Block::named(4096, 4096, x.incarnation())).is_err(),
      "freeing inside a block is refused"
    );
  }

  /// AUD-29-10 (the audit's run). Do: allocate 4,096 bytes from an 8,192-byte region, then free offset 1 /
  /// length 4,095. Expect: refused; free bytes still 4,096; no 8,192-byte block appears; freeing the real
  /// block restores one that allocates.
  #[test]
  fn a_forged_offset_and_length_are_refused_with_the_totals_unchanged() {
    let mut b = Buddy::new(4096, 1).unwrap();
    let live = b.alloc(4096).unwrap();
    let forged = Block::named(1, 4095, live.incarnation());
    assert_eq!(
      b.free(forged),
      Err(MemError::ForeignExtent {
        offset: 1,
        len: 4095,
        reason: ExtentRefusal::Misaligned
      }),
      "a forged block is refused"
    );
    assert_eq!(b.free_bytes(), 4096, "the totals are unchanged");
    assert!(b.alloc(8192).is_err(), "no 8,192-byte block was coalesced");
    b.free(live).unwrap();
    assert_eq!(b.alloc(8192).unwrap().len(), 8192);
  }

  /// The refusal `b.free(block)` gives, which must leave `b`'s free bytes at `free_before`.
  fn refused(b: &mut Buddy, block: Block, free_before: usize) -> ExtentRefusal {
    let reason = match b.free(block) {
      Err(MemError::ForeignExtent { reason, .. }) => reason,
      other => panic!("{block:?} was not refused: {other:?}"),
    };
    assert_eq!(b.free_bytes(), free_before, "{reason:?} changed the totals");
    reason
  }

  /// AUD-29-10. Do: against a live 8 KiB block at offset 0 of a 32 KiB region, free a wrong length at
  /// its offset, its second half (inside it), a granule-aligned but not size-aligned block, a block past
  /// the region, and a length that is no block's. Expect: each is refused with its reason and the totals
  /// unchanged; the live block then frees and the region is whole again.
  #[test]
  fn every_forged_shape_is_refused_by_name() {
    let mut b = Buddy::new(4096, 3).unwrap();
    let live = b.alloc(8192).unwrap();
    let free_before = b.free_bytes();
    let inc = live.incarnation();
    let cases = [
      (Block::named(0, 4096, inc), ExtentRefusal::NotAllocated),
      (Block::named(4096, 4096, inc), ExtentRefusal::NotAllocated),
      (Block::named(4096, 8192, inc), ExtentRefusal::Misaligned),
      (Block::named(65_536, 4096, inc), ExtentRefusal::OutOfRange),
      (Block::named(0, 12_288, inc), ExtentRefusal::WrongLength),
      (Block::named(0, 65_536, inc), ExtentRefusal::WrongLength),
    ];
    for (block, reason) in cases {
      assert_eq!(refused(&mut b, block, free_before), reason, "{block:?}");
    }
    b.free(live).unwrap();
    assert_eq!(b.largest_free(), b.region_bytes());
  }

  /// AUD-29-10. Do: allocate a block, free it, allocate the same place again, then free the first copy
  /// again (a stale extent after reuse) and the current one twice (a duplicate free). Expect: the stale
  /// copy is refused `Stale` without freeing the new owner's block; the duplicate is refused
  /// `NotAllocated`; the totals are unchanged by both.
  #[test]
  fn a_stale_copy_after_reuse_and_a_duplicate_free_are_refused() {
    let mut b = Buddy::new(4096, 1).unwrap();
    let first = b.alloc(8192).unwrap();
    b.free(first).unwrap();
    let second = b.alloc(8192).unwrap();
    assert_eq!(second.offset(), first.offset(), "the same place, reused");
    assert_ne!(second.incarnation(), first.incarnation());
    assert_eq!(refused(&mut b, first, 0), ExtentRefusal::Stale);
    b.free(second).unwrap();
    assert_eq!(refused(&mut b, second, 8192), ExtentRefusal::NotAllocated);
  }

  /// A granule that is not a power of two (no page size is) is refused rather than rounded.
  #[test]
  fn a_granule_that_is_not_a_power_of_two_is_refused() {
    assert_eq!(
      Buddy::new(3000, 2).map(|b| b.region_bytes()),
      Err(MemError::BadCapacity { capacity: 3000 })
    );
  }

  /// AUD-29-11 for the buddy. Do: set the only head's incarnation to the top of its space and allocate.
  /// Expect: `GenerationExhausted` naming the granule, the region still wholly free; an incarnation one
  /// below the top allocates once (to the top) and frees.
  #[test]
  fn a_spent_incarnation_refuses_rather_than_wrapping() {
    let mut b = Buddy::new(4096, 1).unwrap();
    b.set_incarnation_for_test(0, u64::MAX);
    assert_eq!(
      b.alloc(4096),
      Err(MemError::GenerationExhausted { index: 0 })
    );
    assert_eq!(b.free_bytes(), 8192, "nothing changed");
    assert_eq!(b.largest_free(), 8192);
    b.set_incarnation_for_test(0, u64::MAX - 1);
    let last = b.alloc(8192).unwrap();
    assert_eq!(last.incarnation(), u64::MAX);
    b.free(last).unwrap();
  }

  fn allocate_step(b: &mut Buddy, live: &mut Vec<Block>, len: usize) {
    match b.alloc(len) {
      Ok(block) => {
        assert!(block.len() >= len);
        for other in live.iter() {
          let disjoint =
            block.offset + block.len <= other.offset || other.offset + other.len <= block.offset;
          assert!(disjoint, "{block:?} overlaps {other:?}");
        }
        live.push(block);
      }
      Err(MemError::ArenaExhausted { largest_free, .. }) => {
        assert!(largest_free < len || largest_free == 0)
      }
      Err(e) => panic!("{e}"),
    }
  }

  #[test]
  fn random_sequences_never_overlap_and_always_coalesce_back() {
    let granule = 256;
    let mut b = Buddy::new(granule, 10).unwrap(); // 1024 granules
    let mut rng = Xorshift::new(Xorshift::SEED);
    let mut live: Vec<Block> = Vec::new();
    for _ in 0..20_000 {
      if live.is_empty() || rng.below(3) != 0 {
        let len = (rng.below(20) + 1) * granule * (1 << rng.below(4));
        allocate_step(&mut b, &mut live, len);
      } else {
        let block = live.swap_remove(rng.below(live.len()));
        b.free(block).unwrap();
      }
      let used: usize = live.iter().map(|x| x.len).sum();
      assert_eq!(b.free_bytes() + used, b.region_bytes());
    }
    for block in live.drain(..) {
      b.free(block).unwrap();
    }
    assert_eq!(
      b.largest_free(),
      b.region_bytes(),
      "the full region is one block again"
    );
  }

  fn overlaps(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.0 + b.1 && b.0 < a.0 + a.1
  }

  /// A random population: blocks allocated and some freed again, in an allocator of 1,024 granules.
  fn random_population(granule: usize, rng: &mut Xorshift) -> (Buddy, Vec<Block>) {
    let mut source = Buddy::new(granule, 10).unwrap();
    let mut live: Vec<Block> = Vec::new();
    for _ in 0..200 {
      let len = (rng.below(8) + 1) * granule * (1 << rng.below(3));
      allocate_step(&mut source, &mut live, len);
      if !live.is_empty() && rng.below(4) == 0 {
        source
          .free(live.swap_remove(rng.below(live.len())))
          .unwrap();
      }
    }
    (source, live)
  }

  /// Fills `rebuilt` with single granules until it refuses, checking none overlaps `claimed`, then frees
  /// everything and expects the region whole.
  fn fill_then_free_all(rebuilt: &mut Buddy, claimed: Vec<Block>, granule: usize) {
    let mut fresh = Vec::new();
    while let Ok(block) = rebuilt.alloc(granule) {
      for other in &claimed {
        assert!(!overlaps(
          (block.offset, block.len),
          (other.offset, other.len)
        ));
      }
      fresh.push(block);
    }
    for block in claimed.into_iter().chain(fresh) {
      rebuilt.free(block).unwrap();
    }
    assert_eq!(rebuilt.largest_free(), rebuilt.region_bytes());
  }

  /// A-64 (recovery rebuilds the allocation by claim). Do: allocate a random population in one allocator,
  /// then claim exactly its blocks, in a shuffled order, in a fresh one. Expect: every claim succeeds, the
  /// free bytes agree, a later allocation never overlaps a claimed block, and freeing the claims coalesces
  /// the region back to one block.
  #[test]
  fn claiming_a_population_rebuilds_its_allocation() {
    let granule = 256;
    let mut rng = Xorshift::new(Xorshift::SEED);
    // Miri interprets every step; two rounds keep its lane bounded and still cover the claim's paths.
    let rounds = if cfg!(miri) { 2 } else { 50 };
    for _round in 0..rounds {
      let (source, live) = random_population(granule, &mut rng);
      let mut order: Vec<(usize, usize)> = live.iter().map(|b| (b.offset, b.len)).collect();
      for at in (1..order.len()).rev() {
        order.swap(at, rng.below(at + 1));
      }
      let mut rebuilt = Buddy::new(granule, 10).unwrap();
      let claimed: Vec<Block> = order
        .iter()
        .map(|&(offset, len)| rebuilt.claim(offset, len).unwrap())
        .collect();
      assert_eq!(rebuilt.free_bytes(), source.free_bytes());
      fill_then_free_all(&mut rebuilt, claimed, granule);
    }
  }

  /// A-64 hostile image. Do: against a region with one 8 KiB block claimed at offset 8 KiB, claim a
  /// misaligned offset, a length that is no block's, a block past the region, the same block again, a
  /// larger block holding it, and a smaller block inside it. Expect: each refused by name with the totals
  /// unchanged.
  #[test]
  fn a_claim_that_no_allocation_could_have_made_is_refused_by_name() {
    let mut b = Buddy::new(4096, 3).unwrap();
    b.claim(8192, 8192).unwrap();
    let free_before = b.free_bytes();
    let cases = [
      ((1, 4096), ExtentRefusal::Misaligned),
      ((4096, 8192), ExtentRefusal::Misaligned),
      ((0, 12_288), ExtentRefusal::WrongLength),
      ((65_536, 4096), ExtentRefusal::OutOfRange),
      ((8192, 8192), ExtentRefusal::Claimed),
      ((0, 16_384), ExtentRefusal::Claimed),
      ((12_288, 4096), ExtentRefusal::Claimed),
    ];
    for ((offset, len), reason) in cases {
      match b.claim(offset, len) {
        Err(MemError::ForeignExtent { reason: got, .. }) => {
          assert_eq!(got, reason, "claim {offset}+{len}")
        }
        other => panic!("claim {offset}+{len} was not refused: {other:?}"),
      }
      assert_eq!(
        b.free_bytes(),
        free_before,
        "claim {offset}+{len} changed the totals"
      );
    }
    assert_eq!(
      b.claim(0, 8192).unwrap().len(),
      8192,
      "the free neighbour still claims"
    );
  }

  /// A-64. Do: allocate a block, capture and commit (the image names it), free it. Expect: the free is
  /// deferred, its bytes counted, and no allocation reaches them; a plain free of it is refused; the next
  /// capture and commit release it and the region is whole.
  #[test]
  fn a_block_the_committed_image_names_is_not_reused_before_the_next_commit() {
    let mut b = Buddy::new(4096, 1).unwrap();
    let named = b.alloc(4096).unwrap();
    b.capture();
    b.commit_capture();
    assert!(
      b.free(named).is_err(),
      "a committed block is freed only through the deferral"
    );
    assert!(b.free_or_defer(named).unwrap(), "deferred");
    assert_eq!(b.deferred_bytes(), 4096);
    let other = b.alloc(4096).unwrap();
    assert_ne!(
      other.offset(),
      named.offset(),
      "the named block is not handed out"
    );
    assert!(b.alloc(4096).is_err());
    b.free_or_defer(other).unwrap();
    b.capture();
    b.commit_capture();
    assert_eq!(b.deferred_bytes(), 0);
    assert_eq!(b.largest_free(), b.region_bytes(), "released by the commit");
  }

  /// A-64. Do: allocate a block, capture (a publication in progress may name it), free it, then commit.
  /// Expect: the free is deferred, and stays deferred across that commit (the new image may name the block);
  /// the following commit releases it. And a block freed during a capture that is abandoned is released at
  /// once when no committed image names it.
  #[test]
  fn a_free_during_an_open_capture_waits_for_the_image_that_may_name_it() {
    let mut b = Buddy::new(4096, 1).unwrap();
    let block = b.alloc(4096).unwrap();
    b.capture();
    assert!(b.free_or_defer(block).unwrap());
    b.commit_capture();
    assert_eq!(b.deferred_bytes(), 4096, "the new image may name it");
    assert_eq!(b.alloc(8192).ok(), None);
    b.capture();
    b.commit_capture();
    assert_eq!(b.largest_free(), b.region_bytes());

    let abandoned = b.alloc(4096).unwrap();
    b.capture();
    assert!(b.free_or_defer(abandoned).unwrap());
    b.abandon_capture();
    assert_eq!(b.deferred_bytes(), 0, "no committed image names it");
    assert_eq!(b.largest_free(), b.region_bytes());
  }

  /// The deferral's model: the blocks live, the spans the committed image names, the spans an open capture
  /// names, and the spans freed but deferred.
  #[derive(Default)]
  struct DeferralModel {
    live: Vec<Block>,
    committed: Vec<(usize, usize)>,
    capture: Option<Vec<(usize, usize)>>,
    deferred: Vec<(usize, usize)>,
  }

  impl DeferralModel {
    fn allocate(&mut self, b: &mut Buddy, len: usize) {
      if let Ok(block) = b.alloc(len) {
        let span = (block.offset, block.len);
        for named in self.committed.iter().chain(self.capture.iter().flatten()) {
          assert!(!overlaps(span, *named), "{span:?} reuses {named:?}");
        }
        self.live.push(block);
      }
    }

    fn free(&mut self, b: &mut Buddy, rng: &mut Xorshift) {
      if self.live.is_empty() {
        return;
      }
      let block = self.live.swap_remove(rng.below(self.live.len()));
      if b.free_or_defer(block).unwrap() {
        self.deferred.push((block.offset, block.len));
      }
    }

    fn capture(&mut self, b: &mut Buddy) {
      if self.capture.is_none() {
        b.capture();
        self.capture = Some(self.live.iter().map(|x| (x.offset, x.len)).collect());
      }
    }

    fn settle(&mut self, b: &mut Buddy, commit: bool) {
      let Some(taken) = self.capture.take() else {
        return;
      };
      if commit {
        b.commit_capture();
        self.deferred.retain(|span| taken.contains(span));
        self.committed = taken;
      } else {
        b.abandon_capture();
        let committed = &self.committed;
        self.deferred.retain(|span| committed.contains(span));
      }
    }

    fn check_totals(&self, b: &Buddy) {
      let live_bytes: usize = self.live.iter().map(|x| x.len).sum();
      let deferred_bytes: usize = self.deferred.iter().map(|x| x.1).sum();
      assert_eq!(b.deferred_bytes(), deferred_bytes);
      assert_eq!(
        b.free_bytes() + live_bytes + deferred_bytes,
        b.region_bytes()
      );
    }
  }

  /// A-64, the deferral against a model. Do: a random history of allocations, frees, captures, commits and
  /// abandons. The model keeps the blocks the committed image names and those the open capture names. Expect:
  /// no allocation ever overlaps a block either image names, and the free bytes plus the live and deferred
  /// bytes always equal the region.
  #[test]
  fn no_block_an_image_names_is_ever_handed_out_again() {
    let granule = 256;
    let mut b = Buddy::new(granule, 8).unwrap();
    let mut rng = Xorshift::new(Xorshift::SEED);
    let mut model = DeferralModel::default();
    let steps = if cfg!(miri) { 2_000 } else { 20_000 };
    for _ in 0..steps {
      match rng.below(10) {
        0..=4 => model.allocate(&mut b, granule * (1 << rng.below(3))),
        5..=7 => model.free(&mut b, &mut rng),
        8 => model.capture(&mut b),
        _ => {
          let commit = rng.below(2) == 0;
          model.settle(&mut b, commit);
        }
      }
      model.check_totals(&b);
    }
  }

  /// T-1.1's preflight. Do: in a region of eight granules, ask for blocks of each size, then take one granule.
  /// Expect: `can_allocate` says yes exactly for the counts and sizes the free space holds.
  #[test]
  fn can_allocate_counts_what_the_free_blocks_hold() {
    let granule = 256;
    let mut b = Buddy::new(granule, 3).unwrap();
    assert!(b.can_allocate(8, granule));
    assert!(!b.can_allocate(9, granule));
    assert!(b.can_allocate(1, 8 * granule));
    b.alloc(granule).unwrap();
    assert!(
      !b.can_allocate(1, 8 * granule),
      "no whole region after one granule"
    );
    assert!(b.can_allocate(1, 4 * granule));
    assert!(b.can_allocate(7, granule));
    assert!(!b.can_allocate(8, granule));
  }

  /// T-1.1's preflight. Do: fill a region of eight granules one by one, then free one. Expect: no block while full;
  /// after the free, one granule but no two-granule block.
  #[test]
  fn can_allocate_follows_frees_without_promising_a_coalesced_block() {
    let granule = 256;
    let mut b = Buddy::new(granule, 3).unwrap();
    let taken: Vec<Block> = (0..8).map(|_| b.alloc(granule).unwrap()).collect();
    assert!(!b.can_allocate(1, granule));
    b.free(*taken.first().unwrap()).unwrap();
    assert!(b.can_allocate(1, granule));
    assert!(
      !b.can_allocate(1, 2 * granule),
      "one free granule is no two-granule block"
    );
  }
}
