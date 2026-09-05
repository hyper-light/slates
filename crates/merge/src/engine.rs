//! The merge engine on one node (§4.16 "The verdict", "Splice", "Commit"; D-27). A green volume
//! is written only by its merge task. Agents clone a version, edit their clone, seal it, and
//! `submit` an increment against a base version. An increment is the deriver's canonical ops
//! document (`crate::ops_doc::OpsDoc`, every §4.16 operation kind, paths interned, content named by
//! offsets into the sealed post-state) plus that post-state's bytes. The engine resolves the
//! document, maps each content edit forward through everything committed since the base, decides the
//! pure verdict, splices the accepted edits into a new version, and commits it. Conflicts are
//! byte-exact windows (each with its class) the agent rebases against; the merge task never stalls.
//!
//! Consuming the whole ops document (rather than one change per path) is what lets every operation
//! kind merge, and merge together: a file can be edited and chmod'd and xattr'd in one increment; a
//! rename carries the file's edits; a directory move is the deriver's child renames plus mkdir and
//! rmdir, so it merges with no special case here; a hard link is a namespace edge (its content
//! sharing is the volume's concern at apply time, §4.16, not the merge's). The deriver has already
//! composed each path's operations and resolved their intra-increment ordering, so the engine sees
//! a clean, canonical set and only has to decide it against the intervening history.
//!
//! The dimensions merge independently, the way the deriver composes them:
//! - **Content**: each net op's base range is mapped forward; a range disjoint from every
//!   intervening change is accepted at the shifted position, a range that meets one is a conflict
//!   unless the agent produced exactly the green's current bytes for that file (both made the same
//!   edit, which accepts).
//! - **Create / unlink**: a create conflicts with an intervening differing create, accepts an
//!   identical one, else creates; an unlink accepts (or is a no-op if already gone), and a
//!   delete/modify conflict guards an intervening edit.
//! - **Directory, mode, symlink, hard link, xattr**: each is a per-path (xattr: per path and name)
//!   value; two differing changes since the base conflict, an identical one accepts; a file and a
//!   non-file at one path is a type conflict; a directory is removed only once the increment's own
//!   removals empty it.
//!
//! This is the laptop-degenerate engine (R8): one green, one merge task, an in-memory version chain
//! and a `seen` set for idempotent retries. The same pipeline runs in a fleet, where the commit is
//! a fenced ledger-register entry to the green's candidate holders (§4.8, `crate::` is pure here)
//! and holders recompute the verdict before serving — those are the engine's remaining pieces
//! (owed). The per-path content history kept for base reconstruction is a full byte copy per change;
//! the design's chain shares it copy-on-write, the measured optimization (owed).

use std::collections::{BTreeMap, BTreeSet};

use crate::ops_doc::{Op, OpKind, OpsDoc};
use crate::range::Range;
use crate::verdict::MergeConflictClass;

/// An increment submitted against a base version: the deriver's ops document and the sealed
/// post-state its content operations' `src` offsets index into.
#[derive(Clone, Debug)]
pub struct Increment {
  /// The increment's identity (`blake3` of its declared work).
  pub id: [u8; 32],
  /// The version it was based on.
  pub base: u64,
  /// The canonical ops document (every declared operation, paths interned).
  pub doc: OpsDoc,
  /// The sealed post-state bytes; a content op adds `post_state[src .. src + len]`.
  pub post_state: Vec<u8>,
}

/// A conflict window: the file, the range (base coordinates) that met an intervening change, and
/// the class of the conflict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictWindow {
  /// The file.
  pub path: String,
  /// The conflicting range (a zero-length anchor for a whole-path namespace conflict).
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

/// A per-path content history: for each path, the versions at which its bytes changed and the bytes
/// then (`None` when removed), so any base version's content can be reconstructed for the identity
/// check. The design's chain shares these copy-on-write; here they are full copies (owed).
type ContentHistory = BTreeMap<String, Vec<(u64, Option<Vec<u8>>)>>;

