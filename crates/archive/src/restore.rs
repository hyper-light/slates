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
//!
//! **In steps (g251).** The plan and the restore are made a bounded piece at a time ([`Planning`], then [`Planned`] and
//! [`Restoring`]): a node or a directory's entry planned, a chunk indexed, an extent counted, a buffer allocated, an
//! extent's reference gathered, an extent filled (its chunk decoded with the first that names it), or a piece laid into
//! its file; each takes a `spent` question and goes on until it says the step's share is spent, a piece at least. A
//! shard restoring a taken-over volume gives its thread back between steps, where in one call the restore of the
//! 1.99 GB kernel base volume (g232) held its shard for seconds. [`restore`] and [`restore_needed`] are the steps run
//! to their ends, so one code path restores either way.

use std::collections::{BTreeMap, BTreeSet};

use crate::archive::Archive;
use crate::format::{ArchiveError, Chunk};
use crate::manifest::{Entry, Extent, Node, NodeMeta, Xattr, tiled_length, valid_component};

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

/// One piece of a tree still to plan ([`Planning`]): a node at its path, or a directory's entries from the `next` one.
enum Walk<'tree> {
  /// A node at its path.
  Node(String, &'tree Node),
  /// A directory's entries at its path, the `next` one still to plan.
  Entries {
    path: String,
    entries: &'tree [Entry],
    next: usize,
  },
}

/// Where a [`Planning`] stands.
enum PlanStage {
  /// Nothing planned yet: the root's attributes come first.
  Root,
  /// The tree walked a node, or a directory's entry, a piece.
  Walking,
  /// The archive's chunks indexed by identity, from the last to the first (the first of an identity named), `left` of
  /// them still to index.
  Indexing { left: usize },
  /// Each body's bytes counted, an extent a piece: the body and the extent the count stands at.
  Counting { body: usize, extent: usize },
}

/// A restore's plan made in steps (g251, CLAUDE.md "Bounded work everywhere"): the tree walked a node or an entry at a
/// time, each file's and attribute value's tiled length, each name a valid component, each path once, each named
/// file's recorded size its tiled length; then the chunks indexed a chunk at a time and the bytes the restore needs
/// counted an extent at a time. Nothing is decoded or allocated, so a caller that gives its thread back between steps
/// (a shard restoring a taken-over volume in turns of its clock) plans a tree of any size. Made in one call, a 97,254-
/// file kernel tree's plan was a walk of every node inside the one turn that made the volume (g232's report).
pub struct Planning<'tree> {
  archive: &'tree Archive,
  plan: Plan<'tree>,
  pending: Vec<Walk<'tree>>,
  chunks: BTreeMap<[u8; 32], &'tree Chunk>,
  stage: PlanStage,
  /// The bytes counted so far, `None` once they passed `u64` (the restore is then refused over any budget).
  total: Option<u64>,
  /// The largest chunk an extent counted so far names: the one decode held at a time.
  largest_chunk: u64,
}

impl<'tree> Planning<'tree> {
  /// A plan of `archive`'s tree, nothing planned yet.
  pub fn new(archive: &'tree Archive) -> Planning<'tree> {
    Planning {
      archive,
      plan: Plan::default(),
      pending: Vec::new(),
      chunks: BTreeMap::new(),
      stage: PlanStage::Root,
      total: Some(0),
      largest_chunk: 0,
    }
  }

  /// Plans until `spent` says the step's share is spent, a piece at least: `Some` with the planned tree once every piece
  /// is planned. A tree that does not plan is refused typed, as [`restore`] refuses it.
  pub fn step(&mut self, mut spent: impl FnMut() -> bool) -> Result<Option<Planned<'tree>>, ArchiveError> {
    loop {
      if self.piece()? {
        return Ok(Some(self.planned()));
      }
      if spent() {
        return Ok(None);
      }
    }
  }

