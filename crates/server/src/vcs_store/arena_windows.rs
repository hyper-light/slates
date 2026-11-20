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
//! both typed; a release outside one frees its chunk at the next lend. Every reader keeps `&ContentStore`; nothing ripples into git, Mercurial or Subversion.
//!
//! **Room.** A put takes a block from the shard's arena. What the content store may hold is bounded by its own budget,
//! which its owner sets from the shard's admittable bytes before each step (`super::lend`): what the arena holds free and
//! what it could claim from its pool. A put the arena has no block for grows the arena from its pool by the window, and
//! the lend's end extends the shard budget by what the arena grew (`super::Lent`); one the pool cannot cover is refused
//! `Storage`.
//!
//! **Across a restart** (A-727 slice a3). Each window kept is a piece of the shard image's held pieces
//! (`content_holder`, kind `KIND_VCS_WINDOW`, keyed by its handle): its identity, its length and its chunk's block (A-64),
//! carried whole by a checkpoint and by its changes in a delta ([`ChunkWindows::changes`]). A recovery claims every
//! block the pieces name and records their chunks before the volumes ([`ChunkWindows::restore`]), so none is reused
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

/// A window's chunk: its extent in the lent store, and its block as the shard image names it (A-64): region, offset,
/// the block's length, the bytes used and the birth epoch, read when it was sealed, since an image is made outside a
/// lend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Placed {
  extent: Extent,
  region: u16,
  offset: u64,
  block: u64,
  used: u32,
  born: u64,
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

/// Format: a window piece's version (A-727 slice a3).
const PIECE_VERSION: u8 = 1;
/// Format: a window piece's bytes: version, identity, length, whether it has a chunk, then the chunk's region, offset,
/// block length, bytes used and birth epoch, little-endian.
const PIECE_BYTES: usize = 1 + 32 + 8 + 1 + 2 + 8 + 8 + 4 + 8;

/// Why a window piece of the shard image was not restored (A-727 slice a3).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PieceRefused {
  /// Its bytes are not a window piece of this version.
  Malformed,
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
  out
}

/// A window piece read back: its identity, its length, and its chunk's block (region, offset, length, used, born).
type PieceRead = (ContentId, usize, Option<(u16, u64, u64, u32, u64)>);

