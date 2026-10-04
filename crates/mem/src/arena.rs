//! The chunk arena: buddy allocation over one shard's locked regions, addressing bytes by
//! (region, offset, length) and never by a raw pointer that leaves the owning shard's frame
//! (§4.2, `ChunkArena`).
//!
//! Regions are added as the reserve provides them (mapped, pre-faulted, locked in priority
//! order); allocation tries each region's buddy tree in order and refuses with
//! `ArenaExhausted` naming the largest free extent when none fits, so the caller can grow or
//! refuse in turn. Nothing here reaches the system allocator after the region set is built
//! (AC-0.4); a region's buddy state is allocated once when the region is added.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::buddy::{Block, Buddy};
use crate::error::{ExtentRefusal, MemError};
use crate::region::Region;

/// The identities handed to arenas so far in this process: an extent carries its arena's, so a free into
/// another arena is refused (AUD-29-10). Taken once per arena, on a cold path.
static ARENAS: AtomicU64 = AtomicU64::new(0);

/// An extent within a region, as the arena that allocated it issued it. Only this crate constructs one,
/// and a free is accepted only from the issuing arena for the live allocation it names (AUD-29-10).
///
/// ```compile_fail
/// let forged = slates_mem::arena::Extent { region: 0, offset: 1, len: 4095 };
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Extent {
  arena: u64,
  region: u16,
  block: Block,
}

impl Extent {
  /// The region's index within the arena.
  pub const fn region(&self) -> u16 {
    self.region
  }

  /// Byte offset within the region.
  pub const fn offset(&self) -> usize {
    self.block.offset()
  }

  /// Length in bytes (the allocated block, which may exceed what was asked).
  pub const fn len(&self) -> usize {
    self.block.len()
  }

  /// Whether the extent is empty (never: an extent holds at least one granule).
  pub const fn is_empty(&self) -> bool {
    self.block.is_empty()
  }
}

#[derive(Debug)]
struct Slot {
  region: Region,
  buddy: Buddy,
}

/// The arena.
#[derive(Debug)]
pub struct ChunkArena {
  /// This arena's identity, or `None` when the process has spent them (it then refuses every allocation).
  identity: Option<u64>,
  slots: Vec<Slot>,
  granule: usize,
  allocated_bytes: usize,
}

