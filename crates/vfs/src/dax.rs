//! The DAX mapping gate (§4.6, A-9, AC-4.12): *mapping isolation, pinning and teardown*, the invariants a VMM's DAX
//! capability may be advertised behind. A DAX guest reaches the content arena's own blocks directly through its page
//! table and never calls the daemon, so the daemon cannot mediate the guest's individual accesses. The gate makes
//! that safe at the memory level, not by a per-access check the guest bypasses:
//!
//! - **Pinning.** A mapped block is pinned in the arena ([`slates_mem::arena::ChunkArena::pin`]). While pinned the
//!   arena never hands it to another allocation and never frees it — a `free` of it is *deferred*, its bytes kept —
//!   so a guest's page can never fall onto another file's bytes, in this volume or any other on the shard. This
//!   closes exfiltration, cross-file and cross-volume injection, the churn/pile races and the TOCTOU reuse: the
//!   block the mapping names is simply never reused while mapped.
//! - **Teardown (revocation).** Before content a mapping covers is *moved, shared, landed or taken over*, the
//!   mapping is revoked: it leaves the registry and its block is unpinned. A revoked mapping is dead — the guest's
//!   page table no longer maps it, modeled here by [`Store::dax_read_raw`]/[`Store::dax_write_raw`] going inert for a
//!   mapping the registry no longer holds (the MMU, not a daemon call, enforces this after an `munmap` of the guest
//!   window). This closes the block-move lost update and the snapshot-share corruption: the guest re-maps after the
//!   fault and gets a fresh private block.
//! - **Rights.** A writable mapping copies the file up first ([`Volume::dax_map_write`]) so the block the guest
//!   writes is this version's alone — no writable mapping of immutable or snapshot-shared content.
//!
//! The registry lives in the daemon's own [`Store`] (shard-local RAM). No agent, SDK, MCP or guest can reach it: a
//! mapping is *minted by the daemon* on an authenticated device admission, never supplied by the caller, and every
//! verb here is the daemon's. Making the registry un-fabricable and tamper-proof across the fleet is the next step
//! (see `docs/wip/GAPS.md`); nothing here precludes it — the mapping carries no caller-trusted field, and the block
//! it names is validated against the live arena before it is pinned.

use core::num::NonZeroUsize;

use slates_machine::stats::Xorshift;
use slates_mem::arena::Extent as Block;

use crate::error::VfsError;
use crate::ids::InodeNo;
use crate::inode::Body;
use crate::volume::{Store, Volume};

/// Derived: the DAX mapping granule — the host page, the unit a guest page table can point at. One page (4 KiB) is
/// the smallest mapping a virtio-fs DAX window grants; a block spans a whole number of them.
pub const DAX_PAGE: usize = 4096;

/// The pseudo-random **stripe target** within a file's bounded DAX shift sandbox (§4.6 L2, the moving target's
/// placement): given a sandbox of `slots` physical rows and the slot the content sits in now, a uniformly chosen
/// *different* slot to shift it to. Bounded strictly to `[0, slots)` (never out of the sandbox) and never `current`
/// (a shift must move the bytes to escape a hammer), so the content walks the sandbox unpredictably while the memory
/// footprint stays capped — a pre-bounded physical sandbox, no quarantine pool to exhaust (Ada's scale answer to
/// blind-migration quarantine exhaustion at agentic scale).
///
/// The choice is unbiased ([`Xorshift::below`], Lemire). `rng` carries the randomness source: a deterministic
/// xorshift64* (Vigna 2016) seeded **from boot entropy**, so the sequence is unpredictable to a co-located attacker
/// — who cannot observe the daemon's RNG state (it is daemon-only RAM, like the registry), which is what makes a
/// non-cryptographic generator sound here; a cryptographic source may replace it without touching this selection
/// logic. When the L1 hugetlb sandbox lands, [`Volume::dax_migrate`] draws its shift target's slot from here instead
/// of the general arena allocator, binding the offset to a physically-contiguous sandbox so the shift actually moves
/// physical rows — the stated deployment requirement (without contiguity, a virtual shift is security theatre).
#[must_use]
pub fn stripe_target(slots: NonZeroUsize, current: usize, rng: &mut Xorshift) -> usize {
  let n = slots.get();
  if n <= 1 {
    // A one-slot sandbox cannot move; the caller reads this as "no shift available" and keeps the block in place.
    return 0;
  }
  // Pick uniformly in `[0, n-1)` over the *other* slots, then skip `current` by shifting the upper half up one — a
  // bijection onto `{0..n} \ {current}`, so the result is uniform and never `current`, with no reject loop.
  let pick = rng.below(n.saturating_sub(1));
  let current = current.checked_rem(n).unwrap_or(0);
  if pick >= current {
    pick.saturating_add(1)
  } else {
    pick
  }
}

/// What one slice of §4.6 L2's periodic shift did ([`Volume::dax_shift_slice`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShiftSlice {
  /// Windows moved onto a different physical row of the volume's sandbox — the non-vacuity counter: a sweep
  /// that ran but shifted nothing reads as zero here, so a silently dead trigger cannot look like a working one.
  pub shifted: u64,
  /// Windows that could not be moved because they were no longer live when their turn came (the guest unmapped,
  /// a write revoked, a snapshot recalled). Expected and counted, never retried in this round.
  pub refused: u64,
  /// Content bytes copied, which is what the slice's budget is spent in.
  pub bytes: u64,
  /// Where the next slice of this round resumes in the caller's snapshot; equal to its length when the round is
  /// done.
  pub next: usize,
  /// The host backs no §4.6 L1 sandbox, so no window of this volume can be shifted at all and the round stopped
  /// at the first refusal rather than learning it once per window. The caller reads this as "not here", not as a
  /// failure.
  pub unsupported: bool,
}

/// A daemon-minted opaque mapping identity (§4.6 step 2, the tamper-proof registry). The inner value is private to
/// this module, so a [`Mapping`] — and thus its id — can be formed only by the mint path here
/// ([`DaxRegistry::mint_id`]), never from caller input or by tampering: a mapping the daemon never minted cannot be
/// constructed at all, so it can never pass [`DaxRegistry::is_live`]. Identity is decoupled from the (inode, block)
/// tuple — a re-mint of the same block (a migration) is a distinct mapping — which closes the theoretical
/// forge-the-exact-tuple case by construction (the tuple identity was already safe by R1/R10 + the MMU; this makes it
/// un-fabricable at the type level). Monotone and per-`Store`, so it never crosses a host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MappingId(u64);

impl MappingId {
  /// The id's opaque value, so a same-host client can name this mapping when it asks to release it
  /// (§4.6, A-135). Reading the value never *forms* a `MappingId` — the inner stays private, so a
  /// client-supplied `u64` can only be looked up in the registry, never fabricated into a mapping.
  #[must_use]
  pub fn as_u64(self) -> u64 {
    self.0
  }
}

/// One active mapping of a file's content block into a guest's DAX window. Minted by the daemon
/// ([`Volume::dax_map_read`]/[`Volume::dax_map_write`]); never constructed from caller input — its [`MappingId`] has
/// a module-private inner, so a `Mapping` can be formed only by the mint paths in this module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mapping {
  /// The daemon-minted identity: un-fabricable (private inner) and distinct per mint, so a tampered or fabricated
  /// mapping is a different identity the registry never holds.
  pub id: MappingId,
  /// The inode mapped (prefix-encoded, so its volume is `inode.prefix()`).
  pub inode: InodeNo,
  /// The file offset where this mapping's block begins (§4.6 "DAX maps the open frontier"): `0` for a single-block
  /// file, an inline/materialized block, or a whole-file map; the open chunk's file offset for a per-chunk map of a
  /// multi-block file's open tail. A reader of the block at byte `b` is at file offset `block_file_start + b`, so the
  /// device maps a guest `SETUPMAPPING` at file offset `foffset` from block byte `foffset - block_file_start`.
  pub block_file_start: usize,
  /// The arena block holding the content — what the guest's page table points at. Pinned while this mapping lives.
  pub block: Block,
  /// The authorized length of this mapping's block: `file_len` bytes from `block_file_start`, with the bytes from
  /// there to the end of the block's pages reading as zero. For a single-block file this is the file's length; for a
  /// per-chunk map it is the open chunk's valid bytes.
  pub file_len: usize,
  /// Whether the guest may write these pages.
  pub writable: bool,
  /// Whether this mapping **owns** its block — a materialized metadata snapshot (§4.6 metadata DAX: a directory's
  /// entries serialized into a throwaway content block, [`Volume::dax_map_dir`]), freed when the mapping is revoked.
  /// `false` for a file's content block, which the inode owns and a revoke only unpins (never frees) — the common case.
  pub snapshot: bool,
}

impl Mapping {
  /// The window bytes this mapping occupies in [`DAX_PAGE`] units: `file_len` rounded up, at least one page.
  ///
  /// This is the **mediated** path's bound ([`Volume::dax_read`]), and it is *not* what a guest's page table
  /// covers — an earlier version of this line claimed it was, which is false wherever the host's page is larger
  /// than [`DAX_PAGE`]. A device grants whole HOST pages: `file_len` rounded up to the placer's granule, capped at
  /// the block (`bridge-virtiofs`'s authorized window). On an Apple-Silicon host that granule is 16 KiB against a
  /// 4 KiB [`DAX_PAGE`], so the guest's window is four times this; on x86_64 Linux the two coincide. Neither
  /// exposes anything it should not: a map scrubs the whole block past `file_len`, and the device's cap at the
  /// block stops a coarse granule reaching the next one. Both are proven by use — the mediated path by
  /// `a_mapping_does_not_expose_the_bytes_past_the_files_length`, the device's wider window by
  /// `a_device_window_past_the_files_length_shows_zeroes_not_the_truncated_secret`.
  #[must_use]
  pub fn page_span(&self) -> usize {
    self
      .file_len
      .div_ceil(DAX_PAGE)
      .max(1)
      .saturating_mul(DAX_PAGE)
  }
}