  /// One piece of the plan; `true` once every piece is done.
  fn piece(&mut self) -> Result<bool, ArchiveError> {
    match self.stage {
      PlanStage::Root => {
        self.plan.xattrs_of("", &self.archive.root_meta.xattrs)?;
        self
          .pending
          .push(Walk::Node(String::new(), &self.archive.manifest));
        self.stage = PlanStage::Walking;
      }
      PlanStage::Walking => match self.pending.pop() {
        Some(walk) => self.walk(walk)?,
        None => {
          self.stage = PlanStage::Indexing {
            left: self.archive.chunks.len(),
          };
        }
      },
      PlanStage::Indexing { left } => match left.checked_sub(1) {
        Some(at) => {
          if let Some(chunk) = self.archive.chunks.get(at) {
            self.chunks.insert(chunk.identity, chunk);
          }
          self.stage = PlanStage::Indexing { left: at };
        }
        None => self.stage = PlanStage::Counting { body: 0, extent: 0 },
      },
      PlanStage::Counting { body, extent } => {
        let Some(planned) = self.plan.bodies.get(body) else {
          return Ok(true);
        };
        let Some(counted) = planned.extents.get(extent) else {
          self.stage = PlanStage::Counting {
            body: body.saturating_add(1),
            extent: 0,
          };
          return Ok(false);
        };
        // A dense body takes its whole length, once; a sparse file each data extent's.
        let bytes = match (sparse(planned), extent) {
          (false, 0) => Some(planned.len),
          (false, _) => Some(0),
          (true, _) if counted.chunk == HOLE => Some(0),
          (true, _) => Some(counted.len),
        };
        self.total = self
          .total
          .zip(bytes)
          .and_then(|(total, bytes)| total.checked_add(bytes));
        if counted.chunk != HOLE
          && let Some(chunk) = self.chunks.get(&counted.chunk)
        {
          self.largest_chunk = self.largest_chunk.max(chunk.raw_len);
        }
        self.stage = PlanStage::Counting {
          body,
          extent: extent.saturating_add(1),
        };
      }
    }
    Ok(false)
  }

  /// Plans one node, or one entry of a directory, pushing what is left of it back.
  fn walk(&mut self, walk: Walk<'tree>) -> Result<(), ArchiveError> {
    match walk {
      Walk::Node(path, Node::File(extents)) => {
        let len = tiled_length(extents).map_err(|_| ArchiveError::BadExtents)?;
        self.plan.bodies.push(PlannedBody {
          target: Target::File(path),
          extents,
          len,
        });
      }
      Walk::Node(path, Node::Directory(entries)) => {
        if !path.is_empty() {
          self.plan.directories.insert(path.clone());
        }
        self.pending.push(Walk::Entries {
          path,
          entries,
          next: 0,
        });
      }
      Walk::Entries {
        path,
        entries,
        next,
      } => {
        let Some(entry) = entries.get(next) else {
          return Ok(());
        };
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
        if self
          .plan
          .metadata
          .insert(child.clone(), entry.meta.clone())
          .is_some()
        {
          return Err(ArchiveError::DuplicatePath);
        }
        self.plan.xattrs_of(&child, &entry.meta.xattrs)?;
        self.pending.push(Walk::Entries {
          path,
          entries,
          next: next.saturating_add(1),
        });
        self.pending.push(Walk::Node(child, &entry.node));
      }
    }
    Ok(())
  }

  /// The plan made, taken: the bytes it needs every file's data bytes and every attribute value's length and the
  /// largest chunk any extent names, or `u64::MAX` past `u64`.
  fn planned(&mut self) -> Planned<'tree> {
    let needed = self
      .total
      .and_then(|total| total.checked_add(self.largest_chunk))
      .unwrap_or(u64::MAX);
    Planned {
      root: &self.archive.root_meta,
      plan: std::mem::take(&mut self.plan),
      chunks: std::mem::take(&mut self.chunks),
      needed,
    }
  }
}

/// A tree planned for its restore ([`Planning`]): what the restore needs, known before a byte is allocated.
pub struct Planned<'tree> {
  root: &'tree NodeMeta,
  plan: Plan<'tree>,
  chunks: BTreeMap<[u8; 32], &'tree Chunk>,
  needed: u64,
}

impl<'tree> Planned<'tree> {
  /// The bytes the restore needs: every file's data bytes and every attribute value's length, holes excluded from a
  /// file and included in a value, and one chunk's decode space ([`restore_needed`]).
  pub fn needed(&self) -> u64 {
    self.needed
  }

  /// The restore of the planned tree, admitted against `budget` bytes before anything is allocated: refused
  /// [`ArchiveError::OverBudget`] when it needs more.
  pub fn restoring(self, budget: u64) -> Result<Restoring<'tree>, ArchiveError> {
    if self.needed > budget {
      return Err(ArchiveError::OverBudget {
        needed: self.needed,
        budget,
      });
    }
    Ok(Restoring {
      root: self.root,
      bodies: self.plan.bodies,
      directories: self.plan.directories,
      metadata: self.plan.metadata,
      chunks: self.chunks,
      needed: self.needed,
      budget,
      outputs: Vec::new(),
      stage: RestoreStage::Allocating { body: 0, extent: 0 },
      chunks_decoded: 0,
      files: BTreeMap::new(),
      xattrs: BTreeMap::new(),
    })
  }
}

