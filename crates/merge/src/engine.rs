//! The merge engine on one node (§4.16 "The verdict", "Splice", "Commit"; D-27). A green volume
//! is written only by its merge task. Agents clone a version, edit their clone, and `submit` an
//! increment of declared operations against a base version; the engine maps the increment's ranges
//! forward through everything committed since that base, decides the pure verdict, splices the
//! accepted edits into a new version, and commits it. Conflicts are returned as byte-exact windows
//! the agent rebases against; the merge task never stalls on a conflict.
//!
//! This is the laptop-degenerate engine (R8): one green, one merge task, an in-memory version
//! chain and a `seen` set for idempotent retries. The same pipeline runs in a fleet, where the
//! commit is a fenced ledger-register entry to the green's candidate holders (§4.8) and holders
//! recompute the verdict before serving — those are the engine's remaining pieces (owed). This
//! module is pure: it composes [`crate::map`] (position mapping), [`crate::verdict`] (the identity
//! check) and a byte splice; no I/O, no clock, no randomness.
//!
//! Scope: content merges (files' bytes). The verdict here is per file: an increment whose edited
//! ranges are disjoint from every intervening change is accepted and its edits re-applied at the
//! shifted positions; an increment whose ranges meet an intervening change is a conflict unless the
//! agent produced exactly the green's current bytes for that file (both made the same edit, which
//! accepts). Per-range identity within a mixed file, and the namespace merge (create, rename and
//! the rest through the pipeline), are owed.

use std::collections::BTreeMap;

use crate::ops_doc::{Op, OpKind};
use crate::range::Range;

/// One file's change in an increment: its net content ops (base coordinates, `src` into
/// `post_state`) and the file's final bytes (the sealed post-state for this path).
#[derive(Clone, Debug)]
pub struct PathChange {
  /// The net content ops, in the increment's base coordinates.
  pub ops: Vec<Op>,
  /// The file's final bytes.
  pub post_state: Vec<u8>,
}

/// An increment submitted against a base version: an identity (for idempotent retries), the base
/// version it was cloned from, and the per-path changes.
#[derive(Clone, Debug)]
pub struct Increment {
  /// The increment's identity (`blake3` of its declared work).
  pub id: [u8; 32],
  /// The version it was based on.
  pub base: u64,
  /// The changed files.
  pub changes: BTreeMap<String, PathChange>,
}

/// A byte-exact conflict window: the file and the range (in the increment's base coordinates) that
/// met an intervening change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictWindow {
  /// The file.
  pub path: String,
  /// The conflicting range.
  pub range: Range,
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

/// A green volume's merge state: the head content per file, the committed deltas (per version, the
/// ops that produced it, per path, for position mapping), the last version each path changed at
/// (the fast-path index), the `seen` set for idempotent retries, and the head version.
#[derive(Debug, Default)]
pub struct Green {
  content: BTreeMap<String, Vec<u8>>,
  deltas: Vec<BTreeMap<String, Vec<Op>>>,
  last_changed: BTreeMap<String, u64>,
  seen: BTreeMap<[u8; 32], Outcome>,
  fast_path_hits: u64,
}

/// The base range an op touches, for overlap testing: the covered span for an overwrite, delete or
/// truncate; a zero-length anchor for an insert or extend.
fn touched_range(op: &Op) -> Range {
  match op.kind {
    OpKind::Overwrite | OpKind::Delete | OpKind::Truncate => Range::new(op.at, op.len),
    _ => Range::new(op.at, 0),
  }
}

/// Applies content ops (in the given base coordinates, non-decreasing by `at`) to `base`, drawing
/// added bytes from `post_state` — the byte form of the splice, used to build the merged file.
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

  /// The number of paths merged through the fast path (no intervening change since the base). A
  /// test asserts this moves, so a silently-dead fast path cannot pass (the non-vacuity counter).
  pub fn fast_path_hits(&self) -> u64 {
    self.fast_path_hits
  }

  /// The ops each intervening delta in `(base, head]` applied to `path`, as the slice-of-slices the
  /// position mapper takes.
  fn intervening(&self, path: &str, base: u64) -> Vec<&[Op]> {
    let base = usize::try_from(base).unwrap_or(usize::MAX);
    self.deltas[base.min(self.deltas.len())..]
      .iter()
      .filter_map(|delta| delta.get(path).map(Vec::as_slice))
      .collect()
  }

  /// Merges one file's change onto the head, returning either the merged bytes and the ops to
  /// record (in head coordinates), or the first conflicting window.
  fn merge_path(
    &mut self,
    path: &str,
    base: u64,
    change: &PathChange,
  ) -> Result<(Vec<u8>, Vec<Op>), ConflictWindow> {
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
    let mut mapped = Vec::with_capacity(change.ops.len());
    for op in &change.ops {
      match crate::map::map_range(&intervening, touched_range(op)) {
        crate::map::Mapped::Shifted(shifted) => {
          let mut moved = *op;
          moved.at = shifted.start;
          mapped.push(moved);
        }
        crate::map::Mapped::Overlaps => {
          // Pass two: if the agent produced exactly the green's current bytes for this file, both
          // made the same edit — accept it as identical; otherwise it is a conflict.
          let current = self
            .content
            .get(path)
            .map(Vec::as_slice)
            .unwrap_or_default();
          if change.post_state == current {
            return Ok((current.to_vec(), Vec::new()));
          }
          return Err(ConflictWindow {
            path: path.to_owned(),
            range: touched_range(op),
          });
        }
      }
    }
    let empty = Vec::new();
    let current = self.content.get(path).unwrap_or(&empty);
    let merged = apply(current, &mapped, &change.post_state);
    Ok((merged, mapped))
  }

  /// Submits an increment: idempotent by identity, merges every changed file, and commits a new
  /// version when all accept, or returns the conflict windows and changes nothing.
  pub fn submit(&mut self, increment: &Increment) -> Outcome {
    if let Some(outcome) = self.seen.get(&increment.id) {
      return outcome.clone();
    }
    let mut merged_content: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut delta: BTreeMap<String, Vec<Op>> = BTreeMap::new();
    let mut windows = Vec::new();
    for (path, change) in &increment.changes {
      match self.merge_path(path, increment.base, change) {
        Ok((content, ops)) => {
          merged_content.insert(path.clone(), content);
          if !ops.is_empty() {
            delta.insert(path.clone(), ops);
          }
        }
        Err(window) => windows.push(window),
      }
    }
    if !windows.is_empty() {
      let outcome = Outcome::Conflict { windows };
      self.seen.insert(increment.id, outcome.clone());
      return outcome;
    }
    let version = self.head() + 1;
    for (path, content) in merged_content {
      self.content.insert(path.clone(), content);
      self.last_changed.insert(path, version);
    }
    self.deltas.push(delta);
    let outcome = Outcome::Accepted { version };
    self.seen.insert(increment.id, outcome.clone());
    outcome
  }
}