/// A read-only mapping's **integrity guard** (§4.6 multi-tenancy B2, the self-healing safety net): the BLAKE3 of the
/// block's clean bytes and a pinned clean copy to heal from. A read-only DAX block is immutable, so *any* change to
/// its bytes is a bit flip — a Rowhammer victim — never a legitimate write, which is why B2 is sound for read-only
/// content and **not** for writable (there the daemon cannot tell a flip from the guest's own direct write, so a
/// content checksum would false-positive on every write; writable blocks are defended by the zones and the moving
/// target, not here). The clean copy is a separate arena block; at L1 it lives in a guard-separated zone so one
/// hammer cannot reach both. Today it is an explicit 2× copy (opt-in, for integrity-critical content); when the gate
/// maps base-backed read-only content, the base/seal origin is the clean source with no extra memory.
#[derive(Clone, Copy, Debug)]
struct DaxGuard {
  /// The guarded read-only mapping.
  mapping: Mapping,
  /// BLAKE3 of the clean block bytes — the checksum a scrub compares the live block against.
  identity: [u8; 32],
  /// A pinned arena block holding the clean bytes: the heal source a scrub restores from.
  clean: Block,
}

/// The outcome of a [`Store::dax_scrub`] integrity pass over a read-only mapping (§4.6 B2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaxScrub {
  /// No guard is registered for the mapping — nothing was checked.
  Unguarded,
  /// The block's checksum matched its guard: no flip.
  Clean,
  /// A flip was detected and the block was restored from the clean copy, which re-verified — the victim is erased.
  Healed,
  /// A flip was detected but the clean copy no longer verifies either (both corrupted): the block could not be
  /// healed. The caller revokes the mapping and surfaces the corruption rather than serving bad bytes.
  Unhealable,
}

/// The shard's DAX mapping registry (§4.6), owned by [`Store`]. It records every live mapping so the content paths
/// (move, snapshot, landing, takeover) can revoke before they share or reclaim a block, and so the hardware access
/// path can tell a live mapping from a revoked one. Bounded by `cap` (the shard's chunk count), with a typed refusal
/// at the bound; empty on any shard no guest maps, so its cost is a single length check on the hot paths.
pub struct DaxRegistry {
  /// The live mappings. A revoke removes one; a map adds one. Small — one per live guest window.
  mappings: Vec<Mapping>,
  /// The most mappings the shard admits at once (derived from `max_chunks`: a mapping pins a chunk, and the shard
  /// holds no more chunks than that). A map past it is refused rather than growing the registry without bound.
  cap: usize,
  /// Mappings revoked so far, monotone: a non-vacuity counter a test asserts moved, so a dead teardown path cannot
  /// pass for a live one.
  revoked: u64,
  /// Maps refused at the bound, monotone.
  refused: u64,
  /// Migrations performed (§4.6 L2 moving target): a mapped block relocated to a fresh block and the mapping
  /// re-minted on it. Monotone; the non-vacuity counter a migration test asserts moved, so a dead moving-target path
  /// cannot pass for a live one.
  migrated: u64,
  /// Integrity guards for read-only mappings (§4.6 B2, the self-healing safety net). Bounded by `cap` as the
  /// mappings are; empty on any shard that guards nothing.
  guards: Vec<DaxGuard>,
  /// Scrubs run, flips caught, and blocks healed (§4.6 B2 non-vacuity counters): a test asserts the one it drove
  /// moved, so a dead scrub or a dead heal path cannot pass for a live one.
  scrubbed: u64,
  flips: u64,
  healed: u64,
  /// Inodes a daemon-side revoke (`Store::dax_revoke_inode` on a write/truncate of a mapped inode;
  /// `Store::dax_revoke_writable_of_prefix` on a snapshot/clone) marked **revoke-pending**: their mappings left the
  /// registry but their blocks stay pinned until the device munmaps the guest's window (§4.6 step 5, the
  /// daemon-initiated half of the two-phase revoke). The device serve loop drains this each pass; the `bool` is
  /// `writable_only` — `false` tears *every* window of the inode (`Device::request_revoke_inode`, a write/truncate
  /// makes all stale), `true` tears only the writable ones and keeps the read-only (`request_revoke_writable_inode`,
  /// a snapshot shares the block read-only). Empty on any shard with no device drain pending; bounded by `cap`
  /// distinct live inodes (deduped on enqueue, `writable_only=false` superseding `true` for the same inode).
  pending_revokes: Vec<(InodeNo, bool)>,
  /// Volumes whose writable windows a snapshot has recalled (§4.6 step 5, `Store::dax_revoke_writable_of_prefix`).
  /// While a volume is here, [`Volume::dax_map_write`] refuses it `RecallInFlight`.
  ///
  /// Why it exists: without it the recall never converges. The snapshot revokes the volume's writable windows and
  /// refuses itself retryable until the device has munmapped them; a guest that maps a writable window again
  /// between two of those retries puts the snapshot straight back where it started, and a guest doing it
  /// deliberately holds the snapshot off forever. The exclusion is the same shape a write delegation's quiet period
  /// has: once the recall is on, the thing being recalled cannot be taken out again.
  ///
  /// It carries no deadline, deliberately: the `Store` has no clock, and a bound invented here would be a magic
  /// number. The caller that *does* have a clock owns the bound and clears this with
  /// [`DaxRegistry::end_recall`] — see `crates/server/src/verbs.rs`'s `snapshot`. `dax_revoke_volume` clears it too,
  /// so a destroyed volume leaves nothing behind. Bounded by `cap`, one entry per volume, deduped.
  recalling: Vec<u16>,
  /// The next mapping identity to mint (§4.6 step 2). Monotone and per-`Store`, incremented on every mint; starts at
  /// 1 so a zero id is never minted. It never crosses a host (a taken-over successor starts from its own fresh
  /// registry), so it needs no coordination.
  next_id: u64,
}

impl DaxRegistry {
  /// An empty registry admitting at most `cap` live mappings.
  #[must_use]
  pub fn new(cap: usize) -> Self {
    DaxRegistry {
      mappings: Vec::new(),
      cap,
      revoked: 0,
      refused: 0,
      migrated: 0,
      guards: Vec::new(),
      scrubbed: 0,
      flips: 0,
      healed: 0,
      pending_revokes: Vec::new(),
      recalling: Vec::new(),
      next_id: 1,
    }
  }

  /// Marks inode `no` revoke-pending for the device to munmap (§4.6 step 5), deduped so a burst of writes to one
  /// mapped inode enqueues it once. `writable_only` false (a write/truncate — every window stale) **supersedes** a
  /// prior true (a snapshot — writable windows only) for the same inode, since tearing all windows subsumes tearing
  /// the writable ones. Bounded by `cap` distinct inodes.
  fn enqueue_pending_revoke(&mut self, no: InodeNo, writable_only: bool) {
    if let Some(entry) = self
      .pending_revokes
      .iter_mut()
      .find(|(held, _)| *held == no)
    {
      // `false` (all) wins: once any revoke needs every window, the drain must tear every window.
      entry.1 = entry.1 && writable_only;
    } else {
      self.pending_revokes.push((no, writable_only));
    }
  }

  /// Takes the inodes awaiting a device-side revoke and clears the queue — the device serve loop calls this each
  /// pass and asks its device to munmap each inode's windows (all, or writable-only per the `bool`). Empty when none
  /// wait. The clear is what a retrying `snapshot` watches ([`Self::has_pending_of_prefix`]): once the queue no
  /// longer holds the volume's inodes, the device has drained them (take and munmap are one cooperative pass).
  #[must_use]
  pub fn take_pending_revokes(&mut self) -> Vec<(InodeNo, bool)> {
    std::mem::take(&mut self.pending_revokes)
  }

  /// How many inodes await a device-side revoke (the step-5 queue depth; bounded by `cap`). For the bound/dedup
  /// hardening test — a write storm on a mapped inode must not grow it past the one deduped entry.
  #[must_use]
  pub fn pending_len(&self) -> usize {
    self.pending_revokes.len()
  }

  /// Whether any inode of the volume `prefix` still awaits a device-side revoke (§4.6 step 5). A snapshot/clone that
  /// enqueued its volume's writable windows retries until this is false — the device has then munmapped them (a
  /// taken revoke is munmapped in the same cooperative pass), so the frozen blocks can no longer be written through
  /// a live guest window. `false` when the queue holds nothing for the volume.
  #[must_use]
  pub fn has_pending_of_prefix(&self, prefix: u16) -> bool {
    self
      .pending_revokes
      .iter()
      .any(|(no, _)| no.prefix() == prefix)
  }

  /// Whether any **writable** mapping of the volume `prefix` is still live (not yet revoke-pending). A snapshot/clone
  /// enqueues these for the device to munmap before it freezes the shared blocks; `false` once every writable
  /// mapping of the volume has left the registry.
  #[must_use]
  pub fn has_writable_of_prefix(&self, prefix: u16) -> bool {
    self
      .mappings
      .iter()
      .any(|mapping| mapping.writable && mapping.inode.prefix() == prefix)
  }

