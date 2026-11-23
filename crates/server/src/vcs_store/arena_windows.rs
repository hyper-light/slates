//! The repository content store's windows kept as chunks of the shard's own chunk store, lent for each step (A-727
//! slice a2, main's ruling (ii); §4.17.1 "Objects" and "Durability and residency"; A-64; D-6).
//!
//! **Why.** A repository's bytes were held in the process's memory by `slates_vcs::ContentStore` and made durable only
//! by the partition's records, so the partition's snapshot carried every byte a repository held and met its slot's
//! region (the 747 MB wall: BENCHMARKS.md and A-727). Kept as chunks of the shard's chunk store, a window lives in the
//! shard's arena of the anchor's content object, which outlives the daemon, as a volume's chunks do (A-64), charged to
//! the shard's one budget like every other byte, and a green's file chunk and a repository window can share a block.
//!
//! **Measured and rejected first** (2026-11-20): a second chunk arena per shard over the same content pool, its claims
//! under an owner range of its own. Wired into a real shard, its first put found nothing to claim
//! (`ArenaExhausted { requested: 4096, largest_free: 0 }`: the volumes' arena holds the slice's extents), and the shard
//! budget tracks the volumes' arena alone, so a second arena would need a fixed split of the slice or a balancer. One
//! arena and one budget is A-64's design.
//!
//! **The lend.** The shard's chunk store belongs to the volume store (`state.store.content`). Every vcs step already
//! holds `&mut state.store` exclusively (`super::within_budget`, `super::lent_to_read`, `super::with_room`), so the step
//! boundary swaps the shard's chunk store into this storage ([`slates_vcs::WindowStorage::lend_in`], leaving a
//! placeholder, `ChunkStore::empty`, that allocates nothing) and swaps it back when the step ends, by a guard's drop
//! (`super::Lent`). Swapping moves the store's handles, never its chunks. A lend inside a lend takes nothing, so only
//! the outer one hands back. Outside a lend a window read answers none and a put is refused `ContentRefused::Storage`,
//! both typed; a release outside one frees its chunk at the next lend. A read inside a lend borrows a chunk in the clear
//! in place (`ChunkStore::chunk_bytes_lent`), nothing copied or allocated; only a sealed chunk is opened into a buffer. Every reader keeps `&ContentStore`; nothing ripples into git, Mercurial or Subversion.
//!
//! **Room.** A put takes a block from the shard's arena. What the content store may hold is bounded by its own budget,
//! which its owner sets from the shard's admittable bytes before each step (`super::lend`): what the arena holds free and
//! what it could claim from its pool. A put the arena has no block for grows the arena from its pool by the window, and
//! the lend's end extends the shard budget by what the arena grew (`super::Lent`); one the pool cannot cover is refused
//! `Storage`.
//!
//! **Slabs** (goal 93; main 01a20182's arena-15, CI 45224166925: 556,367,872 bytes held for 114,200 windows of
//! 131,425,429 bytes, a 4 KiB page a window of about 1,151 bytes). A window shorter than the arena's granule is written
//! into a slab: one chunk of one granule that windows fill end to end, each window its slot (the chunk and its offset
//! in it). A slab's slots are never written twice: a window let go leaves its slot unused, and the slab's chunk is
//! freed when its last window goes, through the chunk store's one free, which the arena defers while a committed image
//! names the block (A-64). So no committed image ever names a slot holding another window's bytes, the idle purge
//! (A-105) only meets a slab once all its slots are free, and a slab one live window keeps holds the one granule that
//! window held alone before. The slab's block asks its live windows' bytes ([`slates_mem::ChunkArena::reask`]), so the
//! arena's row of repository windows names, held against asked, the granules' rounding and the slots let go. A window
//! of a granule or more takes a chunk of its own, as before. A volume's content never shares a block: its chunks are
//! the copies a guest maps (DAX), written by the volume alone.
//!
//! **Across a restart** (A-727 slice a3). Each window kept is a piece of the shard image's held pieces
//! (`content_holder`, kind `KIND_VCS_WINDOW`, keyed by its handle): its identity, its length, its chunk's block (A-64)
//! and its slot's offset in the chunk, carried whole by a checkpoint and by its changes in a delta. A recovery claims
//! every block the pieces name once, however many slots of a slab they name, and records its chunk before the volumes
//! ([`ChunkWindows::restore`]), so none is reused
//! before the successor's first publication commits; the content store made over the windows holds them for no one,
//! the records' replay holds each again by identity without storing it again, and the windows no record named are
//! freed once the records are replayed (`ContentStore::release_unheld`). A window kept after the last publication is
//! not in the image: the replay puts it again from its record, which still holds its bytes.
//!
//! **Lists and names too** (A-727 slice a3b; the identity rule). An object past one window is its windows under lists
//! in the content store's own memory, named by the BLAKE3 of its bytes through a node-local name to its root. Both ride
//! the shard image beside the windows (`content_holder`, kinds 12 and 13: a list by its identity with its bytes, a name
//! by the object's identity with its root), whole at a checkpoint and by their changes in a delta
//! (`ContentStore::changes`). A recovery hands them to the content store made over the restored windows
//! (`ContentStore::restore_nodes`): each list held by no one and holding again the nodes it names, each name whose root
//! is held; the replay holds them again by identity, and what no record names is freed with what it names. So a record
//! naming an object past a window by its identity alone finds it after a restart.
//!
//! **Owed.** The records naming identities only (slice b; w-balance's name rows, `slates_vcs::rows`), which needs the
//! window durable in a committed image before the entry naming it (w-balance's `homes` tickets). A window homed for
//! another shard (`homes::keep_here`) is restored held by no one and freed after the replay until its holders are
//! recorded. The shard's cipher does not yet seal these chunks (A-99).

