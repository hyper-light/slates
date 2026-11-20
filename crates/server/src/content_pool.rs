//! The shared content pool a shard's arena grows from (A-98, §4.2): every slice's arena range of the anchor's content
//! object, cut into power-of-two parts, each part an extent held by at most one shard at a time.
//!
//! **Why.** A volume lives on one shard. With each shard's arena fixed to its own slice, a volume's ceiling was
//! RAM ÷ shards ÷ classes: 1/54 of the machine on an 18-shard host. Seastar and ScyllaDB accept a fixed per-core share
//! because their data spreads over cores by key; a slates volume has one writer and cannot spread. tcmalloc's
//! per-thread caches over a central free list, and Linux's per-CPU page lists over the zone allocator, are the shape
//! taken: a shard holds what its admissions need and returns what is wholly free.
//!
//! **Claims.** An extent's owner word in the anchor segment is 0 while free, else its partition plus one, taken and
//! given back by compare-and-swap (`AnchorSegment::pool_claim`), so the claim outlives a daemon restart as the content
//! does. A shard claims at admission only (`Store::make_room`), never per write. It tries its own slice's parts first,
//! largest first, then the following partitions' parts, so shards meet in the pool only when a volume outgrows its
//! own slice.
//!
//! **Names.** An extent's id is `partition × POOL_EXTENTS_PER_PARTITION + part`, and the arena holds it under that id,
//! so a block's (region, offset) name is the same in every daemon and an image needs no extent table.
//!
//! **Parts.** The unit is one chunk window rounded up to the mapping granule, so every part holds whole chunks and
//! maps on its own; a part is a power-of-two number of units, since a buddy region uses only the largest
//! power-of-two run of granules it holds. Cut as one region, a reserve of 170.7 MiB under a 1 GiB cap left 25%
//! unallocatable (2026-10-05, BENCHMARKS).
//!
//! **The pressure hold.** The memory-pressure hold (§4.2, admission.md §5.5) is the daemon's, not each shard's: what the
//! daemon could still fill must fit in what the host has free, and one volume on one shard can reach the whole pool. So
//! the hold claims free extents under an owner of its own ([`POOL_HOLD_OWNER`], [`hold_toward`]), and no shard can
//! grow into them; what the pool's whole extents cannot cover is taken from the free room the shards already hold
//! (`daemon::refresh_pressure_hold`). A takeover's recovery runs past the hold and may take its extents
//! ([`ExtentSource::open_hold`]). Before 2026-10-09 the hold was divided evenly among the shards and each share was
//! claimed only by its own shard's admissions, so a volume on one shard of five was held back by a fifth of it.

use slates_anchor::layout::POOL_EXTENTS_PER_PARTITION;
use slates_anchor::{AnchorError, AnchorSegment};
use slates_mem::arena::ExtentSource;
use slates_mem::region::Region;
use slates_mem::shared::Handoff;

use crate::config::DaemonConfig;
use crate::error::ServerError;

/// Format: the partition the pressure hold claims pool extents under (§4.2, admission.md §5.5). Never a partition: an
/// extent's id is the `u16` `partition × POOL_EXTENTS_PER_PARTITION + part`, so every partition is below
/// `u16::MAX / POOL_EXTENTS_PER_PARTITION` (1,023), far short of `u16::MAX`.
pub(crate) const POOL_HOLD_OWNER: u16 = u16::MAX;

/// The pool's extents as the pressure hold sees them (A-98): every partition's slice cut into the same power-of-two
/// parts, each an extent of id `partition × POOL_EXTENTS_PER_PARTITION + part`.
pub(crate) struct PoolGeometry {
  /// Partitions in the segment.
  partitions: u16,
  /// One slice's parts' lengths, in the order their ids number them.
  parts: Vec<u64>,
}

impl PoolGeometry {
  /// The geometry `config` lays the pool out in, as every shard's [`ContentPool`] cuts it.
  pub(crate) fn of(config: &DaemonConfig) -> PoolGeometry {
    let layout = config.shard_content_layout();
    PoolGeometry {
      partitions: config.geometry.partitions.max(1),
      parts: arena_parts(layout.arena.1, config.pool_unit())
        .into_iter()
        .map(|(_, len)| u64::try_from(len).unwrap_or(u64::MAX))
        .collect(),
    }
  }

  /// Every extent, `(id, length)`.
  fn extents(&self) -> impl Iterator<Item = (usize, u64)> + '_ {
    (0..usize::from(self.partitions)).flat_map(move |partition| {
      self.parts.iter().enumerate().map(move |(part, len)| {
        (
          partition
            .saturating_mul(POOL_EXTENTS_PER_PARTITION)
            .saturating_add(part),
          *len,
        )
      })
    })
  }
}

/// The bytes of pool extents the pressure hold owns now.
pub(crate) fn hold_bytes(
  segment: &AnchorSegment,
  geometry: &PoolGeometry,
) -> Result<u64, AnchorError> {
  let mut held = 0u64;
  for (id, len) in geometry.extents() {
    if segment.pool_owner(id)? == Some(POOL_HOLD_OWNER) {
      held = held.saturating_add(len);
    }
  }
  Ok(held)
}