  /// The live window that already covers `file_offset` of inode `no` with the writability asked for, if one
  /// exists (§4.6). A guest that asks twice for the same window needs one window, not two: the map paths return
  /// this instead of minting a second entry, so **the registry grows with the windows that exist rather than
  /// with the requests made**.
  ///
  /// That is what stops a re-map storm being a *cross-tenant* denial. The cap bounds the shard, so before this a
  /// volume re-mapping one file filled the registry and the next volume's first window was refused `NoSpace` —
  /// measured, 32 of 32 taken by one volume. Now a storm on one window is one entry, and to consume entries a
  /// volume must hold windows onto distinct blocks, which its own content bounds.
  ///
  /// **The caller must pin the block again on a hit**, and that is not an optimisation detail — it is the whole
  /// correctness of this. The arena's pin is refcounted (`PinnedBlock::holders`) and the device keeps one
  /// `DaxWindowEntry` **per SETUPMAPPING**, releasing each: two guest windows sharing one mapping and one pin
  /// would have the first unmap release a block the second still maps, reusable under a live guest page table.
  /// One pin per window handed out, one unpin per window released. A first attempt at this dedup missed that and
  /// was withdrawn the same day
  /// (`docs/bugs/2026-10-18-one-volumes-dax-map-storm-denies-every-other-volume.md`); the guard that catches it
  /// is `two_windows_over_one_block_each_hold_its_pin` in `crates/bridge-virtiofs/tests/device.rs`.
  ///
  /// Directory windows do not come through here ([`Volume::dax_map_dir`] has its own path) and keep their
  /// decided point-in-time re-map semantics.
  pub(crate) fn live_window(
    &self,
    no: InodeNo,
    file_offset: usize,
    writable: bool,
  ) -> Option<Mapping> {
    self.mappings.iter().copied().find(|held| {
      held.inode == no
        && held.writable == writable
        && !held.snapshot
        && file_offset >= held.block_file_start
        && file_offset < held.block_file_start.saturating_add(held.file_len.max(1))
    })
  }

  /// The block holding a guarded mapping's clean copy (§4.6 B2), the heal source a scrub restores from.
  /// Exposed so a test can corrupt it: Rowhammer flips physical rows and this copy lives in the same RAM as the
  /// block it protects, so "both corrupt" is a real state and the arm that refuses it
  /// ([`DaxScrub::Unhealable`]) has to be exercised rather than assumed.
  #[must_use]
  pub fn guard_clean_block(&self, mapping: &Mapping) -> Option<Block> {
    self
      .guards
      .iter()
      .find(|guard| &guard.mapping == mapping)
      .map(|guard| guard.clean)
  }

  /// Marks the volume `prefix` as recalling its writable windows, so [`Volume::dax_map_write`] refuses a new one
  /// until the recall ends (see the [`recalling`](Self::recalling) field for why). Idempotent. `false` when the
  /// registry is already holding `cap` recalls, which is the bound: the caller treats that as a refusal rather than
  /// letting the list grow.
  pub fn begin_recall(&mut self, prefix: u16) -> bool {
    if self.recalling.contains(&prefix) {
      return true;
    }
    if self.recalling.len() >= self.cap {
      return false;
    }
    self.recalling.push(prefix);
    true
  }

  /// Whether the volume `prefix` is recalling its writable windows.
  #[must_use]
  pub fn recalling(&self, prefix: u16) -> bool {
    self.recalling.contains(&prefix)
  }

  /// Ends the volume `prefix`'s recall, so writable maps of it are admitted again. Called by whoever began it once
  /// its snapshot has frozen or given up, and by `dax_revoke_volume` so a destroyed volume leaves nothing behind.
  pub fn end_recall(&mut self, prefix: u16) {
    self.recalling.retain(|held| *held != prefix);
  }

  /// The most live mappings — and the most recalls — the registry admits at once.
  #[must_use]
  pub fn cap(&self) -> usize {
    self.cap
  }

  /// How many volumes are recalling (the bound/dedup test's counter).
  #[must_use]
  pub fn recalling_len(&self) -> usize {
    self.recalling.len()
  }

  /// Counts a completed migration (§4.6 L2): the moving target's non-vacuity counter.
  fn count_migration(&mut self) {
    self.migrated = self.migrated.saturating_add(1);
  }

  /// Mints the next opaque [`MappingId`] (§4.6 step 2). The only way to form a [`MappingId`] — its inner is private —
  /// so a `Mapping` carrying a real identity exists only because the daemon minted it here. Monotone; saturating so
  /// an impossibly long-lived shard never wraps to a reused id.
  fn mint_id(&mut self) -> MappingId {
    let id = MappingId(self.next_id);
    self.next_id = self.next_id.saturating_add(1);
    id
  }

  /// Records a daemon-minted mapping, or refuses `NoSpace` at the bound.
  fn register(&mut self, mapping: Mapping) -> Result<(), VfsError> {
    if self.mappings.len() >= self.cap {
      self.refused = self.refused.saturating_add(1);
      return Err(VfsError::NoSpace);
    }
    self.mappings.push(mapping);
    Ok(())
  }

  /// Removes one matching mapping, counting the revoke. Returns whether one was held.
  fn remove(&mut self, mapping: &Mapping) -> bool {
    if let Some(index) = self.mappings.iter().position(|held| held == mapping) {
      self.mappings.swap_remove(index);
      self.revoked = self.revoked.saturating_add(1);
      true
    } else {
      false
    }
  }

  /// Whether the registry still holds `mapping` — the daemon's shadow of whether the guest's page table still maps
  /// it. A revoked mapping is not live, and its hardware accesses are inert.
  #[must_use]
  pub fn is_live(&self, mapping: &Mapping) -> bool {
    self.mappings.contains(mapping)
  }

  /// The live mappings.
  #[must_use]
  pub fn len(&self) -> usize {
    self.mappings.len()
  }

  /// Whether no mapping is live.
  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.mappings.is_empty()
  }

  /// Mappings revoked so far (the non-vacuity counter).
  #[must_use]
  pub fn revoked(&self) -> u64 {
    self.revoked
  }

  /// Maps refused at the bound so far.
  #[must_use]
  pub fn refused(&self) -> u64 {
    self.refused
  }

  /// The live **writable** mappings of the volume with `prefix`: the set §4.6's periodic shift walks, and what a
  /// caller names when it asks for one to be moved. Collected rather than borrowed, because shifting one borrows
  /// the store; bounded by the registry's own cap, as every list here is, and small in practice because L0 keeps
  /// writable DAX rare.
  #[must_use]
  pub fn writable_of_prefix(&self, prefix: u16) -> Vec<Mapping> {
    self
      .mappings
      .iter()
      .filter(|mapping| mapping.writable && mapping.inode.prefix() == prefix)
      .copied()
      .collect()
  }

  /// Migrations performed so far (§4.6 L2 moving target; the non-vacuity counter).
  #[must_use]
  pub fn migrated(&self) -> u64 {
    self.migrated
  }

  /// Live integrity guards (§4.6 B2).
  #[must_use]
  pub fn guards(&self) -> usize {
    self.guards.len()
  }

  /// Integrity scrubs run so far (§4.6 B2 non-vacuity counter).
  #[must_use]
  pub fn scrubbed(&self) -> u64 {
    self.scrubbed
  }

  /// Bit flips caught by a scrub so far (§4.6 B2 non-vacuity counter).
  #[must_use]
  pub fn flips(&self) -> u64 {
    self.flips
  }

  /// Blocks healed from their clean copy after a caught flip so far (§4.6 B2 non-vacuity counter).
  #[must_use]
  pub fn healed(&self) -> u64 {
    self.healed
  }
}

/// The **open (plaintext) chunk** covering file offset `file_offset`, as `(block, block_file_start, valid_len)`
/// (§4.6 "DAX maps the open frontier"). A `Body::Open` file holds exactly one open chunk over its sealed ones; this
/// returns it when `file_offset` lands in it — `block_file_start` is the chunk's file offset (`0` for a single-block
/// file, the open tail's offset for a multi-block file), `valid_len` its used bytes. A **sealed** chunk (ciphertext
/// at rest, A-92/A-99), a hole past the open chunk, or a non-`Open` body is refused `Invalid`, so the caller falls
/// back to FUSE_READ for that range. A guest's whole-page view of the returned block is scrubbed past `valid_len` by
/// the caller.
fn open_chunk_at(
  store: &Store,
  volume: &Volume,
  no: InodeNo,
  file_offset: usize,
) -> Result<(Block, usize, usize), VfsError> {
  let inode = volume.inode(store, no)?;
  let Body::Open { open, .. } = &inode.body else {
    return Err(VfsError::Invalid);
  };
  let start = usize::try_from(open.off).map_err(|_| VfsError::NoSpace)?;
  let len = usize::try_from(open.len).map_err(|_| VfsError::NoSpace)?;
  if file_offset >= start && file_offset < start.saturating_add(len) {
    Ok((open.block, start, len))
  } else {
    Err(VfsError::Invalid)
  }
}

/// The **sealed (or already-reopened) chunk extent** covering file offset `file_offset` (§4.6, the REOPEN arm): the
/// extent — carrying its chunk handle — from a `Body::Open`/`Body::Sealed` file's sealed list that spans the offset, so
/// the caller can reopen it in place. A non-chunked body is refused `Invalid`. An offset in no sealed extent (it is in
/// the open chunk, or past the file) is refused `NotFound` (distinct from the wrong-body `Invalid`), which tells the
/// device's N-map loop to STOP — the guest's window range ran off the end of the file's content, so the remaining pages
/// are simply not backed (as the authorized-window cap always left them).
fn sealed_extent_at(
  store: &Store,
  volume: &Volume,
  no: InodeNo,
  file_offset: usize,
) -> Result<crate::content::Extent, VfsError> {
  let inode = volume.inode(store, no)?;
  let sealed: &[crate::content::Extent] = match &inode.body {
    Body::Open { sealed, .. } => sealed,
    Body::Sealed(sealed) => sealed,
    _ => return Err(VfsError::Invalid),
  };
  for extent in sealed {
    let start = usize::try_from(extent.off).map_err(|_| VfsError::NoSpace)?;
    let len = usize::try_from(extent.len).map_err(|_| VfsError::NoSpace)?;
    if file_offset >= start && file_offset < start.saturating_add(len) {
      return Ok(*extent);
    }
  }
  Err(VfsError::NotFound)
}