use std::any::Any;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use slates_mem::Handle;
use slates_vcs::{ContentId, ContentRefused, StoredWindow, WindowStorage};
use slates_vfs::content::{Chunk, ChunkStore, Extent, ExtentSrc};
use slates_vfs::ids::Epoch;

/// A window's chunk: its extent in the lent store (the chunk and the window's slot in it), and its block as the shard
/// image names it (A-64): region, offset, the block's length, the chunk's bytes used and the birth epoch, read when it
/// was sealed, since an image is made outside a lend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Placed {
  extent: Extent,
  region: u16,
  offset: u64,
  block: u64,
  used: u32,
  born: u64,
}

impl Placed {
  /// The block's key among the slabs: its region and offset.
  fn key(&self) -> (u16, u64) {
    (self.region, self.offset)
  }

  /// The chunk the window lies in and its slot's offset there.
  fn slot(&self) -> Option<(Handle<Chunk>, u32)> {
    match self.extent.src {
      ExtentSrc::Chunk { chunk, at } => Some((chunk, at)),
      ExtentSrc::Zero => None,
    }
  }
}

/// One chunk the windows lie in (module docs, "Slabs"): a slab of several windows' slots, or a window's own chunk.
#[derive(Clone, Copy, Debug)]
struct Slab {
  /// The chunk.
  chunk: Handle<Chunk>,
  /// Its windows kept.
  live: u32,
  /// Their bytes: what the block asks of the arena.
  asked: usize,
  /// Windows let go from it while it is held, and their bytes.
  freed: (u64, u64),
}

/// The slabs' census for the memory status (goal 93): slabs held, windows let go from slabs still held, their bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SlabCensus {
  /// Chunks the windows lie in.
  pub(crate) slabs: u64,
  /// Windows let go whose slab is still held by another.
  pub(crate) freed_kept: u64,
  /// Their bytes.
  pub(crate) freed_kept_bytes: u64,
}

/// One window kept: its identity once the content store names it, its length, and its chunk (none for an empty window).
#[derive(Clone, Copy, Debug)]
struct Kept {
  identity: Option<ContentId>,
  len: usize,
  placed: Option<Placed>,
}

/// The windows kept as chunks of the shard's lent chunk store (module docs).
pub(crate) struct ChunkWindows {
  /// The shard's chunk store while it is lent; a placeholder otherwise.
  chunks: ChunkStore,
  /// Whether the shard's chunk store is lent now.
  lent: bool,
  /// Each window kept, by handle.
  held: BTreeMap<StoredWindow, Kept>,
  /// The next handle.
  next: u64,
  /// Chunks of windows released outside a lend, freed when the store is next lent.
  owed: Vec<Handle<Chunk>>,
  /// The windows kept or let go since the last committed publication (A-727 slice a3): what a delta carries.
  changed: BTreeSet<StoredWindow>,
  /// The windows restored from the shard image, not yet handed to the content store made over this storage.
  restored: Vec<(ContentId, StoredWindow, usize)>,
  /// Every chunk the windows lie in, by its block's region and offset (module docs, "Slabs").
  slabs: BTreeMap<(u16, u64), Slab>,
  /// The slab windows are written into now, the offset of its first unused byte and its chunk's length.
  filling: Option<((u16, u64), usize, usize)>,
  /// Slabs whose asked bytes changed outside a lend, asked again of the arena at the next.
  reask_owed: BTreeSet<(u16, u64)>,
  /// Windows let go from slabs still held, and their bytes.
  freed_kept: (u64, u64),
}