/// Moves the pool extents the pressure hold owns toward `target` bytes, never past it, and returns the bytes it owns
/// after (§4.2, admission.md §5.5). Greedy over the extents it owns and the free ones, largest first, an owned extent
/// before a free one of its length (kept, not swapped): each is kept or claimed while it still fits and released when it
/// does not. The parts are powers of two of one unit, each length dividing every larger one, so taking the largest that
/// fits first is the most that fits (greedy is optimal for divisible lengths). A claim another shard wins first is
/// skipped; a word the segment refuses to reach is a layout fault, returned.
pub(crate) fn hold_toward(
  segment: &AnchorSegment,
  geometry: &PoolGeometry,
  target: u64,
) -> Result<u64, AnchorError> {
  let mut candidates: Vec<(u64, bool, usize)> = Vec::new();
  for (id, len) in geometry.extents() {
    match segment.pool_owner(id)? {
      Some(POOL_HOLD_OWNER) => candidates.push((len, true, id)),
      None => candidates.push((len, false, id)),
      Some(_) => {}
    }
  }
  candidates.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
  let mut held = 0u64;
  for (len, owned, id) in candidates {
    let fits = held.checked_add(len).is_some_and(|total| total <= target);
    match (owned, fits) {
      (true, true) => held = held.saturating_add(len),
      (true, false) => {
        segment.pool_release(id, POOL_HOLD_OWNER)?;
      }
      (false, true) => {
        if segment.pool_claim(id, POOL_HOLD_OWNER)? {
          held = held.saturating_add(len);
        }
      }
      (false, false) => {}
    }
  }
  Ok(held)
}

/// The parts of an arena range of `len` bytes, `(offset, len)` relative to the range: its length cut on `unit` into
/// power-of-two multiples of it, largest first, the remainder under one `unit` left unused. At most one part per bit
/// of `len / unit`.
pub(crate) fn arena_parts(len: usize, unit: usize) -> Vec<(usize, usize)> {
  let unit = unit.max(1);
  let mut units = len.checked_div(unit).unwrap_or(0);
  let mut at = 0usize;
  let mut parts = Vec::new();
  while units > 0 {
    // `units >= 1` in the loop, so it has a top bit.
    let top = 1usize.checked_shl(units.ilog2()).unwrap_or(0);
    let part = top.saturating_mul(unit);
    parts.push((at, part));
    at = at.saturating_add(part);
    units = units.saturating_sub(top);
  }
  parts
}

/// The pool, as one shard sees it.
pub(crate) struct ContentPool {
  /// This shard's own attachment of the anchor segment, for the owner words.
  segment: AnchorSegment,
  /// This shard's partition.
  partition: u16,
  /// Partitions in the segment.
  partitions: u16,
  /// Where each partition's slice starts in the content object, and its arena's offset within the slice.
  stride: usize,
  arena_offset: usize,
  /// The parts of one slice's arena range.
  parts: Vec<(usize, usize)>,
  /// The content object, when the anchor hands one over; without it each extent is a private mapping.
  content: Option<(Handoff, usize)>,
  page: usize,
  huge_pages: bool,
  /// Claims whose extent the OS would not map, so far.
  refused: u64,
  /// Whether this shard may take the pressure hold's extents as well as free ones: open only while a takeover's
  /// recovery runs past the hold ([`ExtentSource::open_hold`]).
  hold_open: bool,
}

impl ContentPool {
  /// The pool for `partition`, over the segment `handoff` names.
  pub(crate) fn open(
    config: &DaemonConfig,
    partition: u16,
    env: &[(String, String)],
    segment: AnchorSegment,
  ) -> ContentPool {
    let layout = config.shard_content_layout();
    let unit = config.pool_unit();
    ContentPool {
      segment,
      partition,
      partitions: config.geometry.partitions.max(1),
      stride: layout.stride,
      arena_offset: layout.arena.0,
      parts: arena_parts(layout.arena.1, unit),
      content: AnchorSegment::content_handoff_in(env),
      page: config.page,
      huge_pages: config.huge_pages,
      refused: 0,
      hold_open: false,
    }
  }

  /// The capacity of one slice's parts: what this shard holds when it has claimed its own slice whole, the scale its
  /// operation headroom is derived against.
  pub(crate) fn slice_capacity(&self) -> usize {
    self.parts.iter().map(|(_, len)| *len).sum()
  }

  /// The extents this partition holds by its owner words (a previous daemon's claims), mapped, for the arena to add
  /// before recovery claims the blocks its image names. A held id this layout has no part for is released.
  pub(crate) fn held(&mut self) -> Result<Vec<(u16, Region)>, ServerError> {
    let mut held = Vec::new();
    for extent in self.segment.pool_held_by(self.partition)? {
      match u16::try_from(extent)
        .ok()
        .filter(|id| self.part(*id).is_some())
      {
        Some(id) => held.push((id, self.map(id)?)),
        None => {
          self.segment.pool_release(extent, self.partition)?;
        }
      }
    }
    Ok(held)
  }