/// The **reopened chunk** covering file offset `file_offset`, as `(block, chunk_start, valid_len)` (§4.6, the REOPEN
/// arm): a sealed-list chunk whose seal has been cleared — decrypted in place for a DAX map. `None` when the offset is
/// in no sealed-list chunk, or the chunk there is still sealed (not reopened). [`Volume::dax_still_current`] leans on
/// this: a mapping whose chunk was re-sealed under it (its seal back) no longer matches and is torn, so the guest
/// re-maps and re-reopens it.
fn reopened_chunk_at(
  store: &Store,
  volume: &Volume,
  no: InodeNo,
  file_offset: usize,
) -> Option<(Block, usize, usize)> {
  let extent = sealed_extent_at(store, volume, no, file_offset).ok()?;
  let crate::content::ExtentSrc::Chunk { chunk, .. } = extent.src else {
    return None;
  };
  let c = store.content.chunk(chunk)?;
  if c.seal.is_some() {
    return None;
  }
  let start = usize::try_from(extent.off).ok()?;
  let len = usize::try_from(extent.len).ok()?;
  Some((c.block, start, len))
}

impl Volume {
  /// Maps the **open (plaintext) chunk** covering file offset `file_offset` into a guest window, **read-only** (§4.6
  /// "DAX maps the open frontier"). The chunk's arena block is mapped directly and **pinned** (never reused, freed or
  /// sealed while mapped — [`Volume::seal_pending`] defers the seal); every byte of the block past the chunk's valid
  /// length is scrubbed to zero, so the guest's whole-page view never reaches a prior allocation's bytes or a sealed
  /// neighbour. An **inline** file (bytes in the inode, no content block) is materialized at offset 0 into a throwaway
  /// block the mapping owns, as [`Volume::dax_map_dir`] does. A **sealed** chunk (ciphertext at rest, A-92/A-99), a
  /// hole, a directory, an attribute inode, or a full registry is refused — the caller falls back to FUSE_READ for
  /// that range. A single-block file maps at offset 0 (the whole block); a multi-block file maps only its open tail.
  pub fn dax_map_read_at(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    file_offset: usize,
  ) -> Result<Mapping, VfsError> {
    // Defence in depth for the device seam: a mapping is of a namespace object only, never an attribute
    // (xattr-value) inode — so a crafted or confused map of an attribute number is `NotFound`, never a window onto
    // an attribute's bytes, mirroring the transports' `namespace_inode` gate.
    self.namespace_inode(&*store, no)?;
    // The window asked for may already exist: hand back the one that does, with a pin of its own for the window
    // this call hands out ([`DaxRegistry::live_window`]). A write or truncate of the inode revokes its mappings,
    // so a live one is never stale.
    if let Some(live) = store.dax.live_window(no, file_offset, false) {
      store.content.arena_mut().pin(live.block)?;
      return Ok(live);
    }
    // An inline file keeps its bytes in the inode with no content block; materialize them at offset 0 into a
    // throwaway block the mapping owns, exactly as `dax_map_dir` does. The clone releases the store borrow before
    // the mutable materialize.
    let inline = match &self.inode(&*store, no)?.body {
      Body::Inline(bytes) if file_offset == 0 => Some(bytes.clone()),
      _ => None,
    };
    if let Some(bytes) = inline {
      return self.map_owned_block(store, no, 0, &bytes);
    }
    // The open (plaintext) chunk at this offset maps zero-copy, directly off its arena block.
    if let Ok((block, block_file_start, file_len)) = open_chunk_at(&*store, self, no, file_offset) {
      store.content.arena_mut().pin(block)?;
      if let Some(tail) = store
        .content
        .arena_mut()
        .bytes_mut(block)
        .and_then(|bytes| bytes.get_mut(file_len..))
      {
        tail.fill(0);
      }
      let mapping = Mapping {
        id: store.dax.mint_id(),
        inode: no,
        block_file_start,
        block,
        file_len,
        writable: false,
        snapshot: false,
      };
      if let Err(refusal) = store.dax.register(mapping) {
        // The registry is full: undo the pin so a refused map leaves nothing held.
        if store.content.arena_mut().unpin(block).is_err() {
          store.release_refusals = store.release_refusals.saturating_add(1);
        }
        return Err(refusal);
      }
      return Ok(mapping);
    }
    // REOPEN arm (A/B against view): a SEALED chunk (ciphertext at rest, A-92/A-99) cannot be mapped zero-copy — a
    // guest MMU would read ciphertext, and a refused SETUPMAPPING SIGBUSes a `dax=always` mmap. Reopen it in place: the
    // chunk's representation transitions sealed→open, decrypted in its OWN block (no throwaway copy, one
    // representation), and that block is mapped directly and pinned exactly as the open chunk is. The inode is marked
    // for re-seal, so the idle sweep re-encrypts the chunk once its window is gone ([`Volume::reseal_pending`]):
    // content is plaintext at rest only while a guest maps it. A write to the chunk revokes the mapping first.
    let extent = sealed_extent_at(&*store, self, no, file_offset)?;
    let chunk_start = usize::try_from(extent.off).map_err(|_| VfsError::NoSpace)?;
    let file_len = usize::try_from(extent.len).map_err(|_| VfsError::NoSpace)?;
    let block = store.content.reopen_in_place(&extent)?;
    store.content.arena_mut().pin(block)?;
    if let Some(tail) = store
      .content
      .arena_mut()
      .bytes_mut(block)
      .and_then(|bytes| bytes.get_mut(file_len..))
    {
      tail.fill(0);
    }
    // Tell the learner: a chunk the sweep re-sealed inside the hold means the guest came back faster than the hold
    // assumed, and the hold grows (§4.6, the learned re-seal hold).
    if let crate::content::ExtentSrc::Chunk { chunk, .. } = extent.src {
      self.note_dax_reopen(chunk.index());
    }
    self.mark_reseal(no);
    let mapping = Mapping {
      id: store.dax.mint_id(),
      inode: no,
      block_file_start: chunk_start,
      block,
      file_len,
      writable: false,
      snapshot: false,
    };
    if let Err(refusal) = store.dax.register(mapping) {
      // The registry is full: undo the pin so a refused map leaves nothing held (the chunk stays reopened; the sweep
      // re-seals it, as it would after a normal unmap).
      if store.content.arena_mut().unpin(block).is_err() {
        store.release_refusals = store.release_refusals.saturating_add(1);
      }
      return Err(refusal);
    }
    Ok(mapping)
  }

  /// Maps inode `no`'s content from offset 0 — the whole single block of a single-block file, or an inline file —
  /// read-only ([`Volume::dax_map_read_at`]). The same-kernel local map (A-135) and the single-block callers use
  /// this; the device maps per `SETUPMAPPING` offset through [`Volume::dax_map_read_at`], so a multi-block file's
  /// open tail is reachable there while offset 0 (its sealed head) is refused to FUSE_READ.
  pub fn dax_map_read(&mut self, store: &mut Store, no: InodeNo) -> Result<Mapping, VfsError> {
    self.dax_map_read_at(store, no, 0)
  }

  /// Maps directory inode `no`'s entries into a guest window, **read-only**, as a flat guest-readable snapshot
  /// (§4.6 metadata DAX, READDIR). A directory's entries are not stored in a mappable content block (they live in a
  /// copy-on-write B+-tree of slotted slab blocks, `dirtree`), so the daemon serializes them into a freshly allocated
  /// content block and maps that — a guest then lists the directory by reading the block through its own page table,
  /// with no FUSE READDIR round-trip per listing. The block is a throwaway the mapping **owns**
  /// ([`Mapping::snapshot`]): pinned while mapped, freed when the mapping is revoked. A read-only snapshot taken at
  /// map time — a later daemon change to the directory revokes the mapping (an inode's mappings are revoked on its
  /// mutation, as a file's are on write), so the guest re-maps to see the change.
  ///
  /// Refused as [`Volume::dax_map_read`] is (an attribute inode is `NotFound`, defence in depth for the device seam);
  /// `NotDirectory` for a non-directory; `NoSpace` if the entry count overflows or the registry is full;
  /// `NameTooLong` for a name past 255 bytes. Layout (little-endian, READDIRPLUS — each entry carries its `stat`):
  /// `u32` entry count, then that many records — `u64` inode, `u8` kind (1 file, 2 dir, 3 symlink, 4 fifo, 5 socket),
  /// `u64` size, `i64` mtime (ns), `u32` mode, `u32` uid, `u32` gid, `u32` nlink, `u8` name length, the name's bytes.
  pub fn dax_map_dir(&mut self, store: &mut Store, no: InodeNo) -> Result<Mapping, VfsError> {
    self.namespace_inode(&*store, no)?;
    // Serialize while the rows borrow the store immutably; the block allocation below is the mutable step.
    let serialized = {
      let rows = self.readdir_no(&*store, no)?;
      let count = u32::try_from(rows.len()).map_err(|_| VfsError::NoSpace)?;
      let mut buf = Vec::new();
      buf.extend_from_slice(&count.to_le_bytes());
      for row in &rows {
        let name = row.name.as_bytes();
        let name_len = u8::try_from(name.len()).map_err(|_| VfsError::NameTooLong)?;
        /// Format: the dax_map_dir listing's entry-kind byte (§4.6 READDIRPLUS layout): 3 symlink, 4 fifo,
        /// 5 socket (1 file and 2 dir are the trivial values the literal check exempts).
        const SYMLINK_KIND: u8 = 3;
        const FIFO_KIND: u8 = 4;
        const SOCKET_KIND: u8 = 5;
        let kind = match row.kind {
          crate::inode::Kind::File => 1u8,
          crate::inode::Kind::Dir => 2,
          crate::inode::Kind::Symlink => SYMLINK_KIND,
          crate::inode::Kind::Fifo => FIFO_KIND,
          crate::inode::Kind::Socket => SOCKET_KIND,
        };
        // READDIRPLUS: each entry carries its attributes, so a guest gets the `stat` of every entry from the one
        // mapped block — no per-entry GETATTR round-trip (the metadata the kernel's readdirplus batches, served
        // through the guest's page table instead of a FUSE call).
        let attrs = self.stat(&*store, row.inode)?;
        buf.extend_from_slice(&row.inode.0.to_le_bytes());
        buf.push(kind);
        buf.extend_from_slice(&attrs.size.to_le_bytes());
        buf.extend_from_slice(&attrs.mtime.to_le_bytes());
        buf.extend_from_slice(&attrs.mode.to_le_bytes());
        buf.extend_from_slice(&attrs.uid.to_le_bytes());
        buf.extend_from_slice(&attrs.gid.to_le_bytes());
        buf.extend_from_slice(&attrs.nlink.to_le_bytes());
        buf.push(name_len);
        buf.extend_from_slice(name);
      }
      buf
    };
    self.map_owned_block(store, no, 0, &serialized)
  }

