//! The merge engine on one node (§4.16 "The verdict", "Splice", "Commit"; D-27). A green volume
//! is written only by its merge task. Agents clone a version, edit their clone, and `submit` an
//! increment of declared operations against a base version; the engine maps the increment's ranges
//! forward through everything committed since that base, decides the pure verdict, splices the
//! accepted edits into a new version, and commits it. Conflicts are returned as byte-exact windows
//! (each with its conflict class) that the agent rebases against; the merge task never stalls.
//!
//! This is the laptop-degenerate engine (R8): one green, one merge task, an in-memory version
//! chain and a `seen` set for idempotent retries. The same pipeline runs in a fleet, where the
//! commit is a fenced ledger-register entry to the green's candidate holders (§4.8) and holders
//! recompute the verdict before serving — those are the engine's remaining pieces (owed). This
//! module is pure: it composes [`crate::map`] (position mapping) and a byte splice; no I/O, no
//! clock, no randomness.
//!
//! The verdict per path:
//! - **Modify** a file: if an intervening change deleted it, that is a delete/modify conflict; else
//!   each edited range is mapped forward — a range disjoint from every intervening change is
//!   accepted and re-applied at the shifted position, a range that meets one is a conflict unless
//!   the agent produced exactly the green's current bytes for that file (both made the same edit).
//! - **Create** a file: a conflict if an intervening change already created it with different bytes
//!   (create/create), accepted if the bytes match, else created.
//! - **Remove** a file: accepted (or a no-op if already gone); a delete/modify conflict if an
//!   intervening change modified it since the base.
//!
//! The namespace merges alongside the content, as independent dimensions (the deriver composes them
//! the same way, §4.16): a directory creation (`Mkdir`) and a mode change (`SetMode`) each merge per
//! path with their own conflict classes — a file and a directory at one path is a `TypeChanged`
//! conflict, two differing mode changes are a `MetaMeta` conflict, and an identical one accepts.
//! Directory removal (needing emptiness), renames and links (cross-path effects), symlinks and
//! xattrs are the continuing step; the deriver already composes them.
//!
//! Per-range identity within a mixed file is owed.

use std::collections::{BTreeMap, BTreeSet};

use crate::ops_doc::{Op, OpKind};
use crate::range::Range;
use crate::verdict::MergeConflictClass;

/// What an increment does to one file.
#[derive(Clone, Debug)]
pub enum PathChange {
  /// Edit an existing file: net content ops (base coordinates, `src` into `post_state`) and the
  /// file's final bytes.
  Modify {
    /// The net content ops.
    ops: Vec<Op>,
    /// The file's final bytes.
    post_state: Vec<u8>,
  },
  /// Create a new file with these bytes.
  Create {
    /// The new file's bytes.
    post_state: Vec<u8>,
  },
  /// Remove the file.
  Remove,
  /// Create a directory at the path.
  Mkdir,
  /// Set the mode of a file or directory at the path.
  SetMode {
    /// The new mode bits.
    mode: u32,
  },
}

/// An increment submitted against a base version.
#[derive(Clone, Debug)]
pub struct Increment {
  /// The increment's identity (`blake3` of its declared work).
  pub id: [u8; 32],
  /// The version it was based on.
  pub base: u64,
  /// The changed files.
  pub changes: BTreeMap<String, PathChange>,
}

/// A conflict window: the file, the range (base coordinates) that met an intervening change, and
/// the class of the conflict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictWindow {
  /// The file.
  pub path: String,
  /// The conflicting range (a zero-length anchor for a whole-file namespace conflict).
  pub range: Range,
  /// The class of the conflict.
  pub class: MergeConflictClass,
}

/// The result of a submit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
  /// The increment was merged; the green's new head version.
  Accepted {
    /// The committed version.
    version: u64,
  },
  /// The increment conflicts; the windows to rebase against.
  Conflict {
    /// The conflicting windows.
    windows: Vec<ConflictWindow>,
  },
}

/// A green volume's merge state.
#[derive(Debug, Default)]
pub struct Green {
  content: BTreeMap<String, Vec<u8>>,
  dirs: BTreeSet<String>,
  modes: BTreeMap<String, u32>,
  deltas: Vec<BTreeMap<String, Vec<Op>>>,
  last_changed: BTreeMap<String, u64>,
  mode_changed: BTreeMap<String, u64>,
  seen: BTreeMap<[u8; 32], Outcome>,
  fast_path_hits: u64,
}

/// The base range an op touches, for overlap testing.
fn touched_range(op: &Op) -> Range {
  match op.kind {
    OpKind::Overwrite | OpKind::Delete | OpKind::Truncate => Range::new(op.at, op.len),
    _ => Range::new(op.at, 0),
  }
}