/// One extent's place in a restore: its body, the buffer of that body it fills, and the extent.
type Fill<'tree> = (usize, usize, &'tree Extent);

/// Where a [`Restoring`] stands.
enum RestoreStage<'tree> {
  /// Each body's buffers allocated, zeroed, a buffer a piece: a dense body one of its length, a sparse file one per
  /// data extent. The body and the extent the allocation stands at.
  Allocating { body: usize, extent: usize },
  /// Which extents name each chunk, gathered an extent a piece, so each chunk is decoded once for all of them.
  Referencing {
    body: usize,
    extent: usize,
    piece: usize,
    references: BTreeMap<[u8; 32], Vec<Fill<'tree>>>,
  },
  /// The chunks decoded, each once, and copied into every extent that names it, an extent a piece; `current` the
  /// chunk being copied out and the extents still to fill from it.
  Decoding {
    references: std::collections::btree_map::IntoIter<[u8; 32], Vec<Fill<'tree>>>,
    current: Option<(Vec<u8>, std::vec::IntoIter<Fill<'tree>>)>,
  },
  /// The bodies laid into files and attribute values, a piece a step: `current` the file being laid, its pieces
  /// still to merge and those merged.
  Assembling {
    bodies: std::iter::Zip<std::vec::IntoIter<PlannedBody<'tree>>, std::vec::IntoIter<Vec<Vec<u8>>>>,
    current: Option<Assembling>,
  },
}

/// A restored file being laid out of its pieces.
struct Assembling {
  path: String,
  len: u64,
  pieces: std::vec::IntoIter<(u64, Vec<u8>)>,
  merged: Vec<(u64, Vec<u8>)>,
}

