//! Content (D-6): file bytes live in open, page-multiple extents until they are sealed into
//! chunks; a write into a sealed chunk copies that chunk into a new open extent (copy-on-write
//! at chunk granularity); holes read as zeros; hashing waits for the seal and dedup for Phase 7.
//!
//! Chunk bytes sit in the shard's buddy arena (`slates-mem`), addressed by extent, never by raw
//! pointer. Each chunk is referenced by exactly one extent of one inode version, so a chunk is
//! released by the birth-epoch rule alone: born after the last snapshot, free now; born before
//! it, onto the snapshot's deadlist.

use slates_machine::{Derived, derived};
use slates_mem::arena::{ChunkArena, Extent as Block};
use slates_mem::{Handle, Slab};

use crate::error::VfsError;
use crate::ids::Epoch;
use crate::snapshot::{Dead, Deadlist};

/// A sealed chunk.
#[derive(Clone, Copy, Debug)]
pub struct Chunk {
  /// The birth epoch.
  pub born: Epoch,
  /// Bytes used.
  pub len: u32,
  /// The arena block holding the bytes (its length is the capacity).
  pub block: Block,
  /// BLAKE3 of the bytes, computed at seal by Phase 7's pass; `None` until then.
  pub identity: Option<[u8; 32]>,
}

/// A file extent: `len` bytes at file offset `off`, from a chunk or a hole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
  /// File offset.
  pub off: u64,
  /// Length.
  pub len: u64,
  /// The source.
  pub src: ExtentSrc,
}

/// Where an extent's bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtentSrc {
  /// A sealed chunk, from byte `at` within it.
  Chunk {
    /// The chunk.
    chunk: Handle<Chunk>,
    /// The offset within the chunk.
    at: u32,
  },
  /// Zeros.
  Zero,
}

/// The mutable extent of an open file: one arena block written in place until sealed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenExtent {
  /// File offset of the block's first byte.
  pub off: u64,
  /// Bytes used from the block.
  pub len: u64,
  /// The block (capacity = `block.len`).
  pub block: Block,
  /// The birth epoch of the block.
  pub born: Epoch,
}

/// The shard's chunk store: the arena and the chunk slab.
pub struct ChunkStore {
  arena: ChunkArena,
  chunks: Slab<Chunk>,
  page: usize,
  chunk_bytes: usize,
}

impl std::fmt::Debug for ChunkStore {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ChunkStore")
      .field("chunks", &self.chunks.len())
      .field("page", &self.page)
      .field("chunk_bytes", &self.chunk_bytes)
      .finish()
  }
}

/// The chunk size: the largest open extent, and so the unit of copy-on-write. Sixteen base pages
/// until the p90 sealed size is measured on real workloads (Phase 1 baselines record it; the
/// formula in §4.5 is "smallest page multiple ≥ p90 sealed size").
pub fn chunk_bytes(page: usize) -> Derived<usize> {
  derived!(
    page.max(1).saturating_mul(16),
    "16 × base page until the p90 sealed size is measured (§4.5 derived constants)",
    ["page.base"]
  )
}

/// The inline threshold: content up to two cache lines stays in the inode.
pub fn inline_bytes(cache_line: usize) -> Derived<usize> {
  derived!(
    cache_line.max(1).saturating_mul(2),
    "2 × cache line: the inode and its bytes share the lines the lookup touched",
    ["cache_line"]
  )
}

impl ChunkStore {
  /// A store over `arena` with `page`-byte granules and `max_chunks` chunk records.
  pub fn new(arena: ChunkArena, page: usize, max_chunks: usize) -> Self {
    let page = page.max(1);
    Self {
      arena,
      chunks: Slab::new(max_chunks.min(page), max_chunks),
      page,
      chunk_bytes: chunk_bytes(page).get(),
    }
  }

  /// The page size.
  pub const fn page(&self) -> usize {
    self.page
  }

  /// The chunk size.
  pub const fn chunk_bytes(&self) -> usize {
    self.chunk_bytes
  }

  /// Chunks live.
  pub fn chunks(&self) -> usize {
    self.chunks.len()
  }

  /// Bytes allocated in the arena.
  pub fn allocated_bytes(&self) -> usize {
    self.arena.allocated_bytes()
  }

  /// The arena, for adding regions.
  pub fn arena_mut(&mut self) -> &mut ChunkArena {
    &mut self.arena
  }

