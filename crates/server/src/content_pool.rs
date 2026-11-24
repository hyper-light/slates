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
//! does. A shard claims at admission only (`Store::make_room`), never per write, and takes the smallest free part
//! that covers what it lacks, else the largest (best fit, A-848), its own slice's before the following partitions'
//! of the same length, so shards meet in the pool only when a volume outgrows its own slice. Before A-848 it took its
//! slice's largest part first: a push seal's 18,564-byte turn grew a shard's arena by 8,589,934,592 bytes (g227,
//! Studio-4, 2026-11-21).
//!
//! **Wants (A-853, the share cap).** A shard whose admission the pool cannot cover posts the room it lacks in its want
//! word, beside the owner words in the anchor segment, bounded by the pool; it withdraws it once it grows. The post
//! that finds no want standing sends one message to each other shard, and a shard holding more capacity than its
//! demand (commitments, the operation headroom, the pressure hold) publishes, whose commit returns every wholly free
//! extent past that demand. No fixed share: a shard grows into the whole pool while no other wants, and gives back when
//! one does. Before A-853 a shard's burst kept the pool until its next publication, and a push on another shard was
//! refused `PastAdmission { left: 0 }` (g227).
//!
//! **Names.** An extent's id is `partition × POOL_EXTENTS_PER_PARTITION + part`, and the arena holds it under that id,
//! so a block's (region, offset) name is the same in every daemon and an image needs no extent table.
//!
//! **Parts.** The unit is one chunk window rounded up to the mapping granule, so every part holds whole chunks and
//! maps on its own; a part is a power-of-two number of units, since a buddy region uses only the largest
//! power-of-two run of granules it holds. A slice is cut into a run doubling from one unit (1, 1, 2, 4, ... units),
//! then the rest's binary parts ([`arena_parts`]), so an arena grows from one unit by its work. Cut as one region, a reserve of 170.7 MiB under a 1 GiB cap left 25%
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
use crate::state::ShardState;

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

/// The parts of one slice's arena range in `config`'s layout ([`arena_parts`]): what a Windows extent's length is read
/// from (`crate::content_extents::extent_len`, A-789).
#[cfg(windows)]
pub(crate) fn parts_of(config: &DaemonConfig) -> Vec<(usize, usize)> {
  arena_parts(config.shard_content_layout().arena.1, config.pool_unit())
}

