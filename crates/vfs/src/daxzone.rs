//! §4.6 L1, *zones and guard rows*: the bounded, physically-contiguous sandbox a tenant's writable DAX rows are
//! shifted within, and the measurement that decides whether this host can offer one at all.
//!
//! **What it is for.** L2's moving target ([`crate::volume::Volume::dax_migrate`]) relocates a writable block so a
//! Rowhammer attacker tracking physical rows loses them. That only works if the new block is on *different physical
//! rows*. A block the general allocator picks is a **virtual** address in a 4 KiB-paged region: the kernel is free
//! to back the new one with the very row the old one used, and the attacker's hammer never notices. §4.6 names this
//! exactly — "without physical contiguity a virtual shift moves no physical rows (security theatre)". A zone is the
//! contiguity: one transparent huge page, verified to have actually been given as one, cut into slots that are
//! therefore distinct physical rows of the same page.
//!
//! **Why a huge page and not a hugetlb reservation.** A reserved hugetlb pool is a host configuration slates would
//! have to require, and a `MFD_HUGETLB` object needs `/proc/sys/vm/nr_hugepages` set by an administrator — a
//! deployment privilege, which R10 refuses to require. A transparent huge page needs nothing: the arena's object is
//! already `MADV_HUGEPAGE`'d, and whether a given span actually got a PMD mapping is a question the kernel answers
//! ([`slates_machine::facts::huge_backed_bytes`]). So the sandbox is *asked for* unprivileged and *verified* before
//! anything is promised, which is the only honest order — measured on Linux 7.2, a 64 KiB and a 1 MiB mapping both
//! take `MADV_HUGEPAGE` with `OK` and receive zero huge pages.
//!
//! **Guard rows.** The first and last slot of a zone are held by the zone and never handed to the tenant, so the
//! tenant's rows are never at the edge of its huge page, where they would be physically adjacent to whatever the
//! kernel placed in the neighbouring one. That is CATT's shape (Brasser et al., USENIX Security 2017): a domain's
//! memory ends in a buffer the other domain's hammer reaches instead of the data.
//!
//! **Why the zone keeps every other slot allocated.** A slot the zone does not hold is a row inside this tenant's
//! huge page that the general allocator may hand to another volume — which is the co-location the zone exists to
//! prevent. So the zone allocates the whole span up front and gives out one slot at a time, and before each grant
//! it re-claims any slot that left (an inode's own delete or truncate frees its block by the ordinary path, which
//! the zone does not sit on). At the moment a tenant receives a writable row, every other row of its page is the
//! zone's: that is the invariant [`DaxZone::shift_target`] restores and then relies on.
//!
//! **The cost, which is the point.** A zone is one huge page per tenant that migrates, for as long as it migrates:
//! bounded, flat, and not a quarantine pool that grows with the attack — §4.6's "memory footprint stays capped",
//! which is the agentic-scale answer. L0 keeps writable DAX rare, so the number of zones is small by construction,
//! and it can never exceed the huge pages the arena itself can back.

use core::num::NonZeroUsize;

use slates_machine::stats::Xorshift;
use slates_mem::arena::{ChunkArena, Extent as Block};

use crate::dax::stripe_target;
use crate::error::VfsError;

/// Derived: the slots held at each end of a zone as guard rows, from an admitted unknown. A hammer's blast radius
/// is measured in DRAM rows, and slates cannot see the host's DRAM geometry — row size, bank interleaving and the
/// physical-to-DRAM map are the memory controller's, invisible to an unprivileged process. Rather than guess a row
/// size, the guard is one whole **slot**, the coarsest unit the arena allocates (one chunk window, 64 KiB on a
/// 4 KiB-paged host), which exceeds any published blast radius by a wide margin; it is stated in slots so it stays
/// correct on every architecture's page size.
const GUARD_SLOTS: usize = 1;

/// Derived: the fewest slots a zone can be cut into and still be a moving target — both guards, plus two places for
/// the content so a shift always has somewhere else to go. A span that cuts into fewer is refused rather than
/// handed out as a sandbox that cannot move.
const MIN_SLOTS: usize = GUARD_SLOTS.saturating_mul(2).saturating_add(2);

