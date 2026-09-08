//! A binary buddy allocator over one region: blocks are power-of-two multiples of the base
//! granule; allocation splits the smallest sufficient free block, freeing coalesces with the
//! buddy while the buddy is free (§4.2; [A: Knowlton, "A fast storage allocator", CACM 1965;
//! A: Knuth, TAOCP vol. 1 §2.5]).
//!
//! The allocator's own state lives beside the region, not inside it, so it never touches
//! chunk pages that may be cold or not yet faulted: one byte of state per granule (free bit and
//! order) and an intrusive doubly-linked free list per order over granule indices. Split and
//! coalesce are O(log region); allocation is O(log region) in the worst case and O(1) when a
//! block of the right order is free.

use crate::error::MemError;

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
  max_order: u32,
  state: Vec<u8>,
  next: Vec<u32>,
  prev: Vec<u32>,
  heads: Vec<u32>,
  free_bytes: usize,
}

/// A block handed out by the allocator: an offset and a length in bytes, both granule multiples.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Block {
  /// Byte offset within the region.
  pub offset: usize,
  /// Length in bytes (a power-of-two multiple of the granule).
  pub len: usize,
}

impl Buddy {
  /// An allocator for a region of `1 << max_order` granules of `granule` bytes each; the whole
  /// region starts free as one block.
  pub fn new(granule: usize, max_order: u32) -> Self {
    let granules = 1usize << max_order;
    let mut state = vec![INSIDE; granules];
    state[0] = FREE_BIT | u8::try_from(max_order).unwrap_or(ORDER_MASK);
    let mut heads = vec![NONE; usize::try_from(max_order).unwrap_or(0) + 1];
    heads[usize::try_from(max_order).unwrap_or(0)] = 0;
    Self {
      granule,
      max_order,
      state,
      next: vec![NONE; granules],
      prev: vec![NONE; granules],
      heads,
      free_bytes: granules.saturating_mul(granule),
    }
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

  /// The largest block that can be allocated right now, in bytes (0 when nothing is free).
  pub fn largest_free(&self) -> usize {
    (0..=self.max_order)
      .rev()
      .find(|order| self.heads[usize::try_from(*order).unwrap_or(0)] != NONE)
      .map_or(0, |order| (1usize << order).saturating_mul(self.granule))
  }

  /// The order (block size exponent in granules) that fits `len` bytes.
  pub fn order_for(&self, len: usize) -> Result<u32, MemError> {
    let granules = len.max(1).div_ceil(self.granule).next_power_of_two();
    let order = granules.trailing_zeros();
    if order > self.max_order {
      return Err(MemError::TooLarge {
        len,
        max: self.region_bytes(),
      });
    }
    Ok(order)
  }

  /// Allocates a block of at least `len` bytes.
  pub fn alloc(&mut self, len: usize) -> Result<Block, MemError> {
    let order = self.order_for(len)?;
    let Some(found) =
      (order..=self.max_order).find(|o| self.heads[usize::try_from(*o).unwrap_or(0)] != NONE)
    else {
      return Err(MemError::ArenaExhausted {
        requested: len,
        largest_free: self.largest_free(),
      });
    };
    let index = self.heads[usize::try_from(found).unwrap_or(0)];
    self.unlink(index, found);
    let mut current = found;
    while current > order {
      current -= 1;
      let buddy = index + (1u32 << current);
      self.mark(buddy, current, true);
      self.link(buddy, current);
    }
    self.mark(index, order, false);
    let block_len = (1usize << order).saturating_mul(self.granule);
    self.free_bytes -= block_len;
    Ok(Block {
      offset: usize::try_from(index)
        .unwrap_or(0)
        .saturating_mul(self.granule),
      len: block_len,
    })
  }

  /// Marks a specific, currently-free block allocated — the recovery re-seed (§4.8 content
  /// recovery). A restarted daemon rebuilds a fresh allocator over the recovered content object,
  /// which believes the whole region is free; before it serves anything it reserves every extent
  /// the recovered metadata still references, so it never hands out a range that still holds a
  /// snapshot's bytes. `block` is an aligned power-of-two block as `alloc` returns (a recovered
  /// extent is exactly that). Refuses (`bad_block`) a misaligned block, or one not wholly free
  /// (already reserved, so a double-reserve of the same extent is a typed refusal, not corruption).
  pub fn reserve(&mut self, block: Block) -> Result<(), MemError> {
    let target = self.order_for(block.len)?;
    let index = u32::try_from(block.offset / self.granule).map_err(|_| bad_block(block))?;
    // The block must sit at a target-order boundary, as every `alloc`ed block does.
    if index & ((1u32 << target) - 1) != 0 {
      return Err(bad_block(block));
    }
    // Find the free block that contains `index`: for each order from the target up, the
    // order-aligned base of a free block of exactly that order is the enclosing block.
    let mut base = index;
    let mut order = target;
    let found = loop {
      let head = self
        .state
        .get(usize::try_from(base).unwrap_or(usize::MAX))
        .copied()
        .unwrap_or(INSIDE);
      if head & FREE_BIT != 0 && u32::from(head & ORDER_MASK) == order {
        break true;
      }
      if order >= self.max_order {
        break false;
      }
      order += 1;
      base = index & !((1u32 << order) - 1);
    };
    if !found {
      return Err(bad_block(block));
    }
    // Split the enclosing free block down to the target order at `index`, freeing the half that
    // does not contain `index` at each step (the mirror of `alloc`'s split, aimed at `index`).
    self.unlink(base, order);
    while order > target {
      order -= 1;
      let upper = base + (1u32 << order);
      if index >= upper {
        self.mark(base, order, true);
        self.link(base, order);
        base = upper;
      } else {
        self.mark(upper, order, true);
        self.link(upper, order);
      }
    }
    self.mark(index, target, false);
    self.free_bytes -= (1usize << target).saturating_mul(self.granule);
    Ok(())
  }

  /// Frees a block previously returned by `alloc`, coalescing with free buddies.
  pub fn free(&mut self, block: Block) -> Result<(), MemError> {
    let index = u32::try_from(block.offset / self.granule).map_err(|_| bad_block(block))?;
    let order = self.order_for(block.len)?;
    let head = self
      .state
      .get(usize::try_from(index).unwrap_or(usize::MAX))
      .copied()
      .unwrap_or(INSIDE);
    if head == INSIDE || head & FREE_BIT != 0 || u32::from(head & ORDER_MASK) != order {
      return Err(bad_block(block));
    }
    self.free_bytes += block.len;
    let (index, order) = self.coalesce(index, order);
    self.mark(index, order, true);
    self.link(index, order);
    Ok(())
  }

  /// Merges the block at `index` with its free buddies upward; returns the merged block.
  fn coalesce(&mut self, mut index: u32, mut order: u32) -> (u32, u32) {
    while order < self.max_order {
      let buddy = index ^ (1u32 << order);
      let buddy_state = self.state[usize::try_from(buddy).unwrap_or(0)];
      if buddy_state & FREE_BIT == 0 || u32::from(buddy_state & ORDER_MASK) != order {
        break;
      }
      self.unlink(buddy, order);
      self.state[usize::try_from(buddy.max(index)).unwrap_or(0)] = INSIDE;
      index = index.min(buddy);
      order += 1;
    }
    (index, order)
  }

  fn mark(&mut self, index: u32, order: u32, free: bool) {
    let byte = u8::try_from(order).unwrap_or(ORDER_MASK) | if free { FREE_BIT } else { 0 };
    self.state[usize::try_from(index).unwrap_or(0)] = byte;
  }

  fn link(&mut self, index: u32, order: u32) {
    let o = usize::try_from(order).unwrap_or(0);
    let i = usize::try_from(index).unwrap_or(0);
    let head = self.heads[o];
    self.next[i] = head;
    self.prev[i] = NONE;
    if head != NONE {
      self.prev[usize::try_from(head).unwrap_or(0)] = index;
    }
    self.heads[o] = index;
  }

  fn unlink(&mut self, index: u32, order: u32) {
    let o = usize::try_from(order).unwrap_or(0);
    let i = usize::try_from(index).unwrap_or(0);
    let (next, prev) = (self.next[i], self.prev[i]);
    if prev == NONE {
      self.heads[o] = next;
    } else {
      self.next[usize::try_from(prev).unwrap_or(0)] = next;
    }
    if next != NONE {
      self.prev[usize::try_from(next).unwrap_or(0)] = prev;
    }
    self.next[i] = NONE;
    self.prev[i] = NONE;
  }
}

fn bad_block(block: Block) -> MemError {
  MemError::TooLarge {
    len: block.len,
    max: block.offset,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_machine::stats::Xorshift;

  #[test]
  fn allocation_splits_and_freeing_coalesces_back_to_one_block() {
    let mut b = Buddy::new(4096, 4); // 16 granules = 64 KiB
    assert_eq!(b.region_bytes(), 65_536);
    let a = b.alloc(4096).unwrap();
    let c = b.alloc(10_000).unwrap();
    assert_eq!(
      a,
      Block {
        offset: 0,
        len: 4096
      }
    );
    assert_eq!(c.len, 16_384);
    assert_eq!(b.free_bytes(), 65_536 - 4096 - 16_384);
    b.free(a).unwrap();
    b.free(c).unwrap();
    assert_eq!(b.free_bytes(), 65_536);
    assert_eq!(b.largest_free(), 65_536);
    assert!(matches!(b.alloc(65_537), Err(MemError::TooLarge { .. })));
  }

  #[test]
  fn exhaustion_is_a_typed_refusal_naming_the_largest_free_block() {
    let mut b = Buddy::new(4096, 2);
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
      b.free(Block {
        offset: 4096,
        len: 4096
      })
      .is_err(),
      "freeing inside a block is refused"
    );
  }

  fn allocate_step(b: &mut Buddy, live: &mut Vec<Block>, len: usize) {
    match b.alloc(len) {
      Ok(block) => {
        assert!(block.len >= len);
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
    let mut b = Buddy::new(granule, 10); // 1024 granules
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

  /// reserve marks a specific block allocated, refuses a double-reserve and a misaligned block, and
  /// a reserved block frees back cleanly (§4.8 recovery re-seed). Do X, expect Y.
  #[test]
  fn reserve_marks_a_block_allocated_and_refuses_bad_input() {
    let mut b = Buddy::new(4096, 4); // 16 granules = 64 KiB
    let target = Block {
      offset: 8192,
      len: 4096,
    };
    b.reserve(target).unwrap();
    assert_eq!(b.free_bytes(), 65_536 - 4096);
    // A double-reserve of the same extent is a typed refusal, never silent corruption.
    assert!(b.reserve(target).is_err(), "double reserve is refused");
    // Every later allocation stays clear of the reserved block.
    let mut live = vec![target];
    for _ in 0..10 {
      if let Ok(block) = b.alloc(4096) {
        for other in &live {
          let disjoint =
            block.offset + block.len <= other.offset || other.offset + other.len <= block.offset;
          assert!(disjoint, "{block:?} overlaps reserved {other:?}");
        }
        live.push(block);
      }
    }
    // A reserved block frees back and coalesces to the whole region.
    let mut fresh = Buddy::new(4096, 4);
    fresh.reserve(target).unwrap();
    fresh.free(target).unwrap();
    assert_eq!(
      fresh.largest_free(),
      65_536,
      "free after reserve coalesces back"
    );
    // A misaligned block is refused (index 1 is not order-1 aligned).
    let mut other = Buddy::new(4096, 4);
    assert!(
      other
        .reserve(Block {
          offset: 4096,
          len: 8192,
        })
        .is_err(),
      "a misaligned reserve is refused"
    );
  }

  /// The recovery-equivalence oracle (§4.8): a fresh allocator re-seeded by reserving exactly the
  /// live extents of a prior allocator matches it — same free bytes — and never hands out a live
  /// extent afterward, so recovered content is never overwritten. Do X, expect Y over a random
  /// history.
  /// Whether two blocks occupy disjoint byte ranges.
  fn blocks_disjoint(a: Block, b: Block) -> bool {
    a.offset + a.len <= b.offset || b.offset + b.len <= a.offset
  }

  /// Takes a fresh allocator through a random alloc/free history and returns its live set.
  fn random_live_set(
    b: &mut Buddy,
    rng: &mut Xorshift,
    granule: usize,
    steps: usize,
  ) -> Vec<Block> {
    let mut live: Vec<Block> = Vec::new();
    for _ in 0..steps {
      if live.is_empty() || rng.below(3) != 0 {
        let len = (rng.below(20) + 1) * granule * (1 << rng.below(4));
        if let Ok(block) = b.alloc(len) {
          live.push(block);
        }
      } else {
        let block = live.swap_remove(rng.below(live.len()));
        b.free(block).unwrap();
      }
    }
    live
  }

  #[test]
  fn reserve_reconstructs_the_allocator_state_for_recovery() {
    let granule = 256;
    let max_order = 10; // 1024 granules
    // A "pre-restart" allocator taken through a random alloc/free history to a live set.
    let mut original = Buddy::new(granule, max_order);
    let mut rng = Xorshift::new(Xorshift::SEED);
    let live = random_live_set(&mut original, &mut rng, granule, 2_000);
    // Recovery: a fresh allocator over the same region, re-seeded by reserving every live extent
    // (in arbitrary order, as recovery replays them).
    let mut recovered = Buddy::new(granule, max_order);
    for block in &live {
      recovered.reserve(*block).unwrap();
    }
    let used: usize = live.iter().map(|x| x.len).sum();
    assert_eq!(
      recovered.free_bytes(),
      original.free_bytes(),
      "the re-seeded allocator has the same free bytes as the original"
    );
    assert_eq!(recovered.free_bytes() + used, recovered.region_bytes());
    // A post-recovery allocation is disjoint from every live extent.
    for _ in 0..500 {
      if let Ok(block) = recovered.alloc((rng.below(20) + 1) * granule) {
        for other in &live {
          assert!(
            blocks_disjoint(block, *other),
            "post-recovery {block:?} overlaps live content {other:?}"
          );
        }
      }
    }
  }
}
