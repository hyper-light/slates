//! The increment assembler (§4.16 "Composition at seal", "Data model"). The content deriver
//! ([`crate::derive`]) composes one path's declared operations; a work volume's journal spans
//! many paths, so the assembler groups the journal by path, composes each path's content, and
//! lays the results out as one [`OpsDoc`] — the increment's declared operations, whose BLAKE3 is
//! half its identity.
//!
//! The post-state layout. An op that adds content names its bytes by an offset into the
//! increment's post-state — the sealed work volume's final content (§4.16 "Splice"). The
//! post-state is the paths' final contents concatenated in sorted-path order; each path occupies
//! a contiguous region, and an op's source offset is its offset within the path's final content
//! (what the deriver returns) plus that region's base offset. Sorting the paths makes the layout,
//! and therefore the identity, independent of the order the journal declared them in.
//!
//! Scope: this assembles content operations across paths. Namespace composition — create and
//! unlink (with their cancellation), rename (mapping later operations and, for a new file renamed
//! over a base file, the write-and-rename content replacement), directories, links, symlinks and
//! metadata — is the deriver's remaining piece (owed; GAPS §8f), built on this assembly and the
//! per-path content deriver. This module is pure: no I/O, no clock, no randomness.

use crate::derive::{ContentOp, compose_content_sized};
use crate::ops_doc::{OpKind, OpsDoc};

/// One declared content operation on a named path, as the journal records it (§4.5). The path is
/// the file it targets; the coordinates are current-file at the moment it happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileOp {
  /// A `write` within the file: `[at, at + len)` replaced in place.
  Overwrite {
    /// The file.
    path: String,
    /// The offset.
    at: u64,
    /// The length.
    len: u64,
  },
  /// A `write` past the end: `len` bytes appended at `at` (the old end).
  Extend {
    /// The file.
    path: String,
    /// The old end.
    at: u64,
    /// The length.
    len: u64,
  },
  /// A `truncate` to `len`.
  Truncate {
    /// The file.
    path: String,
    /// The new length.
    len: u64,
  },
  /// An `edit`'s inserted bytes: `len` bytes inserted at `at`.
  Insert {
    /// The file.
    path: String,
    /// The offset.
    at: u64,
    /// The length.
    len: u64,
  },
  /// An `edit`'s deleted bytes: `[at, at + len)` removed.
  Delete {
    /// The file.
    path: String,
    /// The offset.
    at: u64,
    /// The length.
    len: u64,
  },
}

impl FileOp {
  /// The path this operation targets.
  pub fn path(&self) -> &str {
    match self {
      FileOp::Overwrite { path, .. }
      | FileOp::Extend { path, .. }
      | FileOp::Truncate { path, .. }
      | FileOp::Insert { path, .. }
      | FileOp::Delete { path, .. } => path,
    }
  }

  /// The path-free content operation the deriver composes.
  fn content(&self) -> ContentOp {
    match *self {
      FileOp::Overwrite { at, len, .. } => ContentOp::Overwrite { at, len },
      FileOp::Extend { at, len, .. } => ContentOp::Extend { at, len },
      FileOp::Truncate { len, .. } => ContentOp::Truncate { len },
      FileOp::Insert { at, len, .. } => ContentOp::Insert { at, len },
      FileOp::Delete { at, len, .. } => ContentOp::Delete { at, len },
    }
  }
}

/// Whether an op kind adds content (so its `src` names a post-state offset that must be shifted
/// into the path's region); a `Delete` or `Truncate` adds none and keeps `src = u64::MAX`.
fn adds_content(kind: OpKind) -> bool {
  matches!(kind, OpKind::Overwrite | OpKind::Insert | OpKind::Extend)
}

/// Composes a work volume's content journal into one increment's ops document. `base` gives the
/// base content length of every path that existed at the increment's base version; a path the
/// journal touches but `base` does not is composed against an empty base.
pub fn compose_increment(base: &[(String, u64)], journal: &[FileOp]) -> OpsDoc {
  // The distinct paths, sorted, so the post-state layout and the identity do not depend on the
  // journal's declaration order.
  let mut paths: Vec<&str> = journal.iter().map(FileOp::path).collect();
  paths.sort_unstable();
  paths.dedup();

  let mut doc = OpsDoc::new();
  let mut region_offset = 0u64;
  for path in paths {
    let base_len = base
      .iter()
      .find(|(candidate, _)| candidate == path)
      .map_or(0, |(_, len)| *len);
    let content: Vec<ContentOp> = journal
      .iter()
      .filter(|op| op.path() == path)
      .map(FileOp::content)
      .collect();
    let (ops, final_len) = compose_content_sized(base_len, &content);
    let path_index = doc.paths.intern(path);
    for mut op in ops {
      op.path = path_index;
      if adds_content(op.kind) {
        op.src = op.src.saturating_add(region_offset);
      }
      doc.ops.push(op);
    }
    region_offset = region_offset.saturating_add(final_len);
  }
  doc.canonicalize();
  doc
}