  /// Maps a freshly allocated throwaway block holding `serialized` into a guest window, **read-only**, as a snapshot
  /// the mapping **owns** — pinned while mapped, freed when revoked. `block_file_start` is the file offset the block
  /// represents (`0` for a directory's serialized entries or an inline file; the chunk's offset for a decrypted
  /// sealed-chunk view). Shared by [`Volume::dax_map_dir`], the inline path of [`Volume::dax_map_read_at`], and the
  /// VIEW arm's decrypted sealed-chunk view. A later daemon change to the inode revokes the mapping, so the guest
  /// re-maps to see it. A refused map (an unpinnable block, or a full registry) leaves nothing allocated or held.
  fn map_owned_block(
    &mut self,
    store: &mut Store,
    no: InodeNo,
    block_file_start: usize,
    serialized: &[u8],
  ) -> Result<Mapping, VfsError> {
    let file_len = serialized.len();
    let block = store.content.alloc_block(file_len.max(1), false)?;
    // The copy must happen, and the guest must never be handed a window whose bytes were not written. The buddy
    // allocator returns a block at least as large as asked, so the short case is unreachable — but written as
    // `if let Some(..) {}` with no else it would, if that contract ever changed, register a mapping over an
    // unwritten block and the guest would read zeroes where the entries should be, with nothing refused and
    // nothing counted. Refused instead, and the throwaway freed, so a window that exists is a window that was
    // filled (CLAUDE.md #9: no swallowed failure).
    let filled = match store.content.arena_mut().bytes_mut(block) {
      Some(bytes) if bytes.len() >= file_len => {
        if let Some(head) = bytes.get_mut(..file_len) {
          head.copy_from_slice(serialized);
        }
        if let Some(tail) = bytes.get_mut(file_len..) {
          tail.fill(0);
        }
        true
      }
      _ => false,
    };
    if !filled {
      let _ = store.content.arena_mut().free(block);
      return Err(VfsError::NoSpace);
    }
    if let Err(pin) = store.content.arena_mut().pin(block) {
      // Allocated but unpinnable: free the throwaway so a refused map leaves nothing held.
      let _ = store.content.arena_mut().free(block);
      return Err(VfsError::Memory(pin));
    }
    let mapping = Mapping {
      id: store.dax.mint_id(),
      inode: no,
      block_file_start,
      block,
      file_len,
      writable: false,
      snapshot: true,
    };
    if let Err(refusal) = store.dax.register(mapping) {
      if store.content.arena_mut().unpin(block).is_err() {
        store.release_refusals = store.release_refusals.saturating_add(1);
      }
      let _ = store.content.arena_mut().free(block);
      return Err(refusal);
    }
    Ok(mapping)
  }

  /// Maps inode `no`'s content block **writable** (§4.6), **copying it up first** so the block the guest writes is
  /// this version's alone — a snapshot or clone that shared it keeps its own, untouched. The copy-up goes through the
  /// VFS write boundary (where copy-on-write, quota and journal live); the now-private block is **pinned**, its tail
  /// past the file scrubbed, and the mapping recorded. Refused as [`Volume::dax_map_read`] is.
  pub fn dax_map_write(&mut self, store: &mut Store, no: InodeNo) -> Result<Mapping, VfsError> {
    // L0 (§4.6 multi-tenancy, the Rowhammer gate's outermost layer): a *writable* mapping is the only DAX mode a guest
    // can hammer, so it is refused unless the volume's owner has opted in ([`Volume::enable_writable_dax`]). Read-only
    // DAX (immutable shared content) is the default and never reaches here. Checked before the copy-up below, so a
    // refused writable map mutates nothing.
    if !self.writable_dax {
      return Err(VfsError::NotPermitted);
    }
    // A snapshot of this volume has recalled its writable windows and is waiting for the device to munmap them
    // (§4.6 step 5). Granting another one now would put that snapshot back where it started on its next retry, and
    // a guest doing it deliberately would hold the snapshot off forever — the livelock this exclusion closes.
    // Checked before any effect, so a refused map copies nothing up.
    if store.dax.recalling(no.prefix()) {
      return Err(VfsError::RecallInFlight);
    }
    // As for a read window, and here it also saves the copy-up, which is the expensive half of this path.
    if let Some(live) = store.dax.live_window(no, 0, true) {
      store.content.arena_mut().pin(live.block)?;
      return Ok(live);
    }
    // Defence in depth: a writable mapping, like a read mapping, is of a namespace object only — never an
    // attribute inode (`Volume::dax_map_read`).
    self.namespace_inode(&*store, no)?;
    let file_len = usize::try_from(self.stat(&*store, no)?.size).map_err(|_| VfsError::NoSpace)?;
    let mut current = vec![0u8; file_len];
    self.read(&*store, no, 0, &mut current)?;
    // Force the copy-up through the write boundary. No mapping is registered yet, so this write revokes nothing.
    self.write(store, no, 0, &current)?;
    // The copy-up made the file a single open block at offset 0 (a writable map of a multi-block file is refused
    // here, as before — offset 0 is a sealed chunk once the file spans more than one). Writable per-chunk DAX is the
    // gated follow-on; this stays the single-block writable path.
    let (block, block_file_start, file_len) = open_chunk_at(&*store, self, no, 0)?;
    store.content.arena_mut().pin(block)?;
    if let Some(tail) = store
      .content
      .arena_mut()
      .bytes_mut(block)
      .and_then(|bytes| bytes.get_mut(file_len..))
    {
      tail.fill(0);
    }
    let mapping = Mapping {
      id: store.dax.mint_id(),
      inode: no,
      block_file_start,
      block,
      file_len,
      writable: true,
      snapshot: false,
    };
    if let Err(refusal) = store.dax.register(mapping) {
      if store.content.arena_mut().unpin(block).is_err() {
        store.release_refusals = store.release_refusals.saturating_add(1);
      }
      return Err(refusal);
    }
    Ok(mapping)
  }