/// The parts of an arena range of `len` bytes, `(offset, len)` relative to the range, each a power-of-two number of
/// `unit`s (A-848): first a doubling run (1, 1, 2, 4, ... units, each as long as all before it), then what the run
/// leaves cut into power-of-two parts, largest first; the remainder under one `unit` left unused. A claim takes the
/// smallest free part that covers what it needs ([`ContentPool`]'s `claim`), so an arena grows by its need rounded up
/// to a power of two of units, from one unit, and doubles when claims keep coming. Before A-848 the range was cut
/// into its binary parts, largest first, and a first claim of 18,564 bytes took an 8,589,934,592-byte part (g227,
/// Studio-4). At most about two parts per bit of `len / unit`, and never more than `POOL_EXTENTS_PER_PARTITION`.
pub(crate) fn arena_parts(len: usize, unit: usize) -> Vec<(usize, usize)> {
  let unit = unit.max(1);
  let units = len.checked_div(unit).unwrap_or(0);
  let mut parts: Vec<(usize, usize)> = Vec::new();
  // The doubling run: one unit, then each part as long as every part before it together (1, 1, 2, 4, ... units), so an
  // arena that claims them in turn doubles at each claim, from one unit.
  let mut taken = 0usize;
  while taken < units && parts.len() < POOL_EXTENTS_PER_PARTITION {
    let next = taken.max(1);
    if units.saturating_sub(taken) < next {
      break;
    }
    parts.push((taken.saturating_mul(unit), next.saturating_mul(unit)));
    taken = taken.saturating_add(next);
  }
  // What the run leaves (fewer units than it holds) in power-of-two parts, largest first: one per bit of the rest.
  let mut rest = units.saturating_sub(taken);
  while rest > 0 && parts.len() < POOL_EXTENTS_PER_PARTITION {
    // `rest >= 1` in the loop, so it has a top bit.
    let top = 1usize.checked_shl(rest.ilog2()).unwrap_or(0);
    parts.push((taken.saturating_mul(unit), top.saturating_mul(unit)));
    taken = taken.saturating_add(top);
    rest = rest.saturating_sub(top);
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
    let mut pool = ContentPool {
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
    };
    pool.make_own_extents();
    pool
  }

  /// Makes this partition's slice's extent objects where extents are objects of their own (macOS: `make_extent`), so a
  /// claim later only opens and maps one, never makes it on the provisioning path (main's condition (a), R9). An extent
  /// a predecessor made is adopted as it is. One the OS refuses is counted, and made at its first claim instead.
  fn make_own_extents(&mut self) {
    let Some((content, _)) = &self.content else {
      return;
    };
    for part in 0..self.parts.len() {
      let Some(id) = u16::try_from(
        usize::from(self.partition)
          .saturating_mul(POOL_EXTENTS_PER_PARTITION)
          .saturating_add(part),
      )
      .ok() else {
        continue;
      };
      let (Some(extent), Some((_, (_, len)))) =
        (slates_mem::extent_handoff(content, id), self.part(id))
      else {
        continue;
      };
      if slates_mem::make_extent(&extent, len).is_err() {
        self.refused = self.refused.saturating_add(1);
      }
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
    // An extent of its own on Windows (A-789): its section made ahead of the claim and held by the anchor, duplicated
    // here and mapped committed whole.
    #[cfg(windows)]
    if crate::content_extents::installed() {
      let section = crate::content_extents::section(id).ok_or(ServerError::Memory(
        slates_mem::MemError::OsRefused {
          call: "content extent section",
          code: None,
        },
      ))?;
      // SAFETY: no other view in any process reaches the extent's section while this region lives: this partition holds
      // its owner word, the section is unmapped before the word is released (`release`), and the anchor that holds a
      // duplicate never maps it.
      let object = unsafe { slates_mem::ExclusiveObject::from_section(section, len) }
        .map_err(ServerError::Memory)?;
      return Ok(Region::shared(object, self.page));
    }
    if let Some(extent) = self
      .content
      .as_ref()
      .and_then(|(content, _)| slates_mem::extent_handoff(content, id))
    {
      // An extent of its own (macOS): made at its owner's start or at its last release; made here only if neither
      // could (the OS refused it then).
      // At most two attempts: the open, and once the extent is made, the open again.
      let mut made = false;
      let object = loop {
        // SAFETY: no other mapping in any process reaches the extent's object while this region lives: this partition
        // holds its owner word (claimed by compare-and-swap, or held from the previous daemon, which has exited), and
        // the object is unmapped before its word is released (`release`). The anchor never maps it.
        match unsafe { slates_mem::ExclusiveObject::open_range(&extent, 0, len) } {
          Ok(object) => break object,
          Err(refusal) if made => return Err(ServerError::Memory(refusal)),
          Err(_) => {
            slates_mem::make_extent(&extent, len).map_err(ServerError::Memory)?;
            made = true;
          }
        }
      };
      return Ok(Region::shared(object, self.page));
    }
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
  /// The free extents in the order a claim of `want` bytes tries them (A-848): those that cover `want`, smallest first,
  /// then those that do not, largest first; equal lengths in [`ContentPool::candidates`]' order, so this partition's
  /// own slice before the following ones. An arena so grows by its need rounded up to a power of two of units. Before
  /// A-848 a claim took the first free part, its slice's largest: 8,589,934,592 bytes for an 18,564-byte lend (g227).
  /// Claims come at admission only, never per write, so the list is made per claim.
  fn best_fit(&self, want: usize) -> Vec<u16> {
    let mut free: Vec<(bool, usize, usize, u16)> = Vec::new();
    for (order, id) in self.candidates().enumerate() {
      // An extent whose section is not made, or not yet held by the anchor, is passed over (A-789, Windows): data never
      // lands where a daemon's death would lose it.
      if !claimable_now(id) {
        continue;
      }
      if !matches!(self.segment.pool_owner(usize::from(id)), Ok(None)) {
        continue;
      }
      let Some((_, (_, len))) = self.part(id) else {
        continue;
      };
      let covers = len >= want;
      // Covering parts by length up, the rest by length down.
      let key = if covers {
        len
      } else {
        usize::MAX.saturating_sub(len)
      };
      free.push((!covers, key, order, id));
    }
    free.sort_unstable();
    free.into_iter().map(|(_, _, _, id)| id).collect()
  }
}

impl ContentPool {
  /// Maps extent `id`, which this partition has just claimed, for the arena: `None` when the OS would not map it, the
  /// word then given straight back (if even that is refused, the extent stays held by this partition and is released
  /// by the next daemon's `held`).
  fn hand_out(&mut self, id: u16) -> Option<(u16, Region)> {
    #[cfg(test)]
    let mapped = if REFUSE_NEXT_MAPS.with(|left| {
      left
        .get()
        .checked_sub(1)
        .map(|after| left.set(after))
        .is_some()
    }) {
      Err(ServerError::Memory(slates_mem::MemError::OsRefused {
        call: "a test's refused extent map",
        code: None,
      }))
    } else {
      self.map(id)
    };
    #[cfg(not(test))]
    let mapped = self.map(id);
    match mapped {
      Ok(region) => Some((id, region)),
      Err(refusal) => {
        report_first_refused_map(id, &refusal);
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
        && let Some(handed) = self.hand_out(id)
      {
        return Some(handed);
      }
    }
    None
  }
}

/// Logs the first extent map the OS refused in this process, once (main 01a20182's order of 2026-11-23 on r578): the
/// refusal, and its OS error by name and number where it carries one. A claim tries the next candidate past a refused
/// map, which ends a refusal the pool could serve; a cause every extent meets (descriptors, the shared-memory name
/// space, the VM's limits) would only move to the next, and this line names which the host met.
fn report_first_refused_map(id: u16, refusal: &ServerError) {
  static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
  if LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
    return;
  }
  let os = match refusal {
    ServerError::Memory(slates_mem::MemError::OsRefused {
      code: Some(code), ..
    }) => format!(
      ", errno {code}: {}",
      std::io::Error::from_raw_os_error(*code)
    ),
    _ => String::new(),
  };
  slates_machine::log_line!(
    "slates-server: content pool extent {id} refused its map (first occurrence): {refusal}{os}"
  );
}

/// Whether extent `id` may be claimed now: on Windows, where each extent is a section of its own (A-789), only once it
/// is made and held by the anchor; elsewhere always, as is a Windows daemon's with no anchor holding its extents. What
/// the pool counts as free room ([`ExtentSource::available`]) and what a claim takes agree on this, so a refusal never
/// names room no claim could take: until 2026-11-23 the count took every free extent, and a create refused while the
/// holds were under way named the whole pool as available (5,722,144,768 bytes, diag run 45238179939).
#[cfg(windows)]
fn claimable_now(id: u16) -> bool {
  !crate::content_extents::installed() || crate::content_extents::ready(id)
}

/// [`claimable_now`] off Windows: an extent of the pool is always claimable once free.
#[cfg(not(windows))]
fn claimable_now(_id: u16) -> bool {
  true
}

impl ContentPool {
  /// Gives a released extent's memory back to the system where it is an object of its own (macOS): its name removed,
  /// so the object and every page in it go with the mapping just dropped, then made again, empty, for its next claim
  /// (main's condition (a): never made on the provisioning path). Before, a released extent was only unmapped, and its
  /// pages stayed in the anchor's one content object. A refusal is counted; the extent then keeps its pages.
  fn give_back(&mut self, id: u16) {
    // Windows (A-789): this process's handle closes now, and a task has the anchor let its duplicate go, which closes the
    // section and returns its commit, then makes the extent again and has it held. Until then the extent is not ready.
    #[cfg(windows)]
    if crate::content_extents::installed() {
      drop(crate::content_extents::take_for_remake(id));
      let Some((_, (_, len))) = self.part(id) else {
        crate::content_extents::end_remake(id);
        return;
      };
      if slates_rt::futures::spawn(crate::content_extents::remake(id, len))
        .and_then(slates_rt::futures::detach)
        .is_err()
      {
        crate::content_extents::end_remake(id);
        self.refused = self.refused.saturating_add(1);
      }
      return;
    }
    let (Some(extent), Some((_, (_, len)))) = (
      self
        .content
        .as_ref()
        .and_then(|(content, _)| slates_mem::extent_handoff(content, id)),
      self.part(id),
    ) else {
      return;
    };
    if slates_mem::unlink_handoff(&extent).is_err()
      || slates_mem::make_extent(&extent, len).is_err()
    {
      self.refused = self.refused.saturating_add(1);
    }
  }

  /// Gives a released extent's pages back to the system where the extent is a range of the anchor's one content object
  /// (Linux's memfd), before its mapping is dropped. A memfd's pages belong to the object and outlive every mapping of
  /// them, so a release that only unmapped left the extent's pages resident: a 64 MiB burst's destroyed volume left the
  /// object at 69 to 73 MiB and its arena at 4 (adacachy2 and CI, 2026-11-23). The hole is punched through the extent's
  /// own mapping (`Region::discard`: `MADV_REMOVE`, which on shmem is `fallocate(PUNCH_HOLE | KEEP_SIZE)` of the range,
  /// madvise(2)), the call the idle purge already gives a free run back with, so the pages leave every mapping of the
  /// object, the anchor's included, and the range reads zeros for its next claimant: the object fell to 12 MiB, and the
  /// punch of that 256 MiB extent with 65 MiB resident took 0.17 ms (adacachy2, debug, load 54). A refusal (a mapping
  /// locked whole) is counted; the extent then keeps its pages. A private mapping (no content object) goes with its unmap.
  #[cfg(target_os = "linux")]
  fn punch(&mut self, region: &mut Region) {
    if self.content.is_some() && !region.discard(0, region.len()) {
      self.refused = self.refused.saturating_add(1);
    }
  }

  /// Where an extent is an object or a section of its own (macOS, and Windows under an anchor), [`ContentPool::give_back`]
  /// frees it whole after the unmap: nothing to punch first.
  #[cfg(not(target_os = "linux"))]
  fn punch(&mut self, _region: &mut Region) {}
}

impl ExtentSource for ContentPool {
  fn claim(&mut self, want: usize) -> Option<(u16, Region)> {
    for id in self.best_fit(want) {
      match self.segment.pool_claim(usize::from(id), self.partition) {
        // An extent the OS would not map is counted and given back (`hand_out`), and the next candidate tried: until
        // 2026-11-23 the claim ended there, so one refused map refused the whole growth while the pool had room
        // (main 01a20182's r578: `BudgetExceeded { available: 33945026560 }` for 1 MiB, the arena's `room_lost`).
        Ok(true) => {
          if let Some(handed) = self.hand_out(id) {
            return Some(handed);
          }
        }
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

  fn want(&mut self, bytes: usize) {
    // Bounded: never more than the whole pool, which no arena can lack more of.
    let pool = self
      .parts
      .iter()
      .map(|(_, len)| *len)
      .sum::<usize>()
      .saturating_mul(usize::from(self.partitions));
    let bytes = u64::try_from(bytes.min(pool)).unwrap_or(u64::MAX);
    match self.segment.post_pool_want(self.partition, bytes) {
      // The first post of a want tells the other shards, once: a task, since the arena posting it is borrowed inside
      // this shard's state.
      Ok(true) => {
        if slates_rt::futures::spawn(tell_the_others_of_a_want())
          .and_then(slates_rt::futures::detach)
          .is_err()
        {
          self.refused = self.refused.saturating_add(1);
        }
      }
      Ok(false) => {}
      Err(_) => self.refused = self.refused.saturating_add(1),
    }
  }

  fn release(&mut self, id: u16, mut region: Region) {
    self.punch(&mut region);
    // Unmapped first: the word may be claimed by another shard the moment it is released.
    drop(region);
    self.give_back(id);
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
      .filter(|id| claimable_now(*id))
      .filter(|id| match self.segment.pool_owner(usize::from(*id)) {
        Ok(None) => true,
        Ok(Some(POOL_HOLD_OWNER)) => hold_open,
        _ => false,
      })
      .filter_map(|id| self.part(id).map(|(_, (_, len))| len))
      .sum()
  }

  #[cfg(windows)]
  fn pending(&self) -> usize {
    self
      .candidates()
      .filter(|id| crate::content_extents::remaking(*id))
      .filter(|id| matches!(self.segment.pool_owner(usize::from(*id)), Ok(None)))
      .filter_map(|id| self.part(id).map(|(_, (_, len))| len))
      .sum()
  }
}

/// Counter: wants this shard answered by giving back what its demand does not need (A-853): a publication, after which
/// its wholly free extents went back to the pool.
const WANT_ANSWERED: &str = "pool.want_answered";
/// Counter: wants this shard was told of while it held no more than its demand: nothing to give.
const WANT_NOTHING_TO_GIVE: &str = "pool.want_nothing_to_give";
/// Counter: wants this shard could not answer: its publication refused, or the message to another shard not carried.
const WANT_REFUSED: &str = "pool.want_refused";

/// Tells every other shard that this one has posted a want (A-853, the share cap: main 01a20182 2026-11-21 21:35): one
/// message each on its bounded channel, carried while the channel is full ([`crate::xshard::run_on_waiting`]). Sent
/// only by the post that found no want standing, so a shard refused again and again sends no more than its first.
async fn tell_the_others_of_a_want() {
  let Some((origin, shards)) =
    crate::state::with_state(|state| (state.shard, state.shards.clone()))
  else {
    return;
  };
  for shard in shards.into_iter().filter(|shard| *shard != origin) {
    crate::xshard::run_on_waiting(origin, shard, answer_a_want, WANT_REFUSED);
  }
}

/// Answers another shard's want on this one (A-853): when its arena holds more capacity than its demand (every
/// commitment, the operation headroom and the pressure hold, `Store::demand`), it publishes, and the publication's
/// commit returns every wholly free extent past that demand to the pool (`Store::release_idle`, run after a committed
/// publication so no image names a returned block). A shard at its demand has nothing to give. The want is not read
/// for an amount: everything past demand goes back, since the refused shard may need it all and claims by best fit.
fn answer_a_want(state: &mut ShardState) {
  let capacity = u64::try_from(state.store.content.arena().capacity()).unwrap_or(u64::MAX);
  if capacity <= state.store.demand() {
    state.count(WANT_NOTHING_TO_GIVE, 1);
    return;
  }
  match crate::verbs::publish_shard(state) {
    Ok(_) => state.count(WANT_ANSWERED, 1),
    Err(_) => state.count(WANT_REFUSED, 1),
  }
}

#[cfg(test)]
thread_local! {
  /// Test only: how many of this shard's next extent maps its pool refuses, as the OS may refuse one (an extent's
  /// object not opened, not made again, a range not mapped).
  pub(crate) static REFUSE_NEXT_MAPS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
  use super::ShardState;

  /// Derived: the bytes g227's push seal lent a turn for 64 files (Studio-4, 2026-11-21; GAPS, g227's measurement),
  /// the lend that grew a shard's arena by a whole 8 GiB extent.
  const SEAL_TURN_BYTES: u64 = 18_564;

  /// One ask for room on `state`'s shard: room for what its arena can already admit plus [`SEAL_TURN_BYTES`].
  /// `(asked, lacked, grown, before, admittable)`: what was asked, what the budget lacked for it (the commitments, the
  /// operation headroom and the pressure hold beside the ask, and the pressure debt a growth takes on), what the arena
  /// grew by, its capacity before, and what the shard could admit after.
  fn ask_one_turn(state: &mut ShardState) -> (u64, u64, u64, u64, u64) {
    let before = state.store.budget.capacity();
    let asked = state
      .store
      .budget
      .admittable()
      .saturating_add(SEAL_TURN_BYTES);
    let lacked = state
      .store
      .budget
      .deficit(asked)
      .saturating_add(state.store.budget.debt());
    state.store.make_room(asked);
    let grown = state.store.budget.capacity().saturating_sub(before);
    (
      asked,
      lacked,
      grown,
      before,
      state.store.budget.admittable(),
    )
  }

  /// The most a growth by `lacked` bytes may take (A-848): `lacked` rounded up to a power-of-two number of `unit`s,
  /// the length of the smallest part covering it, or the arena's capacity `before` when the parts left are the doubling
  /// run's next.
  fn bound(lacked: u64, unit: usize, before: u64) -> u64 {
    let unit = u64::try_from(unit).unwrap();
    lacked
      .div_ceil(unit)
      .max(1)
      .next_power_of_two()
      .saturating_mul(unit)
      .max(before)
  }

  /// A-848 (main 2026-11-21 17:20, goal 93; Ada's MEMORY SIZING rule: sized from the work). Do: on a shard, ask room for
  /// what its arena can already admit plus one push seal's turn (18,564 bytes), so the arena must grow. Expect: it grows
  /// (non-vacuity), by no more than what its budget lacked rounded up to a power of two of pool units, or its capacity
  /// before. Before, the first claim took its slice's largest part (8,589,934,592 bytes on Studio-4, g227), however few
  /// bytes the lend needed.
  #[test]
  fn a_small_lend_grows_its_shards_arena_by_its_need_not_an_extent() {
    let ((asked, lacked, grown, before, admittable), unit, slice) =
      crate::daemon::audit_on_shard(|state| {
        let unit = state.config.pool_unit();
        let slice = state.config.shard_content_layout().arena.1;
        (ask_one_turn(state), unit, slice)
      });
    assert!(grown > 0, "the lend grew the arena (non-vacuity)");
    assert!(
      admittable >= asked,
      "asked {asked}, admittable {admittable}"
    );
    assert!(
      grown <= bound(lacked, unit, before),
      "a {SEAL_TURN_BYTES}-byte lend grew the arena by {grown} bytes (lacked {lacked}, unit {unit}, capacity before \
       {before}, slice arena {slice})"
    );
  }

  /// Shape: the turns each shard lends in [`two_shards_lending_small_rooms_in_turn_are_both_admitted`]: enough that a
  /// shard taking more than its turns need would reach the other's slice.
  const TURNS: usize = 8;

  /// A-848 (main 2026-11-21 17:20, case 2). Do: on a two-shard daemon, each shard in turn asks room for what its arena
  /// can already admit plus one push seal's turn, [`TURNS`] times. Expect: every ask admitted (the shard's admittable
  /// at least what it asked), and no turn growing a shard's arena past [`bound`].
  #[test]
  fn two_shards_lending_small_rooms_in_turn_are_both_admitted() {
    let daemon = crate::daemon::audit_daemon_of(2);
    for turn in 0..TURNS {
      for partition in 0..2usize {
        let ((asked, lacked, grown, before, admittable), unit) =
          crate::daemon::observe_at(&daemon, partition, |state| {
            (ask_one_turn(state), state.config.pool_unit())
          });
        assert!(
          admittable >= asked,
          "turn {turn}, shard {partition}: asked {asked}, admittable {admittable}"
        );
        assert!(
          grown <= bound(lacked, unit, before),
          "turn {turn}, shard {partition}: grew the arena by {grown} (lacked {lacked}, unit {unit}, before {before})"
        );
      }
    }
  }

  /// Shape: how long [`a_shards_burst_no_longer_starves_another_shards_push`] gives the burst's shard to answer the
  /// other's want, a bound on the test, not a tuning: a give-back is one publication on an idle shard.
  const WANT_ANSWERED_WITHIN: std::time::Duration = std::time::Duration::from_secs(10);

  /// The share cap (main 01a20182 2026-11-21 21:35, ruled; g227's case). Do: on a two-shard daemon, shard 0 makes room
  /// for all it could admit (a burst, as w-hg's merge lent `admittable()`), which takes the pool; then shard 1 asks room
  /// for one push seal's turn past what it holds. Expect: the first ask refused (the burst holds the pool: the case's
  /// precondition), and a later ask admitted within [`WANT_ANSWERED_WITHIN`], shard 0 having given back what its
  /// demand does not need (its `pool.want_answered` counted, the non-vacuity counter). Before, shard 0 kept the pool and every push on shard 1 was refused
  /// `PastAdmission { left: 0 }` (g227, Studio-4).
  #[test]
  fn a_shards_burst_no_longer_starves_another_shards_push() {
    let daemon = crate::daemon::audit_daemon_of(2);
    let burst = crate::daemon::observe_at(&daemon, 0, |state| {
      let all = state.store.admittable();
      state.store.make_room(all)
    });
    assert!(burst > 0, "the burst grew shard 0's arena (non-vacuity)");
    let (asked, admittable, ..) = crate::daemon::observe_at(&daemon, 1, ask_one_turn_whole);
    assert!(
      admittable < asked,
      "the burst holds the pool: asked {asked}, admittable {admittable}"
    );
    let began = std::time::Instant::now();
    loop {
      let (asked, admittable, ..) = crate::daemon::observe_at(&daemon, 1, ask_one_turn_whole);
      if admittable >= asked {
        break;
      }
      assert!(
        began.elapsed() < WANT_ANSWERED_WITHIN,
        "shard 1 still starved: asked {asked}, admittable {admittable}"
      );
      std::thread::yield_now();
    }
    let answered = crate::daemon::observe_at(&daemon, 0, |state| {
      state
        .refusals
        .get(super::WANT_ANSWERED)
        .copied()
        .unwrap_or(0)
    });
    assert!(answered > 0, "shard 0 answered the want (non-vacuity)");
  }

  /// [`ask_one_turn`] where the turn must be admitted whole: room for what the shard holds free, at least a turn more.
  fn ask_one_turn_whole(state: &mut ShardState) -> (u64, u64, u64) {
    let (asked, lacked, grown, _, admittable) = ask_one_turn(state);
    (asked, admittable, lacked.max(grown))
  }

  /// main 01a20182's r578 (the big Studio, 23 shards, provision_bench's 64-client sweep: 1 MiB creates refused
  /// `BudgetExceeded { available: 33945026560 }`, the daemon's once-line reading `room_lost=1` with the pool's whole
  /// claimable room, no debt and no hold). Do: on a one-shard daemon, have its pool refuse the next extent map, as the OS
  /// may, and make room for a turn past what the shard holds. Expect: the room made, from the next candidate, the
  /// refused map counted by the pool and no growth lost. Failing first: the claim ended at the refused map, so the growth
  /// came up short and gave back what it took (`room_lost` 1), the room unmade.
  #[test]
  fn an_extent_the_os_will_not_map_passes_the_claim_to_the_next() {
    let daemon = crate::daemon::audit_daemon();
    let (asked, lacked, grown, _, admittable, room_lost, refused) =
      crate::daemon::observe_first(&daemon, |state| {
        let before = state.store.content.arena().source_refusals();
        super::REFUSE_NEXT_MAPS.with(|left| left.set(1));
        let (asked, lacked, grown, at, admittable) = ask_one_turn(state);
        super::REFUSE_NEXT_MAPS.with(|left| left.set(0));
        (
          asked,
          lacked,
          grown,
          at,
          admittable,
          state.store.content.arena().room_lost(),
          state.store.content.arena().source_refusals() - before,
        )
      });
    daemon.stop();
    assert!(
      lacked > 0,
      "the turn needed the pool (non-vacuity): asked {asked}"
    );
    assert_eq!(refused, 1, "the refused map counted");
    assert_eq!(room_lost, 0, "no growth came up short");
    assert!(
      grown >= lacked && admittable >= asked,
      "the room made: grew {grown} of {lacked} lacked, admittable {admittable} for {asked}"
    );
  }
}
