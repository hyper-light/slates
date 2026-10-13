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
  /// The region's id: the extent id its blocks are named by.
  id: u16,
  region: Region,
  buddy: Buddy,
  /// The ranges freed and zeroed in place since the idle purge last ran here: resident pages no allocation holds, which
  /// the purge gives back (A-105). A scrubbing free records its block; an allocation or a claim takes its block out; the
  /// purge drains the set. So it holds only free bytes, the purge touches only what was freed, and a block given back
  /// unscrubbed (a recovery's, whose bytes may be claimed again) is never in it.
  dirty: crate::ranges::UnitSet,
}

/// Zeroes the `len` bytes at `offset` of a block just returned to the free lists (A-99: zero on free, as Linux's
/// `init_on_free=1` hardening does for the page allocator). A freed block that held a file's plaintext, a deleted
/// file's or a block a seal moved out of an image, would otherwise keep it in RAM until reused. A block whose free is
/// deferred keeps its bytes until the commit that releases it: the recovery image may still read them.
/// A block released (now, or a deferred one at last): zeroed in place, and unlocked when it was locked (A-99: zero on
/// free, as Linux's `init_on_free=1` hardening does for the page allocator). A locked block is zeroed while still
/// locked, so its plaintext never reaches swap. Its pages stay resident, ready for the next allocation; an idle arena
/// gives them back ([`ChunkArena::purge`], A-105).
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
  /// Lets this source hand out the extents a pressure hold has withheld, as well as free ones, while `open` (§4.2,
  /// admission.md §5.5: a takeover's recovery runs past the hold, which never revokes a committed claim). A source
  /// with no hold has nothing to open.
  fn open_hold(&mut self, _open: bool) {}
}

/// The arena.
pub struct ChunkArena {
  /// This arena's identity, or `None` when the process has spent them (it then refuses every allocation).
  identity: Option<u64>,
  /// The held regions, dense: every walk of the regions (a publication's capture and commit, the capacity, the idle
  /// release) visits only these. Indexed by id instead, a daemon of N partitions kept up to 64 × N entries, mostly
  /// empty and each a few hundred bytes, and every barrier walked all of them: about 9 µs of a 49 µs unlink on an
  /// 18-partition Linux daemon (2026-10-06).
  slots: Vec<Slot>,
  /// Where each id's region is in [`ChunkArena::slots`]; `None` where the arena holds no region of that id. Its length
  /// is one past the highest id held.
  position: Vec<Option<usize>>,
  /// The held ids, in the order they were added: the order allocation tries them.
  order: Vec<u16>,
  granule: usize,
  allocated_bytes: usize,
  /// The pool this arena grows from, when it has one.
  source: Option<Box<dyn ExtentSource>>,
  /// The smallest free block an idle purge gives back to the OS ([`ChunkArena::discarding_from`]); `usize::MAX`, none.
  discard_from: usize,
  /// Bytes released and zeroed in place since the last complete purge: resident RAM no allocation holds.
  resident_free: usize,
  /// Where an unfinished purge resumes: the region id and the byte within it.
  purge_cursor: Option<(u16, usize)>,
  /// Allocations so far, monotone: a caller that sees it unchanged across an interval knows the arena was idle.
  allocations: u64,
  /// Bytes whose reusable mark an allocation cleared before handing them out (A-110, macOS), monotone: the counter a
  /// test of the give-back asserts moved, so a dead clearing path cannot pass as one that had nothing to clear.
  reused: u64,
  /// Whether recovery publications name this arena's blocks ([`ChunkArena::published`]): each region it adds then keeps
  /// its live heads from the start ([`Buddy::new_imaged`]), so no publication scans a region's state.
  published: bool,
  /// Blocks a live DAX mapping holds (§4.6, AC-4.12: mapping isolation, pinning and teardown). While a block is pinned
  /// the arena never hands it to another allocation and never returns it to the free lists: a [`ChunkArena::free`] of a
  /// pinned block is *deferred* — its bytes kept unscrubbed for the mapping's page-table view — until the last
  /// [`ChunkArena::unpin`]. This is the memory-level invariant behind the gate: a guest's mapping can never fall onto
  /// another file's bytes because the block it names is neither reused nor cleared while mapped (the per-access check a
  /// hardware DAX guest bypasses is thereby made unnecessary). Keyed by the block's physical place `(region, offset)`,
  /// which a reuse would change. Bounded structurally by the arena's block count — one entry per distinct allocated
  /// block, so it cannot grow past what [`ChunkArena::capacity`] holds — and empty on any arena no guest maps.
  pins: std::collections::BTreeMap<(u16, usize), PinnedBlock>,
}

/// One pinned block's state (§4.6): the extent as issued, the count of live DAX mappings holding it, and whether a
/// `free` arrived while it was pinned and is waiting for the last holder to drop.
#[derive(Clone, Copy, Debug)]
struct PinnedBlock {
  /// The extent as issued, so the deferred free can release exactly it when the last holder unpins.
  extent: Extent,
  /// Live DAX mappings holding this block; the block is reusable again only at zero.
  holders: u32,
  /// A `free` of this block arrived while it was pinned and waits for the last unpin.
  free_waiting: bool,
  /// Whether that waiting free is to scrub on release (a [`ChunkArena::free`]) rather than give it back untouched (a
  /// recovery [`ChunkArena::give_back`]).
  scrub_on_release: bool,
}