/// Applies content ops to `base`, drawing added bytes from `post_state` — the byte splice.
fn apply(base: &[u8], ops: &[Op], post_state: &[u8]) -> Vec<u8> {
  let base_len = base.len() as u64;
  let mut out = Vec::new();
  let mut pos = 0u64;
  for op in ops {
    if op.at > pos {
      let start = usize::try_from(pos).unwrap_or(0);
      let end = usize::try_from(op.at).unwrap_or(start).min(base.len());
      out.extend_from_slice(&base[start..end]);
      pos = op.at;
    }
    match op.kind {
      OpKind::Delete => pos = pos.saturating_add(op.len),
      OpKind::Truncate => pos = base_len,
      OpKind::Overwrite => {
        push_slice(&mut out, post_state, op.src, op.len);
        pos = pos.saturating_add(op.len);
      }
      OpKind::Insert | OpKind::Extend => push_slice(&mut out, post_state, op.src, op.len),
      _ => {}
    }
  }
  if pos < base_len {
    out.extend_from_slice(&base[usize::try_from(pos).unwrap_or(0)..base.len()]);
  }
  out
}

/// Appends `len` bytes at `src` of `from` to `out`, clamped to `from`'s bounds.
fn push_slice(out: &mut Vec<u8>, from: &[u8], src: u64, len: u64) {
  let start = usize::try_from(src).unwrap_or(usize::MAX).min(from.len());
  let end = start
    .saturating_add(usize::try_from(len).unwrap_or(0))
    .min(from.len());
  out.extend_from_slice(&from[start..end]);
}

/// What merging one path produced.
enum PathMerge {
  /// Set the file to these bytes, recording these ops (head coordinates) as its delta.
  Set(Vec<u8>, Vec<Op>),
  /// Remove the file.
  Remove,
  /// Create a directory at the path.
  MakeDir,
  /// Set the path's mode.
  Mode(u32),
  /// No net change to the file.
  Unchanged,
}

impl Green {
  /// An empty green volume at version 0.
  pub fn new() -> Green {
    Green::default()
  }

  /// The head version (the number of committed deltas).
  pub fn head(&self) -> u64 {
    self.deltas.len() as u64
  }

  /// A file's current bytes, or `None` when it is absent.
  pub fn content(&self, path: &str) -> Option<&[u8]> {
    self.content.get(path).map(Vec::as_slice)
  }

  /// Whether a directory exists at the path.
  pub fn is_dir(&self, path: &str) -> bool {
    self.dirs.contains(path)
  }

  /// A path's current mode, or `None` when none has been set.
  pub fn mode(&self, path: &str) -> Option<u32> {
    self.modes.get(path).copied()
  }

  /// The number of paths merged through the fast path (the non-vacuity counter).
  pub fn fast_path_hits(&self) -> u64 {
    self.fast_path_hits
  }

  /// The ops each intervening delta in `(base, head]` applied to `path`.
  fn intervening(&self, path: &str, base: u64) -> Vec<&[Op]> {
    let base = usize::try_from(base).unwrap_or(usize::MAX);
    self.deltas[base.min(self.deltas.len())..]
      .iter()
      .filter_map(|delta| delta.get(path).map(Vec::as_slice))
      .collect()
  }

  /// A whole-file namespace conflict window at a path.
  fn window(path: &str, class: MergeConflictClass) -> ConflictWindow {
    ConflictWindow {
      path: path.to_owned(),
      range: Range::new(0, 0),
      class,
    }
  }

  /// Merges an edit to an existing file (the content path).
  fn merge_modify(
    &mut self,
    path: &str,
    base: u64,
    ops: &[Op],
    post_state: &[u8],
  ) -> Result<PathMerge, ConflictWindow> {
    if !self.content.contains_key(path) {
      if self.dirs.contains(path) {
        // An intervening change replaced the file with a directory.
        return Err(Green::window(path, MergeConflictClass::TypeChanged));
      }
      // The agent edited a file an intervening change deleted.
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    }
    let base_changed = self.last_changed.get(path).copied().unwrap_or(0);
    let unchanged = base_changed <= base;
    if unchanged {
      self.fast_path_hits = self.fast_path_hits.saturating_add(1);
    }
    let intervening = if unchanged {
      Vec::new()
    } else {
      self.intervening(path, base)
    };
    let mut mapped = Vec::with_capacity(ops.len());
    for op in ops {
      match crate::map::map_range(&intervening, touched_range(op)) {
        crate::map::Mapped::Shifted(shifted) => {
          let mut moved = *op;
          moved.at = shifted.start;
          mapped.push(moved);
        }
        crate::map::Mapped::Overlaps => {
          let current = self
            .content
            .get(path)
            .map(Vec::as_slice)
            .unwrap_or_default();
          if post_state == current {
            return Ok(PathMerge::Unchanged);
          }
          return Err(ConflictWindow {
            path: path.to_owned(),
            range: touched_range(op),
            class: MergeConflictClass::Overlap,
          });
        }
      }
    }
    let empty = Vec::new();
    let current = self.content.get(path).unwrap_or(&empty);
    Ok(PathMerge::Set(apply(current, &mapped, post_state), mapped))
  }

