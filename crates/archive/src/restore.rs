//! Restore (§2.6; Phase 7 task 5). Reconstruct a volume's files from an archive: walk the
//! manifest tree, and for each file resolve its extent list against the archive's chunks — a
//! normal extent reads its bytes from the named chunk (decoded and identity-verified), a
//! zero-chunk extent is a hole that reads as zeros. A file whose extent names a chunk the archive
//! does not hold is a typed refusal, and every chunk is verified before its bytes are used, so a
//! corrupt chunk is caught and named (AC-7.3). Each named node's metadata (mode, times, size,
//! nlink, owner) is surfaced by path alongside the files and directories, and each node's extended
//! attribute values (the root's included) are reconstructed from their extents exactly as file bytes
//! are (format minor 3, AUD-29-56), for a takeover successor or a granted landing to apply; restore
//! itself reconstructs only the in-memory tree.
//!
//! **Admission before allocation (AUD-29-13).** Restore first plans the whole tree without touching any
//! chunk: each file's length is the length its extents tile (checked, `manifest::tiled_length`), each
//! name is one valid component and each path is restored once. The bytes it needs — every file's and
//! every attribute value's length, holes included, and one chunk's decode space — are compared with the caller's budget, and an
//! over-budget restore is refused typed before a byte is allocated; the allocation itself is fallible.
//! Then every chunk an extent names is decoded **once**, and its bytes are copied into every extent
//! that names it — a chunk referenced a thousand times costs one decode, and the transient is one chunk,
//! never the sum. Until 2026-09-30 restore decoded the chunk again for every extent, expanded holes and
//! appended extents without a budget, and ignored every extent's file offset (AUD-29-14).
//!
//! This is the eager, whole-tree restore: its output is dense (a hole is zeros in the file's bytes).
//! Keeping a sparse file sparse through restore is AUD-29-57. Lazy restore — attaching after a
//! metadata-only pass and decompressing each chunk on first read (AC-7.4) — is the runtime's job on top
//! of this and is owed. This module is pure: it reads the parsed archive's byte buffers, no I/O.

use std::collections::{BTreeMap, BTreeSet};

use crate::archive::Archive;
use crate::format::{ArchiveError, Chunk};
use crate::manifest::{Extent, Node, NodeMeta, Xattr, tiled_length, valid_component};

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
  /// The root directory's own metadata (its mode, owner and times), from the archive's head.
  pub root: NodeMeta,
  /// Each node's extended attribute values by path (the root under the empty path) and name: only nodes
  /// that carry attributes appear (format minor 3).
  pub xattrs: BTreeMap<String, BTreeMap<Vec<u8>, Vec<u8>>>,
  /// Chunks decoded to restore the tree — each at most once, however many extents name it (the
  /// non-vacuity counter of the decode-once path).
  pub chunks_decoded: u64,
}

/// Where one planned body restores to: a file's bytes, or one extended attribute's value on a node.
enum Target {
  File(String),
  Xattr { path: String, name: Vec<u8> },
}

/// One body of the plan — a file or an attribute value: where it restores to, its extents and the length
/// they tile.
struct PlannedBody<'tree> {
  target: Target,
  extents: &'tree [Extent],
  len: u64,
}

/// The tree planned without reading a chunk: its bodies, directories and metadata.
#[derive(Default)]
struct Plan<'tree> {
  bodies: Vec<PlannedBody<'tree>>,
  directories: BTreeSet<String>,
  metadata: BTreeMap<String, NodeMeta>,
}

impl<'tree> Plan<'tree> {
  /// Plans the attribute values of the node at `path` (names are already checked by the manifest reader).
  fn xattrs_of(&mut self, path: &str, xattrs: &'tree [Xattr]) -> Result<(), ArchiveError> {
    for xattr in xattrs {
      let len = tiled_length(&xattr.extents).map_err(|_| ArchiveError::BadExtents)?;
      self.bodies.push(PlannedBody {
        target: Target::Xattr {
          path: path.to_owned(),
          name: xattr.name.clone(),
        },
        extents: &xattr.extents,
        len,
      });
    }
    Ok(())
  }
}

/// Plans the tree at `root` under the root's own metadata, iteratively (no recursion, whatever the depth):
/// each file's and attribute value's tiled length, each name a valid component, each path once, and each
/// named file's recorded size its tiled length.
fn plan<'tree>(root_meta: &'tree NodeMeta, root: &'tree Node) -> Result<Plan<'tree>, ArchiveError> {
  let mut plan = Plan::default();
  plan.xattrs_of("", &root_meta.xattrs)?;
  let mut pending: Vec<(String, &Node)> = vec![(String::new(), root)];
  while let Some((path, node)) = pending.pop() {
    match node {
      Node::File(extents) => {
        let len = tiled_length(extents).map_err(|_| ArchiveError::BadExtents)?;
        plan.bodies.push(PlannedBody {
          target: Target::File(path),
          extents,
          len,
        });
      }
      Node::Directory(entries) => {
        if !path.is_empty() {
          plan.directories.insert(path.clone());
        }
        for entry in entries {
          if !valid_component(&entry.name) {
            return Err(ArchiveError::BadName);
          }
          let child = if path.is_empty() {
            entry.name.clone()
          } else {
            format!("{path}/{}", entry.name)
          };
          if let Node::File(extents) = &entry.node
            && tiled_length(extents).ok() != Some(entry.meta.size)
          {
            return Err(ArchiveError::BadExtents);
          }
          if plan
            .metadata
            .insert(child.clone(), entry.meta.clone())
            .is_some()
          {
            return Err(ArchiveError::DuplicatePath);
          }
          plan.xattrs_of(&child, &entry.meta.xattrs)?;
          pending.push((child, &entry.node));
        }
      }
    }
  }
  Ok(plan)
}