impl ChunkArena {
  /// An arena whose blocks are multiples of `granule` bytes (the base page, from the profile).
  pub fn new(granule: usize) -> Self {
    // Checked, never wrapped: 2^64 arenas cannot be made in practice, but a spent space refuses by type.
    let identity = ARENAS
      .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |taken| {
        taken.checked_add(1)
      })
      .ok()
      .and_then(|taken| taken.checked_add(1));
    Self {
      identity,
      slots: Vec::new(),
      granule: granule.max(1),
      allocated_bytes: 0,
    }
  }

  /// The granule: every block is a power-of-two multiple of it.
  pub const fn granule(&self) -> usize {
    self.granule
  }

  /// Adds a region; its length is used up to the largest power-of-two number of granules it
  /// holds. Returns the region's index.
  pub fn add_region(&mut self, region: Region) -> Result<u16, MemError> {
    let granules = region.len() / self.granule;
    if granules == 0 {
      return Err(MemError::TooLarge {
        len: region.len(),
        max: self.granule,
      });
    }
    let max_order = usize::BITS - 1 - granules.leading_zeros();
    let index = u16::try_from(self.slots.len()).map_err(|_| MemError::TooLarge {
      len: self.slots.len(),
      max: usize::from(u16::MAX),
    })?;
    self.slots.push(Slot {
      region,
      buddy: Buddy::new(self.granule, max_order)?,
    });
    Ok(index)
  }

  /// Regions in the arena.
  pub fn regions(&self) -> usize {
    self.slots.len()
  }

  /// Bytes currently allocated, deferred frees included (A-64: they stay allocated until a publication commits).
  pub const fn allocated_bytes(&self) -> usize {
    self.allocated_bytes
  }

  /// Free bytes across regions.
  pub fn free_bytes(&self) -> usize {
    self.slots.iter().map(|s| s.buddy.free_bytes()).sum()
  }

  /// The usable (buddy-allocatable) capacity across regions: the bytes that can actually be
  /// handed out, which is each region's largest power-of-two number of granules — not its mapping
  /// length. Admission must reserve against this, never the mapping, or it over-promises quota the
  /// arena cannot physically back (§4.2, BUG-2). Constant for the life of the arena's regions.
  pub fn capacity(&self) -> usize {
    self.slots.iter().map(|s| s.buddy.region_bytes()).sum()
  }

  /// Locks every region into RAM (§4.2, BUG-1), so content a strict volume backs never swaps.
  /// Idempotent: a region already locked stays locked. Returns [`MemError::LockRefused`] if the OS
  /// refuses (a working-set or `RLIMIT_MEMLOCK` limit), so a strict guarantee that cannot be met
  /// refuses rather than silently becoming swappable service. Regions locked before the refusal
  /// stay locked; the caller unwinds by refusing the admission.
  pub fn lock(&mut self) -> Result<(), MemError> {
    for slot in &mut self.slots {
      slot.region.lock()?;
    }
    Ok(())
  }

  /// Locked bytes across regions.
  pub fn locked_bytes(&self) -> usize {
    self
      .slots
      .iter()
      .filter(|s| s.region.locked())
      .map(|s| s.region.len())
      .sum()
  }

  /// The block an allocation of `len` bytes takes — the smallest power-of-two number of granules holding
  /// it, the buddy's rounding (`Buddy::order_for`) — so a caller charges exactly what [`alloc`](Self::alloc)
  /// will take before it allocates (§4.2 "allocator rounding"). `None` when the block is not representable.
  pub fn block_len(&self, len: usize) -> Option<usize> {
    len
      .max(1)
      .div_ceil(self.granule)
      .checked_next_power_of_two()?
      .checked_mul(self.granule)
  }

  /// Allocates at least `len` bytes from the first region that can serve it.
  pub fn alloc(&mut self, len: usize) -> Result<Extent, MemError> {
    let arena = self
      .identity
      .ok_or(MemError::GenerationExhausted { index: u32::MAX })?;
    let mut largest = 0;
    for (index, slot) in self.slots.iter_mut().enumerate() {
      let Ok(region) = u16::try_from(index) else {
        break;
      };
      match slot.buddy.alloc(len) {
        Ok(block) => {
          self.allocated_bytes = self.allocated_bytes.saturating_add(block.len());
          return Ok(Extent {
            arena,
            region,
            block,
          });
        }
        Err(MemError::ArenaExhausted { largest_free, .. }) => largest = largest.max(largest_free),
        Err(MemError::TooLarge { .. }) => largest = largest.max(slot.buddy.largest_free()),
        Err(e) => return Err(e),
      }
    }
    Err(self.exhausted(len, largest))
  }

  /// The refusal when no region served `len`: too large for any region, or exhausted.
  fn exhausted(&self, len: usize, largest: usize) -> MemError {
    let max = self
      .slots
      .iter()
      .map(|s| s.buddy.region_bytes())
      .max()
      .unwrap_or(0);
    if len > max && max > 0 {
      MemError::TooLarge { len, max }
    } else {
      MemError::ArenaExhausted {
        requested: len,
        largest_free: largest,
      }
    }
  }

  /// Frees an extent this arena issued for a live allocation; anything else is refused
  /// ([`MemError::ForeignExtent`]) with every total unchanged.
  pub fn free(&mut self, extent: Extent) -> Result<(), MemError> {
    let foreign = |reason| MemError::ForeignExtent {
      offset: extent.offset(),
      len: extent.len(),
      reason,
    };
    if self.identity != Some(extent.arena) {
      return Err(foreign(ExtentRefusal::OtherArena));
    }
    let slot = self
      .slots
      .get_mut(usize::from(extent.region))
      .ok_or_else(|| foreign(ExtentRefusal::NoSuchRegion))?;
    let before = slot.buddy.free_bytes();
    slot.buddy.free_or_defer(extent.block)?;
    let released = slot.buddy.free_bytes().saturating_sub(before);
    self.allocated_bytes = self.allocated_bytes.saturating_sub(released);
    Ok(())
  }

  /// Allocates exactly the block of `len` bytes at `offset` in region `region` (A-64: a recovery rebuilding
  /// the blocks its image names). Refused as [`crate::buddy::Buddy::claim`] refuses, or `NoSuchRegion`.
  pub fn claim(&mut self, region: u16, offset: usize, len: usize) -> Result<Extent, MemError> {
    let arena = self
      .identity
      .ok_or(MemError::GenerationExhausted { index: u32::MAX })?;
    let slot = self
      .slots
      .get_mut(usize::from(region))
      .ok_or(MemError::ForeignExtent {
        offset,
        len,
        reason: ExtentRefusal::NoSuchRegion,
      })?;
    let block = slot.buddy.claim(offset, len)?;
    self.allocated_bytes = self.allocated_bytes.saturating_add(block.len());
    Ok(Extent {
      arena,
      region,
      block,
    })
  }

  /// Whether `count` blocks of `len` bytes can be allocated now, across the regions (T-1.1: a whole-value write is
  /// checked before anything changes).
  pub fn can_allocate(&self, count: usize, len: usize) -> bool {
    let mut left = count;
    for slot in &self.slots {
      left = left.saturating_sub(slot.buddy.allocatable(left, len));
    }
    left == 0
  }

  /// Whether `extent` names a live block of this arena that is not waiting on a deferred free (A-64: the
  /// recovery sweep frees a claimed block only once).
  pub fn holds(&self, extent: Extent) -> bool {
    self.identity == Some(extent.arena)
      && self
        .slots
        .get(usize::from(extent.region))
        .is_some_and(|slot| slot.buddy.holds(extent.block))
  }

  /// Bytes freed but held until the next publication commits, because the committed recovery image may name
  /// them (A-64). An allocation refused while this is not zero can succeed after a publication.
  pub fn deferred_bytes(&self) -> usize {
    self.slots.iter().map(|s| s.buddy.deferred_bytes()).sum()
  }

  /// A recovery publication starts: every live block it may name is recorded, and a free of one is deferred
  /// until the publication commits or is abandoned (A-64).
  pub fn capture(&mut self) {
    self.slots.iter_mut().for_each(|s| s.buddy.capture());
  }

  /// The publication of the last [`ChunkArena::capture`] committed: frees every deferred block the new image
  /// cannot name.
  pub fn commit_capture(&mut self) {
    self.settle(Buddy::commit_capture);
  }

  /// The publication of the last [`ChunkArena::capture`] did not commit: frees what only it could have named.
  pub fn abandon_capture(&mut self) {
    self.settle(Buddy::abandon_capture);
  }

  /// Every live block is named by the committed image: the state after a recovery claimed its image's blocks.
  pub fn commit_live(&mut self) {
    self.settle(Buddy::commit_live);
  }

  /// Runs `step` on every region's allocator and takes the bytes it released off the allocated total.
  fn settle(&mut self, step: fn(&mut Buddy)) {
    for slot in &mut self.slots {
      let before = slot.buddy.free_bytes();
      step(&mut slot.buddy);
      let released = slot.buddy.free_bytes().saturating_sub(before);
      self.allocated_bytes = self.allocated_bytes.saturating_sub(released);
    }
  }

  /// The bytes the arena's regions map — the address space taken, which the buddy's usable
  /// [`ChunkArena::capacity`] is at most (§4.2 "segment, slab and buddy geometry report usable
  /// capacity, not mapping length": both are reported, so the difference is visible).
  pub fn mapped_bytes(&self) -> usize {
    self.slots.iter().map(|s| s.region.len()).sum()
  }

  /// The bytes of an extent.
  pub fn bytes(&self, extent: Extent) -> Option<&[u8]> {
    let slot = self.slots.get(usize::from(extent.region))?;
    slot
      .region
      .bytes()
      .get(extent.offset()..extent.offset().checked_add(extent.len())?)
  }

  /// The bytes of an extent, mutably.
  pub fn bytes_mut(&mut self, extent: Extent) -> Option<&mut [u8]> {
    let slot = self.slots.get_mut(usize::from(extent.region))?;
    slot
      .region
      .bytes_mut()
      .get_mut(extent.offset()..extent.offset().checked_add(extent.len())?)
  }

  /// A region, read-only.
  pub fn region(&self, index: u16) -> Option<&Region> {
    self.slots.get(usize::from(index)).map(|s| &s.region)
  }

  /// A region, for the locking sequence and the pre-fault scheduler.
  pub fn region_mut(&mut self, index: u16) -> Option<&mut Region> {
    self
      .slots
      .get_mut(usize::from(index))
      .map(|s| &mut s.region)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn page() -> usize {
    if cfg!(miri) {
      // Miri cannot call sysctl; a common base page is enough for the arithmetic under test.
      return 4096;
    }
    usize::try_from(slates_machine::facts::Facts::query().page.base).unwrap()
  }

  fn two_regions(p: usize) -> ChunkArena {
    let mut arena = ChunkArena::new(p);
    arena
      .add_region(Region::map(p * 4, p, false).unwrap())
      .unwrap();
    arena
      .add_region(Region::map(p * 4, p, false).unwrap())
      .unwrap();
    arena
  }

  /// §4.2 allocator rounding: do: allocate every length from one byte to four granules; expect each block to
  /// be exactly the length `block_len` charged for it before the allocation.
  #[test]
  fn block_len_is_the_block_alloc_takes() {
    let p = page();
    let mut arena = two_regions(p);
    for len in 1..=p * 4 {
      let extent = arena.alloc(len).unwrap();
      assert_eq!(Some(extent.len()), arena.block_len(len), "len {len}");
      arena.free(extent).unwrap();
    }
  }

  #[test]
  fn extents_come_from_regions_in_order() {
    let p = page();
    let mut arena = two_regions(p);
    assert_eq!(arena.regions(), 2);
    let a = arena.alloc(p * 3).unwrap();
    assert_eq!((a.region(), a.offset(), a.len()), (0, 0, p * 4));
    let b = arena.alloc(p).unwrap();
    assert_eq!(b.region(), 1);
    assert_eq!(arena.allocated_bytes(), p * 5);
    arena.bytes_mut(b).unwrap()[0] = 9;
    assert_eq!(arena.bytes(b).unwrap()[0], 9);
    arena.free(a).unwrap();
    arena.free(b).unwrap();
    assert_eq!(arena.allocated_bytes(), 0);
    assert_eq!(arena.free_bytes(), p * 8);
    assert_eq!(arena.locked_bytes(), 0);
  }

  #[test]
  fn exhaustion_names_the_largest_free_extent_and_oversize_is_too_large() {
    let p = page();
    let mut arena = two_regions(p);
    let _a = arena.alloc(p * 4).unwrap();
    let _b = arena.alloc(p).unwrap();
    let refused = arena.alloc(p * 4);
    assert!(
      matches!(refused, Err(MemError::ArenaExhausted { largest_free, .. }) if largest_free == p * 2),
      "{refused:?}"
    );
    assert!(matches!(arena.alloc(p * 8), Err(MemError::TooLarge { .. })));
  }

  #[test]
  fn a_region_smaller_than_a_granule_is_refused() {
    let p = page();
    let mut arena = ChunkArena::new(p * 2);
    assert!(matches!(
      arena.add_region(Region::map(p, p, false).unwrap()),
      Err(MemError::TooLarge { .. })
    ));
    assert!(matches!(
      arena.alloc(1),
      Err(MemError::ArenaExhausted {
        largest_free: 0,
        ..
      })
    ));
  }

  /// A-64 at the arena. Do: allocate a block in the second region, commit a capture, free it, and claim its
  /// neighbour in a fresh arena's same region. Expect: the free is deferred (still allocated, counted
  /// deferred), the next commit releases it; the claim lands exactly where named, and a claim in a region the
  /// arena lacks is refused `NoSuchRegion`.
  #[test]
  fn a_deferred_free_stays_allocated_until_the_commit_and_a_claim_lands_where_named() {
    let p = page();
    let mut arena = two_regions(p);
    let _fill = arena.alloc(p * 4).unwrap();
    let block = arena.alloc(p).unwrap();
    assert_eq!(block.region(), 1);
    arena.capture();
    arena.commit_capture();
    arena.free(block).unwrap();
    assert_eq!(arena.deferred_bytes(), p);
    assert_eq!(
      arena.allocated_bytes(),
      p * 5,
      "a deferred block is still allocated"
    );
    arena.capture();
    arena.commit_capture();
    assert_eq!(arena.deferred_bytes(), 0);
    assert_eq!(arena.allocated_bytes(), p * 4);

    let mut rebuilt = two_regions(p);
    let claimed = rebuilt.claim(1, p * 2, p * 2).unwrap();
    assert_eq!(
      (claimed.region(), claimed.offset(), claimed.len()),
      (1, p * 2, p * 2)
    );
    assert_eq!(rebuilt.allocated_bytes(), p * 2);
    assert!(matches!(
      rebuilt.claim(2, 0, p),
      Err(MemError::ForeignExtent {
        reason: ExtentRefusal::NoSuchRegion,
        ..
      })
    ));
  }
}