/// One tenant's sandbox: a huge page of the content arena, verified physically contiguous, cut into slots the
/// tenant's writable content is shifted between.
pub struct DaxZone {
  /// The arena region the span lies in.
  region: u16,
  /// The span's byte offset within that region. Huge-page aligned in the process's address space.
  base: usize,
  /// One slot in bytes — the arena's block for a whole chunk window, so any content block fits a slot.
  slot_bytes: usize,
  /// Every slot of the span in offset order: `Some` while the zone holds it, `None` while the tenant's content
  /// occupies it or an ordinary free took it away (the two are told apart by the caller's current block).
  slots: Vec<Option<Block>>,
}

impl DaxZone {
  /// Reserves a zone from `arena`, or refuses with the reason this host cannot back one.
  ///
  /// `NoSpace` where the arena cannot spare a huge page or the span cuts into fewer than [`MIN_SLOTS`] slots;
  /// [`VfsError::NoPhysicalIsolation`] where the span is not huge-page aligned, or where the kernel declined to
  /// back it with a huge page after it was faulted in — the measurement that decides it, never the advice.
  ///
  /// Nothing is left allocated on a refusal.
  pub fn reserve(
    arena: &mut ChunkArena,
    huge_bytes: NonZeroUsize,
    slot_bytes: NonZeroUsize,
  ) -> Result<DaxZone, VfsError> {
    let huge = huge_bytes.get();
    let slot = arena.block_len(slot_bytes.get()).ok_or(VfsError::NoSpace)?;
    let count = huge.checked_div(slot).unwrap_or(0);
    if count < MIN_SLOTS {
      return Err(VfsError::NoSpace);
    }
    // Take the span as one allocation, so the buddy gives it at its own alignment and nothing else holds a row
    // inside it; it is cut into slots below, which is why it is freed first.
    let span = arena.alloc(huge).map_err(VfsError::from)?;
    let region = span.region();
    let base = span.offset();
    let outcome = Self::verify_contiguous(arena, region, base, huge);
    if let Err(refusal) = outcome {
      let _ = arena.free(span);
      return Err(refusal);
    }
    arena.free(span).map_err(VfsError::from)?;
    let mut slots = Vec::new();
    slots
      .try_reserve_exact(count)
      .map_err(|_| VfsError::NoSpace)?;
    for index in 0..count {
      let at = base.saturating_add(index.saturating_mul(slot));
      match arena.claim(region, at, slot) {
        Ok(block) => slots.push(Some(block)),
        Err(refusal) => {
          // A slot of a span this call just freed should always claim; if one does not, give back every slot
          // already taken so a refused reservation leaves the arena exactly as it found it.
          for taken in slots.into_iter().flatten() {
            if arena.free(taken).is_err() {
              break;
            }
          }
          return Err(refusal.into());
        }
      }
    }
    Ok(DaxZone {
      region,
      base,
      slot_bytes: slot,
      slots,
    })
  }