impl std::fmt::Debug for ChunkWindows {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("ChunkWindows")
      .field("held", &self.held.len())
      .field("lent", &self.lent)
      .field("changed", &self.changed.len())
      .finish()
  }
}

/// Format: a window piece's version (A-727 slice a3; 2 since the slabs, which added the slot's offset).
const PIECE_VERSION: u8 = 2;
/// Format: a window piece's bytes: version, identity, length, whether it has a chunk, then the chunk's region, offset,
/// block length, bytes used, birth epoch and the window's slot offset in it, little-endian.
const PIECE_BYTES: usize = 1 + 32 + 8 + 1 + 2 + 8 + 8 + 4 + 8 + 4;

/// Why a window piece of the shard image was not restored (A-727 slice a3).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PieceRefused {
  /// Its bytes are not a window piece.
  Malformed,
  /// A window piece of an earlier version, one window a chunk with no slot (before the slabs): its window is put again
  /// by the record replay that holds its bytes.
  OldVersion(u8),
  /// Its chunk's block could not be claimed in the arena, or its chunk not recorded (A-64).
  Unclaimable,
}

/// A window piece's bytes (module docs, "Owed": the table the shard image carries).
fn piece_of(identity: &ContentId, kept: &Kept) -> Vec<u8> {
  let mut out = Vec::with_capacity(PIECE_BYTES);
  out.push(PIECE_VERSION);
  out.extend_from_slice(&identity.0);
  out.extend_from_slice(&u64::try_from(kept.len).unwrap_or(u64::MAX).to_le_bytes());
  let placed = kept.placed.unwrap_or(Placed {
    extent: Extent {
      off: 0,
      len: 0,
      src: ExtentSrc::Zero,
    },
    region: 0,
    offset: 0,
    block: 0,
    used: 0,
    born: 0,
  });
  out.push(u8::from(kept.placed.is_some()));
  out.extend_from_slice(&placed.region.to_le_bytes());
  out.extend_from_slice(&placed.offset.to_le_bytes());
  out.extend_from_slice(&placed.block.to_le_bytes());
  out.extend_from_slice(&placed.used.to_le_bytes());
  out.extend_from_slice(&placed.born.to_le_bytes());
  out.extend_from_slice(&placed.slot().map_or(0, |(_, at)| at).to_le_bytes());
  out
}

/// A window piece's chunk as read back: its block (region, offset, length), the chunk's bytes used, its birth epoch,
/// and the window's slot offset in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PieceChunk {
  region: u16,
  offset: u64,
  block: u64,
  used: u32,
  born: u64,
  at: u32,
}

/// A window piece read back: its identity, its length, and its chunk.
type PieceRead = (ContentId, usize, Option<PieceChunk>);

/// Reads a window piece ([`piece_of`]); `OldVersion` for an earlier version's, `Malformed` for bytes of another
/// length, or a slot past its chunk's bytes.
fn read_piece(bytes: &[u8]) -> Result<PieceRead, PieceRefused> {
  match bytes.first() {
    Some(&PIECE_VERSION) => {}
    Some(&version) if version < PIECE_VERSION => return Err(PieceRefused::OldVersion(version)),
    _ => return Err(PieceRefused::Malformed),
  }
  if bytes.len() != PIECE_BYTES {
    return Err(PieceRefused::Malformed);
  }
  let mut at = 1usize;
  let mut take = |n: usize| -> Result<&[u8], PieceRefused> {
    let field = bytes
      .get(at..at.saturating_add(n))
      .ok_or(PieceRefused::Malformed)?;
    at = at.saturating_add(n);
    Ok(field)
  };
  let identity: [u8; 32] = take(32)?.try_into().map_err(|_| PieceRefused::Malformed)?;
  let le8 = |b: &[u8]| {
    b.try_into()
      .map(u64::from_le_bytes)
      .map_err(|_| PieceRefused::Malformed)
  };
  let len = usize::try_from(le8(take(size_of::<u64>())?)?).map_err(|_| PieceRefused::Malformed)?;
  let has = take(1)?.first().copied();
  let region = take(2)?
    .try_into()
    .map(u16::from_le_bytes)
    .map_err(|_| PieceRefused::Malformed)?;
  let offset = le8(take(size_of::<u64>())?)?;
  let block = le8(take(size_of::<u64>())?)?;
  let used = take(size_of::<u32>())?
    .try_into()
    .map(u32::from_le_bytes)
    .map_err(|_| PieceRefused::Malformed)?;
  let born = le8(take(size_of::<u64>())?)?;
  let at = take(size_of::<u32>())?
    .try_into()
    .map(u32::from_le_bytes)
    .map_err(|_| PieceRefused::Malformed)?;
  let placed = match has {
    Some(0) => None,
    Some(1) => {
      let end = u64::from(at).saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
      if len == 0 || end > u64::from(used) || u64::from(used) > block {
        return Err(PieceRefused::Malformed);
      }
      Some(PieceChunk {
        region,
        offset,
        block,
        used,
        born,
        at,
      })
    }
    _ => return Err(PieceRefused::Malformed),
  };
  Ok((ContentId(identity), len, placed))
}