/// A green volume's merge state. Each dimension is a current value plus the version it last changed
/// at (the base-comparison index); the content dimension also keeps a per-path byte history so a
/// file's bytes at an arbitrary base version can be reconstructed for the identity check.
#[derive(Debug, Default)]
pub struct Green {
  content: BTreeMap<String, Vec<u8>>,
  content_history: ContentHistory,
  dirs: BTreeSet<String>,
  modes: BTreeMap<String, u32>,
  symlinks: BTreeMap<String, String>,
  hardlinks: BTreeMap<String, String>,
  xattrs: BTreeMap<(String, String), Vec<u8>>,
  deltas: Vec<BTreeMap<String, Vec<Op>>>,
  last_changed: BTreeMap<String, u64>,
  mode_changed: BTreeMap<String, u64>,
  symlink_changed: BTreeMap<String, u64>,
  hardlink_changed: BTreeMap<String, u64>,
  xattr_changed: BTreeMap<(String, String), u64>,
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

/// Whether an op kind carries content bytes (its `src` names post-state bytes).
fn is_content(kind: OpKind) -> bool {
  matches!(
    kind,
    OpKind::Overwrite | OpKind::Extend | OpKind::Truncate | OpKind::Insert | OpKind::Delete
  )
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

/// The changes an increment declares, resolved from the ops document into per-dimension groups by
/// path (xattrs by path and name). The deriver has already composed each path's operations.
#[derive(Default)]
struct Resolved {
  content: BTreeMap<String, Vec<Op>>,
  creates: BTreeSet<String>,
  unlinks: BTreeSet<String>,
  mkdirs: BTreeSet<String>,
  rmdirs: BTreeSet<String>,
  modes: BTreeMap<String, u32>,
  symlinks: BTreeMap<String, String>,
  hardlinks: BTreeMap<String, String>,
  renames: BTreeMap<String, String>,
  xattr_sets: Vec<(String, String, Vec<u8>)>,
  xattr_removes: Vec<(String, String)>,
}

impl Resolved {
  /// The paths this increment removes (unlinks, rmdirs and rename sources) — a directory removal's
  /// emptiness check consults this so a directory emptied within the same increment is removable.
  fn cleared(&self) -> BTreeSet<String> {
    let mut cleared = self.unlinks.clone();
    cleared.extend(self.rmdirs.iter().cloned());
    cleared.extend(self.renames.values().cloned());
    cleared
  }
}

/// One accepted per-path effect to apply at commit.
enum Effect {
  /// Set the file to these bytes, recording these ops (head coordinates) as its content delta.
  SetContent(String, Vec<u8>, Vec<Op>),
  /// Remove the file.
  RemoveContent(String),
  /// Create a directory.
  MakeDir(String),
  /// Remove a directory.
  RemoveDir(String),
  /// Set a path's mode.
  Mode(String, u32),
  /// Create or retarget a symlink.
  Symlink(String, String),
  /// Create or retarget a hard link (a namespace edge to the target file).
  Hardlink(String, String),
  /// Set an xattr value.
  SetXattr(String, String, Vec<u8>),
  /// Remove an xattr.
  RemoveXattr(String, String),
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

  /// A symbolic link's target at the path, or `None` when it is not a symlink.
  pub fn symlink(&self, path: &str) -> Option<&str> {
    self.symlinks.get(path).map(String::as_str)
  }

  /// A hard link's target file at the path, or `None` when it is not a hard link.
  pub fn hardlink(&self, path: &str) -> Option<&str> {
    self.hardlinks.get(path).map(String::as_str)
  }

  /// An xattr value at `(path, name)`, or `None` when unset.
  pub fn xattr(&self, path: &str, name: &str) -> Option<&[u8]> {
    self
      .xattrs
      .get(&(path.to_owned(), name.to_owned()))
      .map(Vec::as_slice)
  }

  /// The number of paths merged through the content fast path (the non-vacuity counter).
  pub fn fast_path_hits(&self) -> u64 {
    self.fast_path_hits
  }

  /// A file's bytes at `version`, reconstructed from its history — the latest recorded value at or
  /// before `version`, or `None` when the file was absent then.
  fn content_at(&self, path: &str, version: u64) -> Option<Vec<u8>> {
    self.content_history.get(path).and_then(|history| {
      history
        .iter()
        .rev()
        .find(|(recorded, _)| *recorded <= version)
        .and_then(|(_, bytes)| bytes.clone())
    })
  }

  /// The ops each intervening delta in `(base, head]` applied to `path`.
  fn intervening(&self, path: &str, base: u64) -> Vec<&[Op]> {
    let base = usize::try_from(base).unwrap_or(usize::MAX);
    self.deltas[base.min(self.deltas.len())..]
      .iter()
      .filter_map(|delta| delta.get(path).map(Vec::as_slice))
      .collect()
  }

  /// A whole-path namespace conflict window.
  fn window(path: &str, class: MergeConflictClass) -> ConflictWindow {
    ConflictWindow {
      path: path.to_owned(),
      range: Range::new(0, 0),
      class,
    }
  }

  /// Resolves an increment's ops document into per-dimension groups by path.
  fn resolve(inc: &Increment) -> Resolved {
    let mut r = Resolved::default();
    // Resolve a path-table reference (given as a `u64` field) to its name.
    let name = |index: u64| {
      inc
        .doc
        .paths
        .path(u16::try_from(index).unwrap_or(u16::MAX))
        .unwrap_or_default()
        .to_owned()
    };
    for op in &inc.doc.ops {
      let path = name(u64::from(op.path));
      match op.kind {
        _ if is_content(op.kind) => r.content.entry(path).or_default().push(*op),
        OpKind::Create => {
          r.creates.insert(path);
        }
        OpKind::Unlink => {
          r.unlinks.insert(path);
        }
        OpKind::Mkdir => {
          r.mkdirs.insert(path);
        }
        OpKind::Rmdir => {
          r.rmdirs.insert(path);
        }
        OpKind::SetMode => {
          r.modes
            .insert(path, u32::try_from(op.len).unwrap_or(u32::MAX));
        }
        OpKind::Symlink => {
          r.symlinks.insert(path, name(op.src));
        }
        OpKind::Link => {
          r.hardlinks.insert(path, name(op.src));
        }
        OpKind::Rename => {
          r.renames.insert(path, name(op.src));
        }
        OpKind::SetXattr => {
          let value = post_slice(&inc.post_state, op.src, op.len);
          r.xattr_sets.push((path, name(op.at), value));
        }
        OpKind::RemoveXattr => r.xattr_removes.push((path, name(op.at))),
        OpKind::Overwrite | OpKind::Extend | OpKind::Truncate | OpKind::Insert | OpKind::Delete => {
        }
      }
    }
    r
  }

  /// Whether any file, directory, symlink or hard link under `prefix` survives `cleared`.
  fn has_live_child(&self, prefix: &str, cleared: &BTreeSet<String>) -> bool {
    let live = |path: &String| path.starts_with(prefix) && !cleared.contains(path);
    self.content.keys().any(live)
      || self.dirs.iter().any(live)
      || self.symlinks.keys().any(live)
      || self.hardlinks.keys().any(live)
  }

  /// Whether an intervening change occupied `path` with any non-file kind.
  fn occupied_by_nonfile(&self, path: &str) -> bool {
    self.dirs.contains(path)
      || self.symlinks.contains_key(path)
      || self.hardlinks.contains_key(path)
  }

  /// Merges a file rename to `dst` from `from` (source content captured at merge time), collecting
  /// the destination set and the source removal.
  fn merge_rename(
    &self,
    dst: &str,
    base: u64,
    from: &str,
    effects: &mut Vec<Effect>,
  ) -> Result<(), ConflictWindow> {
    if from == dst {
      return Ok(());
    }
    let Some(source) = self.content.get(from) else {
      return Err(Green::window(dst, MergeConflictClass::RenameRename));
    };
    if self.occupied_by_nonfile(dst) {
      return Err(Green::window(dst, MergeConflictClass::TypeChanged));
    }
    if self.last_changed.get(dst).copied().unwrap_or(0) > base {
      return Err(Green::window(dst, MergeConflictClass::RenameRename));
    }
    let ops = vec![insert_whole(source.len() as u64)];
    effects.push(Effect::SetContent(dst.to_owned(), source.clone(), ops));
    effects.push(Effect::RemoveContent(from.to_owned()));
    Ok(())
  }

  /// Merges a create at `path` with `bytes`.
  fn merge_create(&self, path: &str, bytes: Vec<u8>) -> Result<Option<Effect>, ConflictWindow> {
    if self.occupied_by_nonfile(path) {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    match self.content.get(path) {
      Some(current) if current.as_slice() == bytes.as_slice() => Ok(None),
      Some(_) => Err(Green::window(path, MergeConflictClass::CreateCreate)),
      None => {
        let ops = vec![insert_whole(bytes.len() as u64)];
        Ok(Some(Effect::SetContent(path.to_owned(), bytes, ops)))
      }
    }
  }

  /// Merges an unlink at `path`.
  fn merge_unlink(&self, path: &str, base: u64) -> Result<Option<Effect>, ConflictWindow> {
    if !self.content.contains_key(path) {
      return Ok(None); // already gone (or never a file)
    }
    if self.last_changed.get(path).copied().unwrap_or(0) > base {
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    }
    Ok(Some(Effect::RemoveContent(path.to_owned())))
  }

  /// Merges an edit to an existing file (the content path): each net op is mapped forward, disjoint
  /// ops are accepted at the shifted position, and an op that meets an intervening change is a
  /// conflict unless the agent produced exactly the green's current bytes for the file.
  fn merge_content(
    &mut self,
    inc: &Increment,
    path: &str,
    ops: &[Op],
  ) -> Result<Option<Effect>, ConflictWindow> {
    if !self.content.contains_key(path) {
      if self.occupied_by_nonfile(path) {
        return Err(Green::window(path, MergeConflictClass::TypeChanged));
      }
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    }
    let base_changed = self.last_changed.get(path).copied().unwrap_or(0);
    let unchanged = base_changed <= inc.base;
    if unchanged {
      self.fast_path_hits = self.fast_path_hits.saturating_add(1);
    }
    let intervening = if unchanged {
      Vec::new()
    } else {
      self.intervening(path, inc.base)
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
          // Both agents may have made the identical edit: accept when the agent's final bytes for
          // the file equal the green's current bytes (base reconstructed from history).
          let base_bytes = self.content_at(path, inc.base).unwrap_or_default();
          let agent_final = apply(&base_bytes, ops, &inc.post_state);
          let current = self
            .content
            .get(path)
            .map(Vec::as_slice)
            .unwrap_or_default();
          if agent_final == current {
            return Ok(None);
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
    let next = apply(current, &mapped, &inc.post_state);
    Ok(Some(Effect::SetContent(path.to_owned(), next, mapped)))
  }

  /// Merges a directory creation.
  fn merge_mkdir(&self, path: &str) -> Result<Option<Effect>, ConflictWindow> {
    if self.content.contains_key(path)
      || self.symlinks.contains_key(path)
      || self.hardlinks.contains_key(path)
    {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    if self.dirs.contains(path) {
      return Ok(None);
    }
    Ok(Some(Effect::MakeDir(path.to_owned())))
  }

  /// Merges a directory removal against the increment's own removals.
  fn merge_rmdir(
    &self,
    path: &str,
    cleared: &BTreeSet<String>,
  ) -> Result<Option<Effect>, ConflictWindow> {
    if self.content.contains_key(path) || self.symlinks.contains_key(path) {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    if !self.dirs.contains(path) {
      return Ok(None);
    }
    let prefix = format!("{path}/");
    if self.has_live_child(&prefix, cleared) {
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    }
    Ok(Some(Effect::RemoveDir(path.to_owned())))
  }

  /// Merges a mode change.
  fn merge_setmode(
    &self,
    path: &str,
    base: u64,
    mode: u32,
  ) -> Result<Option<Effect>, ConflictWindow> {
    if !self.content.contains_key(path) && !self.dirs.contains(path) {
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    }
    if self.mode_changed.get(path).copied().unwrap_or(0) > base {
      if self.modes.get(path).copied() == Some(mode) {
        return Ok(None);
      }
      return Err(Green::window(path, MergeConflictClass::MetaMeta));
    }
    Ok(Some(Effect::Mode(path.to_owned(), mode)))
  }

  /// Merges a symbolic link.
  fn merge_symlink(
    &self,
    path: &str,
    base: u64,
    target: &str,
  ) -> Result<Option<Effect>, ConflictWindow> {
    if self.content.contains_key(path) || self.dirs.contains(path) {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    if self.symlink_changed.get(path).copied().unwrap_or(0) > base {
      if self.symlinks.get(path).map(String::as_str) == Some(target) {
        return Ok(None);
      }
      return Err(Green::window(path, MergeConflictClass::CreateCreate));
    }
    Ok(Some(Effect::Symlink(path.to_owned(), target.to_owned())))
  }

  /// Merges a hard link (a namespace edge; a differing intervening link is a conflict).
  fn merge_hardlink(
    &self,
    path: &str,
    base: u64,
    target: &str,
  ) -> Result<Option<Effect>, ConflictWindow> {
    if self.content.contains_key(path) || self.dirs.contains(path) {
      return Err(Green::window(path, MergeConflictClass::TypeChanged));
    }
    if self.hardlink_changed.get(path).copied().unwrap_or(0) > base {
      if self.hardlinks.get(path).map(String::as_str) == Some(target) {
        return Ok(None);
      }
      return Err(Green::window(path, MergeConflictClass::CreateCreate));
    }
    Ok(Some(Effect::Hardlink(path.to_owned(), target.to_owned())))
  }

  /// Merges an xattr set; a differing intervening value for the same `(path, name)` conflicts.
  fn merge_set_xattr(
    &self,
    path: &str,
    name: &str,
    base: u64,
    value: &[u8],
  ) -> Result<Option<Effect>, ConflictWindow> {
    if !self.content.contains_key(path) && !self.dirs.contains(path) {
      return Err(Green::window(path, MergeConflictClass::DeleteModify));
    }
    let key = (path.to_owned(), name.to_owned());
    if self.xattr_changed.get(&key).copied().unwrap_or(0) > base {
      if self.xattrs.get(&key).map(Vec::as_slice) == Some(value) {
        return Ok(None);
      }
      return Err(Green::window(path, MergeConflictClass::MetaMeta));
    }
    Ok(Some(Effect::SetXattr(
      path.to_owned(),
      name.to_owned(),
      value.to_vec(),
    )))
  }

  /// Merges an xattr removal.
  fn merge_remove_xattr(
    &self,
    path: &str,
    name: &str,
    base: u64,
  ) -> Result<Option<Effect>, ConflictWindow> {
    let key = (path.to_owned(), name.to_owned());
    if !self.xattrs.contains_key(&key) {
      return Ok(None);
    }
    if self.xattr_changed.get(&key).copied().unwrap_or(0) > base {
      return Err(Green::window(path, MergeConflictClass::MetaMeta));
    }
    Ok(Some(Effect::RemoveXattr(path.to_owned(), name.to_owned())))
  }

  /// Decides every dimension of an increment, collecting the accepted effects or the conflicts.
  fn decide(&mut self, inc: &Increment, r: &Resolved) -> Result<Vec<Effect>, Vec<ConflictWindow>> {
    let mut effects = Vec::new();
    let mut windows = Vec::new();
    self.decide_content(inc, r, &mut effects, &mut windows);
    self.decide_directories(r, &mut effects, &mut windows);
    self.decide_metadata(inc, r, &mut effects, &mut windows);
    if windows.is_empty() {
      Ok(effects)
    } else {
      Err(windows)
    }
  }

  /// The content dimension: renames, creates, unlinks and edits.
  fn decide_content(
    &mut self,
    inc: &Increment,
    r: &Resolved,
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    for (dst, from) in &r.renames {
      if let Err(window) = self.merge_rename(dst, inc.base, from, effects) {
        windows.push(window);
      }
    }
    for path in &r.creates {
      let content = r.content.get(path).map(Vec::as_slice).unwrap_or(&[]);
      let bytes = apply(&[], content, &inc.post_state);
      record(effects, windows, self.merge_create(path, bytes));
    }
    for path in &r.unlinks {
      record(effects, windows, self.merge_unlink(path, inc.base));
    }
    for (path, ops) in &r.content {
      if r.creates.contains(path) || r.renames.contains_key(path) {
        continue; // content of a created or renamed-into file is handled with that effect
      }
      let result = self.merge_content(inc, path, ops);
      record(effects, windows, result);
    }
  }

  /// The directory dimension: creation and removal (removal against the increment's own removals).
  fn decide_directories(
    &self,
    r: &Resolved,
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    let cleared = r.cleared();
    for path in &r.mkdirs {
      record(effects, windows, self.merge_mkdir(path));
    }
    for path in &r.rmdirs {
      record(effects, windows, self.merge_rmdir(path, &cleared));
    }
  }

  /// The metadata dimensions: mode, symlink, hard link and xattrs.
  fn decide_metadata(
    &self,
    inc: &Increment,
    r: &Resolved,
    effects: &mut Vec<Effect>,
    windows: &mut Vec<ConflictWindow>,
  ) {
    for (path, mode) in &r.modes {
      record(effects, windows, self.merge_setmode(path, inc.base, *mode));
    }
    for (path, target) in &r.symlinks {
      record(effects, windows, self.merge_symlink(path, inc.base, target));
    }
    for (path, target) in &r.hardlinks {
      record(
        effects,
        windows,
        self.merge_hardlink(path, inc.base, target),
      );
    }
    for (path, name, value) in &r.xattr_sets {
      record(
        effects,
        windows,
        self.merge_set_xattr(path, name, inc.base, value),
      );
    }
    for (path, name) in &r.xattr_removes {
      record(
        effects,
        windows,
        self.merge_remove_xattr(path, name, inc.base),
      );
    }
  }

  /// Applies the accepted effects, committing version `version`.
  fn commit(&mut self, effects: Vec<Effect>, version: u64) {
    let mut delta: BTreeMap<String, Vec<Op>> = BTreeMap::new();
    let mut removed_content: Vec<String> = Vec::new();
    let mut set_content: Vec<(String, Vec<u8>, Vec<Op>)> = Vec::new();
    // Content removes are staged before sets so a rename's source removal never deletes a file the
    // same increment recreates or renames into at that path.
    for effect in effects {
      match effect {
        Effect::SetContent(path, bytes, ops) => set_content.push((path, bytes, ops)),
        Effect::RemoveContent(path) => removed_content.push(path),
        Effect::MakeDir(path) => {
          self.dirs.insert(path);
        }
        Effect::RemoveDir(path) => {
          self.dirs.remove(&path);
        }
        Effect::Mode(path, mode) => {
          self.modes.insert(path.clone(), mode);
          self.mode_changed.insert(path, version);
        }
        Effect::Symlink(path, target) => {
          self.symlinks.insert(path.clone(), target);
          self.symlink_changed.insert(path, version);
        }
        Effect::Hardlink(path, target) => {
          self.hardlinks.insert(path.clone(), target);
          self.hardlink_changed.insert(path, version);
        }
        Effect::SetXattr(path, name, value) => {
          let key = (path, name);
          self.xattrs.insert(key.clone(), value);
          self.xattr_changed.insert(key, version);
        }
        Effect::RemoveXattr(path, name) => {
          let key = (path, name);
          self.xattrs.remove(&key);
          self.xattr_changed.insert(key, version);
        }
      }
    }
    for path in removed_content {
      self.content.remove(&path);
      self.last_changed.insert(path.clone(), version);
      self
        .content_history
        .entry(path)
        .or_default()
        .push((version, None));
    }
    for (path, bytes, ops) in set_content {
      self.content.insert(path.clone(), bytes.clone());
      self.last_changed.insert(path.clone(), version);
      self
        .content_history
        .entry(path.clone())
        .or_default()
        .push((version, Some(bytes)));
      delta.insert(path, ops);
    }
    self.deltas.push(delta);
  }

  /// Submits an increment: idempotent by identity, decides every dimension of its ops document, and
  /// commits a new version when all accept, or returns the conflict windows and changes nothing.
  pub fn submit(&mut self, inc: &Increment) -> Outcome {
    if let Some(outcome) = self.seen.get(&inc.id) {
      return outcome.clone();
    }
    let resolved = Green::resolve(inc);
    let outcome = match self.decide(inc, &resolved) {
      Ok(effects) => {
        let version = self.head() + 1;
        self.commit(effects, version);
        Outcome::Accepted { version }
      }
      Err(windows) => Outcome::Conflict { windows },
    };
    self.seen.insert(inc.id, outcome.clone());
    outcome
  }
}

/// Files one per-dimension merge result into the accepted effects or the conflict windows.
fn record(
  effects: &mut Vec<Effect>,
  windows: &mut Vec<ConflictWindow>,
  result: Result<Option<Effect>, ConflictWindow>,
) {
  match result {
    Ok(Some(effect)) => effects.push(effect),
    Ok(None) => {}
    Err(window) => windows.push(window),
  }
}

/// An `Insert` op that adds a whole file's bytes at offset zero from post-state offset zero (used
/// for a create or a rename's destination, whose bytes are supplied directly, not by `src`).
fn insert_whole(len: u64) -> Op {
  Op {
    kind: OpKind::Insert,
    flags: 0,
    path: 0,
    at: 0,
    len,
    src: 0,
  }
}

/// `post_state[src .. src + len]`, clamped to bounds.
fn post_slice(post_state: &[u8], src: u64, len: u64) -> Vec<u8> {
  let start = usize::try_from(src)
    .unwrap_or(usize::MAX)
    .min(post_state.len());
  let end = start
    .saturating_add(usize::try_from(len).unwrap_or(0))
    .min(post_state.len());
  post_state[start..end].to_vec()
}