  /// **Migrates** a live writable mapping to a fresh block (§4.6 L2, the moving target) — the stripe-shift primitive
  /// the periodic defence composes from. The file's content is relocated to a physically different block (so a
  /// Rowhammer attacker tracking the old physical rows loses its target), the inode repointed to it (so every
  /// non-DAX reader — the daemon path, recovery, another client — reaches the moved content, never a stale block),
  /// and the window handed off with the two-phase revoke: the **new** block is pinned and its mapping minted *before*
  /// the **old** mapping is revoked-pending, and the old block stays **pinned** — its free deferred, its bytes kept —
  /// until the device confirms the window unmap and calls [`Store::dax_release_after_unmap`] (the stale-TLB hold). So
  /// at no point is a block both reachable through the guest's possibly-stale page table and reusable by a neighbour.
  ///
  /// **The device half this depends on is not built, and that is why nothing calls this yet.** The intended
  /// hand-off is that the device remaps the guest window old→new — an atomic `MAP_FIXED`/`mremap` over the same
  /// guest address, so the guest re-faults transparently onto the moved bytes — and **no such daemon-initiated
  /// remap exists** in `crates/bridge-virtiofs`. A guest can replace its own window by sending SETUPMAPPING at the
  /// same offset, but the daemon cannot push one, and the only daemon→device channel today is the pending-revoke
  /// queue, which this deliberately does **not** use: an inode-global revoke would tear every window of the inode
  /// rather than move the one that is being shifted. So whoever wires L2's trigger owes the backend-initiated
  /// remap (or a decision to use the revoke-and-refault path instead and accept the tear). Until then this is a
  /// complete and measured primitive with no production caller — tests and `dax_bench` only. Recorded in
  /// `docs/wip/GAPS.md` under the L2 owed list.
  ///
  /// Refused `Invalid` for a read-only mapping (immutable shared
  /// content a guest cannot hammer into a neighbour, and moving it would break the share), a revoked mapping, or one
  /// whose block already moved; `NoSpace`/`PublishNeeded` if no fresh block can back the shift — the caller keeps the
  /// old mapping and retries, never a lost window. The pseudo-random *placement* of the fresh block within a bounded
  /// physically-contiguous sandbox ([`stripe_target`]) is the L1 step bound to the hugetlb sandbox; here the arena
  /// provides the fresh block and the VFS half of the handoff is complete and exact — the device half above is not.
  pub fn dax_migrate(&mut self, store: &mut Store, old: &Mapping) -> Result<Mapping, VfsError> {
    if !old.writable || !store.dax.is_live(old) {
      return Err(VfsError::Invalid);
    }
    let Some(handle) = crate::trie::get(&store.tries, self.inode_root, old.inode) else {
      return Err(VfsError::NotFound);
    };
    let body = std::mem::replace(&mut store.inodes.get_mut(handle)?.body, Body::None);
    let Body::Open { mut open, sealed } = body else {
      store.inodes.get_mut(handle)?.body = body;
      return Err(VfsError::Invalid);
    };
    if !sealed.is_empty() || open.block != old.block {
      store.inodes.get_mut(handle)?.body = Body::Open { open, sealed };
      return Err(VfsError::Invalid);
    }
    // Draw the destination from this volume's L1 sandbox (§4.6): a different physical row of a huge page the
    // kernel confirmed it gave us, with guard rows at both ends and every other row held by the zone itself. A
    // block the general allocator picked would be a *virtual* move the kernel is free to back with the same
    // physical row, which is the security theatre §4.6 names — so a host that cannot offer a sandbox is refused
    // here rather than served a shift that does not shift.
    let slot_bytes = NonZeroUsize::new(store.content.chunk_bytes());
    let target = match slot_bytes
      .ok_or(VfsError::NoPhysicalIsolation)
      .and_then(|slot| {
        store.dax_zones.shift_target(
          old.inode.prefix(),
          store.content.arena_mut(),
          slot,
          old.block,
        )
      }) {
      Ok(target) => target,
      Err(refusal) => {
        store.inodes.get_mut(handle)?.body = Body::Open { open, sealed };
        return Err(refusal);
      }
    };
    // Relocate the content onto that row. The old block is *not* freed: a sandbox slot goes back to the sandbox
    // (below), and a block from outside it is freed here, which — because `old` pins it — is the *deferred* free
    // whose bytes are kept, the stale-TLB hold. On a refusal nothing moved: restore the body and surface it.
    let vacated = match store.content.relocate_into(open.as_mut(), target) {
      Ok(vacated) => vacated,
      Err(refusal) => {
        store.dax_zones.return_slot(old.inode.prefix(), target);
        store.inodes.get_mut(handle)?.body = Body::Open { open, sealed };
        return Err(refusal);
      }
    };
    let new_block = open.block;
    // The inode now reaches the moved block; non-DAX readers see the file unchanged.
    store.inodes.get_mut(handle)?.body = Body::Open { open, sealed };
    // Pin the moved-to block for the new mapping and scrub its tail past the file, as a fresh map does, so the
    // whole-page guest view never reaches the arena's prior bytes.
    if let Err(refusal) = store.content.arena_mut().pin(new_block) {
      // A pin of a just-allocated block failing is a broken arena invariant; the content is safe at `new_block` (the
      // inode points there) and the old mapping is still inert-pending — surface it typed, never a panic.
      return Err(refusal.into());
    }
    if let Some(tail) = store
      .content
      .arena_mut()
      .bytes_mut(new_block)
      .and_then(|bytes| bytes.get_mut(old.file_len..))
    {
      tail.fill(0);
    }
    // Phase one of the handoff: the OLD mapping goes inert (leaves the registry); its block stays pinned until
    // [`Store::dax_release_after_unmap`] is called after the VMM confirms the unmap and cross-core TLB flush.
    store.dax_revoke_pending(old);
    let mapping = Mapping {
      id: store.dax.mint_id(),
      inode: old.inode,
      block_file_start: old.block_file_start,
      block: new_block,
      file_len: old.file_len,
      writable: true,
      snapshot: old.snapshot,
    };
    if let Err(refusal) = store.dax.register(mapping) {
      // The registry had room (we just removed `old`), so this is defensive: unpin the new block so a refused
      // migration leaves nothing held, and surface the refusal.
      if store.content.arena_mut().unpin(new_block).is_err() {
        store.release_refusals = store.release_refusals.saturating_add(1);
      }
      return Err(refusal);
    }
    // The row the content left: a sandbox slot returns to the sandbox, so no neighbour can take a row inside this
    // tenant's page while the tenant still writes the page. A block from outside the sandbox is freed, deferred
    // while `old` pins it, exactly as a relocation through the general allocator frees it.
    if !store.dax_zones.return_slot(old.inode.prefix(), vacated)
      && store.content.arena_mut().free(vacated).is_err()
    {
      store.release_refusals = store.release_refusals.saturating_add(1);
    }
    // Tell the device, or its window still names the mapping this shift revoked and the guest reads a page the
    // registry no longer authorizes until something else happens to tear it. The queue is the one the device
    // already drains every service pass (`AdmittedDevice::service` -> `tear_down_inode_windows`), and
    // `writable_only` is what makes it right here: it tears this inode's *writable* windows — which, since a
    // writable map is the single-block open path, is exactly the one being shifted — and leaves any read-only
    // window of the same inode alone. That answers §4.6's objection to using this queue ("an inode-global revoke
    // would tear every window of the inode rather than move the one that is being shifted"), which was written
    // before the flag existed. The guest re-faults, SETUPMAPPING runs [`Volume::dax_map_write`], and its
    // live-window path finds this freshly-minted mapping and maps the row the content moved to.
    //
    // Enqueued only here, after the new mapping is registered, so a shift that refused part-way tears nothing.
    store.dax.enqueue_pending_revoke(old.inode, true);
    store.dax.count_migration();
    Ok(mapping)
  }

  /// One cooperative slice of §4.6 L2's **periodic shift** (the moving target's trigger): the windows in `due`,
  /// from index `from`, shifted onto different physical rows of this volume's L1 sandbox until `budget_bytes`
  /// have been copied.
  ///
  /// `due` is a snapshot the caller takes once per round ([`DaxRegistry::writable_of_prefix`]) and holds across
  /// slices. Snapshotting is what makes the round terminate and stay fair: a shift mints a *new* mapping, so a
  /// list re-read mid-round would offer the same content again and the sweep would chase its own tail, while a
  /// fixed list visits every window exactly once. An entry that is no longer live when its turn comes — the guest
  /// unmapped it, a write revoked it, a snapshot recalled it — is counted refused and skipped, never retried in
  /// this round.
  ///
  /// Nothing here decides *whether* to shift: that is the caller's cadence, derived from the hammer window
  /// (§4.6: 16–32 ms, from the lowest threshold, not the classic 64 ms). This decides only how much of a round
  /// fits in one slice, so a shard with many mapped windows keeps serving between them (§4.3).
  pub fn dax_shift_slice(
    &mut self,
    store: &mut Store,
    due: &[Mapping],
    from: usize,
    budget_bytes: u64,
  ) -> ShiftSlice {
    let mut slice = ShiftSlice {
      next: from,
      ..ShiftSlice::default()
    };
    let Some(window) = due.get(from..) else {
      slice.next = due.len();
      return slice;
    };
    for mapping in window {
      match self.dax_migrate(store, mapping) {
        Ok(moved) => {
          slice.shifted = slice.shifted.saturating_add(1);
          slice.bytes = slice
            .bytes
            .saturating_add(u64::try_from(moved.file_len).unwrap_or(u64::MAX));
        }
        Err(VfsError::NoPhysicalIsolation) => {
          // This host backs no sandbox, so no window of it can be shifted: the rest of the round would refuse
          // the same way. Stop the round rather than walk every window to learn it again.
          slice.unsupported = true;
          slice.next = due.len();
          return slice;
        }
        Err(_) => slice.refused = slice.refused.saturating_add(1),
      }
      slice.next = slice.next.saturating_add(1);
      if slice.bytes >= budget_bytes {
        break;
      }
    }
    slice
  }

  /// Whether `mapping` still names its file's current content block and length. The content paths revoke a mapping
  /// before the block moves, so a live mapping always passes there; this is a consistency backstop, not the primary
  /// guard. It is the backstop a snapshot reader's `advance` leans on (§4.6 "revoke on advance"): advance does not
  /// enqueue an inode-global revoke (that would tear every reader's windows, not just the one moving), so the
  /// advancing device re-validates its own windows against its new view through this and tears each one whose block
  /// the new view no longer backs — the changed inodes — keeping the windows onto unchanged (block-shared) content.
  pub fn dax_still_current(&self, store: &Store, mapping: &Mapping) -> bool {
    if mapping.snapshot {
      // A materialized snapshot (a directory's entries, or an inline file's bytes) owns a throwaway block that is
      // never the inode's content block, so there is nothing to compare against `file_block`. It stays current as
      // long as the registry still holds it — a mutation of the inode revokes it — which is exactly the liveness the
      // guest's `Store::dax_read_raw` relies on alone. (Before this, `file_block` returned `Invalid` for the inode's
      // non-single-block body and a snapshot mapping read as stale through `Volume::dax_read`, the local-client path.)
      return store.dax.is_live(mapping);
    }
    // The open tail maps directly off the open extent.
    if matches!(
      open_chunk_at(store, self, mapping.inode, mapping.block_file_start),
      Ok((block, start, file_len))
        if block == mapping.block && start == mapping.block_file_start && file_len == mapping.file_len
    ) {
      return true;
    }
    // The REOPEN arm: a reopened sealed-list chunk (decrypted in place, seal cleared) maps directly off its own block.
    // It stays current while the chunk is still reopened on the same block and length; a re-seal under it tears it.
    matches!(
      reopened_chunk_at(store, self, mapping.inode, mapping.block_file_start),
      Some((block, start, file_len))
        if block == mapping.block && start == mapping.block_file_start && file_len == mapping.file_len
    )
  }

  /// The daemon-mediated read of a mapped page (the control path, not what a hardware DAX guest uses): the block's
  /// bytes `[off, off + len)`, bounded to the mapped page span so a guest-chosen length is never an unbounded
  /// allocation (CLAUDE.md #8). Refused if the mapping is revoked or no longer current.
  pub fn dax_read(
    &self,
    store: &Store,
    mapping: &Mapping,
    off: usize,
    len: usize,
  ) -> Result<Vec<u8>, VfsError> {
    if !store.dax.is_live(mapping) || !self.dax_still_current(store, mapping) {
      return Err(VfsError::Invalid);
    }
    let end = off.checked_add(len).ok_or(VfsError::NoSpace)?;
    if end > mapping.page_span() {
      return Err(VfsError::Invalid);
    }
    let block = store
      .content
      .arena()
      .bytes(mapping.block)
      .unwrap_or_default();
    let mut out = vec![0u8; len];
    for (index, slot) in out.iter_mut().enumerate() {
      if let Some(&byte) = block.get(off.saturating_add(index)) {
        *slot = byte;
      }
    }
    Ok(out)
  }

