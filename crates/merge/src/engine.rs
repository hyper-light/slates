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
#[derive(Clone, Debug, PartialEq, Eq)]
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

impl Increment {
  /// Serializes the increment for the durable green chain (§4.16, §4.8): the identity, the base
  /// version, then the ops document and the post-state, each length-delimited. The db stores these
  /// bytes opaquely (it never parses a merge structure); the server encodes here on commit and
  /// decodes on recovery. Little-endian and length-delimited so [`Increment::decode`] is exact.
  pub fn encode(&self) -> Vec<u8> {
    let doc = self.doc.encode();
    let mut out = Vec::with_capacity(
      self.id.len()
        + size_of::<u64>()
        + size_of::<u64>()
        + doc.len()
        + size_of::<u64>()
        + self.post_state.len(),
    );
    out.extend_from_slice(&self.id);
    out.extend_from_slice(&self.base.to_le_bytes());
    out.extend_from_slice(&(doc.len() as u64).to_le_bytes());
    out.extend_from_slice(&doc);
    out.extend_from_slice(&(self.post_state.len() as u64).to_le_bytes());
    out.extend_from_slice(&self.post_state);
    out
  }

  /// Decodes an increment persisted by [`Increment::encode`] — the inverse used to replay a green's
  /// chain on recovery. Every length is bounds-checked against the bytes that remain before it is
  /// read (a torn db entry never allocates a wild length), and any malformation is a typed
  /// [`DocDecodeError`], never a panic.
  pub fn decode(bytes: &[u8]) -> Result<Increment, crate::ops_doc::DocDecodeError> {
    use crate::ops_doc::{DocDecodeError, OpsDoc, Reader};
    let mut reader = Reader::new(bytes);
    let mut id = [0u8; 32];
    let id_len = id.len();
    id.copy_from_slice(reader.bytes(id_len)?);
    let base = reader.u64()?;
    let doc_len = usize::try_from(reader.u64()?).map_err(|_| DocDecodeError::Truncated)?;
    let doc = OpsDoc::decode(reader.bytes(doc_len)?)?;
    let post_len = usize::try_from(reader.u64()?).map_err(|_| DocDecodeError::Truncated)?;
    let post_state = reader.bytes(post_len)?.to_vec();
    if !reader.is_empty() {
      return Err(DocDecodeError::TrailingBytes);
    }
    Ok(Increment {
      id,
      base,
      doc,
      post_state,
    })
  }
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

/// The result of a rebase (§4.16 "Rebase, the only corrective path"). The green is never changed by
/// a rebase; only the work moves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rebased {
  /// The work's pending operations all mapped cleanly onto the head. The work's base becomes
  /// `version`, its full content becomes `files` (the head's content with the work's mapped edits
  /// re-applied), and its declared operations are restated in head coordinates as `journal` (so the
  /// next submit composes against the new base with no further mapping).
  Rebased {
    /// The head the work is now based on.
    version: u64,
    /// The work's full content per path after the rebase (the head's files with the work's edits).
    files: BTreeMap<String, Vec<u8>>,
    /// The work's declared operations restated relative to the new base.
    journal: Vec<crate::increment::VolumeOp>,
  },
  /// The pending operations conflict; the windows to resolve, nothing changed.
  Conflict {
    /// The conflicting windows.
    windows: Vec<ConflictWindow>,
  },
}

/// A per-path content history: for each path, the versions at which its bytes changed and the bytes
/// then (`None` when removed), so any base version's content can be reconstructed for the identity
/// check. The design's chain shares these copy-on-write; here they are full copies (owed).
type ContentHistory = BTreeMap<String, Vec<(u64, Option<Vec<u8>>)>>;

/// A per-path value history for a namespace dimension: the value at each version the path changed,
/// `None` marking a removal (mode, symlink target, hard-link target). Reconstructs a base version.
type ValueHistory<T> = BTreeMap<String, Vec<(u64, Option<T>)>>;

/// A per-`(path, name)` xattr value history: the value at each version it changed, `None` a removal.
type XattrHistory = BTreeMap<(String, String), Vec<(u64, Option<Vec<u8>>)>>;

/// A per-path directory-presence history: whether the directory existed at each version it changed.
type DirHistory = BTreeMap<String, Vec<(u64, bool)>>;

