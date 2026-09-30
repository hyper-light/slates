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

use crate::error::{ExtentRefusal, MemError};

/// Format: state byte layout: bit 7 = free, bits 0..6 = order of the block whose head this is.
const FREE_BIT: u8 = 0x80;
/// Format: the order mask.
const ORDER_MASK: u8 = 0x7F;
/// Format: a granule that is inside a block, not its head.
const INSIDE: u8 = 0xFF;

/// The sentinel for "no link" in the free lists.
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
      *first = FREE_BIT | u8::try_from(max_order).unwrap_or(ORDER_MASK);
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
      next: vec![NONE; granules],
      prev: vec![NONE; granules],
      heads,
      incarnations: vec![0; granules],
      free_bytes: granules.saturating_mul(granule),
    })
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
    (0..=self.max_order)
      .rev()
      .find(|order| self.head_of(*order) != NONE)
      .map_or(0, |order| self.order_bytes(order))
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
    let Some(found) = (order..=self.max_order).find(|o| self.head_of(*o) != NONE) else {
      return Err(MemError::ArenaExhausted {
        requested: len,
        largest_free: self.largest_free(),
      });
    };
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
    self.mark(index, order, false);
    if let Some(slot) = self.slot_mut(index) {
      *slot = incarnation;
    }
    let block_len = self.order_bytes(order);
    self.free_bytes = self.free_bytes.saturating_sub(block_len);
    Ok(Block {
      offset: usize::try_from(index).unwrap_or(0) << self.granule_shift,
      len: block_len,
      incarnation,
    })
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
    self.free_bytes = self.free_bytes.saturating_add(self.order_bytes(order));
    let (index, order) = self.coalesce(index, order);
    self.mark(index, order, true);
    self.link(index, order);
    Ok(())
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
    let byte = u8::try_from(order).unwrap_or(ORDER_MASK) | if free { FREE_BIT } else { 0 };
    self.set_state(index, byte);
  }

  fn set_link(links: &mut [u32], index: u32, value: u32) {
    if let Some(slot) = usize::try_from(index).ok().and_then(|i| links.get_mut(i)) {
      *slot = value;
    }
  }

  fn link_at(links: &[u32], index: u32) -> u32 {
    usize::try_from(index)
      .ok()
      .and_then(|i| links.get(i))
      .copied()
      .unwrap_or(NONE)
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
}