  /// The daemon-mediated write into a writable mapping (the control path): straight into the arena block, bounded to
  /// the file's authorized length. Refused if the mapping is read-only, revoked, no longer current, or past the
  /// length — growing the file needs a quota and lease check the guest cannot make itself.
  pub fn dax_write(
    &self,
    store: &mut Store,
    mapping: &Mapping,
    off: usize,
    bytes: &[u8],
  ) -> Result<(), VfsError> {
    if !mapping.writable || !store.dax.is_live(mapping) || !self.dax_still_current(&*store, mapping)
    {
      return Err(VfsError::Invalid);
    }
    let end = off.checked_add(bytes.len()).ok_or(VfsError::NoSpace)?;
    if end > mapping.file_len {
      return Err(VfsError::Invalid);
    }
    let block = store
      .content
      .arena_mut()
      .bytes_mut(mapping.block)
      .ok_or(VfsError::Invalid)?;
    block
      .get_mut(off..end)
      .ok_or(VfsError::NoSpace)?
      .copy_from_slice(bytes);
    Ok(())
  }
}

impl Store {
  /// The guest's page table reading **directly** — the raw mapped block bytes, modeling the hardware DAX path that
  /// never calls the daemon. A **live** mapping's block is pinned, so these are always the file's own bytes (never a
  /// neighbour's). A **revoked** mapping is inert: its window was `munmap`'d, so the hardware access would fault — the
  /// read returns zeros, never another file's bytes.
  #[must_use]
  pub fn dax_read_raw(&self, mapping: &Mapping, off: usize, len: usize) -> Vec<u8> {
    let span = mapping.page_span();
    let want = len.min(span.saturating_sub(off.min(span)));
    if !self.dax.is_live(mapping) {
      return vec![0u8; want];
    }
    let block = self
      .content
      .arena()
      .bytes(mapping.block)
      .unwrap_or_default();
    let mut out = vec![0u8; want];
    for (index, slot) in out.iter_mut().enumerate() {
      if let Some(&byte) = block.get(off.saturating_add(index)) {
        *slot = byte;
      }
    }
    out
  }

  /// The guest's page table writing **directly** — raw into the mapped block, modeling the hardware DAX path. A
  /// **live** mapping's block is pinned, so the write lands only on the file's own (private) block. A **revoked**
  /// mapping is inert: the window was torn down, so the write touches nothing — this is why teardown, not a per-access
  /// check, is the second half of the gate.
  pub fn dax_write_raw(&mut self, mapping: &Mapping, off: usize, bytes: &[u8]) {
    if !self.dax.is_live(mapping) {
      return;
    }
    let span = mapping.page_span();
    if let Some(block) = self.content.arena_mut().bytes_mut(mapping.block) {
      let end = off.saturating_add(bytes.len()).min(span).min(block.len());
      if off < end
        && let Some((dst, src)) = block
          .get_mut(off..end)
          .zip(bytes.get(..end.saturating_sub(off)))
      {
        dst.copy_from_slice(src);
      }
    }
  }

  /// Whether `mapping` is still live (not revoked).
  #[must_use]
  pub fn dax_is_live(&self, mapping: &Mapping) -> bool {
    self.dax.is_live(mapping)
  }

  /// The live DAX mapping count.
  #[must_use]
  pub fn dax_mappings(&self) -> usize {
    self.dax.len()
  }

  /// Revokes one mapping (§4.6 teardown): removes it from the registry and unpins its block, so the block may be
  /// freed and reused once no other mapping holds it. Returns whether it was held. An unpin the arena refuses (a
  /// broken invariant) is counted in `release_refusals`, never silently dropped.
  pub fn dax_revoke(&mut self, mapping: &Mapping) -> bool {
    if !self.dax.remove(mapping) {
      return false;
    }
    if self.content.arena_mut().unpin(mapping.block).is_err() {
      self.release_refusals = self.release_refusals.saturating_add(1);
    }
    // A revoked mapping's integrity guard (§4.6 B2), if any, goes with it so its clean copy is not left pinned.
    self.drop_guard(mapping);
    // A metadata snapshot owns its throwaway block (no inode references it); free it now the pin is gone, or it would
    // leak. A file's content block (the common case) is inode-owned — a revoke only unpinned it, never freed it.
    self.free_snapshot_block(mapping);
    true
  }

  /// **Nothing in production calls this.** B2 is a complete primitive with no wiring: no caller outside tests and
  /// `dax_bench`, and no surface on the `Bridge` trait, so the device cannot reach it either. Nothing decides
  /// which read-only mappings to guard, and [`Store::dax_scrub`] below has nothing scheduling it — so the
  /// "self-healing safety net" heals nothing in a running daemon today. Two decisions are owed before it can be
  /// wired, and both are Ada's: **what to guard** (a guard pins a full clean copy of the block, so guarding every
  /// read-only mapping doubles the RAM of mapped content — the "do we need it at all, and that much" question),
  /// and **how often to scrub** (a cadence with no anchor to derive from today). Not a live hole: DAX is
  /// unadvertised, so no guest window exists to protect. Recorded in `docs/wip/GAPS.md`.
  ///
  /// Registers an integrity guard for a **read-only** mapping (§4.6 B2, the self-healing safety net): captures the
  /// BLAKE3 of the block's clean bytes and a pinned clean copy to heal from, so a later [`Store::dax_scrub`] catches a
  /// Rowhammer bit flip and restores it. Refused `Invalid` for a *writable* mapping — a writable block changes
  /// legitimately, so a content checksum cannot tell a flip from the guest's own direct write; writable blocks are
  /// defended by the zones and the moving target, never a checksum — or for a revoked mapping; `NoSpace`/
  /// `PublishNeeded` if no block can back the clean copy or the guard bound is reached. Re-guarding a mapping refreshes
  /// it (the old clean copy is freed).
  pub fn dax_guard(&mut self, mapping: &Mapping) -> Result<(), VfsError> {
    if mapping.writable || !self.dax.is_live(mapping) {
      return Err(VfsError::Invalid);
    }
    if self.dax.guards.len() >= self.dax.cap {
      return Err(VfsError::NoSpace);
    }
    // The clean bytes and their checksum, captured now while the content is known good.
    let identity = match self.content.arena().bytes(mapping.block) {
      Some(bytes) => *blake3::hash(bytes).as_bytes(),
      None => return Err(VfsError::Invalid),
    };
    let size = self
      .content
      .arena()
      .bytes(mapping.block)
      .map(<[u8]>::len)
      .unwrap_or(0);
    let locked = self.content.arena().is_locked(mapping.block);
    let clean = self.content.alloc_block(size, locked)?;
    if let Err(refusal) = self.content.arena_mut().pin(clean) {
      if self.content.arena_mut().free(clean).is_err() {
        self.release_refusals = self.release_refusals.saturating_add(1);
      }
      return Err(refusal.into());
    }
    // Copy the clean bytes into the heal source (both blocks are `size` bytes).
    let carry = self
      .content
      .arena()
      .bytes(mapping.block)
      .map(<[u8]>::to_vec)
      .unwrap_or_default();
    if let Some(dst) = self.content.arena_mut().bytes_mut(clean)
      && let Some(slot) = dst.get_mut(..carry.len())
    {
      slot.copy_from_slice(&carry);
    }
    // Replace any existing guard for this mapping (free its old clean copy), then record the fresh one.
    self.drop_guard(mapping);
    self.dax.guards.push(DaxGuard {
      mapping: *mapping,
      identity,
      clean,
    });
    Ok(())
  }

  /// Scrubs a guarded read-only mapping (§4.6 B2): re-checksums the live block and, on a mismatch (a bit flip),
  /// restores it from the clean copy and re-verifies — the asynchronous safety net for the one hazard the daemon
  /// cannot see directly (a hardware write/flip on a mapped page). Returns [`DaxScrub`].
  ///
  /// **There is no background scanner.** This sentence used to read "a background scanner calls this over its
  /// guards a bounded number per step; an explicit validation pass calls it on demand", and neither exists:
  /// nothing outside tests and `dax_bench` calls this, and the `Bridge` trait has no scrub surface, so the device
  /// serve loop cannot reach it. The scanner, and the cadence it would run at, are owed with the guard policy
  /// above.
  pub fn dax_scrub(&mut self, mapping: &Mapping) -> DaxScrub {
    let Some(guard) = self
      .dax
      .guards
      .iter()
      .find(|g| &g.mapping == mapping)
      .copied()
    else {
      return DaxScrub::Unguarded;
    };
    self.dax.scrubbed = self.dax.scrubbed.saturating_add(1);
    let clean = match self.content.arena().bytes(mapping.block) {
      Some(live) => *blake3::hash(live).as_bytes() == guard.identity,
      None => return DaxScrub::Unguarded,
    };
    if clean {
      return DaxScrub::Clean;
    }
    // A flip. Heal from the clean copy — but only if the clean copy itself still verifies (else both are corrupt).
    self.dax.flips = self.dax.flips.saturating_add(1);
    let clean_ok = matches!(
      self.content.arena().bytes(guard.clean),
      Some(c) if *blake3::hash(c).as_bytes() == guard.identity
    );
    if !clean_ok {
      return DaxScrub::Unhealable;
    }
    let restore = self
      .content
      .arena()
      .bytes(guard.clean)
      .map(<[u8]>::to_vec)
      .unwrap_or_default();
    if let Some(dst) = self.content.arena_mut().bytes_mut(mapping.block) {
      let n = dst.len().min(restore.len());
      if let (Some(d), Some(s)) = (dst.get_mut(..n), restore.get(..n)) {
        d.copy_from_slice(s);
      }
    }
    // Re-verify after the heal: the victim must be erased, or it is unhealable and the caller revokes.
    let healed = matches!(
      self.content.arena().bytes(mapping.block),
      Some(after) if *blake3::hash(after).as_bytes() == guard.identity
    );
    if healed {
      self.dax.healed = self.dax.healed.saturating_add(1);
      DaxScrub::Healed
    } else {
      DaxScrub::Unhealable
    }
  }