/// A green volume's merge state. Each dimension is a current value plus the version it last changed
/// at (the base-comparison index), and a per-path/key *history* of `(version, value)` entries so any
/// base version can be reconstructed for a lagging work (the design's chain shares these histories
/// copy-on-write; here they are full copies, the measured optimization owed). The content dimension's
/// history holds the bytes; the namespace dimensions' histories hold presence or the value.
#[derive(Debug, Default)]
pub struct Green {
  content: BTreeMap<String, Vec<u8>>,
  content_history: ContentHistory,
  dirs: BTreeSet<String>,
  modes: BTreeMap<String, u32>,
  symlinks: BTreeMap<String, String>,
  hardlinks: BTreeMap<String, String>,
  xattrs: BTreeMap<(String, String), Vec<u8>>,
  dir_history: DirHistory,
  mode_history: ValueHistory<u32>,
  symlink_history: ValueHistory<String>,
  hardlink_history: ValueHistory<String>,
  xattr_history: XattrHistory,
  deltas: Vec<BTreeMap<String, Vec<Op>>>,
  last_changed: BTreeMap<String, u64>,
  mode_changed: BTreeMap<String, u64>,
  symlink_changed: BTreeMap<String, u64>,
  hardlink_changed: BTreeMap<String, u64>,
  xattr_changed: BTreeMap<(String, String), u64>,
  seen: BTreeMap<[u8; 32], Outcome>,
  fast_path_hits: u64,
}

/// The value of a `(version, Option<value>)` history at `version`: the last entry recorded at or
/// before it, cloned. `None` when nothing was recorded by then or the last entry was a removal.
fn history_at<T: Clone>(history: &[(u64, Option<T>)], version: u64) -> Option<T> {
  history
    .iter()
    .rev()
    .find(|(recorded, _)| *recorded <= version)
    .and_then(|(_, value)| value.clone())
}

/// Whether a `(version, present)` history has the entity present at `version`: the last entry at or
/// before it says so (absent when nothing was recorded by then).
fn present_at(history: &[(u64, bool)], version: u64) -> bool {
  history
    .iter()
    .rev()
    .find(|(recorded, _)| *recorded <= version)
    .is_some_and(|(_, present)| *present)
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
  out.extend_from_slice(span_bytes(from, src, len));
}

