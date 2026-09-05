//! The content-addressed chunk store with deduplication (Phase 7 task 1; D-6, D-17). Chunks are
//! addressed by the BLAKE3 of their raw bytes, so two identical chunks — the same `target/` output
//! built by two clones, say — are one stored copy referenced twice. The store keeps each distinct
//! chunk once with a reference count, and reports the accounting the design's worked example
//! names: `unique_bytes` (the stored bytes of the distinct chunks) shrinks as duplicates fold in,
//! while `referenced_bytes` (what the references would occupy undeduplicated) is unchanged.
//!
//! This is the pure core of the runtime's content index. In a fleet the index is partitioned per
//! shard by hash prefix and the identity pass hashes sealed chunks in the background on idle time
//! (Phase 7 task 1, owed); the data structure and its accounting are the same. This module is
//! pure: a map and counters, no I/O, no clock, no randomness.

use std::collections::BTreeMap;

use crate::format::Chunk;

/// One distinct chunk and how many references point at it.
#[derive(Clone, Debug)]
struct Entry {
  chunk: Chunk,
  references: u64,
}

/// A content-addressed store: distinct chunks by identity, each with a reference count.
#[derive(Clone, Debug, Default)]
pub struct ContentStore {
  entries: BTreeMap<[u8; 32], Entry>,
}

impl ContentStore {
  /// An empty store.
  pub fn new() -> ContentStore {
    ContentStore {
      entries: BTreeMap::new(),
    }
  }

  /// Adds a reference to `chunk`, deduplicating by identity: if the identity is already present the
  /// reference count rises and the stored bytes are unchanged; otherwise the chunk is stored.
  /// Returns the chunk's identity (its content address).
  pub fn insert(&mut self, chunk: Chunk) -> [u8; 32] {
    let identity = chunk.identity;
    self
      .entries
      .entry(identity)
      .and_modify(|entry| entry.references = entry.references.saturating_add(1))
      .or_insert(Entry {
        chunk,
        references: 1,
      });
    identity
  }

  /// The chunk with `identity`, if the store holds it.
  pub fn get(&self, identity: &[u8; 32]) -> Option<&Chunk> {
    self.entries.get(identity).map(|entry| &entry.chunk)
  }

  /// Whether the store holds a chunk with `identity`.
  pub fn contains(&self, identity: &[u8; 32]) -> bool {
    self.entries.contains_key(identity)
  }

  /// Drops one reference to `identity`; when the last reference goes the chunk is evicted.
  /// Returns whether the chunk was evicted. Releasing an absent identity does nothing.
  pub fn release(&mut self, identity: &[u8; 32]) -> bool {
    let Some(entry) = self.entries.get_mut(identity) else {
      return false;
    };
    entry.references = entry.references.saturating_sub(1);
    if entry.references == 0 {
      self.entries.remove(identity);
      return true;
    }
    false
  }

  /// The number of references to `identity` (zero when absent).
  pub fn references(&self, identity: &[u8; 32]) -> u64 {
    self
      .entries
      .get(identity)
      .map_or(0, |entry| entry.references)
  }

  /// The number of distinct chunks stored.
  pub fn unique_count(&self) -> usize {
    self.entries.len()
  }

  /// The stored bytes of the distinct chunks (what deduplication actually occupies).
  pub fn unique_bytes(&self) -> u64 {
    self
      .entries
      .values()
      .map(|entry| entry.chunk.stored_len)
      .sum()
  }

  /// The raw bytes the references would occupy without deduplication (each reference counts its
  /// chunk's raw length).
  pub fn referenced_bytes(&self) -> u64 {
    self
      .entries
      .values()
      .map(|entry| entry.chunk.raw_len.saturating_mul(entry.references))
      .sum()
  }

  /// The total number of references across all chunks.
  pub fn total_references(&self) -> u64 {
    self.entries.values().map(|entry| entry.references).sum()
  }
}