/// Test support (A-870 piece 2's oracle, by slot since the slabs): a window piece's identity and the bytes its slot
/// takes in the arena (region, offset, length), none for one of no chunk or not a window piece.
#[cfg(test)]
pub(crate) fn piece_block(bytes: &[u8]) -> Option<(ContentId, u16, u64, u64)> {
  let (identity, len, placed) = read_piece(bytes).ok()?;
  let placed = placed?;
  Some((
    identity,
    placed.region,
    placed.offset.saturating_add(u64::from(placed.at)),
    u64::try_from(len).ok()?,
  ))
}

/// The pieces of one slab a shard image names: each window's handle, identity, length and chunk.
type SlabPieces = Vec<(u64, ContentId, usize, PieceChunk)>;

impl ChunkWindows {
  /// Windows kept as chunks of a chunk store lent to them a step at a time; `page` the placeholder's page.
  pub(crate) fn new(page: usize) -> ChunkWindows {
    ChunkWindows {
      chunks: ChunkStore::empty(page),
      lent: false,
      held: BTreeMap::new(),
      next: 0,
      owed: Vec::new(),
      changed: BTreeSet::new(),
      restored: Vec::new(),
      slabs: BTreeMap::new(),
      filling: None,
      reask_owed: BTreeSet::new(),
      freed_kept: (0, 0),
    }
  }

  /// The slabs' census (goal 93): chunks the windows lie in, and the windows let go from slabs still held.
  pub(crate) fn slab_census(&self) -> SlabCensus {
    SlabCensus {
      slabs: u64::try_from(self.slabs.len()).unwrap_or(u64::MAX),
      freed_kept: self.freed_kept.0,
      freed_kept_bytes: self.freed_kept.1,
    }
  }

  /// `bytes` placed in the lent store (module docs, "Slabs"): in the filling slab's next slot when they are shorter
  /// than a granule and fit, else in a chunk of their own, a fresh slab of one granule when shorter than one.
  fn place(&mut self, bytes: &[u8]) -> Result<Option<Placed>, ContentRefused> {
    // Derived: a slab is one granule, the arena's smallest block (`ChunkStore::block_bytes` of one byte). A slab one
    // live window keeps then holds what that window held alone before the slabs, so the slabs never hold more than
    // a chunk a window did; and arena-15's windows (CI 45224166925: 114,200 of 131,425,429 bytes, 1,151 bytes each)
    // fill a 4 KiB granule three at a time and a 16 KiB one fourteen. A window of a granule or more shares nothing.
    let granule = self.chunks.block_bytes(1);
    let shares = bytes.len() < granule;
    if shares && let Some(placed) = self.write_into_filling(bytes)? {
      return Ok(Some(placed));
    }
    let want = if shares { granule } else { bytes.len() };
    let Some(placed) = self
      .seal(bytes, want)
      .map_err(|_| ContentRefused::Storage)?
    else {
      return Ok(None);
    };
    let (chunk, _) = placed.slot().ok_or(ContentRefused::Storage)?;
    let key = placed.key();
    self.slabs.insert(
      key,
      Slab {
        chunk,
        live: 1,
        asked: bytes.len(),
        freed: (0, 0),
      },
    );
    self.reask(key);
    if shares {
      // The slab with the more room left fills on: a window that did not fit the last leaves it filling where its own
      // slab has less room.
      let end = usize::try_from(placed.used).unwrap_or(0);
      let room = end.saturating_sub(bytes.len());
      let room_before = self.filling.map_or(0, |(_, cursor, end_before)| {
        end_before.saturating_sub(cursor)
      });
      if room >= room_before {
        self.filling = Some((key, bytes.len(), end));
      }
    }
    Ok(Some(placed))
  }

