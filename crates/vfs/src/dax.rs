//! Experiment (§4.6 A-9, AC-4.12): a **writable DAX** data path, built on the *real* content arena so it can be
//! attacked for real. A guest reaches only the pages the daemon maps into the shared window; the window is the
//! content's own `mlock`'d arena blocks, so a mapping that exposes or lets a guest write the wrong bytes is a true
//! cross-authorization escape — never a disk escape (the arena never reaches disk, R1).
//!
//! The guest's page table exposes a **whole page**, so `guest_read`/`guest_write` here return the raw arena block
//! bytes, not a bounded copy — exactly what the hardware would. The daemon's job is that every byte of a mapped
//! page is one the guest may see: the file's bytes, then zeros — never a block's stale tail past the file (a
//! truncate can leave a block whose written extent `dirty_to` runs past the new length), never a neighbour's
//! block, never a chunk shared with a snapshot it could corrupt. Each is a test in `tests/dax.rs`.

use slates_mem::arena::Extent as Block;

use crate::error::VfsError;
use crate::ids::InodeNo;
use crate::inode::Body;
use crate::volume::{Store, Volume};

/// Derived: the DAX mapping granule — the host page, the unit a guest page table can point at.
pub const DAX_PAGE: usize = 4096;

/// One active mapping of a file's content block into the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mapping {
  /// The inode mapped.
  pub inode: InodeNo,
  /// The arena block holding the content — what the guest's page table points at.
  pub block: Block,
  /// The file's authorized length: the bytes from here to the end of the block's pages must read as zero.
  pub file_len: usize,
  /// Whether the guest may write these pages.
  pub writable: bool,
}

/// A file's content block and its authorized length (`attrs.size`). The experiment maps single-block unsealed
/// files (`Body::Open`), the shape a freshly written file takes.
fn file_block(store: &Store, volume: &Volume, no: InodeNo) -> Result<(Block, usize), VfsError> {
  let size = usize::try_from(volume.stat(store, no)?.size).map_err(|_| VfsError::NoSpace)?;
  let inode = volume.inode(store, no)?;
  match &inode.body {
    Body::Open { open, sealed } if sealed.is_empty() => Ok((open.block, size)),
    _ => Err(VfsError::Invalid),
  }
}

/// The mappings the daemon has set up into the shared window. The window is the arena itself (the blocks the
/// mappings name), so reads and writes here go through the real content memory a guest would touch.
#[derive(Default)]
pub struct DaxMapper {
  mappings: Vec<Mapping>,
}

impl DaxMapper {
  /// A mapper with no mappings.
  #[must_use]
  pub fn new() -> Self {
    DaxMapper {
      mappings: Vec::new(),
    }
  }

  /// Maps inode `no`'s content block into the window, read-only, **scrubbing every byte of the block past the
  /// file's length to zero** so the guest's whole-page view never reaches a truncate's leftover tail or a prior
  /// allocation's bytes. The scrub writes the real arena block — safe only when the block is this version's
  /// alone; a block shared with a snapshot would be corrupted (the `copies_up` guard, next vector).
  pub fn setup_read_mapping(
    &mut self,
    store: &mut Store,
    volume: &Volume,
    no: InodeNo,
  ) -> Result<Mapping, VfsError> {
    let (block, file_len) = file_block(&*store, volume, no)?;
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
    self.mappings.push(mapping);
    Ok(mapping)
  }

  /// Whether `mapping` still names its file's current content block. The arena bumps the generational incarnation
  /// on free and reuse, so a block freed under a mapping and handed to another file no longer equals the mapping's
  /// extent. Enforcing that here makes a stale mapping a **typed miss** — the pinning/teardown that raw
  /// `arena.bytes` (which slices by region+offset and ignores the incarnation) does not do on its own, and the
  /// reason a mapped file's content must not move or free under a live mapping.
  fn still_current(&self, store: &Store, volume: &Volume, mapping: &Mapping) -> bool {
    matches!(
      file_block(store, volume, mapping.inode),
      Ok((block, file_len)) if block == mapping.block && file_len == mapping.file_len
    )
  }

  /// What a guest reads from a mapped page: the **raw** bytes of the block, `[off, off + len)`, exactly as its
  /// page table exposes them. Past the block's end reads as zero (the page's remainder). The authorized file is
  /// only `[0, file_len)`; anything the guest reads past that which is **not** zero is a leak. Refused if the
  /// mapping is stale (its block freed or reused — [`Self::still_current`]).
  pub fn guest_read(
    &self,
    store: &Store,
    volume: &Volume,
    mapping: &Mapping,
    off: usize,
    len: usize,
  ) -> Result<Vec<u8>, VfsError> {
    if !self.mappings.contains(mapping) || !self.still_current(store, volume, mapping) {
      return Err(VfsError::Invalid);
    }
    let block = store
      .content
      .arena()
      .bytes(mapping.block)
      .unwrap_or_default();
    let mut out = vec![0u8; len];
    for (i, slot) in out.iter_mut().enumerate() {
      if let Some(&b) = block.get(off.saturating_add(i)) {
        *slot = b;
      }
    }
    Ok(out)
  }

  /// Maps inode `no`'s content block **writable**, **copying it up first** so the block the guest writes is this
  /// version's alone — a snapshot or clone that shared it keeps its own, untouched by the guest's writes. The
  /// copy-up is forced by reading the file's own bytes and writing them back through the VFS boundary (where
  /// copy-on-write lives); the now-private block's tail past the file is then scrubbed and the block mapped.
  pub fn setup_write_mapping(
    &mut self,
    store: &mut Store,
    volume: &mut Volume,
    no: InodeNo,
  ) -> Result<Mapping, VfsError> {
    let file_len =
      usize::try_from(volume.stat(&*store, no)?.size).map_err(|_| VfsError::NoSpace)?;
    let mut current = vec![0u8; file_len];
    volume.read(&*store, no, 0, &mut current)?;
    volume.write(store, no, 0, &current)?;
    // Re-fetch: `write` copied the shared block up, so this is a block only this version names.
    let (block, file_len) = file_block(&*store, volume, no)?;
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
    self.mappings.push(mapping);
    Ok(mapping)
  }

  /// A guest writing `bytes` at `off` into a writable mapping — straight into the arena block, as a DAX guest
  /// would, with no `Volume::write` and so no copy-up. Refused if the mapping is read-only, past the authorized
  /// length, or stale (its block freed or reused — [`Self::still_current`]).
  pub fn guest_write(
    &self,
    store: &mut Store,
    volume: &Volume,
    mapping: &Mapping,
    off: usize,
    bytes: &[u8],
  ) -> Result<(), VfsError> {
    if !mapping.writable
      || !self.mappings.contains(mapping)
      || !self.still_current(&*store, volume, mapping)
    {
      return Err(VfsError::Invalid);
    }
    let end = off.checked_add(bytes.len()).ok_or(VfsError::NoSpace)?;
    // Only the file's authorized bytes may be written. Growing it needs a quota and lease check the guest cannot
    // make itself, so a write into the page's scrubbed tail — past `file_len` — is refused, never kept.
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