  /// The slice and part extent `id` names, if this layout has it.
  fn part(&self, id: u16) -> Option<(u16, (usize, usize))> {
    let per = POOL_EXTENTS_PER_PARTITION;
    let slice = u16::try_from(usize::from(id) / per).ok()?;
    if slice >= self.partitions {
      return None;
    }
    let part = *self.parts.get(usize::from(id) % per)?;
    Some((slice, part))
  }

  /// Maps extent `id`: its range of the content object, or a private mapping of its length.
  fn map(&self, id: u16) -> Result<Region, ServerError> {
    let Some((slice, (offset, len))) = self.part(id) else {
      return Err(ServerError::Memory(slates_mem::MemError::OutOfRange {
        offset: usize::from(id),
        len: 0,
      }));
    };
    let at = usize::from(slice)
      .saturating_mul(self.stride)
      .saturating_add(self.arena_offset)
      .saturating_add(offset);
    match &self.content {
      Some((handoff, object_len)) if at.saturating_add(len) <= *object_len => {
        // SAFETY: no other mapping in any process reaches `[at, at + len)` while this region lives. The range is
        // extent `id`, whose owner word this partition holds (claimed by compare-and-swap, or held from the previous
        // daemon, which has exited), and an extent is unmapped before its word is released (`release`), so no two
        // shards map it at once. The extents are disjoint parts of the slices' arena ranges, and this daemon's other
        // copies of the content object reach only the write logs and the image slots, which the layout keeps apart.
        // The anchor that holds the object never touches its bytes.
        let object = unsafe { slates_mem::ExclusiveObject::open_range(handoff, at, len) }
          .map_err(ServerError::Memory)?;
        Ok(Region::shared(object, self.page))
      }
      _ => Ok(Region::map(len, self.page, self.huge_pages)?),
    }
  }

  /// The extent ids in the order this partition tries them: its own slice's parts, then each following partition's.
  fn candidates(&self) -> impl Iterator<Item = u16> + use<> {
    let (partition, partitions, parts) = (self.partition, self.partitions, self.parts.len());
    (0..partitions)
      .map(move |step| {
        partition
          .saturating_add(step)
          .checked_rem(partitions)
          .unwrap_or(0)
      })
      .flat_map(move |slice| {
        (0..parts).filter_map(move |part| {
          u16::try_from(
            usize::from(slice)
              .saturating_mul(POOL_EXTENTS_PER_PARTITION)
              .saturating_add(part),
          )
          .ok()
        })
      })
  }
}

impl ContentPool {
  /// Maps extent `id`, which this partition has just claimed, for the arena: `None` when the OS would not map it, the
  /// word then given straight back (if even that is refused, the extent stays held by this partition and is released
  /// by the next daemon's `held`).
  fn hand_out(&mut self, id: u16) -> Option<(u16, Region)> {
    match self.map(id) {
      Ok(region) => Some((id, region)),
      Err(_) => {
        self.refused = self.refused.saturating_add(1);
        let _released = self.segment.pool_release(usize::from(id), self.partition);
        None
      }
    }
  }

  /// Takes one of the pressure hold's extents for this partition (a takeover's recovery past the hold): released by the
  /// hold, then claimed. Another claimant may win it between the two, and the next is tried.
  fn claim_from_hold(&mut self) -> Option<(u16, Region)> {
    for id in self.candidates() {
      let word = usize::from(id);
      if !matches!(self.segment.pool_owner(word), Ok(Some(POOL_HOLD_OWNER))) {
        continue;
      }
      if matches!(self.segment.pool_release(word, POOL_HOLD_OWNER), Ok(true))
        && matches!(self.segment.pool_claim(word, self.partition), Ok(true))
      {
        return self.hand_out(id);
      }
    }
    None
  }
}

impl ExtentSource for ContentPool {
  fn claim(&mut self) -> Option<(u16, Region)> {
    for id in self.candidates() {
      match self.segment.pool_claim(usize::from(id), self.partition) {
        Ok(true) => return self.hand_out(id),
        Ok(false) => {}
        // A word the segment refuses to reach (its geometry is checked at attach, so this is a layout fault): counted,
        // and the next candidate tried.
        Err(_) => self.refused = self.refused.saturating_add(1),
      }
    }
    if self.hold_open {
      return self.claim_from_hold();
    }
    None
  }

  fn open_hold(&mut self, open: bool) {
    self.hold_open = open;
  }

  fn release(&mut self, id: u16, region: Region) {
    // Unmapped first: the word may be claimed by another shard the moment it is released.
    drop(region);
    if !matches!(
      self.segment.pool_release(usize::from(id), self.partition),
      Ok(true)
    ) {
      self.refused = self.refused.saturating_add(1);
    }
  }

  fn refused(&self) -> u64 {
    self.refused
  }

  fn available(&self) -> usize {
    let hold_open = self.hold_open;
    self
      .candidates()
      .filter(|id| match self.segment.pool_owner(usize::from(*id)) {
        Ok(None) => true,
        Ok(Some(POOL_HOLD_OWNER)) => hold_open,
        _ => false,
      })
      .filter_map(|id| self.part(id).map(|(_, (_, len))| len))
      .sum()
  }
}