  /// `bytes` written into the filling slab's next slot, or none when there is no filling slab or they do not fit.
  fn write_into_filling(&mut self, bytes: &[u8]) -> Result<Option<Placed>, ContentRefused> {
    let Some((key, cursor, end)) = self.filling else {
      return Ok(None);
    };
    let Some(slot_end) = cursor
      .checked_add(bytes.len())
      .filter(|slot_end| *slot_end <= end)
    else {
      return Ok(None);
    };
    let Some(slab) = self.slabs.get(&key).copied() else {
      self.filling = None;
      return Ok(None);
    };
    // Derived: a slab's windows are at most its granule's bytes (a window takes one at least), so its count is bounded
    // by the granule and refused typed only past `u32`, which no granule the arena takes reaches.
    let live = slab
      .live
      .checked_add(1)
      .ok_or(ContentRefused::CountExhausted)?;
    let at = u32::try_from(cursor).map_err(|_| ContentRefused::CountExhausted)?;
    let chunk = *self
      .chunks
      .chunk(slab.chunk)
      .ok_or(ContentRefused::Storage)?;
    // A slot no window has used: the slab's chunk was sealed over the whole granule, zero past its first window, and
    // its slots are never written twice (module docs, "Slabs"), so no image or reader names these bytes yet.
    self
      .chunks
      .arena_mut()
      .bytes_mut(chunk.block)
      .and_then(|block| block.get_mut(cursor..slot_end))
      .ok_or(ContentRefused::Storage)?
      .copy_from_slice(bytes);
    self.filling = Some((key, slot_end, end));
    if let Some(kept) = self.slabs.get_mut(&key) {
      kept.live = live;
      kept.asked = kept.asked.saturating_add(bytes.len());
    }
    self.reask(key);
    Ok(Some(Placed {
      extent: Extent {
        off: 0,
        len: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        src: ExtentSrc::Chunk {
          chunk: slab.chunk,
          at,
        },
      },
      region: key.0,
      offset: key.1,
      block: u64::try_from(chunk.block.len()).unwrap_or(u64::MAX),
      used: chunk.len,
      born: chunk.born.0,
    }))
  }

  /// Asks the arena for the slab's live windows' bytes now when the store is lent, at the next lend otherwise.
  fn reask(&mut self, key: (u16, u64)) {
    let Some(slab) = self.slabs.get(&key).copied() else {
      return;
    };
    if !self.lent {
      self.reask_owed.insert(key);
      return;
    }
    if let Some(block) = self.chunks.chunk(slab.chunk).map(|chunk| chunk.block) {
      self.chunks.arena_mut().reask(block, slab.asked);
    }
  }

  /// Frees a chunk now when the store is lent, at the next lend otherwise.
  fn free_or_owe(&mut self, chunk: Handle<Chunk>) {
    if self.lent {
      // A chunk the store cannot free stays held until the arena goes; the content store's own counts are settled.
      // A block a committed image names is not reused before a newer one commits: the arena defers it (A-64).
      let _freed = self.chunks.free_chunk(chunk);
    } else {
      self.owed.push(chunk);
    }
  }

  /// `bytes` sealed as one chunk of the lent store of `want` bytes at least, the rest zero: its extent and block.
  fn seal(&mut self, bytes: &[u8], want: usize) -> Result<Option<Placed>, slates_vfs::VfsError> {
    let mut open = match self
      .chunks
      .open(0, want, Epoch(0), (false, slates_mem::Owner::VcsWindows))
    {
      Ok(open) => open,
      Err(_) => {
        // The arena holds no block for it: grown from its pool by the window, once; the lend's end extends the shard
        // budget by what was grown (`super::Lent`), as `Store::make_room` does.
        let _grown = self.chunks.arena_mut().grow(want);
        self
          .chunks
          .open(0, want, Epoch(0), (false, slates_mem::Owner::VcsWindows))?
      }
    };
    // A slab's chunk is sealed over its whole granule, so its later slots lie inside the chunk's bytes.
    let written = self.chunks.write_open(&mut open, 0, bytes).and_then(|()| {
      match want.checked_sub(1).filter(|last| *last >= bytes.len()) {
        Some(last) => self.chunks.write_open(&mut open, last, &[0]),
        None => Ok(()),
      }
    });
    if let Err(refused) = written {
      let _given_back = self.chunks.release_open(open);
      return Err(refused);
    }
    let Some(sealed) = self.chunks.seal(open, None)? else {
      return Ok(None);
    };
    let ExtentSrc::Chunk { chunk, .. } = sealed.src else {
      return Ok(None);
    };
    let record = self
      .chunks
      .chunk(chunk)
      .ok_or(slates_vfs::VfsError::RecoveryIncomplete)?;
    Ok(Some(Placed {
      extent: Extent {
        off: 0,
        len: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        src: ExtentSrc::Chunk { chunk, at: 0 },
      },
      region: record.block.region(),
      offset: u64::try_from(record.block.offset()).unwrap_or(u64::MAX),
      block: u64::try_from(record.block.len()).unwrap_or(u64::MAX),
      used: record.len,
      born: record.born.0,
    }))
  }

