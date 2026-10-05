//! Content (D-6): file bytes live in open, page-multiple extents until they are sealed into
//! chunks; a write into a sealed chunk copies that chunk into a new open extent (copy-on-write
//! at chunk granularity); holes read as zeros; hashing waits for the seal and dedup for Phase 7.
//!
//! Chunk bytes sit in the shard's buddy arena (`slates-mem`), addressed by extent, never by raw
//! pointer. A chunk may be referenced by several versions of one inode — a copy-up clones the
//! body, so a retired version and its successor share every chunk until the head rewrites a
//! window — and is released exactly once, by the head, by the birth-epoch rule alone when the
//! head stops reaching it: born after the last snapshot, free now; born before it, onto the
//! snapshot's deadlist as its own `Dead::Chunk`. A retired version's release never frees its
//! chunks (`volume::release_dead`); a destroy or a recovery rebuild lists them itself.

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
  /// How the bytes are sealed in the arena (A-99), or `None` when they are in the clear.
  pub seal: Option<ChunkSeal>,
}

/// Format: a content key's identity, what a recovery image names the key by: the volume id's 16 bytes, then the
/// 16 bytes of the daemon life's salt it was registered under (A-99).
pub const KEY_IDENTITY_BYTES: usize = 32;

/// A content key's identity (A-99): see [`KEY_IDENTITY_BYTES`].
pub type KeyIdentity = [u8; KEY_IDENTITY_BYTES];

/// Format: an AES-256-GCM tag's bytes (NIST SP 800-38D; hyper-seal's `TAG`), one per sealed segment.
pub const TAG_BYTES: usize = 16;

/// A sealed segment's tag.
pub type Tag = [u8; TAG_BYTES];

/// How a chunk is sealed (A-99): the key it was sealed under, by the cipher's reference, the version its segments
/// were sealed at, and its run of tags in the store's tag slabs, one per granule-sized segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkSeal {
  /// The key, as [`ChunkCipher`] names it.
  pub key: u32,
  /// The version every segment of the chunk was sealed at; never reused under `key` (A-99's nonce argument).
  pub version: u64,
  /// The run of tags.
  tags: TagRun,
  /// How many tags the chunk has (its segments); the run may be longer, a power of two.
  segments: u32,
}

/// The cipher a store seals its chunks with (A-99): one segment at a time, under a key the cipher names by a small
/// reference, so a chunk record stays a fixed size. The server implements it over hyper-seal's `VersionKey`; the store
/// holds no key. A key reference's identity (what a recovery image records) is the cipher's.
pub trait ChunkCipher: Send {
  /// Seals `segment` in place as segment `index` of version `version` under key `key`; its tag.
  fn seal(
    &self,
    key: u32,
    version: u64,
    index: u32,
    last: bool,
    segment: &mut [u8],
  ) -> Result<Tag, VfsError>;
  /// Opens `segment` in place, or `Integrity` when it or its tag changed or the key is not the one it was sealed under.
  fn open(
    &self,
    key: u32,
    version: u64,
    index: u32,
    last: bool,
    segment: &mut [u8],
    tag: &Tag,
  ) -> Result<(), VfsError>;
  /// The stable identity of key `key`, which a recovery image records in place of the reference.
  fn identity(&self, key: u32) -> Option<KeyIdentity>;
  /// The reference for a key a recovery image names by `identity`, registering it if this cipher can derive it.
  fn reference(&mut self, identity: &KeyIdentity) -> Result<u32, VfsError>;
  /// The key this cipher seals volume `volume`'s new chunks under for the rest of its life, registering it.
  fn key_for_volume(&mut self, volume: [u8; 16]) -> Result<u32, VfsError>;
}

/// A chunk's run of tags (A-99): its slot in the store's tag slab.
type TagRun = Handle<Box<[Tag]>>;

/// The segment tags of a store's sealed chunks (A-99): one run per sealed chunk, exactly its segments long, in a slab
/// that grows a page of slots at a time as chunks seal and is bounded by the chunk slab, since every run belongs to one
/// chunk record. A run's length is a runtime quantity: a chunk is sixteen host pages and a segment is the arena's
/// granule, which may be smaller than a page (4 KiB under macOS arm64's 16 KiB page: 64 segments a chunk).
///
/// Measured and replaced (2026-10-05): the first build was one buddy pool sized for `max_chunks` full runs, made whole
/// at store construction: 268 MB of tags and a 16.7 M-granule buddy per shard (5.6 M chunk records on this host), which
/// a fresh mapping zero-fills lazily but a recycled one zeroes by hand, 2–25 ms per shard in release. Under a test
/// process's parallel daemon starts that took a debug shard's start to 0.7–1.1 s, past the 1 s the control loop waits
/// for each shard (GAPS, boot); shard starts went from p99 811 ms to 41 ms with the slab. Fixed-length run classes (1,
/// 2, 4, 8 and 16 tags) were tried next and refused every full chunk on a 4 KiB granule under a 16 KiB page.
struct TagStore {
  runs: Slab<Box<[Tag]>>,
}

impl TagStore {
  /// A tag slab for `max_chunks` chunk records, growing a base page of slots at a time.
  fn new(max_chunks: usize, page: usize) -> TagStore {
    let per_page = (page / std::mem::size_of::<Box<[Tag]>>().max(1)).max(1);
    TagStore {
      runs: Slab::new(per_page, max_chunks),
    }
  }

