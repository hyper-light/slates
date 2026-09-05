//! Restore (§2.6; Phase 7 task 5). Reconstruct a volume's files from an archive: walk the
//! manifest tree, and for each file resolve its extent list against the archive's chunks — a
//! normal extent reads its bytes from the named chunk (decoded and identity-verified), a
//! zero-chunk extent is a hole that reads as zeros. A file whose extent names a chunk the archive
//! does not hold is a typed refusal, and every chunk is verified before its bytes are used, so a
//! corrupt chunk is caught and named (AC-7.3). Each named node's metadata (mode, times, size,
//! nlink, xattr flags) is surfaced by path alongside the files and directories, for a granted
//! landing to apply to the host path; restore itself reconstructs only the in-memory tree.
//!
//! This is the eager, whole-tree restore. Lazy restore — attaching after a metadata-only pass and
//! decompressing each chunk on first read (AC-7.4) — is the runtime's job on top of this and is
//! owed. This module is pure: it reads the parsed archive's byte buffers, no I/O.

use std::collections::{BTreeMap, BTreeSet};

use crate::archive::Archive;
use crate::format::{ArchiveError, Chunk};
use crate::manifest::{Extent, Node, NodeMeta};

/// A restored volume: the files' contents by path, the directories present, and each named node's
/// metadata. The metadata is what a granted landing applies to the host path (mode, times); restore
/// itself only reconstructs the in-memory tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Restored {
  /// Each file's path and its reconstructed bytes.
  pub files: BTreeMap<String, Vec<u8>>,
  /// The directory paths.
  pub directories: BTreeSet<String>,
  /// Each named node's metadata, by path (the root has no naming entry, so no entry for it).
  pub metadata: BTreeMap<String, NodeMeta>,
}

/// A lookup from chunk identity to the chunk, built once from the archive's chunks so restore does
/// not rescan for each extent.
fn chunk_index(archive: &Archive) -> BTreeMap<[u8; 32], &Chunk> {
  archive
    .chunks
    .iter()
    .map(|chunk| (chunk.identity, chunk))
    .collect()
}

/// Reconstructs one file's bytes from its extent list, resolving each extent against `index`. A
/// zero-chunk extent is a hole (zeros); any other extent reads the decoded, identity-verified
/// chunk at its sub-chunk offset.
fn restore_file(
  index: &BTreeMap<[u8; 32], &Chunk>,
  extents: &[Extent],
) -> Result<Vec<u8>, ArchiveError> {
  let mut out = Vec::new();
  for extent in extents {
    let len = usize::try_from(extent.len).unwrap_or(usize::MAX);
    if extent.chunk == [0u8; 32] {
      // A hole: `len` zero bytes, no chunk needed.
      out.resize(out.len().saturating_add(len), 0);
      continue;
    }
    let chunk = index.get(&extent.chunk).ok_or(ArchiveError::MissingChunk)?;
    let content = Archive::content(chunk)?;
    let start = usize::try_from(extent.chunk_offset).unwrap_or(usize::MAX);
    let end = start.checked_add(len).ok_or(ArchiveError::BadLength)?;
    let slice = content.get(start..end).ok_or(ArchiveError::BadLength)?;
    out.extend_from_slice(slice);
  }
  Ok(out)
}

/// Walks the manifest subtree at `node` under `prefix`, restoring files and recording directories.
fn walk(
  index: &BTreeMap<[u8; 32], &Chunk>,
  prefix: &str,
  node: &Node,
  restored: &mut Restored,
) -> Result<(), ArchiveError> {
  match node {
    Node::File(extents) => {
      restored
        .files
        .insert(prefix.to_owned(), restore_file(index, extents)?);
    }
    Node::Directory(entries) => {
      if !prefix.is_empty() {
        restored.directories.insert(prefix.to_owned());
      }
      for entry in entries {
        let child = if prefix.is_empty() {
          entry.name.clone()
        } else {
          format!("{prefix}/{}", entry.name)
        };
        restored.metadata.insert(child.clone(), entry.meta);
        walk(index, &child, &entry.node, restored)?;
      }
    }
  }
  Ok(())
}

/// Restores the whole volume the archive holds: every file's bytes and every directory, resolved
/// from the manifest tree and the chunks. A missing or corrupt chunk is a typed refusal.
pub fn restore(archive: &Archive) -> Result<Restored, ArchiveError> {
  let index = chunk_index(archive);
  let mut restored = Restored::default();
  walk(&index, "", &archive.manifest, &mut restored)?;
  Ok(restored)
}