  /// The handles of the windows kept or let go since the last committed publication (A-870: the pages a delta
  /// re-encodes).
  pub(crate) fn changed_handles(&self) -> Vec<u64> {
    self.changed.iter().map(|window| window.0).collect()
  }

  /// The pieces of the windows kept whose handles lie in `lo..=hi`, each its handle and bytes (A-870: one page of the
  /// shard image).
  pub(crate) fn pieces_in(&self, lo: u64, hi: u64) -> Vec<(u64, Vec<u8>)> {
    self
      .held
      .range(StoredWindow(lo)..=StoredWindow(hi))
      .filter_map(|(window, kept)| Some((window.0, piece_of(kept.identity.as_ref()?, kept))))
      .collect()
  }

  /// Forgets the changes a committed publication carried.
  pub(crate) fn clear_changes(&mut self) {
    self.changed.clear();
  }

  /// The windows a shard image's pieces name, each chunk's block claimed in `chunks` once, however many windows' slots
  /// it holds, and its chunk recorded over it (A-64, A-727 slice a3): what a recovery restores before the volumes and
  /// the records. A piece that cannot be restored is left out, with its slab's claim given back when it was the slab's
  /// that failed, and counted in the second value: its window is put again by the record replay that still holds its
  /// bytes. A restored slab fills no further: its unused bytes stay unused until its last window goes.
  pub(crate) fn restore(
    page: usize,
    pieces: &[(u64, Vec<u8>)],
    chunks: &mut ChunkStore,
  ) -> (ChunkWindows, u64) {
    let mut windows = ChunkWindows::new(page);
    let mut refused = 0u64;
    let mut slabs: BTreeMap<(u16, u64), SlabPieces> = BTreeMap::new();
    for (handle, bytes) in pieces {
      match read_piece(bytes) {
        Ok((identity, 0, None)) => {
          if windows.adopt(*handle, identity, 0, None).is_err() {
            refused = refused.saturating_add(1);
          }
        }
        Ok((identity, len, Some(chunk))) => slabs
          .entry((chunk.region, chunk.offset))
          .or_default()
          .push((*handle, identity, len, chunk)),
        Ok((_, _, None)) | Err(_) => refused = refused.saturating_add(1),
      }
    }
    for (key, slots) in slabs {
      if windows.restore_slab(key, &slots, chunks).is_err() {
        refused = refused.saturating_add(u64::try_from(slots.len()).unwrap_or(u64::MAX));
      }
    }
    (windows, refused)
  }