  /// Opens a new extent at `off` with room for at least `want` bytes (page-multiple, at most a
  /// chunk).
  pub fn open(&mut self, off: u64, want: usize, born: Epoch) -> Result<OpenExtent, VfsError> {
    let want = want.clamp(1, self.chunk_bytes);
    let block = self.arena.alloc(want.next_multiple_of(self.page))?;
    Ok(OpenExtent {
      off,
      len: 0,
      block,
      born,
    })
  }

  /// Grows an open extent's block to hold `need` bytes, copying what was written; refuses past a
  /// chunk.
  pub fn grow(&mut self, open: &mut OpenExtent, need: usize) -> Result<(), VfsError> {
    if need <= open.block.len {
      return Ok(());
    }
    if need > self.chunk_bytes {
      return Err(VfsError::FileTooLarge);
    }
    let block = self.arena.alloc(need.next_multiple_of(self.page))?;
    let used = usize::try_from(open.len).unwrap_or(0);
    let mut carry = vec![0u8; used];
    if let Some(src) = self.arena.bytes(open.block) {
      carry.copy_from_slice(&src[..used]);
    }
    if let Some(dst) = self.arena.bytes_mut(block) {
      dst[..used].copy_from_slice(&carry);
    }
    self.arena.free(open.block)?;
    open.block = block;
    Ok(())
  }

  /// Writes into an open extent at `at` (relative to the extent), zero-filling any gap, growing
  /// the block as needed.
  pub fn write_open(
    &mut self,
    open: &mut OpenExtent,
    at: usize,
    bytes: &[u8],
  ) -> Result<(), VfsError> {
    let end = at.checked_add(bytes.len()).ok_or(VfsError::FileTooLarge)?;
    self.grow(open, end)?;
    let used = usize::try_from(open.len).unwrap_or(0);
    let dst = self
      .arena
      .bytes_mut(open.block)
      .ok_or(VfsError::StaleHandle)?;
    if at > used {
      dst[used..at].fill(0);
    }
    dst[at..end].copy_from_slice(bytes);
    if end > used {
      open.len = u64::try_from(end).unwrap_or(u64::MAX);
    }
    Ok(())
  }

  /// The bytes of an open extent.
  pub fn open_bytes(&self, open: &OpenExtent) -> &[u8] {
    let used = usize::try_from(open.len).unwrap_or(0);
    self
      .arena
      .bytes(open.block)
      .map_or(&[], |b| &b[..used.min(b.len())])
  }

  /// Truncates an open extent to `len` bytes (zeroing nothing: the length caps reads).
  pub fn truncate_open(open: &mut OpenExtent, len: u64) {
    open.len = open.len.min(len);
  }

  /// Seals an open extent into a chunk and returns the extent that names it (or `None` for an
  /// empty extent, whose block is released).
  pub fn seal(&mut self, open: OpenExtent) -> Result<Option<Extent>, VfsError> {
    if open.len == 0 {
      self.arena.free(open.block)?;
      return Ok(None);
    }
    let chunk = self.chunks.insert(Chunk {
      born: open.born,
      len: u32::try_from(open.len).unwrap_or(u32::MAX),
      block: open.block,
      identity: None,
    })?;
    Ok(Some(Extent {
      off: open.off,
      len: open.len,
      src: ExtentSrc::Chunk { chunk, at: 0 },
    }))
  }

  /// The bytes of a sealed extent.
  pub fn extent_bytes(&self, extent: &Extent) -> Option<&[u8]> {
    match extent.src {
      ExtentSrc::Zero => None,
      ExtentSrc::Chunk { chunk, at } => {
        let c = self.chunks.get(chunk).ok()?;
        let bytes = self.arena.bytes(c.block)?;
        let start = usize::try_from(at).ok()?;
        let end = start.checked_add(usize::try_from(extent.len).ok()?)?;
        bytes.get(start..end.min(usize::try_from(c.len).ok()?))
      }
    }
  }

  /// Copies a sealed extent's bytes into a new open extent (copy-on-write), zero-filling
  /// beyond the extent's length up to the chunk's capacity is not needed: the open extent's
  /// length is the extent's.
  pub fn reopen(&mut self, extent: &Extent, born: Epoch) -> Result<OpenExtent, VfsError> {
    let len = usize::try_from(extent.len).unwrap_or(usize::MAX);
    let mut open = self.open(extent.off, len, born)?;
    let bytes = self
      .extent_bytes(extent)
      .map(<[u8]>::to_vec)
      .unwrap_or_else(|| vec![0u8; len]);
    self.write_open(&mut open, 0, &bytes)?;
    Ok(open)
  }

