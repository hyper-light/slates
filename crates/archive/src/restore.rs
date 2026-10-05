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
//! **Sparse files stay sparse (AUD-29-57).** A file restores as its length and its data pieces — each
//! non-hole extent's bytes at its offset — so a hole costs nothing: the budget admits a file's data bytes,
//! not its logical length, and a caller writes only the pieces. Until 2026-10-01 every file restored as one
//! dense buffer of its logical length. An extended attribute value restores dense (its length is admitted
//! like any other byte). Lazy restore — attaching after a
//! metadata-only pass and decompressing each chunk on first read (AC-7.4) — is the runtime's job on top
//! of this and is owed. This module is pure: it reads the parsed archive's byte buffers, no I/O.

use std::collections::{BTreeMap, BTreeSet};

use crate::archive::Archive;
use crate::format::{ArchiveError, Chunk};
use crate::manifest::{Extent, Node, NodeMeta, Xattr, tiled_length, valid_component};

/// A restored file, sparse (AUD-29-57): its length, and its data pieces — each a non-hole extent's bytes at
/// its offset, ascending, contiguous pieces merged. Everything else up to the length is a hole.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestoredFile {
  /// The file's length.
  pub len: u64,
  /// The data pieces: `(offset, bytes)`, ascending and disjoint.
  pub pieces: Vec<(u64, Vec<u8>)>,
}