  /// Merges a create.
  fn merge_create(&self, path: &str, post_state: &[u8]) -> Result<PathMerge, ConflictWindow> {
    if self.dirs.contains(path) {
      // An intervening change made this path a directory.
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    match self.content.get(path) {
      // Already created by an intervening increment: identical bytes accept, else conflict.
      Some(current) if current.as_slice() == post_state => Ok(PathMerge::Unchanged),
      Some(_) => Err(Green::window(path, MergeConflictClass::CreateCreate)),
      None => {
        let ops = vec![Op {
          kind: OpKind::Insert,
          flags: 0,
          path: 0,
          at: 0,
          len: post_state.len() as u64,
          src: 0,
        }];
        Ok(PathMerge::Set(post_state.to_vec(), ops))
      }
    }
  }

  /// Merges a remove.
  fn merge_remove(&self, path: &str, base: u64) -> Result<PathMerge, ConflictWindow> {
    if !self.content.contains_key(path) {
      return Ok(PathMerge::Unchanged); // already gone
    }
    let base_changed = self.last_changed.get(path).copied().unwrap_or(0);
    if base_changed > base {
      // The agent removed a file an intervening change modified.
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    }
    Ok(PathMerge::Remove)
  }

  /// Merges a directory creation. A file at the path is a type conflict; an existing directory is an
  /// identical accept (both agents made it); otherwise the directory is created.
  fn merge_mkdir(&self, path: &str) -> Result<PathMerge, ConflictWindow> {
    if self.content.contains_key(path) {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    if self.dirs.contains(path) {
      return Ok(PathMerge::Unchanged);
    }
    Ok(PathMerge::MakeDir)
  }

  /// Merges a mode change. The path must be present (a file or a directory); if an intervening
  /// change set a different mode it is a metadata conflict, an equal one an identical accept.
  fn merge_setmode(&self, path: &str, base: u64, mode: u32) -> Result<PathMerge, ConflictWindow> {
    if !self.content.contains_key(path) && !self.dirs.contains(path) {
      // The path an intervening change removed cannot take a mode.
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    }
    let changed = self.mode_changed.get(path).copied().unwrap_or(0);
    if changed > base {
      if self.modes.get(path).copied() == Some(mode) {
        return Ok(PathMerge::Unchanged);
      }
      return Err(Green::window(path, MergeConflictClass::MetaMeta));
    }
    Ok(PathMerge::Mode(mode))
  }

  /// Merges one file's change.
  fn merge_path(
    &mut self,
    path: &str,
    base: u64,
    change: &PathChange,
  ) -> Result<PathMerge, ConflictWindow> {
    match change {
      PathChange::Modify { ops, post_state } => self.merge_modify(path, base, ops, post_state),
      PathChange::Create { post_state } => self.merge_create(path, post_state),
      PathChange::Remove => self.merge_remove(path, base),
      PathChange::Mkdir => self.merge_mkdir(path),
      PathChange::SetMode { mode } => self.merge_setmode(path, base, *mode),
    }
  }

  /// Submits an increment: idempotent by identity, merges every changed file, and commits a new
  /// version when all accept, or returns the conflict windows and changes nothing.
  pub fn submit(&mut self, increment: &Increment) -> Outcome {
    if let Some(outcome) = self.seen.get(&increment.id) {
      return outcome.clone();
    }
    let mut sets: BTreeMap<String, (Vec<u8>, Vec<Op>)> = BTreeMap::new();
    let mut removes: Vec<String> = Vec::new();
    let mut mkdirs: Vec<String> = Vec::new();
    let mut set_modes: Vec<(String, u32)> = Vec::new();
    let mut windows = Vec::new();
    for (path, change) in &increment.changes {
      match self.merge_path(path, increment.base, change) {
        Ok(PathMerge::Set(content, ops)) => {
          sets.insert(path.clone(), (content, ops));
        }
        Ok(PathMerge::Remove) => removes.push(path.clone()),
        Ok(PathMerge::MakeDir) => mkdirs.push(path.clone()),
        Ok(PathMerge::Mode(mode)) => set_modes.push((path.clone(), mode)),
        Ok(PathMerge::Unchanged) => {}
        Err(window) => windows.push(window),
      }
    }
    if !windows.is_empty() {
      let outcome = Outcome::Conflict { windows };
      self.seen.insert(increment.id, outcome.clone());
      return outcome;
    }
    let version = self.head() + 1;
    let mut delta: BTreeMap<String, Vec<Op>> = BTreeMap::new();
    for (path, (content, ops)) in sets {
      self.content.insert(path.clone(), content);
      self.last_changed.insert(path.clone(), version);
      delta.insert(path, ops);
    }
    for path in removes {
      self.content.remove(&path);
      self.last_changed.insert(path, version);
    }
    for path in mkdirs {
      self.dirs.insert(path);
    }
    for (path, mode) in set_modes {
      self.modes.insert(path.clone(), mode);
      self.mode_changed.insert(path, version);
    }
    self.deltas.push(delta);
    let outcome = Outcome::Accepted { version };
    self.seen.insert(increment.id, outcome.clone());
    outcome
  }
}