  /// Drops a mapping's integrity guard (§4.6 B2) if it has one, freeing the clean copy; returns whether one was held.
  /// The explicit teardown a caller runs before it revokes a guarded mapping; [`Store::dax_revoke`] also calls it.
  pub fn dax_unguard(&mut self, mapping: &Mapping) -> bool {
    self.drop_guard(mapping)
  }

  /// Removes `mapping`'s guard and releases its clean copy (unpin then free). Internal to the B2 paths.
  fn drop_guard(&mut self, mapping: &Mapping) -> bool {
    let Some(index) = self.dax.guards.iter().position(|g| &g.mapping == mapping) else {
      return false;
    };
    let guard = self.dax.guards.swap_remove(index);
    if self.content.arena_mut().unpin(guard.clean).is_err() {
      self.release_refusals = self.release_refusals.saturating_add(1);
    }
    if self.content.arena_mut().free(guard.clean).is_err() {
      self.release_refusals = self.release_refusals.saturating_add(1);
    }
    true
  }

  /// Phase one of revoking a mapping a **live DAX guest** holds (§4.6 teardown; the asynchronous-revocation
  /// ordering): remove it from the registry — so the hardware path goes inert and no new access is authorized —
  /// but **keep the block pinned**. The guest's page table may still map the block until the VMM has unmapped the
  /// window and flushed every physical core's TLB; the block must not be reused in that gap, or a neighbour that
  /// took it could be read or written through the guest's stale TLB entry (the one cross-tenant dirty-state hazard
  /// left once spatial isolation holds). The pin is released only by [`Store::dax_release_after_unmap`], which the
  /// device calls after the VMM acknowledges the unmap. Synchronous callers with no live guest keep using
  /// [`Store::dax_revoke`] (unregister and unpin at once); this pair is for the device path. Returns whether held.
  pub fn dax_revoke_pending(&mut self, mapping: &Mapping) -> bool {
    if !self.dax.remove(mapping) {
      return false;
    }
    // The mapping is leaving the registry (inert to the hardware path), so its integrity guard (§4.6 B2) — a pinned
    // clean copy of a read-only block, kept to heal a Rowhammer flip — has no live mapping left to protect and must
    // not stay pinned. Drop it now, in phase one: the guard's clean copy is a *separate* block, never the guest's
    // window, so freeing it has no stale-TLB bearing — only `mapping.block` must stay pinned until
    // [`Store::dax_release_after_unmap`]. The synchronous [`Store::dax_revoke`] already drops the guard; without
    // this, a read-only guarded mapping revoked through the two-phase path (a write or truncate of its inode, which
    // revokes every mapping including the read-only ones, or a device window teardown) left its clean copy pinned
    // until the volume was dropped — a guard-sized leak per such revoke.
    self.drop_guard(mapping);
    true
  }

  /// Phase two: unpin the block of a mapping revoked by [`Store::dax_revoke_pending`], once the VMM has confirmed
  /// the window unmap and the cross-core TLB flush, so the block may now be freed and reused. The daemon must never
  /// call this before that acknowledgement — holding the pin across the unmap is exactly what closes the stale-TLB
  /// cross-tenant read. `true` when the hold was dropped (the block is no longer pinned here, and a deferred free,
  /// if one was waiting, is released and scrubbed); `false` only when the arena refused the unpin
  /// ([`MemError::ForeignExtent`] — a double release or a block this held no pin on), counted in
  /// `release_refusals`, never a silent double-free.
  pub fn dax_release_after_unmap(&mut self, mapping: &Mapping) -> bool {
    match self.content.arena_mut().unpin(mapping.block) {
      Ok(_) => {
        // A metadata snapshot owns its throwaway block; free it now the pin is gone (phase two), or it would leak.
        self.free_snapshot_block(mapping);
        true
      }
      Err(_) => {
        self.release_refusals = self.release_refusals.saturating_add(1);
        false
      }
    }
  }

  /// Frees the throwaway content block a metadata-snapshot mapping ([`Volume::dax_map_dir`]) owns, once its pin is
  /// gone. A no-op for a file's content block (the inode owns it; only the inode's own lifecycle frees it). A free
  /// the arena refuses is counted, never a silent leak or double-free.
  fn free_snapshot_block(&mut self, mapping: &Mapping) {
    if mapping.snapshot && self.content.arena_mut().free(mapping.block).is_err() {
      self.release_refusals = self.release_refusals.saturating_add(1);
    }
  }

  /// The backing descriptor, byte offset and length a **live** mapping's block occupies in the content arena's shared
  /// object (§4.6), for the virtio-fs device to map its pages into a VMM's DAX cache region. `Some((fd, offset, len))`
  /// only when the mapping is live *and* its block lives in a shared-object (anchor-backed) region; `None` for a
  /// revoked mapping (nothing to map) or a private (scratch, anonymous) volume — so DAX is served only on anchor-backed
  /// volumes, the ones whose content survives a restart and has a descriptor to hand a VMM. Unix only (the DAX window
  /// is Linux/vhost-user). The mapping's pin keeps the block alive, so the fd's bytes are stable while the device maps
  /// them; the caller maps `[offset, offset + len)` read-only or writable per `mapping.writable`.
  #[cfg(unix)]
  #[must_use]
  pub fn dax_backing(
    &self,
    mapping: &Mapping,
  ) -> Option<(std::os::fd::BorrowedFd<'_>, usize, usize)> {
    if !self.dax.is_live(mapping) {
      return None;
    }
    self.content.arena().block_backing(mapping.block)
  }

  /// Revokes every mapping of inode `no` (§4.6): a VFS mutation of a DAX-mapped file tears its mappings down, so the
  /// one write boundary stays single (the guest re-maps after). Returns how many were revoked.
  pub fn dax_revoke_inode(&mut self, no: InodeNo) -> usize {
    let victims: Vec<Mapping> = self
      .dax
      .mappings
      .iter()
      .filter(|mapping| mapping.inode == no)
      .copied()
      .collect();
    // §4.6 step 5 (two-phase): leave each block **pinned** and enqueue the inode for the device to munmap the
    // guest's window before unpinning, so a block the daemon is about to move/reuse is never reachable through a
    // stale TLB. The mapping leaves the registry now (inert to the hardware-path model); the device serve loop drains
    // `take_pending_revokes` and calls `Device::request_revoke_inode` (munmap, then release-after-unmap = unpin). If
    // no device holds the window (a daemon-only volume, a VFS test), the mapping is still revoked (inert) and the
    // block is reclaimed on the device's terminal teardown (`release_dax`) or when the volume is dropped. (DAX is
    // unadvertised, so no live mapping exists in production yet — this changes only the mapped-inode path.)
    let count = victims
      .iter()
      .filter(|mapping| self.dax_revoke_pending(mapping))
      .count();
    if count > 0 {
      // A write/truncate makes **every** window of the inode stale, so the drain tears them all (`writable_only`
      // false).
      self.dax.enqueue_pending_revoke(no, false);
    }
    count
  }

  /// Revokes every **writable** mapping of the volume with `prefix` (§4.6): a snapshot or clone shares the volume's
  /// current blocks, so a writable mapping of one must be torn down before the share, or the guest's later write
  /// would reach the frozen version. Read-only mappings survive — immutable shared content is the gate's allowed DAX
  /// case. Two-phase (§4.6 step 5), so the device actually munmaps the guest window: each writable mapping leaves the
  /// registry **revoke-pending** (its block kept pinned) and is enqueued `writable_only` for the device to munmap,
  /// which a retrying `snapshot` awaits ([`DaxRegistry::has_pending_of_prefix`]) before it freezes. The inode-keyed
  /// enqueue carries `writable_only=true` so the drain tears only the writable windows and keeps the read-only ones.
  /// Returns how many were revoked-pending. Empty on any volume no guest maps writable (the common case).
  /// This revoke does **not** begin the recall ([`DaxRegistry::begin_recall`]), deliberately. The recall is an
  /// exclusion that lasts across retries, so it belongs to whoever runs that retry loop and can end it — the
  /// `snapshot` verb. Begun here it would also be begun by `Volume::snapshot`, which has no "the snapshot is over"
  /// moment to end it at, and the volume would never take a writable window again (two tests caught exactly that).
  pub fn dax_revoke_writable_of_prefix(&mut self, prefix: u16) -> usize {
    let victims: Vec<Mapping> = self
      .dax
      .mappings
      .iter()
      .filter(|mapping| mapping.writable && mapping.inode.prefix() == prefix)
      .copied()
      .collect();
    let mut count = 0usize;
    for mapping in &victims {
      if self.dax_revoke_pending(mapping) {
        self.dax.enqueue_pending_revoke(mapping.inode, true);
        count = count.saturating_add(1);
      }
    }
    count
  }

  /// Revokes every mapping of the volume with `prefix` (§4.6: "eventual reclamation", AC-4.12): the teardown a
  /// `destroy` runs so the volume's mapped blocks are not held pinned (their frees deferred) after it is gone. Unlike
  /// [`Store::dax_revoke_writable_of_prefix`] this takes read-only mappings too — the whole volume is going away.
  /// Returns how many were revoked.
  pub fn dax_revoke_volume(&mut self, prefix: u16) -> usize {
    // The volume is going away, so any recall of it goes with it: nothing is left to block a later volume that
    // happens to reuse the prefix.
    self.dax.end_recall(prefix);
    // And so does its L1 sandbox (§4.6): a huge page held for a tenant that no longer exists is a page the rest
    // of the shard cannot use, and a later volume that reuses the prefix must get a sandbox of its own rather
    // than inherit rows this one wrote.
    self
      .dax_zones
      .release_volume(prefix, self.content.arena_mut());
    let victims: Vec<Mapping> = self
      .dax
      .mappings
      .iter()
      .filter(|mapping| mapping.inode.prefix() == prefix)
      .copied()
      .collect();
    victims
      .iter()
      .filter(|mapping| self.dax_revoke(mapping))
      .count()
  }
}