  /// A zeroed run of `segments` tags, or `NoSpace` when the allocator or the slab's bound refuses it.
  fn alloc(&mut self, segments: usize) -> Result<TagRun, VfsError> {
    let mut run = Vec::new();
    run
      .try_reserve_exact(segments)
      .map_err(|_| VfsError::NoSpace)?;
    run.resize(segments, Tag::default());
    Ok(self.runs.insert(run.into_boxed_slice())?)
  }

  fn free(&mut self, run: TagRun) -> Result<(), VfsError> {
    Ok(self.runs.discard(run)?)
  }

  fn tag(&self, run: TagRun, index: usize) -> Option<&Tag> {
    self.runs.get(run).ok()?.get(index)
  }

  fn tag_mut(&mut self, run: TagRun, index: usize) -> Option<&mut [u8]> {
    Some(self.runs.get_mut(run).ok()?.get_mut(index)?.as_mut_slice())
  }
}

/// Format: the largest segment a read opens on the stack (the granule the daemon allocates in is at most this, 4 KiB;
/// a larger granule in a test opens through the heap).
const STACK_SEGMENT: usize = 4096;

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

/// Copies the overlap of `bytes`, which sit at file offset `base`, with a buffer `out` for file offset `off`, into
/// that buffer; nothing when they do not overlap. Checked throughout: a range past either end copies nothing rather
/// than indexing out of bounds.
pub(crate) fn copy_overlap(bytes: &[u8], base: u64, off: u64, out: &mut [u8]) {
  let source_end = base.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
  let target_end = off.saturating_add(u64::try_from(out.len()).unwrap_or(u64::MAX));
  let start = base.max(off);
  let end = source_end.min(target_end);
  if start >= end {
    return;
  }
  let span = |from: u64, to: u64| -> Option<std::ops::Range<usize>> {
    Some(usize::try_from(from).ok()?..usize::try_from(to).ok()?)
  };
  let (Some(from), Some(to)) = (
    span(start.saturating_sub(base), end.saturating_sub(base)),
    span(start.saturating_sub(off), end.saturating_sub(off)),
  ) else {
    return;
  };
  if let (Some(source), Some(target)) = (bytes.get(from), out.get_mut(to)) {
    target.copy_from_slice(source);
  }
}

/// The shard's chunk store: the arena and the chunk slab.
pub struct ChunkStore {
  arena: ChunkArena,
  chunks: Slab<Chunk>,
  page: usize,
  granule: usize,
  chunk_bytes: usize,
  /// The cipher chunks are sealed with (A-99), when the shard seals.
  cipher: Option<Box<dyn ChunkCipher>>,
  /// The segment tags of sealed chunks.
  tags: TagStore,
  /// The next version a seal takes: held in memory only, for this store's life (A-99's nonce argument: a key epoch
  /// is drawn per daemon life, so the counter never needs to survive one).
  next_version: u64,
  /// Seals that fell back to the clear because the cipher refused (counted, never silent).
  seal_refusals: u64,
  /// Chunks sealed so far (the non-vacuity count of A-99's sealing).
  sealed: u64,
  /// Seals written to a new block because a recovery image named the open one (A-99 with A-64).
  moved_out_of_image: u64,
}

impl std::fmt::Debug for ChunkStore {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ChunkStore")
      .field("chunks", &self.chunks.len())
      .field("page", &self.page)
      .field("granule", &self.granule)
      .field("chunk_bytes", &self.chunk_bytes)
      .field("sealing", &self.cipher.is_some())
      .finish()
  }
}