  /// The chunk behind an extent.
  pub fn chunk(&self, handle: Handle<Chunk>) -> Option<&Chunk> {
    self.chunks.get(handle).ok()
  }

  /// Releases a chunk by the epoch rule: freed now when born after `last_snapshot`, else sent
  /// to the deadlist. Returns the bytes released from the head's accounting (its length).
  pub fn release_chunk(
    &mut self,
    handle: Handle<Chunk>,
    last_snapshot: Option<Epoch>,
    dead: &mut Deadlist,
  ) -> Result<u64, VfsError> {
    let chunk = *self.chunks.get(handle)?;
    match last_snapshot {
      Some(snap) if chunk.born <= snap => dead.push(Dead::Chunk(handle, chunk.born)),
      _ => self.free_chunk(handle)?,
    }
    Ok(u64::from(chunk.len))
  }

  /// Frees a chunk and its block unconditionally (the deadlist walker's terminal step).
  pub fn free_chunk(&mut self, handle: Handle<Chunk>) -> Result<(), VfsError> {
    let chunk = self.chunks.remove(handle)?;
    self.arena.free(chunk.block)?;
    Ok(())
  }

  /// Releases an open extent's block (the file was truncated or unlinked before sealing).
  pub fn release_open(&mut self, open: OpenExtent) -> Result<(), VfsError> {
    self.arena.free(open.block)?;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_mem::region::Region;

  fn store() -> ChunkStore {
    let page = 4096;
    let mut arena = ChunkArena::new(page);
    arena
      .add_region(Region::map(page * 256, page, false).unwrap())
      .unwrap();
    ChunkStore::new(arena, page, 1024)
  }

  #[test]
  fn an_open_extent_grows_by_pages_and_zero_fills_gaps() {
    let mut s = store();
    let mut open = s.open(0, 10, Epoch(0)).unwrap();
    assert_eq!(open.block.len, 4096);
    s.write_open(&mut open, 0, b"hello").unwrap();
    s.write_open(&mut open, 5000, b"far").unwrap();
    assert_eq!(open.block.len, 8192, "grew by a page multiple");
    assert_eq!(open.len, 5003);
    assert_eq!(&s.open_bytes(&open)[..5], b"hello");
    assert_eq!(
      &s.open_bytes(&open)[5..8],
      &[0, 0, 0],
      "the gap is zero-filled"
    );
  }

  #[test]
  fn sealing_makes_a_chunk_that_reads_back_and_releases_by_the_epoch_rule() {
    let mut s = store();
    let mut open = s.open(0, 10, Epoch(0)).unwrap();
    s.write_open(&mut open, 5000, b"far").unwrap();
    let extent = s.seal(open).unwrap().unwrap();
    assert_eq!(extent.len, 5003);
    assert_eq!(&s.extent_bytes(&extent).unwrap()[5000..5003], b"far");
    let mut dead = Deadlist::default();
    let ExtentSrc::Chunk { chunk, .. } = extent.src else {
      panic!()
    };
    assert_eq!(
      s.release_chunk(chunk, Some(Epoch(0)), &mut dead).unwrap(),
      5003
    );
    assert_eq!(
      dead.len(),
      1,
      "born at the snapshot's epoch: onto the deadlist"
    );
    assert_eq!(s.chunks(), 1);
    s.free_chunk(chunk).unwrap();
    assert_eq!(s.chunks(), 0);
    assert_eq!(s.allocated_bytes(), 0);
  }

  #[test]
  fn writes_past_a_chunk_are_refused_and_reopen_copies_the_bytes() {
    let mut s = store();
    let mut open = s.open(0, 1, Epoch(0)).unwrap();
    assert!(matches!(
      s.write_open(&mut open, s.chunk_bytes(), b"x"),
      Err(VfsError::FileTooLarge)
    ));
    s.write_open(&mut open, 0, b"abc").unwrap();
    let extent = s.seal(open).unwrap().unwrap();
    let copy = s.reopen(&extent, Epoch(1)).unwrap();
    assert_eq!(s.open_bytes(&copy), b"abc");
    assert_eq!(copy.born, Epoch(1));
  }
}
