//! The chunk arena: buddy allocation over one shard's locked regions, addressing bytes by
//! (region, offset, length) and never by a raw pointer that leaves the owning shard's frame
//! (§4.2, `ChunkArena`).
//!
//! Regions are added as the reserve provides them (mapped, pre-faulted, locked in priority
//! order); allocation tries each region's buddy tree in order and refuses with
//! `ArenaExhausted` naming the largest free extent when none fits, so the caller can grow or
//! refuse in turn. Nothing here reaches the system allocator after the region set is built
//! (AC-0.4); a region's buddy state is allocated once when the region is added.

use crate::buddy::{Block, Buddy};
use crate::error::MemError;
use crate::region::Region;

/// An extent within a region.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Extent {
  /// The region's index within the arena.
  pub region: u16,
  /// Byte offset within the region.
  pub offset: usize,
  /// Length in bytes (the allocated block, which may exceed what was asked).
  pub len: usize,
}

#[derive(Debug)]
struct Slot {
  region: Region,
  buddy: Buddy,
}

/// The arena.
#[derive(Debug)]
pub struct ChunkArena {
  slots: Vec<Slot>,
  granule: usize,
  allocated_bytes: usize,
}

impl ChunkArena {
  /// An arena whose blocks are multiples of `granule` bytes (the base page, from the profile).
  pub fn new(granule: usize) -> Self {
    Self {
      slots: Vec::new(),
      granule: granule.max(1),
      allocated_bytes: 0,
    }
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
      buddy: Buddy::new(self.granule, max_order),
    });
    Ok(index)
  }

  /// Regions in the arena.
  pub fn regions(&self) -> usize {
    self.slots.len()
  }

  /// Bytes currently allocated.
  pub const fn allocated_bytes(&self) -> usize {
    self.allocated_bytes
  }

  /// Free bytes across regions.
  pub fn free_bytes(&self) -> usize {
    self.slots.iter().map(|s| s.buddy.free_bytes()).sum()
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

  /// Allocates at least `len` bytes from the first region that can serve it.
  pub fn alloc(&mut self, len: usize) -> Result<Extent, MemError> {
    let mut largest = 0;
    for (index, slot) in self.slots.iter_mut().enumerate() {
      match slot.buddy.alloc(len) {
        Ok(block) => {
          self.allocated_bytes += block.len;
          return Ok(Extent {
            region: u16::try_from(index).unwrap_or(u16::MAX),
            offset: block.offset,
            len: block.len,
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

  /// Frees an extent.
  pub fn free(&mut self, extent: Extent) -> Result<(), MemError> {
    let slot = self
      .slots
      .get_mut(usize::from(extent.region))
      .ok_or(MemError::TooLarge {
        len: extent.len,
        max: 0,
      })?;
    slot.buddy.free(Block {
      offset: extent.offset,
      len: extent.len,
    })?;
    self.allocated_bytes -= extent.len;
    Ok(())
  }

  /// The bytes of an extent.
  pub fn bytes(&self, extent: Extent) -> Option<&[u8]> {
    let slot = self.slots.get(usize::from(extent.region))?;
    slot
      .region
      .bytes()
      .get(extent.offset..extent.offset.checked_add(extent.len)?)
  }

  /// The bytes of an extent, mutably.
  pub fn bytes_mut(&mut self, extent: Extent) -> Option<&mut [u8]> {
    let slot = self.slots.get_mut(usize::from(extent.region))?;
    slot
      .region
      .bytes_mut()
      .get_mut(extent.offset..extent.offset.checked_add(extent.len)?)
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

  #[test]
  fn extents_come_from_regions_in_order() {
    let p = page();
    let mut arena = two_regions(p);
    assert_eq!(arena.regions(), 2);
    let a = arena.alloc(p * 3).unwrap();
    assert_eq!((a.region, a.offset, a.len), (0, 0, p * 4));
    let b = arena.alloc(p).unwrap();
    assert_eq!(b.region, 1);
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
}