/// The chunk size: the largest open extent, and so the unit of copy-on-write. Sixteen base pages
/// (`slates_archive::format::CHUNK_PAGES`, the archive format's chunk rule, so an archive reader's cap
/// covers every chunk this store cuts) until the p90 sealed size is measured on real workloads (Phase 1
/// baselines record it; the formula in §4.5 is "smallest page multiple ≥ p90 sealed size").
pub fn chunk_bytes(page: usize) -> Derived<usize> {
  derived!(
    page
      .max(1)
      .saturating_mul(usize::try_from(slates_archive::format::CHUNK_PAGES).unwrap_or(usize::MAX)),
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
  /// A store over `arena` with `max_chunks` chunk records. Blocks are allocated in the arena's granule, which may
  /// be smaller than the host `page`; chunks are sixteen host pages either way, so a small file takes a small
  /// block and a large file is cut into as few chunks as before.
  pub fn new(arena: ChunkArena, page: usize, max_chunks: usize) -> Self {
    let page = page.max(1);
    let granule = arena.granule().clamp(1, page);
    let chunk_bytes = chunk_bytes(page).get();
    Self {
      arena,
      chunks: Slab::new(max_chunks.min(page), max_chunks),
      page,
      granule,
      chunk_bytes,
      cipher: None,
      tags: TagStore::new(max_chunks, page),
      next_version: 0,
      seal_refusals: 0,
      sealed: 0,
      moved_out_of_image: 0,
    }
  }

  /// Seals chunks from now on with `cipher` (A-99): a chunk sealed under a key is encrypted in the arena and opened
  /// on every read.
  pub fn set_cipher(&mut self, cipher: Box<dyn ChunkCipher>) {
    self.cipher = Some(cipher);
  }

  /// The cipher, for the owner registering a volume's key or a recovery naming one.
  pub fn cipher_mut(&mut self) -> Option<&mut (dyn ChunkCipher + 'static)> {
    self.cipher.as_deref_mut()
  }

  /// The identity of key `key` (what an image records), when this store seals.
  pub fn key_identity(&self, key: u32) -> Option<KeyIdentity> {
    self.cipher.as_ref().and_then(|cipher| cipher.identity(key))
  }

  /// Seals that fell back to the clear because the cipher refused, so far.
  pub const fn seal_refusals(&self) -> u64 {
    self.seal_refusals
  }

  /// Chunks sealed so far.
  pub const fn sealed(&self) -> u64 {
    self.sealed
  }

  /// The tags of a sealed chunk, in segment order (what a recovery image carries).
  pub fn chunk_tags(&self, seal: &ChunkSeal) -> Vec<Tag> {
    (0..usize::try_from(seal.segments).unwrap_or(0))
      .filter_map(|index| self.tags.tag(seal.tags, index).copied())
      .collect()
  }

  /// Encrypts the first `len` bytes of `block` in place under `key`, one granule a segment, into a fresh tag run:
  /// the chunk's seal. A refusal partway opens what it sealed, so the block holds its plaintext again, and returns
  /// `None` (the chunk stays in the clear, counted): content already acknowledged is never lost to the cipher.
  /// Seals the `len` bytes of `block` under `key` where no recovery image is hurt: in place when no image names the
  /// block, or else in a new block, the old one retired through the arena's deferral, so the image's plaintext open
  /// extent still reads as plaintext after a crash before the next publication (A-64 shadow paging, as ZFS and WAFL
  /// never overwrite a block a committed tree names). The block the chunk now lives in, and its seal; with no room
  /// for the copy, the bytes stay in the clear where they are, counted as a refused seal.
  fn seal_where_safe(&mut self, block: Block, len: usize, key: u32) -> (Block, Option<ChunkSeal>) {
    if !self.arena.imaged(block) {
      return (block, self.seal_block(block, len, key));
    }
    let Some(copy) = self.copied_out_of_image(block, len) else {
      self.seal_refusals = self.seal_refusals.saturating_add(1);
      return (block, None);
    };
    let seal = self.seal_block(copy, len, key);
    // The free is deferred: the image names the old block until the next commit.
    let _ = self.arena.free(block);
    self.moved_out_of_image = self.moved_out_of_image.saturating_add(1);
    (copy, seal)
  }

  /// A new block holding the first `len` bytes of `block`, or `None` when the arena or the allocator has no room.
  fn copied_out_of_image(&mut self, block: Block, len: usize) -> Option<Block> {
    let mut plain = Vec::new();
    plain.try_reserve_exact(len).ok()?;
    plain.extend_from_slice(self.arena.bytes(block)?.get(..len)?);
    let copy = self.arena.alloc(block.len()).ok()?;
    match self
      .arena
      .bytes_mut(copy)
      .and_then(|bytes| bytes.get_mut(..len))
    {
      Some(target) => {
        target.copy_from_slice(&plain);
        Some(copy)
      }
      None => {
        let _ = self.arena.free(copy);
        None
      }
    }
  }

  /// Seals that wrote the chunk to a new block because a recovery image named its open block (A-99 with A-64).
  pub const fn moved_out_of_image(&self) -> u64 {
    self.moved_out_of_image
  }

  fn seal_block(&mut self, block: Block, len: usize, key: u32) -> Option<ChunkSeal> {
    let segments = len.div_ceil(self.granule).max(1);
    let version = self.next_version;
    let tags = self.tags.alloc(segments).ok()?;
    let sealed = self.seal_segments(block, len, (key, version), tags, segments);
    match sealed {
      Some(()) => {
        self.next_version = version.checked_add(1)?;
        self.sealed = self.sealed.saturating_add(1);
        Some(ChunkSeal {
          key,
          version,
          tags,
          segments: u32::try_from(segments).ok()?,
        })
      }
      None => {
        let _ = self.tags.free(tags);
        self.seal_refusals = self.seal_refusals.saturating_add(1);
        None
      }
    }
  }

  /// [`Self::seal_block`]'s pass: every segment sealed, or the sealed prefix opened back and `None`.
  fn seal_segments(
    &mut self,
    block: Block,
    len: usize,
    (key, version): (u32, u64),
    tags: TagRun,
    segments: usize,
  ) -> Option<()> {
    let granule = self.granule;
    let cipher = self.cipher.as_ref()?;
    let bytes = self.arena.bytes_mut(block)?.get_mut(..len)?;
    let mut sealed = 0usize;
    for (index, segment) in bytes.chunks_mut(granule).enumerate() {
      let last = index.saturating_add(1) == segments;
      let tag = u32::try_from(index)
        .ok()
        .and_then(|at| cipher.seal(key, version, at, last, segment).ok());
      match (tag, self.tags.tag_mut(tags, index)) {
        (Some(tag), Some(slot)) => {
          slot.copy_from_slice(&tag);
          sealed = index.saturating_add(1);
        }
        _ => break,
      }
    }
    if sealed == segments {
      return Some(());
    }
    // Opened back in place: the tags written so far are the ones these segments were sealed under.
    for (index, segment) in bytes.chunks_mut(granule).take(sealed).enumerate() {
      let last = index.saturating_add(1) == segments;
      if let (Ok(at), Some(tag)) = (u32::try_from(index), self.tags.tag(tags, index)) {
        let _ = cipher.open(key, version, at, last, segment, tag);
      }
    }
    None
  }

  /// Opens the segments of a sealed chunk that a read of `[from, to)` (bytes within the chunk) touches, copying the
  /// requested bytes into `out` as [`copy_overlap`] would place them. Each segment is opened in a scratch buffer, so
  /// the arena keeps its ciphertext.
  fn open_into(
    &self,
    (chunk, seal): (&Chunk, &ChunkSeal),
    (from, to): (usize, usize),
    (base, off): (u64, u64),
    out: &mut [u8],
  ) -> Result<(), VfsError> {
    let cipher = self.cipher.as_ref().ok_or(VfsError::Integrity)?;
    let used = usize::try_from(chunk.len).map_err(|_| VfsError::Invalid)?;
    let sealed = self.arena.bytes(chunk.block).ok_or(VfsError::StaleHandle)?;
    let segments = usize::try_from(seal.segments).map_err(|_| VfsError::Invalid)?;
    let mut heap = Vec::new();
    let first = from / self.granule;
    let last_touched = to.saturating_sub(1) / self.granule;
    for index in first..=last_touched.min(segments.saturating_sub(1)) {
      let start = index.saturating_mul(self.granule);
      let end = start.saturating_add(self.granule).min(used);
      let source = sealed.get(start..end).ok_or(VfsError::StaleHandle)?;
      let tag = self.tags.tag(seal.tags, index).ok_or(VfsError::Integrity)?;
      let at = u32::try_from(index).map_err(|_| VfsError::Integrity)?;
      let last = index.saturating_add(1) == segments;
      // The opened segment sits at chunk byte `start`, which is file offset `base - from + start`.
      let file_at = base
        .saturating_add(u64::try_from(from.max(start).saturating_sub(from)).unwrap_or(u64::MAX));
      // A segment the read covers whole is opened in the caller's buffer, where its bytes belong: no scratch
      // to zero and no second copy (A-99 piece 4, measured in `examples/sealed_read_bench.rs`). On a refused
      // tag the read fails whole, so the ciphertext left there is never returned as content.
      let in_place = if from <= start && end <= to {
        file_at
          .checked_sub(off)
          .and_then(|at_out| usize::try_from(at_out).ok())
          .and_then(|at_out| out.get_mut(at_out..at_out.checked_add(source.len())?))
      } else {
        None
      };
      if let Some(destination) = in_place {
        destination.copy_from_slice(source);
        cipher.open(seal.key, seal.version, at, last, destination, tag)?;
        continue;
      }
      // A segment the read covers in part (at most the first and the last) is opened in scratch.
      let mut stack = [0u8; STACK_SEGMENT];
      let scratch = if source.len() <= STACK_SEGMENT {
        stack.get_mut(..source.len()).ok_or(VfsError::Invalid)?
      } else {
        heap.resize(source.len(), 0);
        heap.as_mut_slice()
      };
      scratch.copy_from_slice(source);
      cipher.open(seal.key, seal.version, at, last, scratch, tag)?;
      let plain =
        scratch.get(from.max(start).saturating_sub(start)..to.min(end).saturating_sub(start));
      if let Some(plain) = plain {
        copy_overlap(plain, file_at, off, out);
      }
    }
    Ok(())
  }

  /// The host page size.
  pub const fn page(&self) -> usize {
    self.page
  }

  /// The allocation unit: every block, and so every window's charge, is a power-of-two multiple of it.
  pub const fn granule(&self) -> usize {
    self.granule
  }

  /// The chunk size.
  pub const fn chunk_bytes(&self) -> usize {
    self.chunk_bytes
  }

  /// Chunk records the slab can still take (a seal takes one).
  pub fn chunk_room(&self) -> usize {
    self.chunks.room()
  }

  /// The chunk slab's capacity in records.
  pub fn chunk_capacity(&self) -> usize {
    self.chunks.max_slots()
  }

  /// Chunks live.
  pub fn chunks(&self) -> usize {
    self.chunks.len()
  }

  /// Bytes allocated in the arena.
  pub fn allocated_bytes(&self) -> usize {
    self.arena.allocated_bytes()
  }

  /// The bytes the arena maps (its address space), of which the budget's usable capacity is at most
  /// the buddy-allocatable part (§4.2 mapped versus usable).
  pub fn mapped_bytes(&self) -> usize {
    self.arena.mapped_bytes()
  }

  /// The most bytes the chunk-record slab can ever hold (§4.2 metadata dimension): its bound in
  /// records times a record slot's bytes.
  pub const fn max_record_footprint_bytes(&self) -> usize {
    self.chunks.max_footprint_bytes()
  }

  /// The arena, for reading blocks.
  pub fn arena(&self) -> &ChunkArena {
    &self.arena
  }

  /// The arena, for adding regions and allocating blocks.
  pub fn arena_mut(&mut self) -> &mut ChunkArena {
    &mut self.arena
  }

  /// Allocates a block of at least `len` bytes. An arena that is out of room while freed blocks wait for a
  /// publication (A-64: the committed recovery image may name them) refuses [`VfsError::PublishNeeded`], not a
  /// memory refusal: the room exists, and the shard's next publication releases it.
  fn alloc(&mut self, len: usize) -> Result<Block, VfsError> {
    match self.arena.alloc(len) {
      Ok(block) => Ok(block),
      Err(slates_mem::MemError::ArenaExhausted { .. }) if self.arena.deferred_bytes() > 0 => {
        Err(VfsError::PublishNeeded)
      }
      Err(refusal) => Err(refusal.into()),
    }
  }

  /// Opens a new extent at `off` with room for at least `want` bytes (page-multiple, at most a
  /// chunk).
  pub fn open(&mut self, off: u64, want: usize, born: Epoch) -> Result<OpenExtent, VfsError> {
    let want = want.clamp(1, self.chunk_bytes);
    let block = self.alloc(want.next_multiple_of(self.granule))?;
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
    if need <= open.block.len() {
      return Ok(());
    }
    if need > self.chunk_bytes {
      return Err(VfsError::FileTooLarge);
    }
    let block = self.alloc(need.next_multiple_of(self.granule))?;
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

  /// The arena block `materialized` bytes of one window take: the smallest power-of-two number
  /// of pages holding them (the buddy's block), at most a chunk — the window's charge (§4.2
  /// "allocator rounding").
  pub fn block_bytes(&self, materialized: usize) -> usize {
    let granules = materialized
      .max(1)
      .div_ceil(self.granule)
      .next_power_of_two();
    granules.saturating_mul(self.granule).min(self.chunk_bytes)
  }

  /// Truncates an open extent to `len` bytes and rebuilds its block at the size the kept bytes
  /// take when that is smaller — the block is the window's charge (§4.2 "allocator rounding"):
  /// it is [`ChunkStore::block_bytes`] of the materialized length, growing and shrinking with it,
  /// so the charged bytes and the arena's allocated bytes never diverge.
  pub fn shrink_open(&mut self, open: &mut OpenExtent, len: u64) -> Result<(), VfsError> {
    open.len = open.len.min(len);
    let keep = usize::try_from(open.len).unwrap_or(usize::MAX);
    let want = self.block_bytes(keep);
    if want >= open.block.len() {
      return Ok(());
    }
    // A smaller block is an economy, never a requirement: when the arena cannot give it now, the extent keeps its
    // block, whose length its charge follows (`volume::content_by_epoch`).
    let Ok(block) = self.alloc(want) else {
      return Ok(());
    };
    let mut carry = vec![0u8; keep];
    if let Some(src) = self.arena.bytes(open.block) {
      carry.copy_from_slice(&src[..keep]);
    }
    if let Some(dst) = self.arena.bytes_mut(block) {
      dst[..keep].copy_from_slice(&carry);
    }
    self.arena.free(open.block)?;
    open.block = block;
    Ok(())
  }

  /// Seals an open extent into a chunk and returns the extent that names it (or `None` for an
  /// empty extent, whose block is released).
  pub fn seal(&mut self, open: OpenExtent, key: Option<u32>) -> Result<Option<Extent>, VfsError> {
    if open.len == 0 {
      self.arena.free(open.block)?;
      return Ok(None);
    }
    let len = usize::try_from(open.len).map_err(|_| VfsError::Invalid)?;
    let (block, seal) = match (key, self.cipher.is_some()) {
      (Some(key), true) => self.seal_where_safe(open.block, len, key),
      _ => (open.block, None),
    };
    let inserted = self.chunks.insert(Chunk {
      born: open.born,
      len: u32::try_from(open.len).unwrap_or(u32::MAX),
      block,
      identity: None,
      seal,
    });
    let chunk = match inserted {
      Ok(chunk) => chunk,
      Err(refusal) => {
        if let Some(seal) = seal {
          self.tags.free(seal.tags)?;
        }
        return Err(refusal.into());
      }
    };
    Ok(Some(Extent {
      off: open.off,
      len: open.len,
      src: ExtentSrc::Chunk { chunk, at: 0 },
    }))
  }

  /// Copies the part of a sealed extent that overlaps a read of `out.len()` bytes at file offset `off` into the
  /// matching place in `out`; a hole leaves `out` as it is (the caller zero-fills first). The one way a sealed
  /// chunk's bytes are read (A-99): every reader goes through here, so the chunk's bytes can be opened from their
  /// sealed form into `out` without any reader seeing the arena. A chunk the extent names that is no longer there is
  /// `StaleHandle`, never zeros presented as content.
  pub fn read_extent_into(
    &self,
    extent: &Extent,
    off: u64,
    out: &mut [u8],
  ) -> Result<(), VfsError> {
    let ExtentSrc::Chunk { chunk, at } = extent.src else {
      return Ok(());
    };
    let c = self.chunks.get(chunk)?;
    let start = usize::try_from(at).map_err(|_| VfsError::Invalid)?;
    let used = usize::try_from(c.len).map_err(|_| VfsError::Invalid)?;
    let wanted = usize::try_from(extent.len).map_err(|_| VfsError::Invalid)?;
    let end = start.saturating_add(wanted).min(used);
    // Only the part of the extent the read overlaps is opened: its chunk bytes are `[lo, hi)`.
    let read_end = off.saturating_add(u64::try_from(out.len()).unwrap_or(u64::MAX));
    let extent_end = extent
      .off
      .saturating_add(u64::try_from(end.saturating_sub(start)).unwrap_or(u64::MAX));
    let (lo_file, hi_file) = (extent.off.max(off), extent_end.min(read_end));
    if lo_file >= hi_file {
      return Ok(());
    }
    let lo = start
      .saturating_add(usize::try_from(lo_file.saturating_sub(extent.off)).unwrap_or(usize::MAX));
    let hi = start
      .saturating_add(usize::try_from(hi_file.saturating_sub(extent.off)).unwrap_or(usize::MAX));
    if let Some(seal) = &c.seal {
      return self.open_into((c, seal), (lo, hi), (lo_file, off), out);
    }
    let bytes = self.arena.bytes(c.block).ok_or(VfsError::StaleHandle)?;
    let source = bytes.get(lo..hi).ok_or(VfsError::StaleHandle)?;
    copy_overlap(source, lo_file, off, out);
    Ok(())
  }

  /// Copies a sealed extent's bytes into a new open extent (copy-on-write), zero-filling
  /// beyond the extent's length up to the chunk's capacity is not needed: the open extent's
  /// length is the extent's.
  pub fn reopen(&mut self, extent: &Extent, born: Epoch) -> Result<OpenExtent, VfsError> {
    let len = usize::try_from(extent.len).unwrap_or(usize::MAX);
    let mut bytes = vec![0u8; len];
    self.read_extent_into(extent, extent.off, &mut bytes)?;
    let mut open = self.open(extent.off, len, born)?;
    if let Err(refusal) = self.write_open(&mut open, 0, &bytes) {
      // Refused whole: the block taken for the copy goes back.
      self.arena.free(open.block)?;
      return Err(refusal);
    }
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
    if let Some(seal) = chunk.seal {
      self.tags.free(seal.tags)?;
    }
    Ok(())
  }

  /// Claims exactly the arena block a recovered image names (A-64); refused `RecoveryIncomplete` when no
  /// allocation could have made it or another claim already holds any of it.
  pub fn claim_block(&mut self, region: u16, offset: u64, len: u64) -> Result<Block, VfsError> {
    let offset = usize::try_from(offset).map_err(|_| VfsError::RecoveryIncomplete)?;
    let len = usize::try_from(len).map_err(|_| VfsError::RecoveryIncomplete)?;
    self
      .arena
      .claim(region, offset, len)
      .map_err(|_| VfsError::RecoveryIncomplete)
  }

  /// Records a recovered chunk over a claimed block (A-64); refused when its used bytes exceed the block. A sealed
  /// chunk comes with its key (already registered with the cipher), its version and its tags in segment order
  /// (A-99): the tags are taken into a fresh run.
  pub fn adopt_chunk(
    &mut self,
    mut chunk: Chunk,
    sealed: Option<(u32, u64, &[Tag])>,
  ) -> Result<Handle<Chunk>, VfsError> {
    if usize::try_from(chunk.len).map_or(true, |len| len > chunk.block.len()) {
      return Err(VfsError::RecoveryIncomplete);
    }
    chunk.seal = match sealed {
      Some((key, version, tags)) => {
        let segments = usize::try_from(chunk.len)
          .unwrap_or(usize::MAX)
          .div_ceil(self.granule)
          .max(1);
        if tags.len() != segments || self.cipher.is_none() {
          return Err(VfsError::RecoveryIncomplete);
        }
        let run = self.tags.alloc(segments)?;
        for (index, tag) in tags.iter().enumerate() {
          if let Some(slot) = self.tags.tag_mut(run, index) {
            slot.copy_from_slice(tag);
          }
        }
        Some(ChunkSeal {
          key,
          version,
          tags: run,
          segments: u32::try_from(segments).map_err(|_| VfsError::RecoveryIncomplete)?,
        })
      }
      None => None,
    };
    match self.chunks.insert(chunk) {
      Ok(handle) => Ok(handle),
      Err(refusal) => {
        if let Some(seal) = chunk.seal {
          self.tags.free(seal.tags)?;
        }
        Err(refusal.into())
      }
    }
  }

  /// Whether `block` is live and not waiting on a deferred free.
  pub fn holds(&self, block: Block) -> bool {
    self.arena.holds(block)
  }

  /// Releases an open extent's block (the file was truncated or unlinked before sealing).
  pub fn release_open(&mut self, open: OpenExtent) -> Result<(), VfsError> {
    self.arena.free(open.block)?;
    Ok(())
  }

  /// Releases a block no chunk or open extent holds (A-64: a recovery claim given back).
  pub fn release_block(&mut self, block: Block) -> Result<(), VfsError> {
    self.arena.free(block)?;
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
    assert_eq!(open.block.len(), 4096);
    s.write_open(&mut open, 0, b"hello").unwrap();
    s.write_open(&mut open, 5000, b"far").unwrap();
    assert_eq!(open.block.len(), 8192, "grew by a page multiple");
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
    let extent = s.seal(open, None).unwrap().unwrap();
    assert_eq!(extent.len, 5003);
    let mut read = [0u8; 3];
    s.read_extent_into(&extent, 5000, &mut read).unwrap();
    assert_eq!(&read, b"far");
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
    let extent = s.seal(open, None).unwrap().unwrap();
    let copy = s.reopen(&extent, Epoch(1)).unwrap();
    assert_eq!(s.open_bytes(&copy), b"abc");
    assert_eq!(copy.born, Epoch(1));
  }

  /// A deterministic cipher for the store's plumbing (never a real one): each byte is XORed with a stream drawn from
  /// the key, version, segment and position, and the tag is a keyed sum over the sealed bytes, so any change to them
  /// or to the tag fails to open. The server's cipher is hyper-seal's AES-256-GCM.
  struct TestCipher;

  fn stream(key: u32, version: u64, index: u32, at: usize) -> u8 {
    let mixed = u64::from(key)
      .wrapping_mul(0x9E37_79B9)
      .wrapping_add(version.wrapping_mul(0x85EB_CA6B))
      .wrapping_add(u64::from(index).wrapping_mul(0xC2B2_AE35))
      .wrapping_add(u64::try_from(at).unwrap());
    (mixed ^ (mixed >> 13)).to_le_bytes()[0]
  }

  fn tag_of(key: u32, version: u64, index: u32, last: bool, sealed: &[u8]) -> Tag {
    let mut sum = u64::from(key) ^ version.rotate_left(17) ^ u64::from(index) ^ u64::from(last);
    for (at, byte) in sealed.iter().enumerate() {
      sum = sum.rotate_left(5) ^ u64::from(*byte) ^ at as u64;
    }
    let mut tag = [0u8; TAG_BYTES];
    tag[..8].copy_from_slice(&sum.to_le_bytes());
    tag
  }

  impl ChunkCipher for TestCipher {
    fn seal(
      &self,
      key: u32,
      version: u64,
      index: u32,
      last: bool,
      segment: &mut [u8],
    ) -> Result<Tag, VfsError> {
      for (at, byte) in segment.iter_mut().enumerate() {
        *byte ^= stream(key, version, index, at);
      }
      Ok(tag_of(key, version, index, last, segment))
    }
    fn open(
      &self,
      key: u32,
      version: u64,
      index: u32,
      last: bool,
      segment: &mut [u8],
      tag: &Tag,
    ) -> Result<(), VfsError> {
      if tag_of(key, version, index, last, segment) != *tag {
        return Err(VfsError::Integrity);
      }
      for (at, byte) in segment.iter_mut().enumerate() {
        *byte ^= stream(key, version, index, at);
      }
      Ok(())
    }
    fn identity(&self, key: u32) -> Option<KeyIdentity> {
      let mut identity = KeyIdentity::default();
      identity[..4].copy_from_slice(&key.to_le_bytes());
      Some(identity)
    }
    fn reference(&mut self, identity: &KeyIdentity) -> Result<u32, VfsError> {
      Ok(u32::from_le_bytes(identity[..4].try_into().unwrap()))
    }
    fn key_for_volume(&mut self, volume: [u8; 16]) -> Result<u32, VfsError> {
      Ok(u32::from(volume[0]))
    }
  }

  /// Shape: the bytes the sealing test writes: three whole segments and a partial fourth.
  const SEALED_LEN: usize = 3 * 4096 + 1000;
  /// Shape: the key reference the sealing test seals under.
  const KEY: u32 = 7;

  fn sealed_store() -> (ChunkStore, Extent, Vec<u8>) {
    let mut s = store();
    s.set_cipher(Box::new(TestCipher));
    let plain: Vec<u8> = (0..SEALED_LEN)
      .map(|at| u8::try_from(at % 251).unwrap())
      .collect();
    let mut open = s.open(100, SEALED_LEN, Epoch(0)).unwrap();
    s.write_open(&mut open, 0, &plain).unwrap();
    let extent = s.seal(open, Some(KEY)).unwrap().unwrap();
    (s, extent, plain)
  }

  /// A-99: do seal a chunk of three and a part segments under a key, read it back whole, read odd spans across
  /// segment edges, and copy it up; expect the arena to hold no stretch of the plaintext, every read to return the
  /// plaintext's bytes for its span, and the copy-up's open extent to hold the plaintext.
  #[test]
  fn a_sealed_chunk_keeps_ciphertext_in_the_arena_and_reads_back_plain() {
    let (mut s, extent, plain) = sealed_store();
    let ExtentSrc::Chunk { chunk, .. } = extent.src else {
      panic!()
    };
    let c = *s.chunk(chunk).unwrap();
    assert!(c.seal.is_some(), "the chunk was sealed");
    let held = &s.arena().bytes(c.block).unwrap()[..SEALED_LEN];
    assert_ne!(held, &plain[..], "the arena holds ciphertext");
    assert!(
      !held.windows(64).any(|w| plain.windows(64).any(|p| p == w)),
      "no stretch of plaintext is in the arena"
    );
    let mut whole = vec![0u8; SEALED_LEN];
    s.read_extent_into(&extent, 100, &mut whole).unwrap();
    assert_eq!(whole, plain);
    for (from, len) in [
      (100, 1),
      (4000 + 100, 300),
      (4096 + 100, 4096),
      (8190 + 100, 5000),
      (12_000, 288),
    ] {
      let mut span = vec![0u8; len];
      s.read_extent_into(&extent, from, &mut span).unwrap();
      let at = usize::try_from(from - 100).unwrap();
      assert_eq!(
        span,
        plain[at..(at + len).min(SEALED_LEN)]
          .iter()
          .copied()
          .chain(std::iter::repeat(0))
          .take(len)
          .collect::<Vec<_>>(),
        "span at {from}"
      );
    }
    let copied = s.reopen(&extent, Epoch(1)).unwrap();
    assert_eq!(&s.open_bytes(&copied)[..SEALED_LEN], &plain[..]);
    s.release_open(copied).unwrap();
  }

  /// A-99: do flip one byte of a sealed chunk in the arena, then read a span that does not touch its segment and one
  /// that does; expect the first to read back plain and the second refused `Integrity`, never wrong bytes. Then free
  /// the chunk; expect its tags returned (a further full chunk seals).
  #[test]
  fn a_changed_byte_in_a_sealed_chunk_is_refused_and_freeing_returns_its_tags() {
    let (mut s, extent, plain) = sealed_store();
    let ExtentSrc::Chunk { chunk, .. } = extent.src else {
      panic!()
    };
    let block = s.chunk(chunk).unwrap().block;
    s.arena_mut().bytes_mut(block).unwrap()[4096 + 10] ^= 1;
    let mut first = vec![0u8; 100];
    s.read_extent_into(&extent, 100, &mut first).unwrap();
    assert_eq!(first, plain[..100]);
    let mut touched = vec![0u8; 100];
    assert!(matches!(
      s.read_extent_into(&extent, 100 + 4096, &mut touched),
      Err(VfsError::Integrity)
    ));
    s.free_chunk(chunk).unwrap();
    assert_eq!(s.seal_refusals(), 0);
  }

  /// A-99: on a store whose granule is its page and on one whose granule is a quarter of its page (macOS arm64: 4 KiB
  /// under 16 KiB, so a full chunk has 64 segments), do seal chunks of every length from one byte to a full chunk and
  /// free half of them as it goes, then seal again; expect every chunk sealed (no fallback to the clear), every one to
  /// read back its bytes, and freed runs reused, so the tag slab neither refuses a chunk's run nor loses one.
  #[test]
  fn every_chunk_length_up_to_a_full_chunk_seals_and_its_tag_run_is_reused() {
    for (granule, page) in [(4096, 4096), (4096, 4 * 4096)] {
      let mut arena = ChunkArena::new(granule);
      arena
        .add_region(Region::map(page * 256, granule, false).unwrap())
        .unwrap();
      let mut s = ChunkStore::new(arena, page, 1024);
      s.set_cipher(Box::new(TestCipher));
      let chunk = s.chunk_bytes();
      let lengths = [
        1,
        4096,
        4097,
        2 * 4096 + 1,
        5 * 4096,
        9 * 4096,
        chunk / 2 + 1,
        chunk,
      ];
      let mut sealed = Vec::new();
      for (round, len) in lengths.iter().copied().chain(lengths).enumerate() {
        let plain: Vec<u8> = (0..len)
          .map(|at| u8::try_from((at + round) % 251).unwrap())
          .collect();
        let mut open = s.open(0, len, Epoch(0)).unwrap();
        s.write_open(&mut open, 0, &plain).unwrap();
        let extent = s.seal(open, Some(KEY)).unwrap().unwrap();
        let mut back = vec![0u8; len];
        s.read_extent_into(&extent, 0, &mut back).unwrap();
        assert_eq!(back, plain, "a {len}-byte chunk reads back (page {page})");
        sealed.push(extent);
        if round % 2 == 1 {
          let ExtentSrc::Chunk { chunk, .. } = sealed.swap_remove(0).src else {
            panic!()
          };
          s.free_chunk(chunk).unwrap();
        }
      }
      assert_eq!(s.seal_refusals(), 0, "page {page}");
      assert_eq!(s.sealed(), u64::try_from(2 * lengths.len()).unwrap());
    }
  }

  /// A-99 piece 4: do read whole segments of a sealed chunk (the reads opened in the caller's buffer), one span
  /// covering two whole segments and one a single segment, then flip a byte of the middle segment and read it whole
  /// again; expect the plaintext for the untouched reads, `Integrity` for the tampered one, and the refused read's
  /// buffer holding the segment's ciphertext rather than zeros, which shows the read took the in-place path (the
  /// scratch path never writes a refused segment into the caller's buffer), so this test cannot pass vacuously.
  #[test]
  fn whole_segments_open_in_the_callers_buffer_and_a_tampered_one_is_refused() {
    let (mut s, extent, plain) = sealed_store();
    // The extent starts at file offset 100; segment `n` is file bytes `100 + n * 4096` for 4096 bytes.
    let mut two = vec![0u8; 2 * 4096];
    s.read_extent_into(&extent, 100, &mut two).unwrap();
    assert_eq!(two, plain[..2 * 4096]);
    let mut third = vec![0u8; 4096];
    s.read_extent_into(&extent, 100 + 2 * 4096, &mut third)
      .unwrap();
    assert_eq!(third, plain[2 * 4096..3 * 4096]);
    let ExtentSrc::Chunk { chunk, .. } = extent.src else {
      panic!()
    };
    let block = s.chunk(chunk).unwrap().block;
    s.arena_mut().bytes_mut(block).unwrap()[4096 + 10] ^= 1;
    let mut middle = vec![0u8; 4096];
    assert!(matches!(
      s.read_extent_into(&extent, 100 + 4096, &mut middle),
      Err(VfsError::Integrity)
    ));
    assert_ne!(middle, vec![0u8; 4096]);
    assert_ne!(middle, plain[4096..2 * 4096]);
    let mut untouched = vec![0u8; 4096];
    s.read_extent_into(&extent, 100, &mut untouched).unwrap();
    assert_eq!(untouched, plain[..4096]);
  }
}