/// A restore made in steps (g251, CLAUDE.md "Bounded work everywhere"): what [`restore`] does, a bounded piece a step,
/// so a caller that gives its thread back between steps restores a volume of any size. A piece is one buffer allocated,
/// one extent's reference gathered, one extent filled from its chunk (the chunk decoded, once, with the first), or one
/// data piece laid into its file; none is larger than one chunk's bytes. Made in one call, a taken-over volume's
/// restore decoded and copied all of it inside the one turn that made the volume: 0.14 to 0.18 s at 300 MB on a Mac
/// Studio (A-374), seconds for the 1.99 GB kernel base volume (g232).
pub struct Restoring<'tree> {
  root: &'tree NodeMeta,
  bodies: Vec<PlannedBody<'tree>>,
  directories: BTreeSet<String>,
  metadata: BTreeMap<String, NodeMeta>,
  chunks: BTreeMap<[u8; 32], &'tree Chunk>,
  needed: u64,
  budget: u64,
  outputs: Vec<Vec<Vec<u8>>>,
  stage: RestoreStage<'tree>,
  chunks_decoded: u64,
  files: BTreeMap<String, RestoredFile>,
  xattrs: BTreeMap<String, BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl<'tree> Restoring<'tree> {
  /// Restores until `spent` says the step's share is spent, a piece at least: `Some` with the restored volume once every
  /// piece is done. A missing or corrupt chunk, or an allocation refused, is a typed refusal, as [`restore`]'s.
  pub fn step(&mut self, mut spent: impl FnMut() -> bool) -> Result<Option<Restored>, ArchiveError> {
    loop {
      if self.piece()? {
        return Ok(Some(Restored {
          files: std::mem::take(&mut self.files),
          directories: std::mem::take(&mut self.directories),
          metadata: std::mem::take(&mut self.metadata),
          root: self.root.clone(),
          xattrs: std::mem::take(&mut self.xattrs),
          chunks_decoded: self.chunks_decoded,
        }));
      }
      if spent() {
        return Ok(None);
      }
    }
  }

  /// One piece of the restore; `true` once every piece is done.
  fn piece(&mut self) -> Result<bool, ArchiveError> {
    let stage = std::mem::replace(
      &mut self.stage,
      RestoreStage::Allocating { body: 0, extent: 0 },
    );
    let next = match stage {
      RestoreStage::Allocating { body, extent } => self.allocate(body, extent)?,
      RestoreStage::Referencing {
        body,
        extent,
        piece,
        references,
      } => self.reference(body, extent, piece, references),
      RestoreStage::Decoding {
        references,
        current,
      } => self.decode(references, current)?,
      RestoreStage::Assembling { bodies, current } => match self.assemble(bodies, current) {
        Some(next) => next,
        None => return Ok(true),
      },
    };
    self.stage = next;
    Ok(false)
  }

  /// One buffer allocated, zeroed (fallibly: a refusal, never an abort): a dense body's one of its length, a sparse
  /// file's one for its next data extent.
  fn allocate(&mut self, body: usize, extent: usize) -> Result<RestoreStage<'tree>, ArchiveError> {
    let Some(planned) = self.bodies.get(body) else {
      return Ok(RestoreStage::Referencing {
        body: 0,
        extent: 0,
        piece: 0,
        references: BTreeMap::new(),
      });
    };
    if self.outputs.len() <= body {
      self.outputs.push(Vec::new());
    }
    let buffers = self.outputs.last_mut().ok_or(ArchiveError::BadLength)?;
    if !sparse(planned) {
      buffers.push(zeroed(planned.len, self.needed, self.budget)?);
      return Ok(RestoreStage::Allocating {
        body: body.saturating_add(1),
        extent: 0,
      });
    }
    match planned.extents.get(extent) {
      None => Ok(RestoreStage::Allocating {
        body: body.saturating_add(1),
        extent: 0,
      }),
      Some(data) => {
        if data.chunk != HOLE {
          buffers.push(zeroed(data.len, self.needed, self.budget)?);
        }
        Ok(RestoreStage::Allocating {
          body,
          extent: extent.saturating_add(1),
        })
      }
    }
  }

  /// One extent's reference gathered under the chunk it names: its body, its buffer (a sparse file's `piece`, counted
  /// over its data extents; a dense body's only one) and the extent.
  fn reference(
    &mut self,
    body: usize,
    extent: usize,
    piece: usize,
    mut references: BTreeMap<[u8; 32], Vec<Fill<'tree>>>,
  ) -> RestoreStage<'tree> {
    let Some(planned) = self.bodies.get(body) else {
      return RestoreStage::Decoding {
        references: references.into_iter(),
        current: None,
      };
    };
    let Some(data) = planned.extents.get(extent) else {
      return RestoreStage::Referencing {
        body: body.saturating_add(1),
        extent: 0,
        piece: 0,
        references,
      };
    };
    let mut next_piece = piece;
    if data.chunk != HOLE {
      let buffer = if sparse(planned) { piece } else { 0 };
      references
        .entry(data.chunk)
        .or_default()
        .push((body, buffer, data));
      next_piece = piece.saturating_add(1);
    }
    RestoreStage::Referencing {
      body,
      extent: extent.saturating_add(1),
      piece: next_piece,
      references,
    }
  }

  /// One extent filled from its chunk, the chunk decoded (and verified) once, with the first extent that names it.
  fn decode(
    &mut self,
    mut references: std::collections::btree_map::IntoIter<[u8; 32], Vec<Fill<'tree>>>,
    current: Option<(Vec<u8>, std::vec::IntoIter<Fill<'tree>>)>,
  ) -> Result<RestoreStage<'tree>, ArchiveError> {
    let (content, mut fills) = match current {
      Some(current) => current,
      None => {
        let Some((identity, fills)) = references.next() else {
          let bodies = std::mem::take(&mut self.bodies);
          let outputs = std::mem::take(&mut self.outputs);
          return Ok(RestoreStage::Assembling {
            bodies: bodies.into_iter().zip(outputs),
            current: None,
          });
        };
        let chunk = self
          .chunks
          .get(&identity)
          .ok_or(ArchiveError::MissingChunk)?;
        let content = Archive::content(chunk)?;
        self.chunks_decoded = self.chunks_decoded.saturating_add(1);
        (content, fills.into_iter())
      }
    };
    let Some((body, buffer, extent)) = fills.next() else {
      return Ok(RestoreStage::Decoding {
        references,
        current: None,
      });
    };
    let planned = self.bodies.get(body).ok_or(ArchiveError::BadLength)?;
    let out = self
      .outputs
      .get_mut(body)
      .and_then(|buffers| buffers.get_mut(buffer))
      .ok_or(ArchiveError::BadLength)?;
    // A sparse piece starts at its extent; a dense body's buffer at the body's offset zero.
    let at = if sparse(planned) { 0 } else { extent.offset };
    fill_extent(out, extent, &content, at)?;
    Ok(RestoreStage::Decoding {
      references,
      current: Some((content, fills)),
    })
  }

  /// One piece of a file laid out, contiguous pieces merged, or one attribute value placed; `None` once every body is
  /// laid out.
  fn assemble(
    &mut self,
    mut bodies: std::iter::Zip<std::vec::IntoIter<PlannedBody<'tree>>, std::vec::IntoIter<Vec<Vec<u8>>>>,
    current: Option<Assembling>,
  ) -> Option<RestoreStage<'tree>> {
    if let Some(mut file) = current {
      match file.pieces.next() {
        Some(piece) => {
          merge_piece(&mut file.merged, piece);
          return Some(RestoreStage::Assembling {
            bodies,
            current: Some(file),
          });
        }
        None => {
          self.files.insert(
            file.path,
            RestoredFile {
              len: file.len,
              pieces: file.merged,
            },
          );
          return Some(RestoreStage::Assembling {
            bodies,
            current: None,
          });
        }
      }
    }
    let (body, buffers) = bodies.next()?;
    let current = match body.target {
      Target::File(path) => {
        let pieces: Vec<(u64, Vec<u8>)> = body
          .extents
          .iter()
          .filter(|extent| extent.chunk != HOLE)
          .map(|extent| extent.offset)
          .zip(buffers)
          .collect();
        Some(Assembling {
          path,
          len: body.len,
          pieces: pieces.into_iter(),
          merged: Vec::new(),
        })
      }
      Target::Xattr { path, name } => {
        let value = buffers.into_iter().next().unwrap_or_default();
        self.xattrs.entry(path).or_default().insert(name, value);
        None
      }
    };
    Some(RestoreStage::Assembling { bodies, current })
  }
}

