//! The chunk arena: buddy allocation over one shard's locked regions, addressing bytes by
//! (region, offset, length) and never by a raw pointer that leaves the owning shard's frame
//! (§4.2, `ChunkArena`).
//!
//! Regions are added as the reserve provides them (mapped, pre-faulted, locked in priority
//! order); allocation tries each region's buddy tree in order and refuses with
//! `ArenaExhausted` naming the largest free extent when none fits, so the caller can grow or
//! refuse in turn. Nothing here reaches the system allocator after the region set is built
//! (AC-0.4); a region's buddy state is allocated once when the region is added.
//!
//! **Regions by id, and growth (A-98).** A region is held under a `u16` id that an extent carries and a recovery
//! image names, so a region may be added under a chosen id ([`ChunkArena::add_region_at`]) and removed once wholly
//! free ([`ChunkArena::remove_region`]) without renaming any other block. An arena may own an [`ExtentSource`]: the
//! shared pool a shard's arena grows from when its budget would refuse ([`ChunkArena::grow`]), and returns an extent
//! to once the extent is wholly free. Allocation tries the held regions in the order they were added.

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

/// Zeroes the `len` bytes at `offset` of a block just returned to the free lists (A-99: zero on free, as Linux's
/// `init_on_free=1` hardening does for the page allocator). A freed block that held a file's plaintext, a deleted
/// file's or a block a seal moved out of an image, would otherwise keep it in RAM until reused. A block whose free is
/// deferred keeps its bytes until the commit that releases it: the recovery image may still read them.
/// A deferred block released at last: zeroed, and unlocked when it was locked.
fn release_block(region: &mut Region, offset: usize, len: usize, locked: bool) {
  scrub(region, offset, len);
  if locked {
    region.unlock_range(offset, len);
  }
}

fn scrub(region: &mut Region, offset: usize, len: usize) {
  if let Some(bytes) = offset
    .checked_add(len)
    .and_then(|end| region.bytes_mut().get_mut(offset..end))
  {
    bytes.fill(0);
  }
}

/// Where an arena's further regions come from (A-98): a pool of extents shared with other arenas, each handed out
/// to one arena at a time under a stable id.
pub trait ExtentSource: Send {
  /// One more extent for this arena, with the id its blocks are named by, or `None` when the pool has none free.
  fn claim(&mut self) -> Option<(u16, Region)>;
  /// Returns extent `id`, wholly free, to the pool.
  fn release(&mut self, id: u16, region: Region);
  /// Claims the pool took but could not hand out (the OS refused to map the extent), so far: a claim refused this
  /// way is counted here rather than lost.
  fn refused(&self) -> u64;
  /// The bytes of free extents this source could still hand out now, as an arena would hold them: what an admission
  /// may count on beyond the arena's own capacity. A snapshot: another arena may claim them first.
  fn available(&self) -> usize;
}

/// The arena.
pub struct ChunkArena {
  /// This arena's identity, or `None` when the process has spent them (it then refuses every allocation).
  identity: Option<u64>,
  /// Regions by id; `None` where the arena holds no region of that id.
  slots: Vec<Option<Slot>>,
  /// The held ids, in the order they were added: the order allocation tries them.
  order: Vec<u16>,
  granule: usize,
  allocated_bytes: usize,
  /// The pool this arena grows from, when it has one.
  source: Option<Box<dyn ExtentSource>>,
}

