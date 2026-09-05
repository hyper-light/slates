//! The whole-volume deriver (§4.16 "Composition at seal", "Declared operations"). The content
//! deriver ([`crate::derive`]) composes one path's declared content; this module composes a work
//! volume's whole journal — content and the namespace operations create and unlink — into one
//! [`OpsDoc`], the increment's declared operations whose BLAKE3 is half its identity.
//!
//! Per path, the journal is a state machine: a create makes a fresh empty file; an unlink removes
//! it; content operations accumulate against whatever file is present. Composition follows
//! (§4.16): a create followed by an unlink of a new file cancels (nothing is declared); a base
//! file unlinked is one `Unlink`; a base file edited in place is its content net ops; a base
//! file whose content was replaced (unlinked then recreated, or created over) is a delete of the
//! base and the new content; a new file that survives is one `Create` and its content as inserts.
//! Composition is arithmetic on the declared operations, never a comparison of file states
//! (D-27's never-diff clause).
//!
//! The post-state layout. An op that adds content names its bytes by an offset into the
//! increment's post-state — the sealed work volume's final content. The post-state is the
//! surviving paths' final contents concatenated in sorted-path order; each occupies a contiguous
//! region, and an op's source offset is its offset within the path's final content plus the
//! region's base offset. Sorting the paths makes the layout, and the identity, independent of the
//! journal's declaration order.
//!
//! Scope: content, create and unlink. Rename (mapping later operations, and the write-and-rename
//! content replacement when a new file is renamed over a base file), directories, hard links,
//! symlinks and metadata are the deriver's remaining piece (owed; GAPS §8f). An operation the
//! journal could not have produced on a valid volume — content on a missing file, a create over
//! an existing one, an unlink of a missing one — is a typed [`DeriveError`], never a panic. This
//! module is pure: no I/O, no clock, no randomness.

use crate::derive::{ContentOp, compose_content_sized};
use crate::ops_doc::{Op, OpKind, OpsDoc};

/// One declared operation on a named path, as the journal records it (§4.5). Content coordinates
/// are current-file at the moment the operation happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VolumeOp {
  /// A `write` within a file: `[at, at + len)` replaced in place.
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
  /// A new, empty regular file created at `path`.
  Create {
    /// The file.
    path: String,
  },
  /// The name `path` removed.
  Unlink {
    /// The file.
    path: String,
  },
}

impl VolumeOp {
  /// The path this operation targets.
  pub fn path(&self) -> &str {
    match self {
      VolumeOp::Overwrite { path, .. }
      | VolumeOp::Extend { path, .. }
      | VolumeOp::Truncate { path, .. }
      | VolumeOp::Insert { path, .. }
      | VolumeOp::Delete { path, .. }
      | VolumeOp::Create { path }
      | VolumeOp::Unlink { path } => path,
    }
  }

  /// The path-free content operation, or `None` for a namespace operation.
  fn content(&self) -> Option<ContentOp> {
    match *self {
      VolumeOp::Overwrite { at, len, .. } => Some(ContentOp::Overwrite { at, len }),
      VolumeOp::Extend { at, len, .. } => Some(ContentOp::Extend { at, len }),
      VolumeOp::Truncate { len, .. } => Some(ContentOp::Truncate { len }),
      VolumeOp::Insert { at, len, .. } => Some(ContentOp::Insert { at, len }),
      VolumeOp::Delete { at, len, .. } => Some(ContentOp::Delete { at, len }),
      VolumeOp::Create { .. } | VolumeOp::Unlink { .. } => None,
    }
  }
}

/// A refusal from the deriver: an operation the journal could not have produced on a valid
/// volume. Each names the offending path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeriveError {
  /// A content operation on a path with no file present (never created, or already unlinked).
  ContentOnMissing(String),
  /// A create on a path that already holds a file.
  CreateOverExisting(String),
  /// An unlink of a path with no file present.
  UnlinkMissing(String),
}

impl std::fmt::Display for DeriveError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::ContentOnMissing(path) => write!(f, "content operation on missing file {path}"),
      Self::CreateOverExisting(path) => write!(f, "create over existing file {path}"),
      Self::UnlinkMissing(path) => write!(f, "unlink of missing file {path}"),
    }
  }
}

impl std::error::Error for DeriveError {}

/// What one path's journal composed to.
enum PathOutcome {
  /// Nothing net (an empty or fully cancelling journal): no ops, no post-state region.
  Nothing,
  /// A base file edited in place: content net ops, and its final content length.
  Edited(Vec<Op>, u64),
  /// A base path whose content was replaced: content net ops (a delete of the base and the new
  /// content), and the new final length. No `Create` — the path existed at base.
  Replaced(Vec<Op>, u64),
  /// A new file that survives: a `Create` then its content as inserts, and its final length.
  Created(Vec<Op>, u64),
  /// A base file removed: one `Unlink`, no post-state region.
  Removed,
}

/// The per-path state during replay.
struct PathState {
  exists_at_base: bool,
  base_len: u64,
  present: bool,
  /// Whether the file currently present is the base file (content relative to `base_len`) or a
  /// new one created this increment (content relative to empty).
  from_base: bool,
  content: Vec<ContentOp>,
}

