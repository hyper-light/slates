//! The repository content store's windows kept in the shard's repository arena (A-727 slice a2; §4.17.1 "Objects" and
//! "Durability and residency"; A-64; D-6).
//!
//! **Why.** A repository's bytes were held in the process's memory by `slates_vcs::ContentStore` and made durable only
//! by the partition's records, so the partition's snapshot carried every byte a repository held and met its slot's
//! region (the 747 MB wall: BENCHMARKS.md and A-727). Kept here, a window is a sealed chunk of a chunk store whose
//! arena's extents are claimed from the anchor's content pool, so its bytes live in the anchor's content object, which
//! outlives the daemon, as a volume's chunks do (A-64).
//!
//! **What it is.** A [`slates_vcs::WindowStorage`]: each window put is opened, written and sealed as one chunk of its
//! own chunk store ([`ChunkStore`]) over an arena separate from the volumes' (its extents claimed under the repository
//! owner range, `crate::content_pool::ContentPool::open_repository`), and named by a handle the content store keeps. A
//! read copies the chunk out (`ChunkStore::read_extent_into`), so the content store answers it as an owned `Cow`; a
//! release frees the chunk. An empty window takes no chunk.
//!
//! **Growth.** The chunk store takes blocks from its arena; an arena with no room grows from the pool by the window's
//! length and the put is tried once more. What the content store may hold is bounded by its own budget, which its owner
//! sets from the shard's admittable bytes before each step (`super::lend`), so the arena grows only within what the
//! shard admitted.
//!
//! **Owed** (A-727 slice a3). The chunks are not yet named by a shard image, so a restarted daemon finds this arena's
//! extents held but no chunk named, and remakes the store from the partition's records, which still hold the bytes;
//! the image's table of these chunks and its recovery before the records come next, then the records naming
//! identities only. The shard's cipher does not yet seal these chunks (A-99): owed with the image.

use std::borrow::Cow;
use std::collections::BTreeMap;

use slates_mem::arena::ChunkArena;
use slates_vcs::{ContentRefused, StoredWindow, WindowStorage};
use slates_vfs::content::{ChunkStore, Extent, ExtentSrc};
use slates_vfs::ids::Epoch;

/// A window's place: its sealed chunk's extent, or none for an empty window.
type Place = Option<Extent>;

/// The windows kept in the shard's repository arena (module docs).
pub(crate) struct ArenaWindows {
  /// The chunk store over the repository arena.
  chunks: ChunkStore,
  /// Each window kept, by handle.
  held: BTreeMap<StoredWindow, Place>,
  /// The next handle.
  next: u64,
  /// Puts that met a full arena and grew it from the pool: the non-vacuity count of growth.
  grown: u64,
}

impl std::fmt::Debug for ArenaWindows {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("ArenaWindows")
      .field("held", &self.held.len())
      .field("grown", &self.grown)
      .finish()
  }
}

impl ArenaWindows {
  /// The windows kept over `arena` (the shard's repository arena), its chunk records within `max_chunks`, at `page`.
  pub(crate) fn new(arena: ChunkArena, page: usize, max_chunks: usize) -> ArenaWindows {
    ArenaWindows {
      chunks: ChunkStore::new(arena, page, max_chunks),
      held: BTreeMap::new(),
      next: 0,
      grown: 0,
    }
  }

  /// `bytes` sealed as one chunk: its extent.
  fn seal(&mut self, bytes: &[u8]) -> Result<Option<Extent>, slates_vfs::VfsError> {
    let mut open = self.chunks.open(0, bytes.len(), Epoch(0), false)?;
    if let Err(refused) = self.chunks.write_open(&mut open, 0, bytes) {
      let _given_back = self.chunks.release_open(open);
      return Err(refused);
    }
    self.chunks.seal(open, None)
  }

  /// How many puts grew the arena (tests' non-vacuity count).
  #[cfg(test)]
  pub(crate) fn grown(&self) -> u64 {
    self.grown
  }
}

impl WindowStorage for ArenaWindows {
  fn put(&mut self, bytes: &[u8]) -> Result<StoredWindow, ContentRefused> {
    let place = if bytes.is_empty() {
      None
    } else {
      match self.seal(bytes) {
        Ok(place) => place,
        Err(_) => {
          // A full arena grows from the pool by the window's length, and the put is tried once more.
          self.chunks.arena_mut().grow(bytes.len());
          self.grown = self.grown.saturating_add(1);
          self.seal(bytes).map_err(|_| ContentRefused::Storage)?
        }
      }
    };
    let window = StoredWindow(self.next);
    self.next = self
      .next
      .checked_add(1)
      .ok_or(ContentRefused::CountExhausted)?;
    self.held.insert(window, place);
    Ok(window)
  }

  fn read(&self, window: StoredWindow) -> Option<Cow<'_, [u8]>> {
    let place = self.held.get(&window)?;
    let Some(extent) = place else {
      return Some(Cow::Borrowed(&[]));
    };
    let mut bytes = vec![0; usize::try_from(extent.len).ok()?];
    self.chunks.read_extent_into(extent, 0, &mut bytes).ok()?;
    Some(Cow::Owned(bytes))
  }

  fn release(&mut self, window: StoredWindow) {
    if let Some(Some(Extent {
      src: ExtentSrc::Chunk { chunk, .. },
      ..
    })) = self.held.remove(&window)
    {
      // A chunk the store cannot free stays held until the arena goes; the content store's own counts are settled.
      let _freed = self.chunks.free_chunk(chunk);
    }
  }
}

#[cfg(test)]
#[path = "arena_windows/tests.rs"]
mod tests;