  /// Whether `[base, base + huge)` of `region` really is one physical huge page, by measurement.
  ///
  /// Two questions, both of which must be yes. First, is the span huge-page aligned in this process's address
  /// space — a PMD entry maps an aligned span, so an unaligned one is at best two halves of two pages. Second, did
  /// the kernel actually give it a huge page: the span is faulted in and the mapping's PMD-mapped bytes are read
  /// before and after, so the growth is attributable to *this* span rather than to whatever the mapping held
  /// already. Off Linux the question has no answer and the zone is refused, which is correct — macOS offers user
  /// programs no superpages and Windows large pages need a privilege R10 refuses to require.
  fn verify_contiguous(
    arena: &mut ChunkArena,
    region: u16,
    base: usize,
    huge: usize,
  ) -> Result<(), VfsError> {
    let page = arena
      .region(region)
      .map(slates_mem::region::Region::page)
      .unwrap_or(0)
      .max(1);
    let address = arena
      .region(region)
      .map(|r| (r.base() as usize).saturating_add(base))
      .ok_or(VfsError::NoPhysicalIsolation)?;
    if address.checked_rem(huge).unwrap_or(1) != 0 {
      return Err(VfsError::NoPhysicalIsolation);
    }
    // Drop whatever already backs the span before measuring, so the measurement is *attributable*. The
    // mapping's PMD-mapped bytes are a whole-mapping total, so a reading taken over a span the arena had
    // already faulted counts pages this reservation did not get and the growth understates by exactly what was
    // there — which is how a span that really is one huge page comes back looking short. The span is
    // huge-aligned and huge-sized (checked above), so it covers whole huge pages and nothing else's bytes go
    // with it, and the slots cut from it are handed out empty anyway.
    if let Some(mapping) = arena.region_mut(region) {
      mapping.discard(base, huge);
    }
    let before = arena
      .region(region)
      .and_then(slates_mem::region::Region::huge_backed_bytes)
      .ok_or(VfsError::NoPhysicalIsolation)?;
    let from_page = base.checked_div(page).unwrap_or(0);
    let to_page = base.saturating_add(huge).checked_div(page).unwrap_or(0);
    if let Some(mapping) = arena.region_mut(region) {
      mapping.touch_pages(from_page, to_page);
    }
    let wanted = u64::try_from(huge).unwrap_or(u64::MAX);
    let after = arena
      .region(region)
      .and_then(slates_mem::region::Region::huge_backed_bytes)
      .ok_or(VfsError::NoPhysicalIsolation)?;
    if after.saturating_sub(before) >= wanted {
      return Ok(());
    }
    // The advice is a *preference*: the kernel grants it out of whatever contiguous memory it has at fault
    // time, so the same span comes back huge on a quiet machine and base-paged on a busy one — measured here
    // 2026-10-19, two of four reservations declined under 59 parallel tests on a host with 81 GB free. A
    // sandbox cannot be best-effort, so ask the kernel to do the compaction work now instead
    // (`MADV_COLLAPSE`), and measure again. A kernel that does not know the advice, or cannot compact, simply
    // leaves the second measurement short and the zone is refused.
    if let Some(mapping) = arena.region_mut(region) {
      mapping.collapse_huge(base, huge);
    }
    let collapsed = arena
      .region(region)
      .and_then(slates_mem::region::Region::huge_backed_bytes)
      .ok_or(VfsError::NoPhysicalIsolation)?;
    if collapsed.saturating_sub(before) < wanted {
      return Err(VfsError::NoPhysicalIsolation);
    }
    Ok(())
  }

  /// The slot `block` occupies, if it is one of this zone's.
  #[must_use]
  pub fn slot_of(&self, block: Block) -> Option<usize> {
    if block.region() != self.region || block.len() != self.slot_bytes {
      return None;
    }
    let from_base = block.offset().checked_sub(self.base)?;
    let index = from_base.checked_div(self.slot_bytes)?;
    (from_base.checked_rem(self.slot_bytes) == Some(0) && index < self.slots.len()).then_some(index)
  }

  /// A slot to shift `current` into: a different physical row of this zone, drawn by [`stripe_target`].
  ///
  /// Before it picks, every slot the zone is missing is re-claimed, so the tenant never receives a row whose page
  /// is shared with a neighbour — the invariant the zone exists for, restored at the only moment it matters. A slot
  /// that cannot be re-claimed is one a neighbour holds now; it is left out of the draw rather than taken, so a
  /// partially-lost zone keeps shifting within the rows it still owns instead of refusing.
  ///
  /// `None` when no other row is available — a zone whose rows all left. The caller keeps the content where it is
  /// and surfaces that, never a shift that does not move.
  pub fn shift_target(
    &mut self,
    arena: &mut ChunkArena,
    current: Option<Block>,
    rng: &mut Xorshift,
    mapped: &[Block],
  ) -> Option<Block> {
    let here = current.and_then(|block| self.slot_of(block));
    self.reclaim_missing(arena, here, mapped);
    // The draw runs over the non-guard slots, with one place standing for where the content is now — inside the
    // zone at `here`, or outside it on a first shift. `stripe_target` returns a *different* place, so the content
    // always moves, and every other place is a slot the zone holds.
    let last = self.slots.len().saturating_sub(GUARD_SLOTS);
    let mut places: Vec<Option<usize>> = Vec::new();
    let mut at_now = None;
    for index in GUARD_SLOTS..last {
      if Some(index) == here {
        at_now = Some(places.len());
        places.push(None);
      } else if self.slots.get(index).is_some_and(Option::is_some) {
        places.push(Some(index));
      }
    }
    if here.is_none() {
      // The content is outside the zone, so its place is one the draw must avoid but cannot land on.
      at_now = Some(places.len());
      places.push(None);
    }
    let span = NonZeroUsize::new(places.len())?;
    let pick = stripe_target(span, at_now?, rng);
    let index = places.get(pick).copied().flatten()?;
    self.slots.get_mut(index)?.take()
  }