  /// One slab of a shard image restored ([`ChunkWindows::restore`]): its pieces agreeing on its chunk, their slots
  /// apart and their handles new, its block claimed and its chunk recorded once, its windows kept in their slots.
  fn restore_slab(
    &mut self,
    key: (u16, u64),
    slots: &[(u64, ContentId, usize, PieceChunk)],
    chunks: &mut ChunkStore,
  ) -> Result<(), PieceRefused> {
    let first = slots
      .first()
      .map(|slot| slot.3)
      .ok_or(PieceRefused::Malformed)?;
    let same = |chunk: &PieceChunk| {
      (chunk.block, chunk.used, chunk.born) == (first.block, first.used, first.born)
    };
    let mut ranges: Vec<(u64, u64)> = Vec::with_capacity(slots.len());
    let mut handles = BTreeSet::new();
    for (handle, _, len, chunk) in slots {
      let fresh = handles.insert(*handle) && !self.held.contains_key(&StoredWindow(*handle));
      if !same(chunk) || !fresh {
        return Err(PieceRefused::Malformed);
      }
      let at = u64::from(chunk.at);
      ranges.push((
        at,
        at.saturating_add(u64::try_from(*len).unwrap_or(u64::MAX)),
      ));
    }
    ranges.sort_unstable();
    if ranges
      .windows(2)
      .any(|pair| matches!(pair, [(_, end), (next, _)] if end > next))
    {
      return Err(PieceRefused::Malformed);
    }
    let live = u32::try_from(slots.len()).map_err(|_| PieceRefused::Malformed)?;
    let claimed = chunks
      .claim_chunk_blocks(
        key.0,
        (key.1, first.block),
        (first.used, slates_mem::Owner::VcsWindows),
      )
      .map_err(|_| PieceRefused::Unclaimable)?;
    let adopted = chunks.adopt_chunk(
      Chunk {
        born: Epoch(first.born),
        len: first.used,
        block: claimed,
        identity: None,
        seal: None,
        pins: slates_vfs::content::Pins::default(),
      },
      None,
    );
    let chunk = match adopted {
      Ok(chunk) => chunk,
      Err(_) => {
        let _given_back = chunks.give_back_chunk_blocks(claimed, first.used);
        return Err(PieceRefused::Unclaimable);
      }
    };
    let asked = slots
      .iter()
      .map(|slot| slot.2)
      .fold(0usize, usize::saturating_add);
    chunks.arena_mut().reask(claimed, asked);
    for (handle, identity, len, piece) in slots {
      let placed = Placed {
        extent: Extent {
          off: 0,
          len: u64::try_from(*len).unwrap_or(u64::MAX),
          src: ExtentSrc::Chunk {
            chunk,
            at: piece.at,
          },
        },
        region: key.0,
        offset: key.1,
        block: first.block,
        used: first.used,
        born: first.born,
      };
      self.adopt(*handle, *identity, *len, Some(placed))?;
    }
    self.slabs.insert(
      key,
      Slab {
        chunk,
        live,
        asked,
        freed: (0, 0),
      },
    );
    Ok(())
  }

  /// A restored window kept under its handle, for the content store made over this storage.
  fn adopt(
    &mut self,
    handle: u64,
    identity: ContentId,
    len: usize,
    placed: Option<Placed>,
  ) -> Result<(), PieceRefused> {
    let window = StoredWindow(handle);
    if self.held.contains_key(&window) {
      return Err(PieceRefused::Malformed);
    }
    self.held.insert(
      window,
      Kept {
        identity: Some(identity),
        len,
        placed,
      },
    );
    self.restored.push((identity, window, len));
    self.next = self.next.max(handle.saturating_add(1));
    Ok(())
  }
}

impl WindowStorage for ChunkWindows {
  fn put(&mut self, bytes: &[u8]) -> Result<StoredWindow, ContentRefused> {
    let placed = if bytes.is_empty() {
      None
    } else if self.lent {
      self.place(bytes)?
    } else {
      return Err(ContentRefused::Storage);
    };
    let window = StoredWindow(self.next);
    self.next = self
      .next
      .checked_add(1)
      .ok_or(ContentRefused::CountExhausted)?;
    self.held.insert(
      window,
      Kept {
        identity: None,
        len: bytes.len(),
        placed,
      },
    );
    self.changed.insert(window);
    Ok(window)
  }