/// Plans `archive`'s whole tree in one call ([`Planning`] run to its end).
fn planned_whole(archive: &Archive) -> Result<Planned<'_>, ArchiveError> {
  let mut planning = Planning::new(archive);
  loop {
    if let Some(planned) = planning.step(|| false)? {
      return Ok(planned);
    }
  }
}

/// The zero chunk identity: an extent naming it is a hole.
const HOLE: [u8; 32] = [0u8; 32];

/// Whether `body` restores sparse (a file: its data pieces only) or dense (an attribute value).
fn sparse(body: &PlannedBody<'_>) -> bool {
  matches!(body.target, Target::File(_))
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

/// Lays `piece` after `merged`, pieces at ascending offsets: onto the last one when it is contiguous with it, else as a
/// piece of its own. A merge whose growth cannot be reserved keeps the pieces apart — still correct, never an abort.
fn merge_piece(merged: &mut Vec<(u64, Vec<u8>)>, (offset, bytes): (u64, Vec<u8>)) {
  if let Some((start, previous)) = merged.last_mut()
    && start.checked_add(u64::try_from(previous.len()).unwrap_or(u64::MAX)) == Some(offset)
    && previous.try_reserve_exact(bytes.len()).is_ok()
  {
    previous.extend_from_slice(&bytes);
    return;
  }
  merged.push((offset, bytes));
}

/// The bytes [`restore`] admits the archive's restore against, planned without decoding a chunk: what a caller must
/// have room for before it restores (A-98: a shard growing its arena from the pool by exactly this). A tree that does
/// not plan is refused as [`restore`] refuses it. The plan in one call; [`Planning`] makes it in steps.
pub fn restore_needed(archive: &Archive) -> Result<u64, ArchiveError> {
  planned_whole(archive).map(|planned| planned.needed())
}

/// Restores the whole volume the archive holds: every file's bytes, every directory and every extended
/// attribute value, planned and admitted against `budget` bytes before anything is allocated (see the
/// module doc), each chunk decoded once. A tree that does not plan, a missing or corrupt chunk, or a
/// restore past the budget is a typed refusal. The restore in one call; [`Planning`] and [`Restoring`] make it in steps,
/// and this is them run to their ends.
pub fn restore(archive: &Archive, budget: u64) -> Result<Restored, ArchiveError> {
  let mut restoring = planned_whole(archive)?.restoring(budget)?;
  loop {
    if let Some(restored) = restoring.step(|| false)? {
      return Ok(restored);
    }
  }
}
