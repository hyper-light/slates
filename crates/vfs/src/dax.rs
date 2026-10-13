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

use slates_mem::arena::Extent as Block;

use crate::error::VfsError;
use crate::ids::InodeNo;
use crate::inode::Body;
use crate::volume::{Store, Volume};

/// Derived: the DAX mapping granule — the host page, the unit a guest page table can point at. One page (4 KiB) is
/// the smallest mapping a virtio-fs DAX window grants; a block spans a whole number of them.
pub const DAX_PAGE: usize = 4096;

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
    }
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

  /// Maps inode `no`'s content block **writable** (§4.6), **copying it up first** so the block the guest writes is
  /// this version's alone — a snapshot or clone that shared it keeps its own, untouched. The copy-up goes through the
  /// VFS write boundary (where copy-on-write, quota and journal live); the now-private block is **pinned**, its tail
  /// past the file scrubbed, and the mapping recorded. Refused as [`Volume::dax_map_read`] is.
  pub fn dax_map_write(&mut self, store: &mut Store, no: InodeNo) -> Result<Mapping, VfsError> {
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
    };
    if let Err(refusal) = store.dax.register(mapping) {
      if store.content.arena_mut().unpin(block).is_err() {
        store.release_refusals = store.release_refusals.saturating_add(1);
      }
      return Err(refusal);
    }
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
    true
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