/// Reads a window piece ([`piece_of`]); `Malformed` for bytes of another length or version.
fn read_piece(bytes: &[u8]) -> Result<PieceRead, PieceRefused> {
  if bytes.len() != PIECE_BYTES || bytes.first() != Some(&PIECE_VERSION) {
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
  let placed = match has {
    Some(0) => None,
    Some(1) => Some((region, offset, block, used, born)),
    _ => return Err(PieceRefused::Malformed),
  };
  Ok((ContentId(identity), len, placed))
}

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
    }
  }

  /// `bytes` sealed as one chunk of the lent store: its extent and block.
  fn seal(&mut self, bytes: &[u8]) -> Result<Option<Placed>, slates_vfs::VfsError> {
    let mut open = match self.chunks.open(0, bytes.len(), Epoch(0), false) {
      Ok(open) => open,
      Err(_) => {
        // The arena holds no block for it: grown from its pool by the window, once; the lend's end extends the shard
        // budget by what was grown (`super::Lent`), as `Store::make_room` does.
        let _grown = self.chunks.arena_mut().grow(bytes.len());
        self.chunks.open(0, bytes.len(), Epoch(0), false)?
      }
    };
    if let Err(refused) = self.chunks.write_open(&mut open, 0, bytes) {
      let _given_back = self.chunks.release_open(open);
      return Err(refused);
    }
    let Some(extent) = self.chunks.seal(open, None)? else {
      return Ok(None);
    };
    let ExtentSrc::Chunk { chunk, .. } = extent.src else {
      return Ok(None);
    };
    let chunk = self
      .chunks
      .chunk(chunk)
      .ok_or(slates_vfs::VfsError::RecoveryIncomplete)?;
    Ok(Some(Placed {
      extent,
      region: chunk.block.region(),
      offset: u64::try_from(chunk.block.offset()).unwrap_or(u64::MAX),
      block: u64::try_from(chunk.block.len()).unwrap_or(u64::MAX),
      used: chunk.len,
      born: chunk.born.0,
    }))
  }

  /// The piece of every window kept and named, by handle (A-727 slice a3): what a shard image carries whole.
  pub(crate) fn pieces(&self) -> Vec<(u64, Vec<u8>)> {
    self
      .held
      .iter()
      .filter_map(|(window, kept)| Some((window.0, piece_of(kept.identity.as_ref()?, kept))))
      .collect()
  }

  /// The windows kept or let go since the last committed publication: each kept one's piece, none for one let go (A-727
  /// slice a3): what a delta carries.
  pub(crate) fn changes(&self) -> Vec<(u64, Option<Vec<u8>>)> {
    self
      .changed
      .iter()
      .map(|window| {
        let piece = self
          .held
          .get(window)
          .and_then(|kept| Some(piece_of(kept.identity.as_ref()?, kept)));
        (window.0, piece)
      })
      .collect()
  }

  /// Forgets the changes a committed publication carried.
  pub(crate) fn clear_changes(&mut self) {
    self.changed.clear();
  }

  /// The windows a shard image's pieces name, each chunk's block claimed in `chunks` and its chunk recorded over it
  /// (A-64, A-727 slice a3): what a recovery restores before the volumes and the records. A piece that cannot be
  /// restored is left out with its claim given back, and counted in the second value: its window is put again by the
  /// record replay that still holds its bytes.
  pub(crate) fn restore(
    page: usize,
    pieces: &[(u64, Vec<u8>)],
    chunks: &mut ChunkStore,
  ) -> (ChunkWindows, u64) {
    let mut windows = ChunkWindows::new(page);
    let mut refused = 0u64;
    for (handle, bytes) in pieces {
      match windows.restore_one(*handle, bytes, chunks) {
        Ok(()) => {}
        Err(_) => refused = refused.saturating_add(1),
      }
    }
    (windows, refused)
  }

  /// One window piece restored ([`ChunkWindows::restore`]).
  fn restore_one(
    &mut self,
    handle: u64,
    bytes: &[u8],
    chunks: &mut ChunkStore,
  ) -> Result<(), PieceRefused> {
    let (identity, len, placed) = read_piece(bytes)?;
    let window = StoredWindow(handle);
    if self.held.contains_key(&window) {
      return Err(PieceRefused::Malformed);
    }
    let placed = match placed {
      None if len == 0 => None,
      None => return Err(PieceRefused::Malformed),
      Some((region, offset, block, used, born)) => {
        if usize::try_from(used).ok() != Some(len) {
          return Err(PieceRefused::Malformed);
        }
        let claimed = chunks
          .claim_chunk_blocks(region, (offset, block), used)
          .map_err(|_| PieceRefused::Unclaimable)?;
        let adopted = chunks.adopt_chunk(
          Chunk {
            born: Epoch(born),
            len: used,
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
            let _given_back = chunks.give_back_chunk_blocks(claimed, used);
            return Err(PieceRefused::Unclaimable);
          }
        };
        Some(Placed {
          extent: Extent {
            off: 0,
            len: u64::from(used),
            src: ExtentSrc::Chunk { chunk, at: 0 },
          },
          region,
          offset,
          block,
          used,
          born,
        })
      }
    };
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
      self.seal(bytes).map_err(|_| ContentRefused::Storage)?
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
    let mut bytes = vec![0; usize::try_from(placed.extent.len).ok()?];
    self
      .chunks
      .read_extent_into(&placed.extent, 0, &mut bytes)
      .ok()?;
    Some(Cow::Owned(bytes))
  }

  fn release(&mut self, window: StoredWindow) {
    let Some(kept) = self.held.remove(&window) else {
      return;
    };
    self.changed.insert(window);
    if let Some(Placed {
      extent: Extent {
        src: ExtentSrc::Chunk { chunk, .. },
        ..
      },
      ..
    }) = kept.placed
    {
      if self.lent {
        // A chunk the store cannot free stays held until the arena goes; the content store's own counts are settled.
        // A block a committed image names is not reused before a newer one commits: the arena defers it (A-64).
        let _freed = self.chunks.free_chunk(chunk);
      } else {
        self.owed.push(chunk);
      }
    }
  }

  fn lend_in(&mut self, lent: &mut dyn Any) -> bool {
    match lent.downcast_mut::<ChunkStore>() {
      Some(chunks) if !self.lent => {
        std::mem::swap(&mut self.chunks, chunks);
        self.lent = true;
        for chunk in std::mem::take(&mut self.owed) {
          let _freed = self.chunks.free_chunk(chunk);
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

#[cfg(test)]
#[path = "arena_windows/tests.rs"]
mod tests;