  /// Takes back a slot the tenant has finished with, so the zone owns that row again. `false` where the block is
  /// not this zone's slot or the zone already holds it, in which case nothing changed.
  pub fn return_slot(&mut self, block: Block) -> bool {
    let Some(index) = self.slot_of(block) else {
      return false;
    };
    match self.slots.get_mut(index) {
      Some(place @ None) => {
        *place = Some(block);
        true
      }
      _ => false,
    }
  }

  /// Re-claims every slot the zone is missing except the one at `keep`, which the tenant's content occupies,
  /// and any row in `mapped`, which a live window still names.
  ///
  /// The `mapped` check is not redundant with the arena's. A row a live mapping names is normally *allocated*,
  /// so the claim fails on its own — but a block whose pin was dropped while its mapping stayed live (the
  /// shared per-block pin recorded in §4.6 item 2) has its deferred free complete and looks free, and the zone
  /// would take a row a guest is still mapping through. The chaos property found exactly that: "the sandbox is
  /// holding row 23 while a live window sits on it". Refusing to claim a row any live mapping names is sound
  /// whatever made it look free.
  fn reclaim_missing(&mut self, arena: &mut ChunkArena, keep: Option<usize>, mapped: &[Block]) {
    for index in 0..self.slots.len() {
      if Some(index) == keep || self.slots.get(index).is_some_and(Option::is_some) {
        continue;
      }
      let at = self
        .base
        .saturating_add(index.saturating_mul(self.slot_bytes));
      if mapped
        .iter()
        .any(|block| block.region() == self.region && block.offset() == at)
      {
        continue;
      }
      if let Ok(block) = arena.claim(self.region, at, self.slot_bytes)
        && let Some(place) = self.slots.get_mut(index)
      {
        *place = Some(block);
      }
    }
  }

  /// The slots this zone holds now, over the slots it was cut into — the non-vacuity counter a test reads to know
  /// a shift really drew from a sandbox the zone owned, and the signal a lost row leaves behind.
  #[must_use]
  pub fn held(&self) -> (usize, usize) {
    (
      self.slots.iter().filter(|slot| slot.is_some()).count(),
      self.slots.len(),
    )
  }

  /// Whether the zone is holding the row at `index` itself, rather than having lent it to the tenant or lost
  /// it to an ordinary free. The zone must never hold a row a live window sits on — it lends a row or holds
  /// it, never both — and that is what a chaos schedule checks after every step.
  #[must_use]
  pub fn holds_slot(&self, index: usize) -> bool {
    self.slots.get(index).is_some_and(Option::is_some)
  }

  /// One slot in bytes, so a caller can tell whether a content block fits a row of this zone.
  #[must_use]
  pub fn slot_bytes(&self) -> usize {
    self.slot_bytes
  }

  /// Hands every slot the zone still holds back to the arena, so a zone that is dropped leaves nothing allocated.
  /// The count released, for the caller's accounting; a slot the arena refuses is left, never double-freed.
  pub fn release(&mut self, arena: &mut ChunkArena) -> usize {
    let mut released = 0usize;
    for place in &mut self.slots {
      if let Some(block) = place.take() {
        if arena.free(block).is_ok() {
          released = released.saturating_add(1);
        } else {
          *place = Some(block);
        }
      }
    }
    released
  }
}