  fn read(&self, window: StoredWindow) -> Option<Cow<'_, [u8]>> {
    let kept = self.held.get(&window)?;
    let Some(placed) = kept.placed else {
      return Some(Cow::Borrowed(&[]));
    };
    if !self.lent {
      return None;
    }
    // A chunk in the clear is lent in place (Ada's ruling: a read allocates nothing); a sealed one is opened into a
    // buffer, the one copy its cipher needs (A-99).
    if let Some(bytes) = self.chunks.chunk_bytes_lent(&placed.extent) {
      return Some(Cow::Borrowed(bytes));
    }
    let mut bytes = vec![0; usize::try_from(placed.extent.len).ok()?];
    self
      .chunks
      .read_extent_into(&placed.extent, 0, &mut bytes)
      .ok()?;
    Some(Cow::Owned(bytes))
  }

  fn read_range_in(
    &self,
    window: StoredWindow,
    (offset, into): (usize, &mut [u8]),
    kept_in: &dyn Any,
  ) -> Option<()> {
    let kept = self.held.get(&window)?;
    let Some(placed) = kept.placed else {
      return into.is_empty().then_some(());
    };
    let end = offset.checked_add(into.len())?;
    if u64::try_from(end).ok()? > placed.extent.len {
      return None;
    }
    let chunks = if self.lent {
      &self.chunks
    } else {
      kept_in.downcast_ref::<ChunkStore>()?
    };
    chunks
      .read_extent_into(&placed.extent, u64::try_from(offset).ok()?, into)
      .ok()
  }

  fn read_in<'a>(&'a self, window: StoredWindow, kept_in: &'a dyn Any) -> Option<Cow<'a, [u8]>> {
    if self.lent {
      return self.read(window);
    }
    // Not lent this step: the shard's chunk store holds the chunks, read in its place (A-760: a replay's read).
    let kept = self.held.get(&window)?;
    let Some(placed) = kept.placed else {
      return Some(Cow::Borrowed(&[]));
    };
    let chunks = kept_in.downcast_ref::<ChunkStore>()?;
    let mut bytes = vec![0; usize::try_from(placed.extent.len).ok()?];
    chunks
      .read_extent_into(&placed.extent, 0, &mut bytes)
      .ok()?;
    Some(Cow::Owned(bytes))
  }

  fn release(&mut self, window: StoredWindow) {
    let Some(kept) = self.held.remove(&window) else {
      return;
    };
    self.changed.insert(window);
    let Some(placed) = kept.placed else {
      return;
    };
    let key = placed.key();
    let Some(slab) = self.slabs.get_mut(&key) else {
      // Every window's chunk is a slab's; one that is not is freed alone, as before the slabs.
      if let Some((chunk, _)) = placed.slot() {
        self.free_or_owe(chunk);
      }
      return;
    };
    slab.live = slab.live.saturating_sub(1);
    slab.asked = slab.asked.saturating_sub(kept.len);
    if slab.live > 0 {
      // The slot stays unused while the slab is held (module docs, "Slabs").
      let len = u64::try_from(kept.len).unwrap_or(u64::MAX);
      slab.freed = (
        slab.freed.0.saturating_add(1),
        slab.freed.1.saturating_add(len),
      );
      self.freed_kept = (
        self.freed_kept.0.saturating_add(1),
        self.freed_kept.1.saturating_add(len),
      );
      self.reask(key);
      return;
    }
    let chunk = slab.chunk;
    let freed = slab.freed;
    self.slabs.remove(&key);
    self.reask_owed.remove(&key);
    self.freed_kept = (
      self.freed_kept.0.saturating_sub(freed.0),
      self.freed_kept.1.saturating_sub(freed.1),
    );
    if self.filling.is_some_and(|(filling, _, _)| filling == key) {
      self.filling = None;
    }
    self.free_or_owe(chunk);
  }

  fn lend_in(&mut self, lent: &mut dyn Any) -> bool {
    match lent.downcast_mut::<ChunkStore>() {
      Some(chunks) if !self.lent => {
        std::mem::swap(&mut self.chunks, chunks);
        self.lent = true;
        for chunk in std::mem::take(&mut self.owed) {
          let _freed = self.chunks.free_chunk(chunk);
        }
        for key in std::mem::take(&mut self.reask_owed) {
          self.reask(key);
        }
        true
      }
      _ => false,
    }
  }

  fn hand_back(&mut self, lent: &mut dyn Any) -> bool {
    match lent.downcast_mut::<ChunkStore>() {
      Some(chunks) if self.lent => {
        std::mem::swap(&mut self.chunks, chunks);
        self.lent = false;
        true
      }
      _ => false,
    }
  }

  fn named(&mut self, window: StoredWindow, identity: &ContentId) {
    if let Some(kept) = self.held.get_mut(&window) {
      kept.identity = Some(*identity);
    }
  }

  fn restored(&mut self) -> Vec<(ContentId, StoredWindow, usize)> {
    std::mem::take(&mut self.restored)
  }

  fn as_any(&self) -> Option<&dyn Any> {
    Some(self)
  }

  fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
    Some(self)
  }
}

/// Test support: the slabs' census of the windows a content store keeps, when they are kept as chunks.
#[cfg(test)]
fn chunk_windows_census(store: &slates_vcs::ContentStore) -> SlabCensus {
  store
    .storage()
    .as_any()
    .and_then(|any| any.downcast_ref::<ChunkWindows>())
    .map(ChunkWindows::slab_census)
    .unwrap_or_default()
}

#[cfg(test)]
#[path = "arena_windows/tests.rs"]
mod tests;