impl PathState {
  /// The starting state for a path, given whether it existed at base and its base length.
  fn new(exists_at_base: bool, base_len: u64) -> PathState {
    PathState {
      exists_at_base,
      base_len,
      present: exists_at_base,
      from_base: exists_at_base,
      content: Vec::new(),
    }
  }

  /// Applies one of this path's operations.
  fn apply(&mut self, op: &VolumeOp) -> Result<(), DeriveError> {
    if let Some(content) = op.content() {
      if !self.present {
        return Err(DeriveError::ContentOnMissing(op.path().to_owned()));
      }
      self.content.push(content);
      return Ok(());
    }
    match op {
      VolumeOp::Create { path } => {
        if self.present {
          return Err(DeriveError::CreateOverExisting(path.clone()));
        }
        self.present = true;
        self.from_base = false;
        self.content.clear();
      }
      VolumeOp::Unlink { path } => {
        if !self.present {
          return Err(DeriveError::UnlinkMissing(path.clone()));
        }
        self.present = false;
        self.from_base = false;
        self.content.clear();
      }
      _ => {}
    }
    Ok(())
  }

  /// Composes the path's final outcome.
  fn outcome(self) -> PathOutcome {
    if !self.present {
      // Removed if it was a base file; otherwise created-then-unlinked, which cancels.
      return if self.exists_at_base {
        PathOutcome::Removed
      } else {
        PathOutcome::Nothing
      };
    }
    if self.from_base {
      // A base file edited in place (or untouched).
      let (ops, final_len) = compose_content_sized(self.base_len, &self.content);
      if ops.is_empty() {
        PathOutcome::Nothing
      } else {
        PathOutcome::Edited(ops, final_len)
      }
    } else if self.exists_at_base {
      // The base path holds new content: replace the base with it (a delete then the new bytes).
      let mut journal = Vec::with_capacity(self.content.len() + 1);
      journal.push(ContentOp::Truncate { len: 0 });
      journal.extend(self.content);
      let (ops, final_len) = compose_content_sized(self.base_len, &journal);
      PathOutcome::Replaced(ops, final_len)
    } else {
      // A new file: create it and add its bytes.
      let (ops, final_len) = compose_content_sized(0, &self.content);
      PathOutcome::Created(ops, final_len)
    }
  }
}

/// A namespace op with a path index and empty coordinates.
fn namespace_op(kind: OpKind, path: u16) -> Op {
  Op {
    kind,
    flags: 0,
    path,
    at: 0,
    len: 0,
    src: u64::MAX,
  }
}

/// Whether an op kind adds content (so its `src` names a post-state offset that must be shifted
/// into the path's region).
fn adds_content(kind: OpKind) -> bool {
  matches!(kind, OpKind::Overwrite | OpKind::Insert | OpKind::Extend)
}

/// Composes a work volume's journal into one increment's ops document, or refuses with a typed
/// error. `base` gives the base content length of every path that existed at the increment's base
/// version.
pub fn compose_volume(base: &[(String, u64)], journal: &[VolumeOp]) -> Result<OpsDoc, DeriveError> {
  // The distinct paths, sorted, so the post-state layout and the identity do not depend on the
  // journal's declaration order.
  let mut paths: Vec<&str> = journal.iter().map(VolumeOp::path).collect();
  paths.sort_unstable();
  paths.dedup();

  // Compose each path independently (content, create and unlink do not couple paths).
  let mut outcomes: Vec<(&str, PathOutcome)> = Vec::new();
  for path in paths {
    let base_entry = base.iter().find(|(candidate, _)| candidate == path);
    let mut state = PathState::new(base_entry.is_some(), base_entry.map_or(0, |(_, len)| *len));
    for op in journal.iter().filter(|op| op.path() == path) {
      state.apply(op)?;
    }
    outcomes.push((path, state.outcome()));
  }

  // Assemble the ops document, laying out the post-state region for each path that has content.
  let mut doc = OpsDoc::new();
  let mut region_offset = 0u64;
  for (path, outcome) in outcomes {
    match outcome {
      PathOutcome::Nothing => {}
      PathOutcome::Removed => {
        let index = doc.paths.intern(path);
        doc.ops.push(namespace_op(OpKind::Unlink, index));
      }
      PathOutcome::Edited(ops, final_len) | PathOutcome::Replaced(ops, final_len) => {
        let index = doc.paths.intern(path);
        push_content(&mut doc, ops, index, region_offset);
        region_offset = region_offset.saturating_add(final_len);
      }
      PathOutcome::Created(ops, final_len) => {
        let index = doc.paths.intern(path);
        doc.ops.push(namespace_op(OpKind::Create, index));
        push_content(&mut doc, ops, index, region_offset);
        region_offset = region_offset.saturating_add(final_len);
      }
    }
  }
  doc.canonicalize();
  Ok(doc)
}

/// Pushes a path's content ops into the document, setting their path index and shifting the
/// source offset of content-adding ops into the path's post-state region.
fn push_content(doc: &mut OpsDoc, ops: Vec<Op>, index: u16, region_offset: u64) {
  for mut op in ops {
    op.path = index;
    if adds_content(op.kind) {
      op.src = op.src.saturating_add(region_offset);
    }
    doc.ops.push(op);
  }
}