/// The shard's zones, one per volume that has shifted a writable DAX block, and the generator their placement is
/// drawn from.
///
/// Bounded by the arena itself: a zone is a huge page, so there can never be more of them than the arena has huge
/// pages to back, and the count is capped at that rather than at a number chosen here (#8 — every structure has a
/// derived bound and a typed refusal at it). In practice L0 keeps it far below: only a volume whose owner opted
/// into writable DAX *and* whose content is being shifted has one.
pub struct DaxZones {
  /// This host's huge page, or `None` where it has none and no zone can be built.
  huge: Option<NonZeroUsize>,
  /// The placement generator (§4.6 L2). Shared by every zone: one unpredictable sequence, not one per tenant,
  /// so a tenant cannot learn its own stream by watching its own shifts.
  rng: Xorshift,
  /// Each volume's zone, by volume prefix.
  zones: Vec<(u16, DaxZone)>,
  /// Volumes whose reservation the host refused since the last re-arm, so the question is not asked again at
  /// the shift cadence. **Measured 2026-10-19 on fedora 6.19.10: a refused reservation costs 53 µs** — it
  /// allocates the span, discards it, walks `/proc/self/smaps` three times (a walk of the whole address space
  /// each, which is why [`slates_machine::facts::huge_backed_bytes`] says to ask once and keep the answer),
  /// faults a huge page's worth and frees it. At the 16 ms hammer cadence that is 0.33% of a core per window
  /// spent learning the same "no", and it grows with the process's address space. Re-armed by the caller at
  /// its own slower cadence ([`DaxZones::rearm_refusals`]), so a host that frees memory later still gets a
  /// sandbox — just not at 62 attempts a second.
  refused: Vec<u16>,
}

impl DaxZones {
  /// The shard's zones, with the placement generator seeded from what this process has that a co-located attacker
  /// cannot observe: the load address of its own stack (ASLR, fresh every exec) and the wall clock at boot. Both
  /// are weak alone and neither is a cryptographic source; what makes the pair sound here is that the generator's
  /// state never leaves the daemon's RAM — it is as unreachable as the registry — so an attacker has no sequence
  /// to extrapolate from, only the shifts it can feel. A cryptographic source may replace this without touching
  /// the selection logic (the note on [`stripe_target`]).
  #[must_use]
  pub fn new() -> DaxZones {
    let here = 0u8;
    let address = std::ptr::from_ref(&here) as u64;
    let clock = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .map_or(0u128, |since| since.as_nanos());
    let mut mixer = blake3::Hasher::new();
    mixer.update(&address.to_le_bytes());
    mixer.update(&clock.to_le_bytes());
    let mixed = mixer.finalize();
    // Format: the generator's state is one 64-bit word, taken from the head of the mix.
    let mut word = [0u8; core::mem::size_of::<u64>()];
    if let Some(head) = mixed.as_bytes().get(..word.len()) {
      word.copy_from_slice(head);
    }
    DaxZones {
      huge: slates_machine::facts::huge_page_bytes()
        .and_then(|bytes| usize::try_from(bytes).ok())
        .and_then(NonZeroUsize::new),
      rng: Xorshift::new(u64::from_le_bytes(word)),
      zones: Vec::new(),
      refused: Vec::new(),
    }
  }

  /// A row of this volume's sandbox to shift the content at `current` onto, reserving the sandbox on first use.
  ///
  /// [`VfsError::NoPhysicalIsolation`] where this host cannot back a sandbox (no huge page, or the kernel declined
  /// one for the span), where the shard already holds as many as the arena has huge pages, or where the sandbox
  /// has no other row left to offer; `NoSpace` where the arena cannot spare the page now. A refusal takes nothing
  /// from the sandbox, so the caller's content stays exactly where it is.
  pub fn shift_target(
    &mut self,
    prefix: u16,
    arena: &mut ChunkArena,
    slot_bytes: NonZeroUsize,
    current: Block,
    mapped: &[Block],
  ) -> Result<Block, VfsError> {
    self.reserve_for(prefix, arena, slot_bytes)?;
    // Split the borrow: the draw needs the volume's zone and the shared generator at once, and they are separate
    // fields of this struct.
    let DaxZones { rng, zones, .. } = self;
    zones
      .iter_mut()
      .find(|(held, _)| *held == prefix)
      .and_then(|(_, zone)| zone.shift_target(arena, Some(current), rng, mapped))
      .ok_or(VfsError::NoPhysicalIsolation)
  }

  /// Gives a row back to the volume's sandbox, so the zone owns it again: `true` when it was taken back. `false`
  /// where the block is not one of that volume's sandbox rows — the caller's signal that it owns the block and
  /// must free it itself.
  pub fn return_slot(&mut self, prefix: u16, block: Block) -> bool {
    self
      .zones
      .iter_mut()
      .find(|(held, _)| *held == prefix)
      .is_some_and(|(_, zone)| zone.return_slot(block))
  }

