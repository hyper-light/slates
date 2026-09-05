//! Resumable transfer by the missing set (§2.6; Phase 7 task 5). The archive is content
//! addressed, so a transfer is deduplicated against what the receiver already holds: the receiver
//! reports the chunk identities it has, the sender computes the archive's chunks that are missing
//! from that set, and ships only those. The same mechanism is replication and clone-from-archive
//! — a receiver that already holds a previous version's chunks receives only the new ones.
//!
//! This module is pure: set arithmetic over chunk identities and a filter over the archive's
//! chunks, no I/O. It composes with [`crate::restore`]: the receiver combines its held chunks with
//! the shipped chunks and restores the manifest.

use std::collections::BTreeSet;

use crate::archive::Archive;
use crate::format::Chunk;

/// The archive's distinct chunk identities that are not in `held`, sorted — the set the sender must
/// ship so the receiver holds every chunk the manifest references. Deterministic (sorted), so a
/// resumed transfer computes the same set.
pub fn missing_set(archive: &Archive, held: &BTreeSet<[u8; 32]>) -> Vec<[u8; 32]> {
  let mut missing: BTreeSet<[u8; 32]> = BTreeSet::new();
  for chunk in &archive.chunks {
    if !held.contains(&chunk.identity) {
      missing.insert(chunk.identity);
    }
  }
  missing.into_iter().collect()
}

/// The archive's chunks whose identity is in `wanted`, each at most once — what the sender packages
/// for a transfer. Order follows the archive's chunk order.
pub fn chunks_for(archive: &Archive, wanted: &BTreeSet<[u8; 32]>) -> Vec<Chunk> {
  let mut taken: BTreeSet<[u8; 32]> = BTreeSet::new();
  let mut out = Vec::new();
  for chunk in &archive.chunks {
    if wanted.contains(&chunk.identity) && taken.insert(chunk.identity) {
      out.push(chunk.clone());
    }
  }
  out
}