/// The `len` bytes at `src` of `from`, clamped to `from`'s bounds (an op's declared bytes in a
/// post-state). Used by the per-range verdict's byte check and by [`apply`]'s splice.
fn span_bytes(from: &[u8], src: u64, len: u64) -> &[u8] {
  let start = usize::try_from(src).unwrap_or(usize::MAX).min(from.len());
  let end = start
    .saturating_add(usize::try_from(len).unwrap_or(0))
    .min(from.len());
  &from[start..end]
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

  /// Whether this increment itself brings `path` into being — a file it creates or renames into, or a
  /// directory it makes. The metadata dimensions (mode, xattr) consult this so a file created and
  /// chmod'd (or xattr'd) in one increment is not a delete/modify conflict against the not-yet-
  /// committed path: the deriver composed them into one increment, so they belong together.
  fn establishes(&self, path: &str) -> bool {
    self.creates.contains(path) || self.mkdirs.contains(path) || self.renames.contains_key(path)
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
  /// Remove a symlink (its path is unlinked).
  RemoveSymlink(String),
  /// Create or retarget a hard link (a namespace edge to the target file).
  Hardlink(String, String),
  /// Remove a hard link's name (the shared file's fate is the volume's concern at apply time).
  RemoveHardlink(String),
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

  /// The green's current state as a deriver [`Base`](crate::increment::Base) — every file with its
  /// content length, and the directories, modes, symlinks, hard links and xattrs — so an increment
  /// based on the head can be composed against what the green actually holds. (An *older* version's
  /// base, for a work that lagged behind an intervening submit, is reconstructed by [`Green::base_at`];
  /// symlinks, hard links and xattrs at an older version are still owed there.)
  pub fn current_base(&self) -> crate::increment::Base {
    crate::increment::Base {
      files: self
        .content
        .iter()
        .map(|(path, bytes)| (path.clone(), bytes.len() as u64))
        .collect(),
      dirs: self.dirs.iter().cloned().collect(),
      modes: self
        .modes
        .iter()
        .map(|(path, mode)| (path.clone(), *mode))
        .collect(),
      symlinks: self
        .symlinks
        .iter()
        .map(|(path, target)| (path.clone(), target.clone()))
        .collect(),
      xattrs: self
        .xattrs
        .iter()
        .map(|((path, name), value)| (path.clone(), name.clone(), value.clone()))
        .collect(),
      hardlinks: self
        .hardlinks
        .iter()
        .map(|(path, target)| (path.clone(), target.clone()))
        .collect(),
    }
  }

  /// A file's current bytes, or `None` when it is absent.
  pub fn content(&self, path: &str) -> Option<&[u8]> {
    self.content.get(path).map(Vec::as_slice)
  }

  /// Every file the green currently holds, with its bytes — so a new work volume can be seeded with
  /// the base content it inherits and edits splice against it.
  pub fn files(&self) -> impl Iterator<Item = (&str, &[u8])> {
    self
      .content
      .iter()
      .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
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

  /// The files whose content changed strictly after `version` (§4.16 the last-changed index): what a
  /// lagging work must reconcile, and what a rebase remaps against. Sorted by path.
  pub fn changed_since(&self, version: u64) -> Vec<String> {
    self
      .last_changed
      .iter()
      .filter(|(_, changed)| **changed > version)
      .map(|(path, _)| path.clone())
      .collect()
  }

  /// The number of paths merged through the content fast path (the non-vacuity counter).
  pub fn fast_path_hits(&self) -> u64 {
    self.fast_path_hits
  }

  /// The green's state at an older `version` as a deriver [`Base`](crate::increment::Base), for a
  /// work whose base lagged behind an intervening submit. Files come from the content history (exact,
  /// renames and deletes included); directories and modes are replayed from the deltas up to the
  /// version. Symlinks, hard links and xattrs at an older version are owed — reconstructed empty here,
  /// which is exact for the file-and-directory workflows and conservative otherwise.
  pub fn base_at(&self, version: u64) -> crate::increment::Base {
    if version >= self.head() {
      return self.current_base();
    }
    // Each dimension is reconstructed from its own `(version, value)` history: the last entry at or
    // before `version` is that dimension's state then. Files come from the content history (a path
    // absent or removed by then yields no entry); directories from a presence history; modes,
    // symlinks, hard links and xattrs from a value history (a removal is a `None` entry).
    let files = self
      .content_history
      .keys()
      .filter_map(|path| {
        self
          .content_at(path, version)
          .map(|bytes| (path.clone(), bytes.len() as u64))
      })
      .collect();
    let dirs = self
      .dir_history
      .iter()
      .filter(|(_, history)| present_at(history, version))
      .map(|(path, _)| path.clone())
      .collect();
    let modes = self
      .mode_history
      .iter()
      .filter_map(|(path, history)| history_at(history, version).map(|mode| (path.clone(), mode)))
      .collect();
    let symlinks = self
      .symlink_history
      .iter()
      .filter_map(|(path, history)| {
        history_at(history, version).map(|target| (path.clone(), target))
      })
      .collect();
    let hardlinks = self
      .hardlink_history
      .iter()
      .filter_map(|(path, history)| {
        history_at(history, version).map(|target| (path.clone(), target))
      })
      .collect();
    let xattrs = self
      .xattr_history
      .iter()
      .filter_map(|((path, name), history)| {
        history_at(history, version).map(|value| (path.clone(), name.clone(), value))
      })
      .collect();
    crate::increment::Base {
      files,
      dirs,
      modes,
      symlinks,
      xattrs,
      hardlinks,
    }
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
    // The source must still name something — a file, a symlink or a hard link. A source an
    // intervening change renamed away or removed is a rename/rename conflict (checked first, so a
    // vanished source is named even when the destination is also occupied).
    let source_is_file = self.content.contains_key(from);
    let source_exists =
      source_is_file || self.symlinks.contains_key(from) || self.hardlinks.contains_key(from);
    if !source_exists {
      return Err(Green::window(dst, MergeConflictClass::RenameRename));
    }
    // The destination must be free: a non-file already there is a type change; a file the base did
    // not still hold (changed since base) is a rename/rename. An unchanged base file at the
    // destination is replaced (the design's rename-over-a-base-file case).
    if self.occupied_by_nonfile(dst) {
      return Err(Green::window(dst, MergeConflictClass::TypeChanged));
    }
    if self.last_changed.get(dst).copied().unwrap_or(0) > base {
      return Err(Green::window(dst, MergeConflictClass::RenameRename));
    }
    // Move whatever the source names, capturing its current value so an intervening change to the
    // source follows the move, and removing the source name.
    if let Some(source) = self.content.get(from) {
      let ops = vec![insert_whole(source.len() as u64)];
      effects.push(Effect::SetContent(dst.to_owned(), source.clone(), ops));
      effects.push(Effect::RemoveContent(from.to_owned()));
    } else if let Some(target) = self.symlinks.get(from) {
      effects.push(Effect::Symlink(dst.to_owned(), target.clone()));
      effects.push(Effect::RemoveSymlink(from.to_owned()));
    } else if let Some(target) = self.hardlinks.get(from) {
      effects.push(Effect::Hardlink(dst.to_owned(), target.clone()));
      effects.push(Effect::RemoveHardlink(from.to_owned()));
    }
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
  /// Merges an unlink of whatever the path names — a regular file, a symlink, or a hard link
  /// (§4.16). Each is a delete/modify conflict if that dimension changed since the increment's base;
  /// a path that names nothing is a no-op (idempotent removal). A directory is not unlinked (rmdir
  /// removes directories); a path that is a directory falls through to the no-op.
  fn merge_unlink(&self, path: &str, base: u64) -> Result<Option<Effect>, ConflictWindow> {
    if self.content.contains_key(path) {
      if self.last_changed.get(path).copied().unwrap_or(0) > base {
        return Err(Green::window(path, MergeConflictClass::DeleteModify));
      }
      return Ok(Some(Effect::RemoveContent(path.to_owned())));
    }
    if self.symlinks.contains_key(path) {
      if self.symlink_changed.get(path).copied().unwrap_or(0) > base {
        return Err(Green::window(path, MergeConflictClass::DeleteModify));
      }
      return Ok(Some(Effect::RemoveSymlink(path.to_owned())));
    }
    if self.hardlinks.contains_key(path) {
      if self.hardlink_changed.get(path).copied().unwrap_or(0) > base {
        return Err(Green::window(path, MergeConflictClass::DeleteModify));
      }
      return Ok(Some(Effect::RemoveHardlink(path.to_owned())));
    }
    Ok(None) // already gone (or never present)
  }

  /// Merges an edit to an existing file (the content path), by the two pure passes of §4.16 / D-27.
  /// Pass one maps each net op forward: an op no intervening change touched accepts at its shifted
  /// position; an op that meets an intervening change is a same-range candidate. Pass two is a
  /// memcmp **of that span alone** — the agent's declared bytes for the span against the green's
  /// current bytes there — so an identical (convergent) edit accepts as a no-op while a genuine
  /// divergence is a byte-exact conflict. The span-only check is the fix for the whole-file
  /// identity check's bug: a disjoint edit elsewhere in the file no longer poisons an identical
  /// overlap (T-6.x). A length-changing op (insert/delete/truncate) that overlaps is not yet decided
  /// per range — its coordinate mapping under a conflicting neighbour is owed — so the whole-file
  /// identity check still stands for the path in that case, exactly as before (never a regression).
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
    // Pass one: classify each op. Disjoint ops go to `mapped` (accept at the shifted position);
    // overlapping overwrites become same-range candidates; a length-changing op that overlaps falls
    // the path back to the whole-file identity check (owed).
    let mut mapped = Vec::with_capacity(ops.len());
    let mut candidates: Vec<&Op> = Vec::new();
    let mut shifting_overlap: Option<Range> = None;
    for op in ops {
      match crate::map::map_range(&intervening, touched_range(op)) {
        crate::map::Mapped::Shifted(shifted) => {
          let mut moved = *op;
          moved.at = shifted.start;
          mapped.push(moved);
        }
        crate::map::Mapped::Overlaps => {
          if op.kind == OpKind::Overwrite {
            candidates.push(op);
          } else {
            shifting_overlap.get_or_insert(touched_range(op));
          }
        }
      }
    }

    // A length-changing op overlapped: the whole-file identity check decides the path (owed: full
    // per-range identity for shifting ops), so a currently-accepted convergent case never regresses.
    if let Some(range) = shifting_overlap {
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
        range,
        class: MergeConflictClass::Overlap,
      });
    }

    // Pass two: each overlapping overwrite must reproduce, byte for byte, what the intervening
    // change already placed at that span; a differing span is a byte-exact conflict. An identical
    // span is a no-op (the green already holds those bytes), so it is not added to `mapped`.
    for op in &candidates {
      let span = touched_range(op);
      let head_start = match crate::map::map_range(&intervening, Range::new(span.start, 0)) {
        crate::map::Mapped::Shifted(head) => head.start,
        // A zero-length point cannot overlap (the edge rule), so this arm is unreachable; take the
        // base offset rather than panic.
        crate::map::Mapped::Overlaps => span.start,
      };
      let agent_bytes = span_bytes(&inc.post_state, op.src, op.len);
      let current = self
        .content
        .get(path)
        .map(Vec::as_slice)
        .unwrap_or_default();
      let start = usize::try_from(head_start).unwrap_or(usize::MAX);
      let end = start.saturating_add(agent_bytes.len());
      let identical = end <= current.len() && &current[start..end] == agent_bytes;
      if !identical {
        return Err(ConflictWindow {
          path: path.to_owned(),
          range: span,
          class: MergeConflictClass::Overlap,
        });
      }
    }

    // Every op accepted. When only identical overlaps remained (nothing disjoint to apply), the
    // green already holds the result: accept with no new effect, as an identical edit always has.
    if mapped.is_empty() {
      return Ok(None);
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
    if self.content.contains_key(path)
      || self.symlinks.contains_key(path)
      || self.hardlinks.contains_key(path)
    {
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
    established: bool,
  ) -> Result<Option<Effect>, ConflictWindow> {
    if !self.content.contains_key(path) && !self.dirs.contains(path) && !established {
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
    // A file, directory or hard link already at this path is a different kind — a type conflict. An
    // existing symlink is not: that is the retarget case decided just below.
    if self.content.contains_key(path)
      || self.dirs.contains(path)
      || self.hardlinks.contains_key(path)
    {
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
    // A file, directory or symlink already at this path is a different kind — a type conflict. An
    // existing hard link is not: that is the retarget case decided just below.
    if self.content.contains_key(path)
      || self.dirs.contains(path)
      || self.symlinks.contains_key(path)
    {
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
    established: bool,
  ) -> Result<Option<Effect>, ConflictWindow> {
    if !self.content.contains_key(path) && !self.dirs.contains(path) && !established {
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
      record(
        effects,
        windows,
        self.merge_setmode(path, inc.base, *mode, r.establishes(path)),
      );
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
        self.merge_set_xattr(path, name, inc.base, value, r.establishes(path)),
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
          self.dirs.insert(path.clone());
          self
            .dir_history
            .entry(path)
            .or_default()
            .push((version, true));
        }
        Effect::RemoveDir(path) => {
          self.dirs.remove(&path);
          self
            .dir_history
            .entry(path)
            .or_default()
            .push((version, false));
        }
        Effect::Mode(path, mode) => {
          self.modes.insert(path.clone(), mode);
          self.mode_changed.insert(path.clone(), version);
          self
            .mode_history
            .entry(path)
            .or_default()
            .push((version, Some(mode)));
        }
        Effect::Symlink(path, target) => {
          self.symlinks.insert(path.clone(), target.clone());
          self.symlink_changed.insert(path.clone(), version);
          self
            .symlink_history
            .entry(path)
            .or_default()
            .push((version, Some(target)));
        }
        Effect::RemoveSymlink(path) => {
          self.symlinks.remove(&path);
          self.symlink_changed.insert(path.clone(), version);
          self
            .symlink_history
            .entry(path)
            .or_default()
            .push((version, None));
        }
        Effect::Hardlink(path, target) => {
          self.hardlinks.insert(path.clone(), target.clone());
          self.hardlink_changed.insert(path.clone(), version);
          self
            .hardlink_history
            .entry(path)
            .or_default()
            .push((version, Some(target)));
        }
        Effect::RemoveHardlink(path) => {
          self.hardlinks.remove(&path);
          self.hardlink_changed.insert(path.clone(), version);
          self
            .hardlink_history
            .entry(path)
            .or_default()
            .push((version, None));
        }
        Effect::SetXattr(path, name, value) => {
          let key = (path, name);
          self.xattrs.insert(key.clone(), value.clone());
          self.xattr_changed.insert(key.clone(), version);
          self
            .xattr_history
            .entry(key)
            .or_default()
            .push((version, Some(value)));
        }
        Effect::RemoveXattr(path, name) => {
          let key = (path, name);
          self.xattrs.remove(&key);
          self.xattr_changed.insert(key.clone(), version);
          self
            .xattr_history
            .entry(key)
            .or_default()
            .push((version, None));
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

  /// Rebases a work's increment onto the green's head (§4.16 "Rebase, the only corrective path"):
  /// runs the same verdict `submit` would, but commits nothing to the green — instead, when every
  /// operation maps cleanly, it returns the work's new base (the head), its full rebased content
  /// (the head's files with the work's mapped edits re-applied), and its journal restated in head
  /// coordinates, so the agent can keep editing on a fresh base and submit without further mapping.
  /// A conflict returns the windows and changes nothing; the agent resolves each window and rebases
  /// again. The green — content, deltas and counters — is left exactly as it was.
  pub fn rebase(&mut self, inc: &Increment) -> Rebased {
    let to = self.head();
    let resolved = Green::resolve(inc);
    // The verdict shares `submit`'s decision but must not perturb the green; the only field `decide`
    // writes is the fast-path counter (through `merge_content`), which a rebase restores.
    let fast_path_before = self.fast_path_hits;
    let decision = self.decide(inc, &resolved);
    self.fast_path_hits = fast_path_before;
    match decision {
      Err(windows) => Rebased::Conflict { windows },
      Ok(effects) => self.rebased_from(effects, to),
    }
  }

  /// Restates the accepted effects of a clean rebase as the work's new full content and journal: the
  /// content starts from the head's files and takes each effect; the journal is the effects as
  /// declared operations in head coordinates (a content op keeps its mapped range, a file new to the
  /// head is created first). The green is not touched.
  fn rebased_from(&self, effects: Vec<Effect>, to: u64) -> Rebased {
    use crate::increment::VolumeOp;
    let mut files = self.content.clone();
    let mut journal: Vec<VolumeOp> = Vec::new();
    for effect in effects {
      match effect {
        Effect::SetContent(path, bytes, ops) => {
          if !self.content.contains_key(&path) {
            journal.push(VolumeOp::Create { path: path.clone() });
          }
          for op in &ops {
            if let Some(volume_op) = content_volume_op(&path, op) {
              journal.push(volume_op);
            }
          }
          files.insert(path, bytes);
        }
        Effect::RemoveContent(path) => {
          journal.push(VolumeOp::Unlink { path: path.clone() });
          files.remove(&path);
        }
        Effect::MakeDir(path) => journal.push(VolumeOp::Mkdir { path }),
        Effect::RemoveDir(path) => journal.push(VolumeOp::Rmdir { path }),
        Effect::Mode(path, mode) => journal.push(VolumeOp::SetMode { path, mode }),
        Effect::Symlink(path, target) => journal.push(VolumeOp::Symlink { path, target }),
        Effect::RemoveSymlink(path) | Effect::RemoveHardlink(path) => {
          journal.push(VolumeOp::Unlink { path });
        }
        Effect::Hardlink(path, target) => journal.push(VolumeOp::Link { path, target }),
        Effect::SetXattr(path, name, value) => {
          journal.push(VolumeOp::SetXattr { path, name, value });
        }
        Effect::RemoveXattr(path, name) => journal.push(VolumeOp::RemoveXattr { path, name }),
      }
    }
    Rebased::Rebased {
      version: to,
      files,
      journal,
    }
  }
}

/// Restates a mapped content op as a declared operation (head coordinates; the post-state offset is
/// re-derived when the rebased journal is next composed, so it is dropped here). Returns `None` for a
/// non-content op kind.
fn content_volume_op(path: &str, op: &Op) -> Option<crate::increment::VolumeOp> {
  use crate::increment::VolumeOp;
  let path = path.to_owned();
  Some(match op.kind {
    OpKind::Overwrite => VolumeOp::Overwrite {
      path,
      at: op.at,
      len: op.len,
    },
    OpKind::Insert => VolumeOp::Insert {
      path,
      at: op.at,
      len: op.len,
    },
    OpKind::Extend => VolumeOp::Extend {
      path,
      at: op.at,
      len: op.len,
    },
    OpKind::Delete => VolumeOp::Delete {
      path,
      at: op.at,
      len: op.len,
    },
    OpKind::Truncate => VolumeOp::Truncate { path, len: op.len },
    _ => return None,
  })
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