  /// This volume's zone, for a caller reading its shape; `None` before one is reserved.
  #[must_use]
  pub fn of_volume(&self, prefix: u16) -> Option<&DaxZone> {
    self
      .zones
      .iter()
      .find(|(held, _)| *held == prefix)
      .map(|(_, zone)| zone)
  }

  /// Reserves this volume's zone if it has none yet, refusing as [`DaxZones::shift_target`] documents.
  fn reserve_for(
    &mut self,
    prefix: u16,
    arena: &mut ChunkArena,
    slot_bytes: NonZeroUsize,
  ) -> Result<(), VfsError> {
    if self.zones.iter().any(|(held, _)| *held == prefix) {
      return Ok(());
    }
    // Asked and answered: the host refused this volume a sandbox since the last re-arm, and asking again costs
    // 53 µs of address-space walking to hear it again (the note on `refused`).
    if self.refused.contains(&prefix) {
      return Err(VfsError::NoPhysicalIsolation);
    }
    let huge = self.huge.ok_or(VfsError::NoPhysicalIsolation)?;
    let ceiling = arena.capacity().checked_div(huge.get()).unwrap_or(0);
    if self.zones.len() >= ceiling {
      return Err(VfsError::NoPhysicalIsolation);
    }
    // Room for the record before the page is taken, so a reservation is never left holding a huge page it has
    // nowhere to record (nothing to unwind, because nothing was taken yet).
    self.zones.try_reserve(1).map_err(|_| VfsError::NoSpace)?;
    match DaxZone::reserve(arena, huge, slot_bytes) {
      Ok(zone) => {
        self.zones.push((prefix, zone));
        Ok(())
      }
      Err(refusal) => {
        // Remember the no. A volume the list has no room for is simply asked again next time — the cost of
        // that is the 53 µs, not a correctness problem, so the bound refuses the *memo*, never the mapping.
        if self.refused.try_reserve(1).is_ok() {
          self.refused.push(prefix);
        }
        Err(refusal)
      }
    }
  }

  /// Forgets the volumes the host refused a sandbox, so each is asked once more.
  ///
  /// The caller's cadence is the bound: a refusal costs 53 µs of address-space walking to re-learn, so asking
  /// at the 16 ms hammer cadence burns 0.33% of a core per window for nothing, while never asking again would
  /// leave a tenant unsandboxed for the life of the volume because the host was momentarily fragmented. The
  /// daemon re-arms on its reap tick — the cadence it already does cold work at — so a host that frees memory
  /// gets the sandbox within one tick, and the question is asked at most once per volume per tick.
  pub fn rearm_refusals(&mut self) {
    self.refused.clear();
  }

  /// Volumes currently remembered as refused — the counter that says the memo is doing its job, and the signal
  /// that this host is not backing sandboxes.
  #[must_use]
  pub fn refused_len(&self) -> usize {
    self.refused.len()
  }

  /// The placement generator, for a caller drawing a shift target.
  pub fn generator(&mut self) -> &mut Xorshift {
    &mut self.rng
  }

  /// Releases a volume's zone back to the arena, so a destroyed volume's sandbox does not hold a huge page for a
  /// tenant that is gone. The slots released; zero where the volume had no zone.
  pub fn release_volume(&mut self, prefix: u16, arena: &mut ChunkArena) -> usize {
    self.refused.retain(|held| *held != prefix);
    let Some(at) = self.zones.iter().position(|(held, _)| *held == prefix) else {
      return 0;
    };
    let mut released = 0usize;
    if let Some((_, zone)) = self.zones.get_mut(at) {
      released = zone.release(arena);
    }
    if released > 0 || self.zones.get(at).is_some_and(|(_, z)| z.held().0 == 0) {
      self.zones.remove(at);
    }
    released
  }

  /// How many zones the shard holds — the non-vacuity counter for "a sandbox was really built".
  #[must_use]
  pub fn len(&self) -> usize {
    self.zones.len()
  }

  /// Whether the shard holds no zone.
  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.zones.is_empty()
  }
}

impl Default for DaxZones {
  fn default() -> Self {
    Self::new()
  }
}