/// The zero chunk identity: an extent naming it is a hole.
const HOLE: [u8; 32] = [0u8; 32];

/// The bytes restoring `plan` needs from `chunks`: every body's length (files and attribute values), and
/// the largest chunk any extent names (the one decode held at a time). `None` past `u64`.
fn needed_bytes(plan: &Plan<'_>, chunks: &BTreeMap<[u8; 32], &Chunk>) -> Option<u64> {
  let mut largest_chunk = 0u64;
  let mut total = 0u64;
  for body in &plan.bodies {
    total = total.checked_add(body.len)?;
    for extent in body.extents.iter().filter(|extent| extent.chunk != HOLE) {
      if let Some(chunk) = chunks.get(&extent.chunk) {
        largest_chunk = largest_chunk.max(chunk.raw_len);
      }
    }
  }
  total.checked_add(largest_chunk)
}

/// A zero-filled buffer of `len` bytes, allocated fallibly: a refusal, never an abort.
fn zeroed(len: u64, needed: u64, budget: u64) -> Result<Vec<u8>, ArchiveError> {
  let over = ArchiveError::OverBudget { needed, budget };
  let len = usize::try_from(len).map_err(|_| over)?;
  let mut bytes = Vec::new();
  bytes.try_reserve_exact(len).map_err(|_| over)?;
  bytes.resize(len, 0);
  Ok(bytes)
}

/// Copies `content`, a decoded chunk, into `out` for `extent`: the extent's slice of the chunk to its
/// file offset.
fn fill_extent(out: &mut [u8], extent: &Extent, content: &[u8]) -> Result<(), ArchiveError> {
  let len = usize::try_from(extent.len).map_err(|_| ArchiveError::BadLength)?;
  let from = usize::try_from(extent.chunk_offset).map_err(|_| ArchiveError::BadLength)?;
  let at = usize::try_from(extent.offset).map_err(|_| ArchiveError::BadLength)?;
  let source = from
    .checked_add(len)
    .and_then(|end| content.get(from..end))
    .ok_or(ArchiveError::BadLength)?;
  let target = at
    .checked_add(len)
    .and_then(|end| out.get_mut(at..end))
    .ok_or(ArchiveError::BadLength)?;
  target.copy_from_slice(source);
  Ok(())
}

/// Restores the whole volume the archive holds: every file's bytes, every directory and every extended
/// attribute value, planned and admitted against `budget` bytes before anything is allocated (see the
/// module doc), each chunk decoded once. A tree that does not plan, a missing or corrupt chunk, or a
/// restore past the budget is a typed refusal.
pub fn restore(archive: &Archive, budget: u64) -> Result<Restored, ArchiveError> {
  let plan = plan(&archive.root_meta, &archive.manifest)?;
  let chunks: BTreeMap<[u8; 32], &Chunk> = archive
    .chunks
    .iter()
    .rev()
    .map(|chunk| (chunk.identity, chunk))
    .collect();
  let needed = needed_bytes(&plan, &chunks).unwrap_or(u64::MAX);
  if needed > budget {
    return Err(ArchiveError::OverBudget { needed, budget });
  }
  // Which extents name each chunk, so each chunk is decoded once for all of them.
  let mut references: BTreeMap<[u8; 32], Vec<(usize, &Extent)>> = BTreeMap::new();
  for (body_index, body) in plan.bodies.iter().enumerate() {
    for extent in body.extents.iter().filter(|extent| extent.chunk != HOLE) {
      references
        .entry(extent.chunk)
        .or_default()
        .push((body_index, extent));
    }
  }
  let mut outputs = plan
    .bodies
    .iter()
    .map(|body| zeroed(body.len, needed, budget))
    .collect::<Result<Vec<_>, _>>()?;
  let mut chunks_decoded = 0u64;
  for (identity, extents) in &references {
    let chunk = chunks.get(identity).ok_or(ArchiveError::MissingChunk)?;
    let content = Archive::content(chunk)?;
    chunks_decoded = chunks_decoded.saturating_add(1);
    for (body_index, extent) in extents {
      let out = outputs
        .get_mut(*body_index)
        .ok_or(ArchiveError::BadLength)?;
      fill_extent(out, extent, &content)?;
    }
  }
  let mut files = BTreeMap::new();
  let mut xattrs: BTreeMap<String, BTreeMap<Vec<u8>, Vec<u8>>> = BTreeMap::new();
  for (body, bytes) in plan.bodies.into_iter().zip(outputs) {
    match body.target {
      Target::File(path) => {
        files.insert(path, bytes);
      }
      Target::Xattr { path, name } => {
        xattrs.entry(path).or_default().insert(name, bytes);
      }
    }
  }
  Ok(Restored {
    files,
    directories: plan.directories,
    metadata: plan.metadata,
    root: archive.root_meta.clone(),
    xattrs,
    chunks_decoded,
  })
}
