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

/// One active mapping of a file's content block into a guest's DAX window. Minted by the daemon
/// ([`Volume::dax_map_read`]/[`Volume::dax_map_write`]); never constructed from caller input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mapping {
  /// The inode mapped (prefix-encoded, so its volume is `inode.prefix()`).
  pub inode: InodeNo,
  /// The arena block holding the content — what the guest's page table points at. Pinned while this mapping lives.
  pub block: Block,
  /// The file's authorized length: the bytes from here to the end of the block's pages read as zero.
  pub file_len: usize,
  /// Whether the guest may write these pages.
  pub writable: bool,
  /// Whether this mapping **owns** its block — a materialized metadata snapshot (§4.6 metadata DAX: a directory's
  /// entries serialized into a throwaway content block, [`Volume::dax_map_dir`]), freed when the mapping is revoked.
  /// `false` for a file's content block, which the inode owns and a revoke only unpins (never frees) — the common case.
  pub snapshot: bool,
}

impl Mapping {
  /// The window bytes this mapping occupies — whole pages (`file_len` rounded up to [`DAX_PAGE`]). A guest's page
  /// table covers exactly these pages and no more.
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
    }
  }

  /// Counts a completed migration (§4.6 L2): the moving target's non-vacuity counter.
  fn count_migration(&mut self) {
    self.migrated = self.migrated.saturating_add(1);
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

/// A file's content block and its authorized length (`attrs.size`). The gate maps single-block unsealed files
/// (`Body::Open`), the shape a freshly written file takes.
fn file_block(store: &Store, volume: &Volume, no: InodeNo) -> Result<(Block, usize), VfsError> {
  let size = usize::try_from(volume.stat(store, no)?.size).map_err(|_| VfsError::NoSpace)?;
  let inode = volume.inode(store, no)?;
  match &inode.body {
    Body::Open { open, sealed } if sealed.is_empty() => Ok((open.block, size)),
    _ => Err(VfsError::Invalid),
  }
}

impl Volume {
  /// Maps inode `no`'s content block into a guest window, **read-only** (§4.6). The block is **pinned** (never reused
  /// or freed while mapped) and every byte of the block past the file's length is scrubbed to zero, so the guest's
  /// whole-page view never reaches a truncate's leftover tail or a prior allocation's bytes. Refused for a directory,
  /// a sealed or multi-block file, or when the registry is at its bound.
  pub fn dax_map_read(&mut self, store: &mut Store, no: InodeNo) -> Result<Mapping, VfsError> {
    // Defence in depth for the device seam: a mapping is of a namespace object only, never an attribute
    // (xattr-value) inode — so a crafted or confused map of an attribute number is `NotFound`, never a window onto
    // an attribute's bytes, mirroring the transports' `namespace_inode` gate.
    self.namespace_inode(&*store, no)?;
    let (block, file_len) = file_block(&*store, self, no)?;
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
      inode: no,
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
    Ok(mapping)
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
        let kind = match row.kind {
          crate::inode::Kind::File => 1u8,
          crate::inode::Kind::Dir => 2,
          crate::inode::Kind::Symlink => 3,
          crate::inode::Kind::Fifo => 4,
          crate::inode::Kind::Socket => 5,
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
    let file_len = serialized.len();
    let block = store.content.alloc_block(file_len.max(1), false)?;
    if let Some(bytes) = store.content.arena_mut().bytes_mut(block) {
      if let Some(head) = bytes.get_mut(..file_len) {
        head.copy_from_slice(&serialized);
      }
      if let Some(tail) = bytes.get_mut(file_len..) {
        tail.fill(0);
      }
    }
    if let Err(pin) = store.content.arena_mut().pin(block) {
      // Allocated but unpinnable: free the throwaway so a refused map leaves nothing held.
      let _ = store.content.arena_mut().free(block);
      return Err(VfsError::Memory(pin));
    }
    let mapping = Mapping {
      inode: no,
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
    // Defence in depth: a writable mapping, like a read mapping, is of a namespace object only — never an
    // attribute inode (`Volume::dax_map_read`).
    self.namespace_inode(&*store, no)?;
    let file_len = usize::try_from(self.stat(&*store, no)?.size).map_err(|_| VfsError::NoSpace)?;
    let mut current = vec![0u8; file_len];
    self.read(&*store, no, 0, &mut current)?;
    // Force the copy-up through the write boundary. No mapping is registered yet, so this write revokes nothing.
    self.write(store, no, 0, &current)?;
    let (block, file_len) = file_block(&*store, self, no)?;
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
      inode: no,
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
  /// The device remaps the guest window old→new (an atomic `MAP_FIXED`/`mremap` over the same guest address), so the
  /// guest re-faults transparently onto the moved bytes. Refused `Invalid` for a read-only mapping (immutable shared
  /// content a guest cannot hammer into a neighbour, and moving it would break the share), a revoked mapping, or one
  /// whose block already moved; `NoSpace`/`PublishNeeded` if no fresh block can back the shift — the caller keeps the
  /// old mapping and retries, never a lost window. The pseudo-random *placement* of the fresh block within a bounded
  /// physically-contiguous sandbox ([`stripe_target`]) is the L1 step bound to the hugetlb sandbox; here the arena
  /// provides the fresh block and the handoff mechanics are complete and exact.
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
    // Relocate the content to a fresh block; the old block, pinned by `old`, is freed *deferred* (its bytes kept) —
    // the stale-TLB hold. On a refusal nothing moved: restore the body and surface it.
    if let Err(refusal) = store.content.relocate(open.as_mut()) {
      store.inodes.get_mut(handle)?.body = Body::Open { open, sealed };
      return Err(refusal);
    }
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
      inode: old.inode,
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
    store.dax.count_migration();
    Ok(mapping)
  }

  /// Whether `mapping` still names its file's current content block and length. The content paths revoke a mapping
  /// before the block moves, so a live mapping always passes; this is a consistency backstop, not the primary guard.
  fn dax_still_current(&self, store: &Store, mapping: &Mapping) -> bool {
    matches!(
      file_block(store, self, mapping.inode),
      Ok((block, file_len)) if block == mapping.block && file_len == mapping.file_len
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
    if !mapping.writable
      || !store.dax.is_live(mapping)
      || !self.dax_still_current(&*store, mapping)
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
    let block = self.content.arena().bytes(mapping.block).unwrap_or_default();
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
  /// cannot see directly (a hardware write/flip on a mapped page). Returns [`DaxScrub`]. A background scanner calls
  /// this over its guards a bounded number per step; an explicit validation pass calls it on demand.
  pub fn dax_scrub(&mut self, mapping: &Mapping) -> DaxScrub {
    let Some(guard) = self.dax.guards.iter().find(|g| &g.mapping == mapping).copied() else {
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
    self.dax.remove(mapping)
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
    victims
      .iter()
      .filter(|mapping| self.dax_revoke(mapping))
      .count()
  }

  /// Revokes every **writable** mapping of the volume with `prefix` (§4.6): a snapshot or clone shares the volume's
  /// current blocks, so a writable mapping of one must be torn down before the share, or the guest's later write
  /// would reach the frozen version. Read-only mappings survive — immutable shared content is the gate's allowed DAX
  /// case. Returns how many were revoked.
  pub fn dax_revoke_writable_of_prefix(&mut self, prefix: u16) -> usize {
    let victims: Vec<Mapping> = self
      .dax
      .mappings
      .iter()
      .filter(|mapping| mapping.writable && mapping.inode.prefix() == prefix)
      .copied()
      .collect();
    victims
      .iter()
      .filter(|mapping| self.dax_revoke(mapping))
      .count()
  }

  /// Revokes every mapping of the volume with `prefix` (§4.6: "eventual reclamation", AC-4.12): the teardown a
  /// `destroy` runs so the volume's mapped blocks are not held pinned (their frees deferred) after it is gone. Unlike
  /// [`Store::dax_revoke_writable_of_prefix`] this takes read-only mappings too — the whole volume is going away.
  /// Returns how many were revoked.
  pub fn dax_revoke_volume(&mut self, prefix: u16) -> usize {
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

  /// Revokes every mapping of a specific block (§4.6): the teardown a landing or takeover runs before it reclaims or
  /// moves content that block holds. Returns how many were revoked.
  pub fn dax_revoke_block(&mut self, block: Block) -> usize {
    let victims: Vec<Mapping> = self
      .dax
      .mappings
      .iter()
      .filter(|mapping| mapping.block == block)
      .copied()
      .collect();
    victims
      .iter()
      .filter(|mapping| self.dax_revoke(mapping))
      .count()
  }
}