/// What one [`ChunkArena::purge`] call did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Purged {
  /// Bytes whose pages went back to the OS.
  pub bytes: usize,
  /// Whether free blocks remain to visit: the next call resumes where this one stopped.
  pub more: bool,
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
      position: Vec::new(),
      order: Vec::new(),
      granule: granule.max(1),
      allocated_bytes: 0,
      source: None,
      discard_from: usize::MAX,
      resident_free: 0,
      purge_cursor: None,
      allocations: 0,
      reused: 0,
      published: false,
      pins: std::collections::BTreeMap::new(),
    }
  }

  /// Marks this arena's blocks as named by recovery publications (A-64): every region added from here on keeps its live
  /// heads from the start ([`Buddy::new_imaged`]), so a publication never reads a region's state bytes to find them.
  /// Without it a region's first capture scanned every granule inside whichever verb's shard step published first
  /// (8,388,608 granules, 64.6 ms in a debug build; 2026-10-10). An arena nothing publishes from leaves it unset and
  /// keeps no bitmap.
  pub fn published(mut self) -> Self {
    self.published = true;
    self
  }

  /// Granules the regions' first captures read from their state bytes: zero for a published arena's regions.
  pub fn scanned_granules(&self) -> u64 {
    self
      .slots
      .iter()
      .map(|slot| slot.buddy.scanned_granules())
      .fold(0u64, u64::saturating_add)
  }

  /// Lets an idle purge give free blocks of at least `bytes` back to the OS
  /// (`slates_machine::profile::DerivedConstants::discard_from_bytes`). Without it, free blocks stay resident.
  pub fn discarding_from(mut self, bytes: usize) -> Self {
    self.discard_from = bytes;
    self
  }

  /// Allocations so far (monotone, saturating): unchanged across an interval means the arena allocated nothing in it.
  pub fn allocations(&self) -> u64 {
    self.allocations
  }

  /// Bytes whose reusable mark an allocation cleared before handing them out (A-110; zero where a discard leaves no
  /// mark).
  pub fn reused_bytes(&self) -> u64 {
    self.reused
  }

  /// Bytes released and zeroed since the last complete purge: resident RAM no allocation holds, which
  /// [`ChunkArena::purge`] would give back.
  pub fn resident_free_bytes(&self) -> usize {
    self.resident_free
  }

  /// Gives the pages of free blocks of at least the discard size back to the OS, at most `max_blocks` of them,
  /// resuming where the last call stopped (A-105). Decay-based, as jemalloc purges (Evans, "dirty page decay"): a
  /// block freed under load is zeroed in place and stays resident for the next allocation, which then takes no page
  /// faults (measured: alloc+touch+free of a 64 KiB block costs 0.4 µs zeroed in place against 7 µs given back and
  /// faulted in again), and an idle arena returns what it no longer uses. Nothing to do while no byte was released
  /// since the last complete purge.
  pub fn purge(&mut self, max_blocks: usize) -> Purged {
    if self.resident_free == 0 {
      return Purged::default();
    }
    let discard_from = self.discard_from;
    let mut done = Purged::default();
    let mut visited = 0usize;
    let order = self.order.clone();
    let start = self.purge_cursor.take();
    let mut started = start.is_none();
    for region in order {
      let from = match start {
        Some((at_region, at)) if at_region == region => {
          started = true;
          at
        }
        _ if started => 0,
        _ => continue,
      };
      let Some(slot) = self.slot_mut(region) else {
        continue;
      };
      let mut at = from;
      let mut drained = 0usize;
      let size = slot.region.len();
      while let Some((first, reach)) = slot.dirty.next_run_from(at, size) {
        if visited >= max_blocks {
          self.resident_free = self.resident_free.saturating_sub(drained);
          self.purge_cursor = Some((region, first));
          done.more = true;
          return done;
        }
        visited = visited.saturating_add(1);
        slot.dirty.take(first, reach);
        let len = reach.saturating_sub(first);
        drained = drained.saturating_add(len);
        // A range shorter than the discard size stays resident: keeping it costs less than faulting it back.
        if len >= discard_from && slot.region.discard(first, len) {
          done.bytes = done.bytes.saturating_add(len);
        }
        at = reach;
      }
      self.resident_free = self.resident_free.saturating_sub(drained);
    }
    // Every set is drained: nothing freed is left resident, whatever the count drifted to.
    self.resident_free = 0;
    done
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
  ///
  /// All of the deficit or nothing (§4.2: an admission is refused whole, with nothing changed). A commitment the pool
  /// cannot cover is refused by the admission that follows whatever this arena claims, so it claims nothing for it;
  /// and when another arena claims first, between the look and the claims, what this one took goes back. Before
  /// 2026-10-09 a refused create claimed every free extent on its way to the refusal, and its shard kept them until its
  /// next publication: one oversized request on an idle shard refused every other shard's admissions.
  pub fn make_room(&mut self, budget: &mut crate::budget::ShardBudget, bytes: u64) -> u64 {
    if bytes == 0 {
      return 0;
    }
    let deficit = budget.deficit(bytes);
    if deficit == 0 {
      return 0;
    }
    // Growing from the pool takes on the pressure debt the daemon's sampler left for the next shard to grow (§4.2,
    // `budget::Pressure`): the arena grows by it too, and the budget holds it once grown.
    let needed = deficit.saturating_add(budget.debt());
    if u64::try_from(self.claimable()).unwrap_or(u64::MAX) < needed {
      return 0;
    }
    let before = self.capacity();
    let added =
      u64::try_from(self.grow(usize::try_from(needed).unwrap_or(usize::MAX))).unwrap_or(u64::MAX);
    if added < needed {
      // The claims just taken hold no block, so they are wholly free and go back first (the newest regions are
      // returned first), down to the capacity the arena had before.
      self.shrink(before);
      return 0;
    }
    budget.extend(added);
    budget.absorb_debt();
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

  /// Opens or closes the source's pressure hold to this arena's growth ([`ExtentSource::open_hold`]); nothing without a
  /// source.
  pub fn open_hold(&mut self, open: bool) {
    if let Some(source) = self.source.as_mut() {
      source.open_hold(open);
    }
  }

  /// Claims the source could not hand out so far ([`ExtentSource::refused`]); zero without a source.
  pub fn source_refusals(&self) -> u64 {
    self.source.as_ref().map_or(0, |source| source.refused())
  }

  fn slot(&self, id: u16) -> Option<&Slot> {
    self
      .position
      .get(usize::from(id))
      .copied()
      .flatten()
      .and_then(|at| self.slots.get(at))
  }

  fn slot_mut(&mut self, id: u16) -> Option<&mut Slot> {
    let at = self.position.get(usize::from(id)).copied().flatten()?;
    self.slots.get_mut(at)
  }

  /// The held regions' allocators.
  fn held(&self) -> impl Iterator<Item = &Slot> {
    self.slots.iter()
  }

  /// The held regions' allocators, mutably.
  fn held_mut(&mut self) -> impl Iterator<Item = &mut Slot> {
    self.slots.iter_mut()
  }

  /// The granule: every block is a power-of-two multiple of it.
  pub const fn granule(&self) -> usize {
    self.granule
  }

  /// Adds a region; its length is used up to the largest power-of-two number of granules it
  /// holds. Returns the region's index.
  pub fn add_region(&mut self, region: Region) -> Result<u16, MemError> {
    let index = u16::try_from(self.position.len()).map_err(|_| MemError::TooLarge {
      len: self.position.len(),
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
      id,
      dirty: crate::ranges::UnitSet::new(region.len(), self.granule),
      region,
      buddy: if self.published {
        Buddy::new_imaged(self.granule, max_order)?
      } else {
        Buddy::new(self.granule, max_order)?
      },
    };
    let at = usize::from(id);
    if self.position.len() <= at {
      self.position.resize(at.saturating_add(1), None);
    }
    if let Some(place) = self.position.get_mut(at) {
      *place = Some(self.slots.len());
      self.slots.push(slot);
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
    let at = self
      .position
      .get_mut(usize::from(id))
      .and_then(Option::take)
      .filter(|at| *at < self.slots.len())
      .ok_or(MemError::ForeignExtent {
        offset: 0,
        len: 0,
        reason: ExtentRefusal::NoSuchRegion,
      })?;
    // The last region moves into the freed place, so its position follows it.
    let removed = self.slots.swap_remove(at);
    if let Some(moved) = self.slots.get(at).map(|slot| slot.id)
      && let Some(place) = self.position.get_mut(usize::from(moved))
    {
      *place = Some(at);
    }
    self.order.retain(|held| *held != id);
    while self.position.last().is_some_and(Option::is_none) {
      self.position.pop();
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
    self.allocations = self.allocations.saturating_add(1);
    let arena = self
      .identity
      .ok_or(MemError::GenerationExhausted { index: u32::MAX })?;
    let mut largest = 0;
    let Self {
      order,
      slots,
      position,
      allocated_bytes,
      reused,
      resident_free,
      ..
    } = self;
    for &region in order.iter() {
      let at = position.get(usize::from(region)).copied().flatten();
      let Some(slot) = at.and_then(|at| slots.get_mut(at)) else {
        continue;
      };
      match slot.buddy.alloc(len) {
        Ok(block) => {
          // A block that may carry the OS's reusable mark is cleared before it is handed out, or the kernel could take
          // its pages while they hold data (A-110). One the OS will not clear goes back to the free lists unused.
          match slot.region.prepare(block.offset(), block.len()) {
            Ok(cleared) => {
              *reused = reused.saturating_add(u64::try_from(cleared).unwrap_or(u64::MAX));
              let taken = slot
                .dirty
                .take(block.offset(), block.offset().saturating_add(block.len()));
              *resident_free = resident_free.saturating_sub(taken);
            }
            Err(refused) => {
              slot.buddy.free(block)?;
              return Err(refused);
            }
          }
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
    // A pinned block's free waits for the last unpin (§4.6): its bytes stay readable for the mapping and it is never
    // returned to the free lists, so a guest's page table can never fall onto another file's bytes. The deferred
    // release runs in [`ChunkArena::unpin`] once the last holder drops; `unpin` removes the pin entry before it calls
    // back here, so a deferred free is released exactly once and this never recurses.
    if let Some(pinned) = self.pins.get_mut(&(extent.region(), extent.offset()))
      && pinned.holders > 0
    {
      pinned.free_waiting = true;
      pinned.scrub_on_release = scrubbed;
      return Ok(());
    }
    let mut self_resident = 0usize;
    let slot = self
      .slot_mut(extent.region)
      .ok_or_else(|| foreign(ExtentRefusal::NoSuchRegion))?;
    let before = slot.buddy.free_bytes();
    let locked = slot.buddy.is_locked(extent.block);
    let deferred = slot.buddy.free_or_defer(extent.block)?;
    if !deferred {
      if scrubbed {
        release_block(&mut slot.region, extent.offset(), extent.len(), locked);
        slot.dirty.mark(
          extent.offset(),
          extent.offset().saturating_add(extent.len()),
        );
        self_resident = extent.len();
      } else if locked {
        slot.region.unlock_range(extent.offset(), extent.len());
      }
    }
    let released = slot.buddy.free_bytes().saturating_sub(before);
    self.allocated_bytes = self.allocated_bytes.saturating_sub(released);
    self.resident_free = self.resident_free.saturating_add(self_resident);
    Ok(())
  }

  /// Pins the block `extent` names for one live DAX mapping (§4.6, AC-4.12). Until it is unpinned the arena will not
  /// hand the block to another allocation and will not free it: a [`ChunkArena::free`] of it is deferred, its bytes
  /// kept, so a guest's page-table view can never fall onto another file's bytes. Idempotent per holder — each `pin`
  /// is matched by one [`ChunkArena::unpin`]. Refused [`MemError::ForeignExtent`] for an extent this arena did not
  /// issue for a live block, so a stale or forged extent can never pin memory.
  pub fn pin(&mut self, extent: Extent) -> Result<(), MemError> {
    if !self.holds(extent) {
      return Err(MemError::ForeignExtent {
        offset: extent.offset(),
        len: extent.len(),
        reason: ExtentRefusal::NotAllocated,
      });
    }
    let entry = self
      .pins
      .entry((extent.region(), extent.offset()))
      .or_insert(PinnedBlock {
        extent,
        holders: 0,
        free_waiting: false,
        scrub_on_release: true,
      });
    // Saturating, never wrapping: 2^32 live mappings of one block cannot be made, and a saturated count only keeps the
    // block pinned (it is never under-freed), which is safe.
    entry.holders = entry.holders.saturating_add(1);
    Ok(())
  }

  /// Drops one DAX mapping's hold on `extent`'s block (§4.6: teardown). When the last holder goes and a [`ChunkArena::free`]
  /// arrived while the block was pinned, the block is released now — scrubbed (A-99) unless its deferred free was a
  /// give-back — and this returns `true`. Returns `false` when holders remain or no free was waiting. Refused
  /// [`MemError::ForeignExtent`] ([`ExtentRefusal::NotPinned`]) for a block no mapping holds, so an unbalanced unpin
  /// is a typed miss, never a double free.
  pub fn unpin(&mut self, extent: Extent) -> Result<bool, MemError> {
    let key = (extent.region(), extent.offset());
    let Some(entry) = self.pins.get_mut(&key) else {
      return Err(MemError::ForeignExtent {
        offset: extent.offset(),
        len: extent.len(),
        reason: ExtentRefusal::NotPinned,
      });
    };
    entry.holders = entry.holders.saturating_sub(1);
    if entry.holders > 0 {
      return Ok(false);
    }
    let waiting = entry.free_waiting;
    let scrub = entry.scrub_on_release;
    let pinned = entry.extent;
    self.pins.remove(&key);
    if waiting {
      self.release_extent(pinned, scrub)?;
      Ok(true)
    } else {
      Ok(false)
    }
  }

  /// Whether `extent`'s block is held by at least one live DAX mapping (§4.6): while true the arena neither reuses nor
  /// frees it.
  pub fn is_pinned(&self, extent: Extent) -> bool {
    self
      .pins
      .get(&(extent.region(), extent.offset()))
      .is_some_and(|entry| entry.holders > 0)
  }

  /// Blocks held by a live DAX mapping whose file has already been freed — a `free` arrived while they were pinned and
  /// waits for the last unpin (§4.6). A test asserts this moved, so a dead deferral path cannot pass for a live one.
  pub fn pinned_free_waiting(&self) -> usize {
    self
      .pins
      .values()
      .filter(|entry| entry.holders > 0 && entry.free_waiting)
      .count()
  }

  /// Trims `extent` to its first `keep` bytes, rounded up to a granule (A-118; [`crate::buddy::Buddy::trim`]): the rest
  /// goes back to the free lists, and the head of the kept prefix is returned. Every byte of the block past `dirty` is
  /// zero (the caller's record of what was ever written), so only the freed tail's bytes before `dirty` are scrubbed: a
  /// tail page nothing wrote is never touched, and no written byte is left in free memory (A-99). The tail is recorded
  /// for the idle purge like any freed range. `None`, nothing changed, where the buddy declines (the prefix is the whole
  /// block, the block is locked, or an image may name it).
  pub fn trim(
    &mut self,
    extent: Extent,
    keep: usize,
    dirty: usize,
  ) -> Result<Option<Extent>, MemError> {
    if self.identity != Some(extent.arena) {
      return Err(MemError::ForeignExtent {
        offset: extent.offset(),
        len: extent.len(),
        reason: ExtentRefusal::OtherArena,
      });
    }
    // A pinned block keeps its whole extent (§4.6): trimming it would return the cut tail to the free lists, where a
    // reuse could fall under the mapping's page. Decline the trim (the caller keeps the whole block, as it does for a
    // locked or imaged one); the tail is reclaimed once the mapping unpins.
    if self.is_pinned(extent) {
      return Ok(None);
    }
    let Some(slot) = self.slot_mut(extent.region) else {
      return Err(MemError::ForeignExtent {
        offset: extent.offset(),
        len: extent.len(),
        reason: ExtentRefusal::NoSuchRegion,
      });
    };
    let before = slot.buddy.free_bytes();
    let Some(pieces) = slot.buddy.trim(extent.block, keep)? else {
      return Ok(None);
    };
    let kept: usize = pieces.iter().map(|piece| piece.len()).sum();
    let tail_from = extent.offset().saturating_add(kept);
    let end = extent.offset().saturating_add(extent.len());
    let written_to = extent.offset().saturating_add(dirty).min(end);
    if written_to > tail_from {
      scrub(
        &mut slot.region,
        tail_from,
        written_to.saturating_sub(tail_from),
      );
    }
    slot.dirty.mark(tail_from, end);
    let released = slot.buddy.free_bytes().saturating_sub(before);
    self.allocated_bytes = self.allocated_bytes.saturating_sub(released);
    self.resident_free = self.resident_free.saturating_add(released);
    Ok(pieces.first().map(|head| Extent {
      arena: extent.arena,
      region: extent.region,
      block: *head,
    }))
  }

  /// Frees a block [`ChunkArena::trim`] cut to `keep` bytes, named by its head (A-118), scrubbing the kept bytes as
  /// [`ChunkArena::free`] does; refused, nothing changed, as [`crate::buddy::Buddy::free_or_defer_trimmed`] refuses.
  pub fn free_trimmed(&mut self, head: Extent, keep: usize) -> Result<(), MemError> {
    self.release_trimmed(head, keep, true)
  }

  /// [`ChunkArena::free_trimmed`] with the bytes untouched, as [`ChunkArena::give_back`] is to [`ChunkArena::free`].
  pub fn give_back_trimmed(&mut self, head: Extent, keep: usize) -> Result<(), MemError> {
    self.release_trimmed(head, keep, false)
  }

  fn release_trimmed(&mut self, head: Extent, keep: usize, scrubbed: bool) -> Result<(), MemError> {
    let foreign = |reason| MemError::ForeignExtent {
      offset: head.offset(),
      len: head.len(),
      reason,
    };
    if self.identity != Some(head.arena) {
      return Err(foreign(ExtentRefusal::OtherArena));
    }
    let granule = self.granule;
    let slot = self
      .slot_mut(head.region)
      .ok_or_else(|| foreign(ExtentRefusal::NoSuchRegion))?;
    let before = slot.buddy.free_bytes();
    let deferred = slot.buddy.free_or_defer_trimmed(head.block, keep)?;
    let span = keep.max(1).div_ceil(granule).saturating_mul(granule);
    let mut self_resident = 0usize;
    if !deferred && scrubbed {
      // A trimmed block is never locked (the buddy declines to trim one), so only the scrub is owed.
      scrub(&mut slot.region, head.offset(), span);
      slot
        .dirty
        .mark(head.offset(), head.offset().saturating_add(span));
      self_resident = span;
    }
    let released = slot.buddy.free_bytes().saturating_sub(before);
    self.allocated_bytes = self.allocated_bytes.saturating_sub(released);
    self.resident_free = self.resident_free.saturating_add(self_resident);
    Ok(())
  }

  /// Whether `head` names a live block of this arena trimmed to `keep` bytes, every piece allocated (A-118).
  pub fn holds_trimmed(&self, head: Extent, keep: usize) -> bool {
    self.identity == Some(head.arena)
      && self
        .slot(head.region)
        .is_some_and(|slot| slot.buddy.holds_trimmed(head.block, keep))
  }

  /// The `len` bytes from the start of `head`, which may run past the head into the rest of a trimmed block's pieces
  /// (A-118: they are contiguous); `None` past the region.
  pub fn span(&self, head: Extent, len: usize) -> Option<&[u8]> {
    let slot = self.slot(head.region)?;
    slot
      .region
      .bytes()
      .get(head.offset()..head.offset().checked_add(len)?)
  }

  /// Claims every piece of a block trimmed to `keep` bytes whose head sits at `offset` in region `region` (A-64, A-118:
  /// a recovery rebuilding a trimmed chunk's blocks), returning the head. All or nothing: a refused piece gives back
  /// the pieces already claimed, and the refusal is returned.
  pub fn claim_trimmed(
    &mut self,
    region: u16,
    offset: usize,
    keep: usize,
  ) -> Result<Extent, MemError> {
    let granule = self.granule;
    let granules = keep.max(1).div_ceil(granule);
    let mut claimed: Vec<Extent> = Vec::new();
    let mut at = offset;
    for order in (0..usize::BITS).rev() {
      let piece = 1usize.checked_shl(order).unwrap_or(0);
      if piece == 0 || granules & piece == 0 {
        continue;
      }
      let len = piece.saturating_mul(granule);
      match self.claim(region, at, len) {
        Ok(extent) => claimed.push(extent),
        Err(refusal) => {
          // A piece just claimed always goes back; should one not, that refusal is the one reported.
          for extent in claimed {
            self.give_back(extent)?;
          }
          return Err(refusal);
        }
      }
      at = at.saturating_add(len);
    }
    claimed.first().copied().ok_or(MemError::ForeignExtent {
      offset,
      len: keep,
      reason: ExtentRefusal::WrongLength,
    })
  }

  /// Allocates exactly the block of `len` bytes at `offset` in region `region` (A-64: a recovery rebuilding  /// Allocates exactly the block of `len` bytes at `offset` in region `region` (A-64: a recovery rebuilding
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
    slot.region.forget_marks(block.offset(), block.len());
    let taken = slot
      .dirty
      .take(block.offset(), block.offset().saturating_add(block.len()));
    self.resident_free = self.resident_free.saturating_sub(taken);
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
    for slot in self.slots.iter_mut() {
      let before = slot.buddy.free_bytes();
      let Slot {
        region,
        buddy,
        dirty,
        ..
      } = slot;
      buddy.commit_capture_releasing(|offset, len, locked| {
        release_block(region, offset, len, locked);
        dirty.mark(offset, offset.saturating_add(len));
      });
      released_total =
        released_total.saturating_add(slot.buddy.free_bytes().saturating_sub(before));
    }
    self.allocated_bytes = self.allocated_bytes.saturating_sub(released_total);
    self.resident_free = self.resident_free.saturating_add(released_total);
  }

  /// The publication of the last [`ChunkArena::capture`] did not commit: frees what only it could have named.
  pub fn abandon_capture(&mut self) {
    let mut released_total = 0usize;
    for slot in self.slots.iter_mut() {
      let before = slot.buddy.free_bytes();
      let Slot {
        region,
        buddy,
        dirty,
        ..
      } = slot;
      buddy.abandon_capture_releasing(|offset, len, locked| {
        release_block(region, offset, len, locked);
        dirty.mark(offset, offset.saturating_add(len));
      });
      released_total =
        released_total.saturating_add(slot.buddy.free_bytes().saturating_sub(before));
    }
    self.allocated_bytes = self.allocated_bytes.saturating_sub(released_total);
    self.resident_free = self.resident_free.saturating_add(released_total);
  }

  /// Every live block is named by the committed image: the state after a recovery claimed its image's blocks.
  pub fn commit_live(&mut self) {
    self.settle(Buddy::commit_live);
  }

  /// Runs `step` on every region's allocator and takes the bytes it released off the allocated total.
  fn settle(&mut self, step: fn(&mut Buddy)) {
    let mut released_total = 0usize;
    for slot in self.slots.iter_mut() {
      let before = slot.buddy.free_bytes();
      step(&mut slot.buddy);
      released_total =
        released_total.saturating_add(slot.buddy.free_bytes().saturating_sub(before));
    }
    self.allocated_bytes = self.allocated_bytes.saturating_sub(released_total);
    self.resident_free = self.resident_free.saturating_add(released_total);
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

  /// A-105, the idle purge: do allocate four one-page blocks in one region, write each, free the first and third, and
  /// purge one block per call until the arena says it is done. Expect the freed bytes counted resident until the
  /// purge completes and none after; the purge resuming across calls (more than one call for more than one free
  /// block); every freed byte reading zero (A-99's scrub holds whether or not the OS took the pages); the live blocks
  /// untouched; and a purge with nothing released since doing nothing.
  /// Purges one block per call until the arena says it is done; the calls it took.
  fn purge_to_the_end(arena: &mut ChunkArena) -> usize {
    let mut calls = 0;
    while arena.purge(1).more {
      calls += 1;
      assert!(calls < 8, "the purge ends");
    }
    calls + 1
  }

  /// Whether every byte of the `len` bytes at `at` of `bytes` is `value`.
  fn all_are(bytes: &[u8], at: usize, len: usize, value: u8) -> bool {
    bytes[at..at + len].iter().all(|b| *b == value)
  }

  /// A-110 (macOS). Do: allocate, fill and free a 64 MiB block, purge it, allocate it again and fill it. Expect: the
  /// purge takes the footprint down by most of the block; the second allocation clears the block's mark first (the
  /// reuse counter moves by the block); and filling it brings the footprint back up, which happens only when the mark
  /// was cleared (measured: a marked page written stays out of the footprint, the kernel free to take it). The bytes
  /// read back as written.
  #[cfg(target_os = "macos")]
  #[test]
  #[cfg_attr(miri, ignore)] // Miri models no madvise
  fn a_purged_block_leaves_the_footprint_and_is_cleared_before_reuse() {
    const BLOCK: usize = 64 << 20;
    const MOST_OF_IT: u64 = (BLOCK / 2) as u64;
    let _serial = crate::test_serial::Guard::take();
    let p = page();
    let mut arena = ChunkArena::new(p).discarding_from(p);
    arena
      .add_region(Region::map(BLOCK, p, false).unwrap())
      .unwrap();
    let block = arena.alloc(BLOCK).unwrap();
    arena.bytes_mut(block).unwrap().fill(0xAB);
    arena.free(block).unwrap();
    let footprint = crate::test_serial::footprint;
    let before = footprint().unwrap();
    assert!(purge_to_the_end(&mut arena) > 0);
    let purged = footprint().unwrap();
    assert!(
      before.saturating_sub(purged) >= MOST_OF_IT,
      "the purge gave the block back: {before} -> {purged}"
    );
    let reused_before = arena.reused_bytes();
    let again = arena.alloc(BLOCK).unwrap();
    assert!(
      arena.reused_bytes() - reused_before >= BLOCK as u64,
      "the allocation cleared the block's mark"
    );
    arena.bytes_mut(again).unwrap().fill(0xCD);
    let written = footprint().unwrap();
    assert!(
      written.saturating_sub(purged) >= MOST_OF_IT,
      "the written block counts again: {purged} -> {written}"
    );
    assert!(arena.bytes(again).unwrap().iter().all(|byte| *byte == 0xCD));
  }

  #[test]
  fn an_idle_purge_resumes_across_calls_and_leaves_freed_bytes_zero() {
    let p = page();
    let mut arena = ChunkArena::new(p).discarding_from(p);
    arena
      .add_region(Region::map(p * 4, p, false).unwrap())
      .unwrap();
    let blocks: Vec<Extent> = (0..4).map(|_| arena.alloc(p).unwrap()).collect();
    for (index, block) in blocks.iter().enumerate() {
      let value = u8::try_from(index + 1).unwrap();
      arena.bytes_mut(*block).unwrap().fill(value);
    }
    arena.free(blocks[0]).unwrap();
    arena.free(blocks[2]).unwrap();
    assert_eq!(arena.resident_free_bytes(), 2 * p);
    assert!(
      purge_to_the_end(&mut arena) > 1,
      "one block per call: the second took another"
    );
    assert_eq!(arena.resident_free_bytes(), 0);
    assert_eq!(arena.purge(1), Purged::default(), "nothing released since");
    let bytes = arena.slot(blocks[0].region).unwrap().region.bytes();
    assert!(
      all_are(bytes, 0, p, 0) && all_are(bytes, 2 * p, p, 0),
      "freed blocks read zero"
    );
    assert!(
      all_are(bytes, p, p, 2) && all_are(bytes, 3 * p, p, 4),
      "live blocks keep their bytes"
    );
  }

  /// §4.6, AC-4.12 (pinning, the memory-level gate). Do: allocate a page, fill it, pin it, then free it. Expect: the
  /// free is deferred — the block keeps its bytes (unscrubbed), `pinned_free_waiting` counts it, and a fresh allocation
  /// lands elsewhere, never on the pinned block. Then unpin: the deferred free runs now, the bytes are scrubbed (A-99),
  /// and the place is reusable. This is why a DAX guest's page can never fall onto another file's bytes.
  #[test]
  fn a_pinned_blocks_free_is_deferred_until_unpin_and_the_block_is_never_reused_meanwhile() {
    let p = page();
    let mut arena = ChunkArena::new(p);
    arena
      .add_region(Region::map(p * 4, p, false).unwrap())
      .unwrap();
    let a = arena.alloc(p).unwrap();
    arena.bytes_mut(a).unwrap().fill(0xAA);
    arena.pin(a).unwrap();
    assert!(arena.is_pinned(a));

    // The file frees its block, but a mapping still holds it: the free is deferred, the bytes kept.
    arena.free(a).unwrap();
    assert_eq!(arena.pinned_free_waiting(), 1, "the deferred free is counted");
    assert!(
      arena.bytes(a).unwrap().iter().all(|&b| b == 0xAA),
      "the pinned block keeps its bytes while mapped"
    );

    // A fresh allocation never lands on the pinned block.
    let b = arena.alloc(p).unwrap();
    assert!(
      b.offset() != a.offset(),
      "a new allocation did not reuse the pinned block"
    );

    // The last unpin runs the deferred free now: the bytes are scrubbed and the place is reusable.
    assert!(arena.unpin(a).unwrap(), "the last unpin released the block");
    assert_eq!(arena.pinned_free_waiting(), 0);
    assert!(
      arena.bytes(a).unwrap().iter().all(|&b| b == 0),
      "the released block is scrubbed (A-99)"
    );
  }

  /// §4.6. Do: pin a live block, then unpin it with no free waiting. Expect: the block stays allocated (the unpin was a
  /// no-op release), and a later free reuses its place normally — pinning that is matched by an unpin leaves no trace.
  #[test]
  fn an_unpin_with_no_waiting_free_leaves_the_block_live_and_reusable() {
    let p = page();
    let mut arena = ChunkArena::new(p);
    arena
      .add_region(Region::map(p * 2, p, false).unwrap())
      .unwrap();
    let a = arena.alloc(p).unwrap();
    arena.pin(a).unwrap();
    assert!(!arena.unpin(a).unwrap(), "no free was waiting");
    assert!(arena.holds(a), "the block is still allocated after the unpin");
    arena.free(a).unwrap();
    let b = arena.alloc(p).unwrap();
    assert_eq!(
      b.offset(),
      a.offset(),
      "with the pin gone the block is reused normally"
    );
  }

  /// §4.6. Do: fill a region, pin one block, free it, then allocate. Expect: the allocation is refused — the pinned
  /// block is the only space but the arena never hands it out — and after the unpin the same allocation succeeds on it.
  #[test]
  fn a_pinned_block_is_never_handed_out_even_when_it_is_the_only_space() {
    let p = page();
    let mut arena = ChunkArena::new(p);
    arena
      .add_region(Region::map(p * 2, p, false).unwrap())
      .unwrap();
    let a = arena.alloc(p).unwrap();
    let _b = arena.alloc(p).unwrap(); // region full
    arena.pin(a).unwrap();
    arena.free(a).unwrap(); // deferred: a's page is not on the free list
    assert!(
      arena.alloc(p).is_err(),
      "the pinned block is not handed out though it is the only free space"
    );
    assert!(arena.unpin(a).unwrap(), "unpin releases it");
    assert!(
      arena.alloc(p).is_ok(),
      "once unpinned, the block allocates again"
    );
  }

  /// §4.6. Do: unpin a block that was never pinned, and unpin one more time than it was pinned. Expect: a typed miss
  /// ([`ExtentRefusal::NotPinned`]) both times — never a panic, never a double free.
  #[test]
  fn an_unbalanced_unpin_is_a_typed_miss_not_a_double_free() {
    let p = page();
    let mut arena = ChunkArena::new(p);
    arena
      .add_region(Region::map(p * 2, p, false).unwrap())
      .unwrap();
    let a = arena.alloc(p).unwrap();
    assert!(matches!(
      arena.unpin(a),
      Err(MemError::ForeignExtent {
        reason: ExtentRefusal::NotPinned,
        ..
      })
    ));
    arena.pin(a).unwrap();
    assert!(!arena.unpin(a).unwrap());
    assert!(
      matches!(
        arena.unpin(a),
        Err(MemError::ForeignExtent {
          reason: ExtentRefusal::NotPinned,
          ..
        })
      ),
      "a second unpin finds nothing to drop"
    );
  }

  /// §4.6. Do: pin a block through two holders, free it, and drop one holder. Expect: the block stays pinned and
  /// deferred until the *second* unpin releases it — a shared mapping keeps the bytes until the last holder leaves.
  #[test]
  fn two_holders_keep_a_block_until_the_last_unpin() {
    let p = page();
    let mut arena = ChunkArena::new(p);
    arena
      .add_region(Region::map(p * 4, p, false).unwrap())
      .unwrap();
    let a = arena.alloc(p).unwrap();
    arena.bytes_mut(a).unwrap().fill(0xCD);
    arena.pin(a).unwrap();
    arena.pin(a).unwrap();
    arena.free(a).unwrap();
    assert!(!arena.unpin(a).unwrap(), "one holder remains");
    assert!(
      arena.bytes(a).unwrap().iter().all(|&b| b == 0xCD),
      "the block's bytes survive while a holder remains"
    );
    assert!(arena.unpin(a).unwrap(), "the last holder releases it");
    assert!(arena.bytes(a).unwrap().iter().all(|&b| b == 0));
  }

  /// §4.6. Do: pin a block, then try to pin a freed one. Expect: pinning a block no live allocation names is refused
  /// ([`ExtentRefusal::NotAllocated`]), so a stale extent can never pin memory; and trimming a pinned block is declined
  /// (`Ok(None)`), so a truncate cannot cut a mapped block's tail back to the free lists.
  #[test]
  fn pinning_refuses_a_stale_extent_and_declines_to_trim_a_pinned_block() {
    let p = page();
    let mut arena = ChunkArena::new(p);
    arena
      .add_region(Region::map(p * 16, p, false).unwrap())
      .unwrap();
    let a = arena.alloc(p).unwrap();
    arena.free(a).unwrap();
    assert!(
      matches!(
        arena.pin(a),
        Err(MemError::ForeignExtent {
          reason: ExtentRefusal::NotAllocated,
          ..
        })
      ),
      "a freed extent cannot be pinned"
    );
    let big = arena.alloc(p * 16).unwrap();
    arena.pin(big).unwrap();
    assert!(
      arena.trim(big, p * 5, p * 5).unwrap().is_none(),
      "a pinned block is not trimmed"
    );
    assert!(!arena.unpin(big).unwrap(), "no free was waiting");
    assert!(arena.holds(big), "the block is still allocated after the unpin");
  }

  /// A-118, the arena's trim: do allocate a 16-page block, fill its first 5 pages and a byte of the sixth, trim it to
  /// that length, and read the span; expect the bytes back, 6 pages still allocated and 10 free, the tail free for any
  /// allocation; then free the trimmed block through its head and expect every byte of the region free and zero.
  #[test]
  fn a_trimmed_block_keeps_its_prefix_and_frees_whole() {
    let p = page();
    let mut arena = ChunkArena::new(p);
    arena
      .add_region(Region::map(p * 16, p, false).unwrap())
      .unwrap();
    let block = arena.alloc(p * 16).unwrap();
    let used = p * 5 + 1;
    arena.bytes_mut(block).unwrap()[..used].fill(7);
    // Two bytes past the kept prefix were written too: the trim must scrub them out of the freed tail.
    arena.bytes_mut(block).unwrap()[p * 6..p * 6 + 2].fill(9);
    let head = arena.trim(block, used, p * 6 + 2).unwrap().unwrap();
    assert_eq!(head.offset(), block.offset());
    assert!(
      arena
        .span(head, used)
        .unwrap()
        .iter()
        .all(|byte| *byte == 7),
      "the prefix reads back"
    );
    assert_eq!(arena.allocated_bytes(), p * 6);
    assert_eq!(arena.free_bytes(), p * 10);
    let tail = arena.alloc(p * 8).unwrap();
    assert_eq!(tail.offset(), p * 8, "the freed tail is allocatable");
    let bytes = arena.slot(head.region).unwrap().region.bytes();
    assert!(
      all_are(bytes, p * 6, p * 2, 0),
      "the written bytes past the prefix were scrubbed"
    );
    arena.free(tail).unwrap();
    arena.free_trimmed(head, used).unwrap();
    assert_eq!(arena.allocated_bytes(), 0);
    let bytes = arena.slot(head.region).unwrap().region.bytes();
    assert!(all_are(bytes, 0, p * 16, 0), "the kept bytes were scrubbed");
    assert!(arena.alloc(p * 16).is_ok(), "the region is one block again");
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

  /// A pool whose look at its free bytes counts `phantom` more than it can hand out: another arena claiming between an
  /// arena's look and its claims.
  struct RacedPool {
    pool: TestPool,
    phantom: usize,
  }

  impl ExtentSource for RacedPool {
    fn claim(&mut self) -> Option<(u16, Region)> {
      self.pool.claim()
    }
    fn release(&mut self, id: u16, region: Region) {
      self.pool.release(id, region);
    }
    fn refused(&self) -> u64 {
      0
    }
    fn available(&self) -> usize {
      self.pool.available().saturating_add(self.phantom)
    }
  }

  /// A-98, §4.2 ("refused whole, with nothing changed"). Do: over a pool of two four-granule regions, make room for
  /// twelve granules, then for six. Expect: the first claims nothing (the pool cannot cover it, and the admission it is
  /// for is refused whatever is claimed), leaving both regions in the pool; the second claims both. Before 2026-10-09
  /// the first claimed both regions on its way to a refusal.
  #[test]
  fn making_room_the_pool_cannot_cover_claims_nothing() {
    let p = page();
    let mut arena = pooled(p, &[3, 9]);
    let mut budget = crate::budget::ShardBudget::new(0, 0);
    let refused = arena.make_room(&mut budget, u64::try_from(p * 12).unwrap());
    assert_eq!((refused, arena.capacity(), budget.capacity()), (0, 0, 0));
    assert_eq!(arena.claimable(), p * 8, "both regions stay in the pool");
    let admitted = arena.make_room(&mut budget, u64::try_from(p * 6).unwrap());
    assert_eq!(admitted, u64::try_from(p * 8).unwrap(), "two whole regions");
    assert_eq!(budget.capacity(), admitted);
  }

  /// §4.2 (2026-10-09): a growth from the pool takes on the pressure debt. Do: over a pool of two four-granule regions,
  /// owe a debt of four granules and make room for two. Expect both regions claimed (two granules and the debt need
  /// more than one), the hold four granules and no debt left; with the debt larger than the pool can add beside the
  /// admission, nothing claimed.
  #[test]
  fn making_room_takes_on_the_pressure_debt() {
    let p = page();
    let granules = |n: usize| u64::try_from(p * n).unwrap();
    let mut arena = pooled(p, &[1, 2]);
    let mut budget = crate::budget::ShardBudget::new(0, 0);
    budget.set_pressure(0, granules(7));
    assert_eq!(
      arena.make_room(&mut budget, granules(2)),
      0,
      "nine granules exceed the pool"
    );
    assert_eq!(arena.claimable(), p * 8);
    budget.set_pressure(0, granules(4));
    assert_eq!(arena.make_room(&mut budget, granules(2)), granules(8));
    assert_eq!((budget.hold(), budget.debt()), (granules(4), 0));
  }

  /// A-98, §4.2. Do: over a pool that looks one granule larger than it is (another arena claimed between the look and
  /// the claims), make room for all it looks to hold. Expect: nothing kept — the regions claimed go back to the pool,
  /// the arena's capacity and the budget unchanged.
  #[test]
  fn room_another_arena_took_first_returns_what_was_claimed() {
    let p = page();
    let mut arena = ChunkArena::new(p);
    arena.set_source(Box::new(RacedPool {
      pool: TestPool {
        free: [5u16, 6]
          .iter()
          .map(|id| (*id, Region::map(p * 4, p, false).unwrap()))
          .collect(),
      },
      phantom: p,
    }));
    let mut budget = crate::budget::ShardBudget::new(0, 0);
    let added = arena.make_room(&mut budget, u64::try_from(p * 9).unwrap());
    assert_eq!((added, arena.capacity(), budget.capacity()), (0, 0, 0));
    assert_eq!(arena.regions(), 0);
    assert_eq!(
      arena.claimable(),
      p * 9,
      "both regions are back, as the look counts them"
    );
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

  /// A-64 with a published arena (2026-10-10). Do: in an unpublished arena and in a published one, each of two regions
  /// of sixteen pages, allocate blocks, free one, capture, commit, allocate again, and capture; and claim a block in a
  /// fresh region of each, as recovery does, then capture. Expect: both arenas name exactly the same blocks at every
  /// capture; the unpublished arena's captures scanned every granule of its three regions once, the published arena's
  /// none — the scan gone, not moved.
  #[test]
  fn a_published_arena_names_what_a_scanning_one_names_and_scans_nothing() {
    let p = page();
    let mut scanning = arena_of_regions(p, false);
    let mut published = arena_of_regions(p, true);
    compare_captures(&mut scanning, &mut published, p);
    let claimed_a = claim_in_a_fresh_region(&mut scanning, p);
    let claimed_b = claim_in_a_fresh_region(&mut published, p);
    assert!(scanning.imaged(claimed_a) && published.imaged(claimed_b));
    let granules = u64::try_from((REGIONS + 1) * PAGES).unwrap();
    assert_eq!(
      scanning.scanned_granules(),
      granules,
      "each region scanned once"
    );
    assert_eq!(published.scanned_granules(), 0, "no region scanned");
  }

  /// Shape: the regions [`arena_of_regions`] adds, and the pages each holds.
  const REGIONS: usize = 2;
  /// Shape: see [`REGIONS`].
  const PAGES: usize = 16;

  /// An arena of [`REGIONS`] regions of [`PAGES`] pages each, published or not.
  fn arena_of_regions(p: usize, published: bool) -> ChunkArena {
    let arena = ChunkArena::new(p);
    let mut arena = if published { arena.published() } else { arena };
    for _ in 0..REGIONS {
      arena
        .add_region(Region::map(p * PAGES, p, false).unwrap())
        .unwrap();
    }
    arena
  }

  /// The same allocations, free, captures and commit in both arenas; both must name the same blocks.
  fn compare_captures(scanning: &mut ChunkArena, published: &mut ChunkArena, p: usize) {
    let mut held: Vec<(Extent, Extent)> = [p, 2 * p, p, 4 * p]
      .into_iter()
      .map(|len| (scanning.alloc(len).unwrap(), published.alloc(len).unwrap()))
      .collect();
    let (gone_a, gone_b) = held.remove(1);
    scanning.free(gone_a).unwrap();
    published.free(gone_b).unwrap();
    for arena in [&mut *scanning, &mut *published] {
      arena.capture();
      arena.commit_capture();
    }
    held.push((scanning.alloc(p).unwrap(), published.alloc(p).unwrap()));
    scanning.capture();
    published.capture();
    for (a, b) in &held {
      assert_eq!(scanning.imaged(*a), published.imaged(*b), "{a:?} / {b:?}");
      assert!(
        published.imaged(*b),
        "a live block is named by the open capture"
      );
    }
    assert!(!published.imaged(gone_b) && !scanning.imaged(gone_a));
    scanning.commit_capture();
    published.commit_capture();
  }

  /// Recovery's shape: a fresh region added, a block in it claimed, a capture; the claimed block.
  fn claim_in_a_fresh_region(arena: &mut ChunkArena, p: usize) -> Extent {
    let id = arena
      .add_region(Region::map(p * PAGES, p, false).unwrap())
      .unwrap();
    let claimed = arena.claim(id, 2 * p, 2 * p).unwrap();
    arena.capture();
    claimed
  }
}
