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
//! **Owed** (A-727 slice a3). The chunks are not yet named by the shard image, so a restarted daemon finds them
//! unreferenced, frees them, and remakes the store from the partition's records, which still hold the bytes. The
//! image's table of these chunks and its recovery before the records come next, then the records naming identities
//! only. The shard's cipher does not yet seal these chunks (A-99): owed with the image.

use std::any::Any;
use std::borrow::Cow;
use std::collections::BTreeMap;

use slates_mem::Handle;
use slates_vcs::{ContentRefused, StoredWindow, WindowStorage};
use slates_vfs::content::{Chunk, ChunkStore, Extent, ExtentSrc};
use slates_vfs::ids::Epoch;

/// A window's place: its sealed chunk's extent, or none for an empty window.
type Place = Option<Extent>;

/// The windows kept as chunks of the shard's lent chunk store (module docs).
pub(crate) struct ChunkWindows {
  /// The shard's chunk store while it is lent; a placeholder otherwise.
  chunks: ChunkStore,
  /// Whether the shard's chunk store is lent now.
  lent: bool,
  /// Each window kept, by handle.
  held: BTreeMap<StoredWindow, Place>,
  /// The next handle.
  next: u64,
  /// Chunks of windows released outside a lend, freed when the store is next lent.
  owed: Vec<Handle<Chunk>>,
}

impl std::fmt::Debug for ChunkWindows {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("ChunkWindows")
      .field("held", &self.held.len())
      .field("lent", &self.lent)
      .finish()
  }
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
    }
  }

  /// `bytes` sealed as one chunk of the lent store: its extent.
  fn seal(&mut self, bytes: &[u8]) -> Result<Option<Extent>, slates_vfs::VfsError> {
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
    self.chunks.seal(open, None)
  }
}

impl WindowStorage for ChunkWindows {
  fn put(&mut self, bytes: &[u8]) -> Result<StoredWindow, ContentRefused> {
    let place = if bytes.is_empty() {
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
    self.held.insert(window, place);
    Ok(window)
  }

  fn read(&self, window: StoredWindow) -> Option<Cow<'_, [u8]>> {
    let place = self.held.get(&window)?;
    let Some(extent) = place else {
      return Some(Cow::Borrowed(&[]));
    };
    if !self.lent {
      return None;
    }
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
      if self.lent {
        // A chunk the store cannot free stays held until the arena goes; the content store's own counts are settled.
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
}

#[cfg(test)]
#[path = "arena_windows/tests.rs"]
mod tests;