impl std::fmt::Debug for ChunkArena {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ChunkArena")
      .field("identity", &self.identity)
      .field("order", &self.order)
      .field("granule", &self.granule)
      .field("allocated_bytes", &self.allocated_bytes)
      .field("source", &self.source.is_some())
      .finish_non_exhaustive()
  }
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
      order: Vec::new(),
      granule: granule.max(1),
      allocated_bytes: 0,
      source: None,
    }
  }

  /// Sets the pool this arena grows from (A-98).
  pub fn set_source(&mut self, source: Box<dyn ExtentSource>) {
    self.source = Some(source);
  }

  /// Draws extents from the source until at least `bytes` of capacity were added or the source has none: the
  /// capacity added (A-98). Zero without a source.
  pub fn grow(&mut self, bytes: usize) -> usize {
    let mut added = 0usize;
    while added < bytes {
      let Some((id, region)) = self.source.as_mut().and_then(|source| source.claim()) else {
        break;
      };
      let before = self.capacity();
      if self.add_region_at(id, region).is_err() {
        // A source that hands out an id this arena holds, or a region under a granule, is refused; the claim is
        // not the arena's to keep, and the pool sees no release for it, so stop rather than spin.
        break;
      }
      // A new region holds no block, so nothing of it is locked until a block is allocated there.
      added = added.saturating_add(self.capacity().saturating_sub(before));
    }
    added
  }

  /// Grows from the source by what a commitment of `bytes` lacks in `budget`, and extends `budget` by what was added
  /// (A-98): the bytes added. Nothing grows when the commitment is admittable already; the deficit counts the
  /// budget's pressure hold ([`crate::budget::ShardBudget::deficit`]).
  pub fn make_room(&mut self, budget: &mut crate::budget::ShardBudget, bytes: u64) -> u64 {
    if bytes == 0 {
      return 0;
    }
    let deficit = budget.deficit(bytes);
    if deficit == 0 {
      return 0;
    }
    let added =
      u64::try_from(self.grow(usize::try_from(deficit).unwrap_or(usize::MAX))).unwrap_or(u64::MAX);
    budget.extend(added);
    added
  }

  /// Returns every held region that is wholly free (no live or deferred block) to the source, keeping
  /// `keep_bytes` of capacity: the capacity returned (A-98). Zero without a source.
  pub fn shrink(&mut self, keep_bytes: usize) -> usize {
    if self.source.is_none() {
      return 0;
    }
    let mut returned = 0usize;
    let idle: Vec<u16> = self
      .order
      .iter()
      .rev()
      .copied()
      .filter(|id| {
        self.slot(*id).is_some_and(|slot| {
          slot.buddy.free_bytes() == slot.buddy.region_bytes() && slot.buddy.deferred_bytes() == 0
        })
      })
      .collect();
    for id in idle {
      let capacity = self.capacity();
      let Some(len) = self.slot(id).map(|slot| slot.buddy.region_bytes()) else {
        continue;
      };
      if capacity.saturating_sub(len) < keep_bytes {
        continue;
      }
      if let Ok(region) = self.remove_region(id)
        && let Some(source) = self.source.as_mut()
      {
        source.release(id, region);
        returned = returned.saturating_add(len);
      }
    }
    returned
  }

  /// The bytes this arena could still claim from its source ([`ExtentSource::available`]); zero without a source.
  pub fn claimable(&self) -> usize {
    self.source.as_ref().map_or(0, |source| source.available())
  }

  /// Claims the source could not hand out so far ([`ExtentSource::refused`]); zero without a source.
  pub fn source_refusals(&self) -> u64 {
    self.source.as_ref().map_or(0, |source| source.refused())
  }

  fn slot(&self, id: u16) -> Option<&Slot> {
    self.slots.get(usize::from(id)).and_then(Option::as_ref)
  }

  fn slot_mut(&mut self, id: u16) -> Option<&mut Slot> {
    self.slots.get_mut(usize::from(id)).and_then(Option::as_mut)
  }

  /// The held regions' allocators.
  fn held(&self) -> impl Iterator<Item = &Slot> {
    self.slots.iter().flatten()
  }

  /// The held regions' allocators, mutably.
  fn held_mut(&mut self) -> impl Iterator<Item = &mut Slot> {
    self.slots.iter_mut().flatten()
  }

  /// The granule: every block is a power-of-two multiple of it.
  pub const fn granule(&self) -> usize {
    self.granule
  }

  /// Adds a region; its length is used up to the largest power-of-two number of granules it
  /// holds. Returns the region's index.
  pub fn add_region(&mut self, region: Region) -> Result<u16, MemError> {
    let index = u16::try_from(self.slots.len()).map_err(|_| MemError::TooLarge {
      len: self.slots.len(),
      max: usize::from(u16::MAX),
    })?;
    self.add_region_at(index, region)?;
    Ok(index)
  }

  /// Adds a region under id `id` (A-98: a pool extent, whose id its blocks are named by). Refused
  /// [`MemError::RegionOccupied`] when the arena holds that id, or as [`ChunkArena::add_region`] refuses.
  pub fn add_region_at(&mut self, id: u16, region: Region) -> Result<(), MemError> {
    let granules = region.len().checked_div(self.granule).unwrap_or(0);
    if granules == 0 {
      return Err(MemError::TooLarge {
        len: region.len(),
        max: self.granule,
      });
    }
    if self.slot(id).is_some() {
      return Err(MemError::RegionOccupied { region: id });
    }
    // `granules >= 1`, so it has a top bit: `ilog2` is its order.
    let max_order = granules.ilog2();
    let slot = Slot {
      region,
      buddy: Buddy::new(self.granule, max_order)?,
    };
    let at = usize::from(id);
    if self.slots.len() <= at {
      self.slots.resize_with(at.saturating_add(1), || None);
    }
    if let Some(place) = self.slots.get_mut(at) {
      *place = Some(slot);
      self.order.push(id);
    }
    Ok(())
  }

  /// Removes region `id` and hands its mapping back (A-98: an extent returned to the pool). Refused
  /// [`MemError::RegionInUse`] while it holds a live or deferred block, or `NoSuchRegion` when the arena holds no
  /// such region.
  pub fn remove_region(&mut self, id: u16) -> Result<Region, MemError> {
    let slot = self.slot(id).ok_or(MemError::ForeignExtent {
      offset: 0,
      len: 0,
      reason: ExtentRefusal::NoSuchRegion,
    })?;
    let allocated = slot
      .buddy
      .region_bytes()
      .saturating_sub(slot.buddy.free_bytes());
    if allocated > 0 || slot.buddy.deferred_bytes() > 0 {
      return Err(MemError::RegionInUse {
        region: id,
        allocated: allocated.max(slot.buddy.deferred_bytes()),
      });
    }
    let removed = self
      .slots
      .get_mut(usize::from(id))
      .and_then(Option::take)
      .ok_or(MemError::ForeignExtent {
        offset: 0,
        len: 0,
        reason: ExtentRefusal::NoSuchRegion,
      })?;
    self.order.retain(|held| *held != id);
    while self.slots.last().is_some_and(Option::is_none) {
      self.slots.pop();
    }
    Ok(removed.region)
  }

  /// Regions the arena holds.
  pub fn regions(&self) -> usize {
    self.order.len()
  }

  /// The held region ids, in the order allocation tries them.
  pub fn region_ids(&self) -> &[u16] {
    &self.order
  }

  /// Bytes currently allocated, deferred frees included (A-64: they stay allocated until a publication commits).
  pub const fn allocated_bytes(&self) -> usize {
    self.allocated_bytes
  }

  /// Free bytes across regions.
  pub fn free_bytes(&self) -> usize {
    self.held().map(|s| s.buddy.free_bytes()).sum()
  }

  /// The usable (buddy-allocatable) capacity across regions: the bytes that can actually be
  /// handed out, which is each region's largest power-of-two number of granules — not its mapping
  /// length. Admission must reserve against this, never the mapping, or it over-promises quota the
  /// arena cannot physically back (§4.2, BUG-2). Constant for the life of the arena's regions.
  pub fn capacity(&self) -> usize {
    self.held().map(|s| s.buddy.region_bytes()).sum()
  }

  /// Locks the live `extent` in RAM (§4.2 D-12): a strict volume's content never swaps. A block is a page multiple
  /// on a page boundary, so its pages are its own. Idempotent; refused [`MemError::LockRefused`] by the OS (its
  /// locked-memory limit), the block then unlocked and usable, or [`MemError::ForeignExtent`] for one this arena did
  /// not issue.
  pub fn lock_extent(&mut self, extent: Extent) -> Result<(), MemError> {
    let foreign = |reason| MemError::ForeignExtent {
      offset: extent.offset(),
      len: extent.len(),
      reason,
    };
    if self.identity != Some(extent.arena) {
      return Err(foreign(ExtentRefusal::OtherArena));
    }
    let slot = self
      .slot_mut(extent.region)
      .ok_or_else(|| foreign(ExtentRefusal::NoSuchRegion))?;
    if slot.buddy.is_locked(extent.block) {
      return Ok(());
    }
    slot.region.lock_range(extent.offset(), extent.len())?;
    if let Err(e) = slot.buddy.lock_block(extent.block) {
      slot.region.unlock_range(extent.offset(), extent.len());
      return Err(e);
    }
    Ok(())
  }

  /// Whether the live `extent` is locked in RAM.
  pub fn is_locked(&self, extent: Extent) -> bool {
    self.identity == Some(extent.arena)
      && self
        .slot(extent.region)
        .is_some_and(|slot| slot.buddy.is_locked(extent.block))
  }

  /// The bytes of the blocks locked in RAM: a strict volume's content, never the address space mapped (a whole-arena
  /// lock wired a shard's 16 GiB for one 4 MiB strict volume on macOS, measured 2026-10-06;
  /// `docs/bugs/2026-10-06-a-locked-volume-wired-its-shards-whole-arena.md`).
  pub fn locked_bytes(&self) -> usize {
    self.held().map(|slot| slot.buddy.locked_bytes()).sum()
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
    let Self {
      order,
      slots,
      allocated_bytes,
      ..
    } = self;
    for &region in order.iter() {
      let Some(slot) = slots.get_mut(usize::from(region)).and_then(Option::as_mut) else {
        continue;
      };
      match slot.buddy.alloc(len) {
        Ok(block) => {
          *allocated_bytes = allocated_bytes.saturating_add(block.len());
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

  /// [`ChunkArena::alloc`], locked in RAM before it is handed out (a strict volume's new content). One the OS will not
  /// lock goes back, and the refusal is the lock's.
  pub fn alloc_locked(&mut self, len: usize) -> Result<Extent, MemError> {
    let extent = self.alloc(len)?;
    if let Err(e) = self.lock_extent(extent) {
      self.free(extent)?;
      return Err(e);
    }
    Ok(extent)
  }

  /// The refusal when no region served `len`: too large for any region, or exhausted.
  fn exhausted(&self, len: usize, largest: usize) -> MemError {
    let max = self
      .held()
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
    self.release_extent(extent, true)
  }

  /// Gives back a block a recovery claimed and could not use (A-64), its bytes untouched: a give-back returns the arena
  /// to how it was before the claim, and those bytes are content that survived the restart, which a later recovery of
  /// the same image may still claim. [`ChunkArena::free`] scrubs instead, for bytes no one will read again.
  pub fn give_back(&mut self, extent: Extent) -> Result<(), MemError> {
    self.release_extent(extent, false)
  }

  /// [`ChunkArena::free`] or [`ChunkArena::give_back`]: the block released, zeroed when `scrubbed` and released now.
  fn release_extent(&mut self, extent: Extent, scrubbed: bool) -> Result<(), MemError> {
    let foreign = |reason| MemError::ForeignExtent {
      offset: extent.offset(),
      len: extent.len(),
      reason,
    };
    if self.identity != Some(extent.arena) {
      return Err(foreign(ExtentRefusal::OtherArena));
    }
    let slot = self
      .slot_mut(extent.region)
      .ok_or_else(|| foreign(ExtentRefusal::NoSuchRegion))?;
    let before = slot.buddy.free_bytes();
    let locked = slot.buddy.is_locked(extent.block);
    let deferred = slot.buddy.free_or_defer(extent.block)?;
    if scrubbed && !deferred {
      scrub(&mut slot.region, extent.offset(), extent.len());
    }
    if locked && !deferred {
      slot.region.unlock_range(extent.offset(), extent.len());
    }
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
    let slot = self.slot_mut(region).ok_or(MemError::ForeignExtent {
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
    for slot in self.held() {
      left = left.saturating_sub(slot.buddy.allocatable(left, len));
    }
    left == 0
  }

  /// Whether `extent` names a live block of this arena that is not waiting on a deferred free (A-64: the
  /// recovery sweep frees a claimed block only once).
  pub fn holds(&self, extent: Extent) -> bool {
    self.identity == Some(extent.arena)
      && self
        .slot(extent.region)
        .is_some_and(|slot| slot.buddy.holds(extent.block))
  }

  /// Whether a recovery image may name `extent` (A-64), so its bytes must not change meaning before the next commit: a
  /// seal then writes the chunk elsewhere and defers the old block's free (A-99).
  pub fn imaged(&self, extent: Extent) -> bool {
    self.identity == Some(extent.arena)
      && self
        .slot(extent.region)
        .is_some_and(|slot| slot.buddy.imaged(extent.block))
  }

  /// Bytes freed but held until the next publication commits, because the committed recovery image may name
  /// them (A-64). An allocation refused while this is not zero can succeed after a publication.
  pub fn deferred_bytes(&self) -> usize {
    self.held().map(|s| s.buddy.deferred_bytes()).sum()
  }

  /// A recovery publication starts: every live block it may name is recorded, and a free of one is deferred
  /// until the publication commits or is abandoned (A-64).
  pub fn capture(&mut self) {
    self.held_mut().for_each(|s| s.buddy.capture());
  }

  /// The publication of the last [`ChunkArena::capture`] committed: frees every deferred block the new image
  /// cannot name.
  pub fn commit_capture(&mut self) {
    let mut released_total = 0usize;
    for slot in self.slots.iter_mut().flatten() {
      let before = slot.buddy.free_bytes();
      let Slot { region, buddy } = slot;
      buddy
        .commit_capture_releasing(|offset, len, locked| release_block(region, offset, len, locked));
      released_total =
        released_total.saturating_add(slot.buddy.free_bytes().saturating_sub(before));
    }
    self.allocated_bytes = self.allocated_bytes.saturating_sub(released_total);
  }

  /// The publication of the last [`ChunkArena::capture`] did not commit: frees what only it could have named.
  pub fn abandon_capture(&mut self) {
    let mut released_total = 0usize;
    for slot in self.slots.iter_mut().flatten() {
      let before = slot.buddy.free_bytes();
      let Slot { region, buddy } = slot;
      buddy.abandon_capture_releasing(|offset, len, locked| {
        release_block(region, offset, len, locked)
      });
      released_total =
        released_total.saturating_add(slot.buddy.free_bytes().saturating_sub(before));
    }
    self.allocated_bytes = self.allocated_bytes.saturating_sub(released_total);
  }

  /// Every live block is named by the committed image: the state after a recovery claimed its image's blocks.
  pub fn commit_live(&mut self) {
    self.settle(Buddy::commit_live);
  }

  /// Runs `step` on every region's allocator and takes the bytes it released off the allocated total.
  fn settle(&mut self, step: fn(&mut Buddy)) {
    let mut released_total = 0usize;
    for slot in self.slots.iter_mut().flatten() {
      let before = slot.buddy.free_bytes();
      step(&mut slot.buddy);
      released_total =
        released_total.saturating_add(slot.buddy.free_bytes().saturating_sub(before));
    }
    self.allocated_bytes = self.allocated_bytes.saturating_sub(released_total);
  }

  /// The bytes the arena's regions map — the address space taken, which the buddy's usable
  /// [`ChunkArena::capacity`] is at most (§4.2 "segment, slab and buddy geometry report usable
  /// capacity, not mapping length": both are reported, so the difference is visible).
  pub fn mapped_bytes(&self) -> usize {
    self.held().map(|s| s.region.len()).sum()
  }

  /// The bytes of an extent.
  pub fn bytes(&self, extent: Extent) -> Option<&[u8]> {
    let slot = self.slot(extent.region)?;
    slot
      .region
      .bytes()
      .get(extent.offset()..extent.offset().checked_add(extent.len())?)
  }

  /// The bytes of an extent, mutably.
  pub fn bytes_mut(&mut self, extent: Extent) -> Option<&mut [u8]> {
    let slot = self.slot_mut(extent.region)?;
    slot
      .region
      .bytes_mut()
      .get_mut(extent.offset()..extent.offset().checked_add(extent.len())?)
  }

  /// A region, read-only.
  pub fn region(&self, index: u16) -> Option<&Region> {
    self.slot(index).map(|s| &s.region)
  }

  /// A region, for the locking sequence and the pre-fault scheduler.
  pub fn region_mut(&mut self, index: u16) -> Option<&mut Region> {
    self.slot_mut(index).map(|s| &mut s.region)
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

  /// A pool of regions under chosen ids: hands out the last returned first, as the anchor's pool may.
  struct TestPool {
    free: Vec<(u16, Region)>,
  }

  impl ExtentSource for TestPool {
    fn claim(&mut self) -> Option<(u16, Region)> {
      self.free.pop()
    }
    fn release(&mut self, id: u16, region: Region) {
      self.free.push((id, region));
    }
    fn refused(&self) -> u64 {
      0
    }
    fn available(&self) -> usize {
      self.free.iter().map(|(_, region)| region.len()).sum()
    }
  }

  fn pooled(p: usize, ids: &[u16]) -> ChunkArena {
    let mut arena = ChunkArena::new(p);
    arena.set_source(Box::new(TestPool {
      free: ids
        .iter()
        .map(|id| (*id, Region::map(p * 4, p, false).unwrap()))
        .collect(),
    }));
    arena
  }

  /// A-98: do grow an arena from a pool by more than one region's capacity, allocate, then free and shrink; expect the
  /// capacity to grow by whole regions under the pool's ids, an allocation named by the region's id, a region in use
  /// refused removal, the wholly free regions returned except what `keep_bytes` keeps, and a later growth taking the
  /// returned id again.
  #[test]
  fn an_arena_grows_from_its_pool_under_the_pools_ids_and_returns_free_regions() {
    let p = page();
    let mut arena = pooled(p, &[40, 7]);
    assert_eq!(arena.capacity(), 0);
    assert_eq!(arena.grow(p * 5), p * 8, "two whole regions");
    assert_eq!(arena.region_ids(), [7, 40]);
    assert_eq!(arena.grow(p), 0, "the pool is empty");
    let first = arena.alloc(p * 4).unwrap();
    let second = arena.alloc(p).unwrap();
    assert_eq!((first.region(), second.region()), (7, 40));
    assert!(matches!(
      arena.remove_region(40),
      Err(MemError::RegionInUse { region: 40, .. })
    ));
    returns_free_regions_to_the_pool(&mut arena, (first, second), p);
  }

  /// The return half of [`an_arena_grows_from_its_pool_under_the_pools_ids_and_returns_free_regions`]: `first` fills
  /// region 7 and `second` sits in region 40, each of four granules of `p`.
  fn returns_free_regions_to_the_pool(
    arena: &mut ChunkArena,
    (first, second): (Extent, Extent),
    p: usize,
  ) {
    arena.free(second).unwrap();
    assert_eq!(
      arena.shrink(0),
      p * 4,
      "only the wholly free region returns"
    );
    assert_eq!(arena.region_ids(), [7]);
    arena.free(first).unwrap();
    assert_eq!(arena.shrink(p * 4), 0, "keep_bytes keeps the last region");
    assert_eq!(arena.grow(p), p * 4);
    assert_eq!(
      arena.region_ids(),
      [7, 40],
      "the returned id is taken again"
    );
    assert!(matches!(
      arena.add_region_at(7, Region::map(p * 4, p, false).unwrap()),
      Err(MemError::RegionOccupied { region: 7 })
    ));
  }

  /// A-98 recovery: do add a region under a high id to a fresh arena and claim a block in it; expect the claim to land
  /// at the named id and offset, and a free of it to return the arena to empty.
  #[test]
  fn a_block_is_claimed_in_a_region_added_under_its_id() {
    let p = page();
    let mut arena = ChunkArena::new(p);
    arena
      .add_region_at(300, Region::map(p * 4, p, false).unwrap())
      .unwrap();
    let claimed = arena.claim(300, p * 2, p).unwrap();
    assert_eq!((claimed.region(), claimed.offset()), (300, p * 2));
    assert!(arena.holds(claimed));
    arena.free(claimed).unwrap();
    assert_eq!(arena.allocated_bytes(), 0);
    assert!(arena.remove_region(300).is_ok());
    assert_eq!(arena.regions(), 0);
  }
}