impl RestoredFile {
  /// The bytes the pieces hold.
  pub fn data_bytes(&self) -> u64 {
    self
      .pieces
      .iter()
      .map(|(_, bytes)| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
      .fold(0, u64::saturating_add)
  }

  /// The file as one dense buffer, holes as zeros — for a small body whose bytes are its meaning (a
  /// symlink's target) and for tests; `None` when the length does not fit in memory's address space.
  pub fn dense(&self) -> Option<Vec<u8>> {
    let mut out = vec![0u8; usize::try_from(self.len).ok()?];
    for (offset, bytes) in &self.pieces {
      let at = usize::try_from(*offset).ok()?;
      out
        .get_mut(at..at.checked_add(bytes.len())?)?
        .copy_from_slice(bytes);
    }
    Some(out)
  }
}

/// A restored volume: the files by path (sparse), the directories present, and each named node's
/// metadata. The metadata is what a granted landing applies to the host path (mode, times); restore
/// itself only reconstructs the in-memory tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Restored {
  /// Each file's path and its length and data pieces.
  pub files: BTreeMap<String, RestoredFile>,
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

/// Whether `body` restores sparse (a file: its data pieces only) or dense (an attribute value).
fn sparse(body: &PlannedBody<'_>) -> bool {
  matches!(body.target, Target::File(_))
}

/// The bytes one body takes restored: a file its data extents' bytes, a value its whole length.
fn body_bytes(body: &PlannedBody<'_>) -> Option<u64> {
  if !sparse(body) {
    return Some(body.len);
  }
  body
    .extents
    .iter()
    .filter(|extent| extent.chunk != HOLE)
    .try_fold(0u64, |total, extent| total.checked_add(extent.len))
}

/// The bytes restoring `plan` needs from `chunks`: every file's data bytes and every attribute value's
/// length, and the largest chunk any extent names (the one decode held at a time). `None` past `u64`.
fn needed_bytes(plan: &Plan<'_>, chunks: &BTreeMap<[u8; 32], &Chunk>) -> Option<u64> {
  let mut largest_chunk = 0u64;
  let mut total = 0u64;
  for body in &plan.bodies {
    total = total.checked_add(body_bytes(body)?)?;
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

/// Copies `content`, a decoded chunk, into `out` for `extent`: the extent's slice of the chunk at `at` in
/// `out` (the extent's file offset for a dense buffer, zero for a piece of its own).
fn fill_extent(
  out: &mut [u8],
  extent: &Extent,
  content: &[u8],
  at: u64,
) -> Result<(), ArchiveError> {
  let len = usize::try_from(extent.len).map_err(|_| ArchiveError::BadLength)?;
  let from = usize::try_from(extent.chunk_offset).map_err(|_| ArchiveError::BadLength)?;
  let at = usize::try_from(at).map_err(|_| ArchiveError::BadLength)?;
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

/// Pieces at ascending offsets with contiguous ones merged into one. A merge whose growth cannot be reserved
/// keeps the pieces apart — still correct, never an abort.
fn merged(pieces: impl Iterator<Item = (u64, Vec<u8>)>) -> Vec<(u64, Vec<u8>)> {
  let mut out: Vec<(u64, Vec<u8>)> = Vec::new();
  for (offset, bytes) in pieces {
    if let Some((start, previous)) = out.last_mut()
      && start.checked_add(u64::try_from(previous.len()).unwrap_or(u64::MAX)) == Some(offset)
      && previous.try_reserve_exact(bytes.len()).is_ok()
    {
      previous.extend_from_slice(&bytes);
      continue;
    }
    out.push((offset, bytes));
  }
  out
}

/// The bytes [`restore`] admits the archive's restore against, planned without decoding a chunk: what a caller must
/// have room for before it restores (A-98: a shard growing its arena from the pool by exactly this). A tree that does
/// not plan is refused as [`restore`] refuses it.
pub fn restore_needed(archive: &Archive) -> Result<u64, ArchiveError> {
  let plan = plan(&archive.root_meta, &archive.manifest)?;
  let chunks: BTreeMap<[u8; 32], &Chunk> = archive
    .chunks
    .iter()
    .rev()
    .map(|chunk| (chunk.identity, chunk))
    .collect();
  Ok(needed_bytes(&plan, &chunks).unwrap_or(u64::MAX))
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
  // Each body's buffers: a dense body one buffer of its length; a sparse file one buffer per data extent.
  let mut outputs: Vec<Vec<Vec<u8>>> = plan
    .bodies
    .iter()
    .map(|body| {
      if sparse(body) {
        body
          .extents
          .iter()
          .filter(|extent| extent.chunk != HOLE)
          .map(|extent| zeroed(extent.len, needed, budget))
          .collect()
      } else {
        zeroed(body.len, needed, budget).map(|bytes| vec![bytes])
      }
    })
    .collect::<Result<_, _>>()?;
  // Which extents name each chunk, so each chunk is decoded once for all of them: the body, the buffer and
  // where in it the extent lands.
  let mut references: BTreeMap<[u8; 32], Vec<(usize, usize, &Extent)>> = BTreeMap::new();
  for (body_index, body) in plan.bodies.iter().enumerate() {
    let mut piece = 0usize;
    for extent in body.extents.iter().filter(|extent| extent.chunk != HOLE) {
      let buffer = if sparse(body) { piece } else { 0 };
      references
        .entry(extent.chunk)
        .or_default()
        .push((body_index, buffer, extent));
      piece = piece.saturating_add(1);
    }
  }
  let mut chunks_decoded = 0u64;
  for (identity, extents) in &references {
    let chunk = chunks.get(identity).ok_or(ArchiveError::MissingChunk)?;
    let content = Archive::content(chunk)?;
    chunks_decoded = chunks_decoded.saturating_add(1);
    for (body_index, buffer, extent) in extents {
      let body = plan
        .bodies
        .get(*body_index)
        .ok_or(ArchiveError::BadLength)?;
      let out = outputs
        .get_mut(*body_index)
        .and_then(|buffers| buffers.get_mut(*buffer))
        .ok_or(ArchiveError::BadLength)?;
      // A sparse piece starts at its extent; a dense body's buffer at the body's offset zero.
      let at = if sparse(body) { 0 } else { extent.offset };
      fill_extent(out, extent, &content, at)?;
    }
  }
  let mut files = BTreeMap::new();
  let mut xattrs: BTreeMap<String, BTreeMap<Vec<u8>, Vec<u8>>> = BTreeMap::new();
  for (body, buffers) in plan.bodies.into_iter().zip(outputs) {
    match body.target {
      Target::File(path) => {
        let offsets = body
          .extents
          .iter()
          .filter(|extent| extent.chunk != HOLE)
          .map(|extent| extent.offset);
        files.insert(
          path,
          RestoredFile {
            len: body.len,
            pieces: merged(offsets.zip(buffers)),
          },
        );
      }
      Target::Xattr { path, name } => {
        let value = buffers.into_iter().next().unwrap_or_default();
        xattrs.entry(path).or_default().insert(name, value);
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
