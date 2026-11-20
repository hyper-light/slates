//! The whole-volume deriver (§4.16 "Composition at seal", "Declared operations"). The content
//! deriver ([`crate::derive`]) composes one path's declared content; this module composes a work
//! volume's whole journal — content and the namespace operations create, unlink and rename — into
//! one [`OpsDoc`], the increment's declared operations whose BLAKE3 is half its identity.
//!
//! Replay tracks entities. Each file present in the work volume is an entity that carries its
//! origin (a base path, or nothing when it was created this increment), its base content length,
//! and the content operations declared against it; a create makes a new entity, an unlink kills
//! one, a rename moves one to another path, and content operations accumulate on whichever entity
//! is at the path. Composition follows §4.16:
//!
//! - A create then an unlink of a new file cancels — nothing is declared.
//! - A base file unlinked is one `Unlink`; a base file edited in place is its content net ops.
//! - A base path whose content was replaced (unlinked then recreated, or a create over it) is a
//!   delete of the base and the new content, with no create — the path existed at base.
//! - A new file that survives is one `Create` and its bytes as inserts.
//! - A base file renamed to a fresh path is one `Rename` (its source is the base path) plus its
//!   content; a base file renamed over another base file is one `Rename` that replaces the target.
//! - A new file renamed over a base file is the write-and-rename pattern: it composes to the base
//!   file's content being replaced (a delete and the new bytes), not a rename — the destination
//!   file simply has new content (§4.16).
//! - A base file written to and then removed, or replaced by renaming another base file onto it,
//!   while a hard link the base had to it still names its node, leaves the node to that link with
//!   the writes (A-669; A-646's decision, the heir takes the node as the work left it): the writes
//!   are declared at the link, where the node stands at seal and the seal reads them, beside the
//!   removal, and the engine makes the link the node's home with them. When only a link the work
//!   made still names the node, or the work made the file again at its name, the composition is
//!   owed: `Unsupported`, never the writes dropped. Until A-669 they were dropped, and the link
//!   kept the head's bytes.
//!
//! Composition is arithmetic on the declared operations, never a comparison of file states
//! (D-27's never-diff clause).
//!
//! The post-state layout. An op that adds content names its bytes by an offset into the
//! increment's post-state — the sealed work volume's final content. The post-state is the
//! surviving files' final contents concatenated in sorted final-path order; each occupies a
//! contiguous region, and an op's source offset is its offset within the file's final content plus
//! the region's base offset. Sorting makes the layout, and the identity, independent of the
//! journal's declaration order.
//!
//! Scope: content, create, unlink and rename of regular files, directory create and remove, and
//! the mode of a file or directory (`SetMode`), symbolic links (`Symlink`; the target is stored
//! in the path table), and extended attributes (`SetXattr`/`RemoveXattr`, composed against the
//! base value; the value is laid out in the post-state after the file content, the name in the
//! path table), and hard links (`Link`, composed like symlinks; the node a link names is a record
//! of the node table, the node at the target at the base or the one the increment makes there,
//! A-646). IPC creations (`Mknod`, A-26) carry canonical metadata without content; their
//! modes compose independently and a create/unlink cancels. IPC renames, metadata declarations
//! through aliases, and primary-name removal while aliases exist are typed refusals pending
//! an inode-aware namespace journal. The remaining edges — a
//! file/directory/symlink/link transition at one path, a metadata or link then a rename of that
//! path, symlink rename, a write through a hard link, and the rare rename onto a base path already
//! consumed this increment — are owed (GAPS §8f), each a typed [`DeriveError`]. An operation a valid volume could not
//! have produced — content on a missing file, a create over an existing one, an unlink, rename or
//! rmdir of a missing one, a mkdir over an existing directory — is a typed [`DeriveError`] too,
//! never a panic. This module is pure: no I/O, no clock, no randomness.

use crate::derive::{ContentOp, compose_content_sized};
use crate::ops_doc::{NodeOrigin, NodeRef, Op, OpKind, OpsDoc};
use crate::pace::Ticks;
use std::collections::{BTreeMap, BTreeSet};

/// One declared operation, as the journal records it (§4.5). Content coordinates are current-file
/// at the moment the operation happened.
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
  /// Create a FIFO or socket namespace inode, carrying metadata and no file contents.
  Mknod {
    /// The new name.
    path: String,
    /// The captured metadata; replay never consults a clock or live endpoint.
    node: crate::special::SpecialNode,
    /// Initial permission bits, composed in the independent mode dimension.
    mode: u32,
  },
  /// The name `path` removed.
  Unlink {
    /// The file.
    path: String,
  },
  /// The file at `from` renamed to `to` (replacing whatever was at `to`).
  Rename {
    /// The source name.
    from: String,
    /// The destination name.
    to: String,
  },
  /// A new, empty directory created at `path`.
  Mkdir {
    /// The directory.
    path: String,
  },
  /// The empty directory `path` removed.
  Rmdir {
    /// The directory.
    path: String,
  },
  /// The mode of a file or directory set to `mode`.
  SetMode {
    /// The path.
    path: String,
    /// The new mode.
    mode: u32,
  },
  /// A symbolic link created at `path` pointing at `target`.
  Symlink {
    /// The link's path.
    path: String,
    /// The link's target (an opaque byte string).
    target: String,
  },
  /// An extended attribute `name` on `path` set to `value`.
  SetXattr {
    /// The path.
    path: String,
    /// The attribute name.
    name: String,
    /// The attribute value (opaque bytes).
    value: Vec<u8>,
  },
  /// The extended attribute `name` removed from `path`.
  RemoveXattr {
    /// The path.
    path: String,
    /// The attribute name.
    name: String,
  },
  /// A hard link created at `path` to the existing file `target` (sharing its content).
  Link {
    /// The new name.
    path: String,
    /// The existing file it links to.
    target: String,
  },
  /// A path's tree at a committed version copied to another path by reference (A-586; A-626 part 4; Subversion's copy
  /// with history): `from`'s tree as version `version` of the green held it, at `to`, replacing what `to` holds when
  /// `replace` (A-626's `REPLACE`). What the work does in `to`'s tree after it composes against what it brings.
  Copy {
    /// The copied tree's root.
    from: String,
    /// Where it is copied to.
    to: String,
    /// The green's version it is copied from.
    version: u64,
    /// Whether it replaces what `to` holds.
    replace: bool,
  },
}

impl VolumeOp {
  /// The bytes this operation holds in memory: its own size and the heap its strings and value use. What a work
  /// volume's journal is charged per operation (a work is charged for what it keeps, §4.2).
  pub fn footprint(&self) -> u64 {
    let heap = match self {
      VolumeOp::Overwrite { path, .. }
      | VolumeOp::Extend { path, .. }
      | VolumeOp::Truncate { path, .. }
      | VolumeOp::Insert { path, .. }
      | VolumeOp::Delete { path, .. }
      | VolumeOp::Create { path }
      | VolumeOp::Mknod { path, .. }
      | VolumeOp::Unlink { path }
      | VolumeOp::Mkdir { path }
      | VolumeOp::Rmdir { path }
      | VolumeOp::SetMode { path, .. } => path.len(),
      VolumeOp::Rename { from, to } | VolumeOp::Copy { from, to, .. } => {
        from.len().saturating_add(to.len())
      }
      VolumeOp::Symlink { path, target } | VolumeOp::Link { path, target } => {
        path.len().saturating_add(target.len())
      }
      VolumeOp::SetXattr { path, name, value } => path
        .len()
        .saturating_add(name.len())
        .saturating_add(value.len()),
      VolumeOp::RemoveXattr { path, name } => path.len().saturating_add(name.len()),
    };
    u64::try_from(size_of::<VolumeOp>().saturating_add(heap)).unwrap_or(u64::MAX)
  }
}

impl VolumeOp {
  /// The path-free content operation, or `None` for a namespace operation.
  fn content(&self) -> Option<ContentOp> {
    match *self {
      VolumeOp::Overwrite { at, len, .. } => Some(ContentOp::Overwrite { at, len }),
      VolumeOp::Extend { at, len, .. } => Some(ContentOp::Extend { at, len }),
      VolumeOp::Truncate { len, .. } => Some(ContentOp::Truncate { len }),
      VolumeOp::Insert { at, len, .. } => Some(ContentOp::Insert { at, len }),
      VolumeOp::Delete { at, len, .. } => Some(ContentOp::Delete { at, len }),
      VolumeOp::Create { .. }
      | VolumeOp::Unlink { .. }
      | VolumeOp::Rename { .. }
      | VolumeOp::Mkdir { .. }
      | VolumeOp::Rmdir { .. }
      | VolumeOp::SetMode { .. }
      | VolumeOp::Symlink { .. }
      | VolumeOp::SetXattr { .. }
      | VolumeOp::RemoveXattr { .. }
      | VolumeOp::Link { .. }
      | VolumeOp::Mknod { .. }
      | VolumeOp::Copy { .. } => None,
    }
  }

  /// Every path this operation names: its path, a rename's two ends, and a hard link's target. A
  /// symlink's target is a string the link holds, not a path the composition looks up, so it is not
  /// named. What the deriver reads of a base is exactly these paths' entries (and the hard links
  /// aimed at them), so a base restricted to them composes the same document ([`compose_volume`]).
  pub fn names(&self) -> [Option<&str>; 2] {
    match self {
      VolumeOp::Overwrite { path, .. }
      | VolumeOp::Extend { path, .. }
      | VolumeOp::Truncate { path, .. }
      | VolumeOp::Insert { path, .. }
      | VolumeOp::Delete { path, .. }
      | VolumeOp::Create { path }
      | VolumeOp::Mknod { path, .. }
      | VolumeOp::Unlink { path }
      | VolumeOp::Mkdir { path }
      | VolumeOp::Rmdir { path }
      | VolumeOp::SetMode { path, .. }
      | VolumeOp::Symlink { path, .. }
      | VolumeOp::SetXattr { path, .. }
      | VolumeOp::RemoveXattr { path, .. } => [Some(path), None],
      VolumeOp::Rename { from, to } | VolumeOp::Copy { from, to, .. } => [Some(from), Some(to)],
      VolumeOp::Link { path, target } => [Some(path), Some(target)],
    }
  }

  /// Whether this is a directory operation (`Mkdir` or `Rmdir`).
  fn is_directory(&self) -> bool {
    matches!(self, VolumeOp::Mkdir { .. } | VolumeOp::Rmdir { .. })
  }

  /// Whether this is a symlink create (`Symlink`).
  fn is_symlink(&self) -> bool {
    matches!(self, VolumeOp::Symlink { .. })
  }

  /// Whether this is a hard link create (`Link`).
  fn is_link(&self) -> bool {
    matches!(self, VolumeOp::Link { .. })
  }

  /// The single file path a content, create or unlink operation targets (a rename has two, and a
  /// directory operation is not a file operation).
  fn single_path(&self) -> Option<&str> {
    match self {
      VolumeOp::Overwrite { path, .. }
      | VolumeOp::Extend { path, .. }
      | VolumeOp::Truncate { path, .. }
      | VolumeOp::Insert { path, .. }
      | VolumeOp::Delete { path, .. }
      | VolumeOp::Create { path }
      | VolumeOp::Unlink { path } => Some(path),
      VolumeOp::Rename { .. }
      | VolumeOp::Mkdir { .. }
      | VolumeOp::Rmdir { .. }
      | VolumeOp::SetMode { .. }
      | VolumeOp::Symlink { .. }
      | VolumeOp::SetXattr { .. }
      | VolumeOp::RemoveXattr { .. }
      | VolumeOp::Link { .. }
      | VolumeOp::Mknod { .. }
      | VolumeOp::Copy { .. } => None,
    }
  }
}

/// A refusal from the deriver: an operation a valid volume could not have produced, or one whose
/// composition is not yet implemented. Each names the offending path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeriveError {
  /// A content operation on a path with no file present.
  ContentOnMissing(String),
  /// A create on a path that already holds a file.
  CreateOverExisting(String),
  /// An unlink of a path with no file present.
  UnlinkMissing(String),
  /// A rename whose source path holds no file.
  RenameMissingSource(String),
  /// A namespace combination whose composition is owed: a rename onto, or a create at, a base
  /// path already consumed this increment (its base was renamed away).
  Unsupported(String),
  /// A `Mkdir` on a path that already holds a directory.
  MkdirOverExisting(String),
  /// An `Rmdir` of a path with no directory present.
  RmdirMissing(String),
  /// One path was used as both a file and a directory this increment (a file/directory transition
  /// at a path); its composition is owed.
  PathIsFileAndDirectory(String),
  /// A `SetMode` on a path with no file or directory present at seal.
  SetModeMissing(String),
  /// A `Symlink` on a path that already holds a symlink.
  SymlinkOverExisting(String),
  /// One path was used as conflicting kinds this increment (file, directory and/or symlink), a
  /// transition whose composition is owed.
  PathKindConflict(String),
  /// An xattr operation on a path with no file or directory present at seal.
  XattrMissing(String),
  /// A `Link` on a path that already holds a hard link.
  LinkOverExisting(String),
}

impl std::fmt::Display for DeriveError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::ContentOnMissing(path) => write!(f, "content operation on missing file {path}"),
      Self::CreateOverExisting(path) => write!(f, "create over existing file {path}"),
      Self::UnlinkMissing(path) => write!(f, "unlink of missing file {path}"),
      Self::RenameMissingSource(path) => write!(f, "rename of missing source {path}"),
      Self::Unsupported(path) => {
        write!(
          f,
          "a namespace combination on the reused base path {path} is not yet composed"
        )
      }
      Self::MkdirOverExisting(path) => write!(f, "mkdir over existing directory {path}"),
      Self::RmdirMissing(path) => write!(f, "rmdir of missing directory {path}"),
      Self::PathIsFileAndDirectory(path) => {
        write!(f, "path {path} is used as both a file and a directory")
      }
      Self::SetModeMissing(path) => write!(f, "set mode on missing path {path}"),
      Self::SymlinkOverExisting(path) => write!(f, "symlink over existing symlink {path}"),
      Self::PathKindConflict(path) => write!(f, "path {path} is used as conflicting kinds"),
      Self::XattrMissing(path) => write!(f, "xattr operation on missing path {path}"),
      Self::LinkOverExisting(path) => write!(f, "hard link over existing link {path}"),
    }
  }
}

impl std::error::Error for DeriveError {}

/// The base version's state the deriver composes against: the files present at base (with their
/// content lengths) and the directories present at base. A richer base (modes, xattrs, symlinks,
/// hard links) is the deriver's remaining metadata work (owed; GAPS §8f).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Base {
  /// The base files and their content lengths.
  pub files: Vec<(String, u64)>,
  /// The base IPC inodes, without endpoint state.
  pub specials: Vec<(String, crate::special::SpecialNode)>,
  /// The base directory paths.
  pub dirs: Vec<String>,
  /// The base mode of a path (file or directory), where known.
  pub modes: Vec<(String, u32)>,
  /// The base symlinks and their targets.
  pub symlinks: Vec<(String, String)>,
  /// The base extended attributes: `(path, name, value)`.
  pub xattrs: Vec<(String, String, Vec<u8>)>,
  /// The base hard links and their targets: `(path, target)`.
  pub hardlinks: Vec<(String, String)>,
}

impl Base {
  /// The restricted base of the paths `named` (`Green::base_for`), assembled from each path's own entries as
  /// `Green::path_base` gives them (A-247: a work's shard keeps the entries of the paths it touched, so it composes its
  /// increment without the green). In `base_for`'s order: each named path's entries in path order, then the hard links
  /// whose link or target is named, by link. `None` when a named path's entries are not in `per_path`: a base not
  /// known, never a smaller one.
  pub fn for_names(
    named: &std::collections::BTreeSet<&str>,
    per_path: &std::collections::BTreeMap<String, Base>,
  ) -> Option<Base> {
    let mut base = Base::of_files(Vec::new());
    let mut links: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
    for path in named {
      let entries = per_path.get(*path)?;
      base.files.extend(entries.files.iter().cloned());
      base.specials.extend(entries.specials.iter().cloned());
      base.dirs.extend(entries.dirs.iter().cloned());
      base.modes.extend(entries.modes.iter().cloned());
      base.symlinks.extend(entries.symlinks.iter().cloned());
      base.xattrs.extend(entries.xattrs.iter().cloned());
      for (link, target) in &entries.hardlinks {
        links.insert(link.as_str(), target.as_str());
      }
    }
    base.hardlinks = links
      .into_iter()
      .map(|(link, target)| (link.to_owned(), target.to_owned()))
      .collect();
    Some(base)
  }

  /// A base with the given files and no directories or recorded modes (the common file-only case).
  pub fn of_files(files: Vec<(String, u64)>) -> Base {
    Base {
      files,
      specials: Vec::new(),
      dirs: Vec::new(),
      modes: Vec::new(),
      symlinks: Vec::new(),
      xattrs: Vec::new(),
      hardlinks: Vec::new(),
    }
  }
}

/// The base's entries by path (A-752): what the deriver's lookups read, each O(log n), where a scan of the base's lists
/// made one operation's unit O(base). A path's first entry, as those scans found it.
struct BaseIndex<'b> {
  /// The base files' lengths.
  files: BTreeMap<&'b str, u64>,
  /// The base directories.
  dirs: BTreeSet<&'b str>,
  /// The base modes.
  modes: BTreeMap<&'b str, u32>,
  /// The base symlinks' targets.
  symlinks: BTreeMap<&'b str, &'b str>,
  /// The base xattrs' values, by path then name.
  xattrs: BTreeMap<&'b str, BTreeMap<&'b str, &'b [u8]>>,
  /// The base hard links' targets.
  hardlinks: BTreeMap<&'b str, &'b str>,
  /// The paths a base hard link targets.
  link_targets: BTreeSet<&'b str>,
}

impl<'b> BaseIndex<'b> {
  /// `base` indexed, an entry a unit.
  async fn of(base: &'b Base, pace: &Ticks) -> BaseIndex<'b> {
    let mut index = BaseIndex {
      files: BTreeMap::new(),
      dirs: BTreeSet::new(),
      modes: BTreeMap::new(),
      symlinks: BTreeMap::new(),
      xattrs: BTreeMap::new(),
      hardlinks: BTreeMap::new(),
      link_targets: BTreeSet::new(),
    };
    index.paths(base, pace).await;
    index.links(base, pace).await;
    index
  }

  /// The base's files, directories and modes indexed, an entry a unit.
  async fn paths(&mut self, base: &'b Base, pace: &Ticks) {
    for (path, len) in &base.files {
      pace.tick().await;
      self.files.entry(path.as_str()).or_insert(*len);
    }
    for dir in &base.dirs {
      pace.tick().await;
      self.dirs.insert(dir.as_str());
    }
    for (path, mode) in &base.modes {
      pace.tick().await;
      self.modes.entry(path.as_str()).or_insert(*mode);
    }
  }

  /// The base's symlinks, xattrs and hard links indexed, an entry a unit.
  async fn links(&mut self, base: &'b Base, pace: &Ticks) {
    for (path, target) in &base.symlinks {
      pace.tick().await;
      self
        .symlinks
        .entry(path.as_str())
        .or_insert(target.as_str());
    }
    for (path, name, value) in &base.xattrs {
      pace.tick().await;
      self
        .xattrs
        .entry(path.as_str())
        .or_default()
        .entry(name.as_str())
        .or_insert(value.as_slice());
    }
    for (link, target) in &base.hardlinks {
      pace.tick().await;
      self
        .hardlinks
        .entry(link.as_str())
        .or_insert(target.as_str());
      self.link_targets.insert(target.as_str());
    }
  }

  /// The base hard link target of `path`, if the base has a hard link there.
  fn hardlink_target(&self, path: &str) -> Option<&'b str> {
    self.hardlinks.get(path).copied()
  }

  /// The base value of the xattr `name` on `path`, if the base recorded one.
  fn xattr(&self, path: &str, name: &str) -> Option<&'b [u8]> {
    self.xattrs.get(path)?.get(name).copied()
  }

  /// The base symlink target of `path`, if the base has a symlink there.
  fn symlink_target(&self, path: &str) -> Option<&'b str> {
    self.symlinks.get(path).copied()
  }

  /// The base mode of `path`, if the base recorded one.
  fn mode_of(&self, path: &str) -> Option<u32> {
    self.modes.get(path).copied()
  }
}

/// One file tracked during replay.
#[derive(Clone)]
struct Entity {
  /// The base path this file came from, or `None` when created this increment.
  origin: Option<String>,
  /// The base content length (0 when created).
  base_len: u64,
  /// The content operations declared against it, in current-file coordinates.
  content: Vec<ContentOp>,
  /// Whether it is still present at seal.
  live: bool,
  /// Its current path.
  final_path: String,
  /// Whether it was killed by a rename over it (so no `Unlink` is emitted — the rename replaces
  /// it).
  clobbered: bool,
}

/// A replay's entities with the indexes its operations look them up by (A-752): the live entity at each path, the base
/// paths any entity came from, the base file at its own path that an unlink left (a recreate's target), and the base's
/// file lengths. Each operation looks up what it touches in O(log n), where a scan of every entity and every base file
/// made a push of n files cost O(n^2): a flat 3,500-file push composed in 127 ms, 7,000 in 458 ms (debug, Mac Studio,
/// 2026-11-20), past a shard's step.
struct Entities<'b> {
  /// The entities, in the order the replay made them (what the seal emits by).
  list: Vec<Entity>,
  /// The live entity at each path: at most one, since a create refuses over one and a rename clobbers its destination.
  live: std::collections::BTreeMap<String, usize>,
  /// Every base path an entity came from (a base file materialized, or re-rooted by a rename over it).
  origins: std::collections::BTreeSet<String>,
  /// The base file an unlink left at its own path, neither recreated nor clobbered: at most one a path.
  unlinked: std::collections::BTreeMap<String, usize>,
  /// The base's file lengths by path.
  base: &'b BTreeMap<&'b str, u64>,
}

impl<'b> Entities<'b> {
  /// None yet, over `base`.
  fn over(base: &'b BTreeMap<&'b str, u64>) -> Entities<'b> {
    Entities {
      list: Vec::new(),
      live: BTreeMap::new(),
      origins: BTreeSet::new(),
      unlinked: BTreeMap::new(),
      base,
    }
  }

  /// The index of the live entity at `path`, if any.
  fn live_index(&self, path: &str) -> Option<usize> {
    self.live.get(path).copied()
  }

  /// The base content length of `path`, if it existed at base.
  fn base_len_of(&self, path: &str) -> Option<u64> {
    self.base.get(path).copied()
  }

  /// Whether `path`'s base file has already been materialized into an entity (so it cannot be materialized again).
  fn consumed(&self, path: &str) -> bool {
    self.origins.contains(path)
  }

  /// Adds `entity`, indexed; its index.
  fn push(&mut self, entity: Entity) -> usize {
    let index = self.list.len();
    if entity.live {
      self.live.insert(entity.final_path.clone(), index);
    }
    if let Some(origin) = &entity.origin {
      self.origins.insert(origin.clone());
    }
    self.list.push(entity);
    index
  }

  /// Marks the entity at `index` unlinked: not live, and a recreate's target where it is a base file at its own path.
  fn unlink(&mut self, index: usize) {
    let Some(entity) = self.list.get_mut(index) else {
      return;
    };
    entity.live = false;
    if self.live.get(&entity.final_path) == Some(&index) {
      self.live.remove(&entity.final_path);
    }
    if !entity.clobbered && entity.origin.as_deref() == Some(entity.final_path.as_str()) {
      self.unlinked.insert(entity.final_path.clone(), index);
    }
  }

  /// Marks the entity at `index` clobbered (killed by a rename over it, or superseded by a recreate): not live, and no
  /// longer a recreate's target.
  fn clobber(&mut self, index: usize) {
    let Some(entity) = self.list.get_mut(index) else {
      return;
    };
    entity.live = false;
    entity.clobbered = true;
    if self.live.get(&entity.final_path) == Some(&index) {
      self.live.remove(&entity.final_path);
    }
    if self.unlinked.get(&entity.final_path) == Some(&index) {
      self.unlinked.remove(&entity.final_path);
    }
  }

  /// Moves the live entity at `index` to `to`.
  fn move_to(&mut self, index: usize, to: &str) {
    let Some(entity) = self.list.get_mut(index) else {
      return;
    };
    if self.live.get(&entity.final_path) == Some(&index) {
      self.live.remove(&entity.final_path);
    }
    entity.final_path = to.to_owned();
    if entity.live {
      self.live.insert(to.to_owned(), index);
    }
  }

  /// Re-roots the entity at `index` at the base file `to` (a new file renamed over it, module docs of `apply_rename`).
  fn reroot(&mut self, index: usize, to: &str, base_len: u64) {
    let Some(entity) = self.list.get_mut(index) else {
      return;
    };
    entity.origin = Some(to.to_owned());
    entity.base_len = base_len;
    entity.content.insert(0, ContentOp::Truncate { len: 0 });
    self.origins.insert(to.to_owned());
  }
}

/// Materializes an untouched base file into a live entity and returns its index, or `None` when
/// `path` is not an untouched base file (not at base, or already consumed).
fn materialize_base(entities: &mut Entities<'_>, path: &str) -> Option<usize> {
  if entities.consumed(path) {
    return None;
  }
  let base_len = entities.base_len_of(path)?;
  Some(entities.push(Entity {
    origin: Some(path.to_owned()),
    base_len,
    content: Vec::new(),
    live: true,
    final_path: path.to_owned(),
    clobbered: false,
  }))
}

/// Applies one operation to the entity set.
fn apply_op(entities: &mut Entities<'_>, op: &VolumeOp) -> Result<(), DeriveError> {
  if let Some(content) = op.content() {
    let path = op.single_path().unwrap_or("");
    let index = entities
      .live_index(path)
      .or_else(|| materialize_base(entities, path))
      .ok_or_else(|| DeriveError::ContentOnMissing(path.to_owned()))?;
    entities
      .list
      .get_mut(index)
      .ok_or_else(|| DeriveError::ContentOnMissing(path.to_owned()))?
      .content
      .push(content);
    return Ok(());
  }
  match op {
    VolumeOp::Create { path } => apply_create(entities, path),
    VolumeOp::Unlink { path } => apply_unlink(entities, path),
    VolumeOp::Rename { from, to } => apply_rename(entities, from, to),
    _ => Ok(()),
  }
}

/// A `Create`: refuses over a live or untouched-base file; recreates an in-place-unlinked base
/// file as a content replacement; otherwise makes a fresh new file.
fn apply_create(entities: &mut Entities<'_>, path: &str) -> Result<(), DeriveError> {
  if entities.live_index(path).is_some() {
    return Err(DeriveError::CreateOverExisting(path.to_owned()));
  }
  // An in-place unlink of this base path (dead, not clobbered, still named here): recreating it
  // replaces the base content.
  if let Some(unlinked) = entities.unlinked.get(path).copied() {
    let base_len = entities
      .list
      .get(unlinked)
      .map_or(0, |entity| entity.base_len);
    // The recreation supersedes the unlink: mark the dead entity clobbered so seal emits no
    // stale `Unlink` for a path that is present again.
    entities.clobber(unlinked);
    entities.push(Entity {
      origin: Some(path.to_owned()),
      base_len,
      content: vec![ContentOp::Truncate { len: 0 }],
      live: true,
      final_path: path.to_owned(),
      clobbered: false,
    });
    return Ok(());
  }
  if entities.base_len_of(path).is_some() {
    // A base path here: an untouched one is still present (create over it fails); a consumed one
    // (its base was renamed away — the in-place-unlink case was handled above) is the owed
    // recreate-at-a-renamed-away-base-path case.
    return if entities.consumed(path) {
      Err(DeriveError::Unsupported(path.to_owned()))
    } else {
      Err(DeriveError::CreateOverExisting(path.to_owned()))
    };
  }
  entities.push(Entity {
    origin: None,
    base_len: 0,
    content: Vec::new(),
    live: true,
    final_path: path.to_owned(),
    clobbered: false,
  });
  Ok(())
}

/// An `Unlink`: kills the live (or materialized-base) entity at `path`. Its writes stay with it: a hard link may still
/// name its node, and then the seal declares them there ([`heirs_of_writes`], A-669). Until A-669 they were cleared
/// here, and the green handed the link the head's bytes, so a write was lost with its file's name.
fn apply_unlink(entities: &mut Entities<'_>, path: &str) -> Result<(), DeriveError> {
  let index = entities
    .live_index(path)
    .or_else(|| materialize_base(entities, path))
    .ok_or_else(|| DeriveError::UnlinkMissing(path.to_owned()))?;
  if entities.list.get(index).is_none() {
    return Err(DeriveError::UnlinkMissing(path.to_owned()));
  }
  entities.unlink(index);
  Ok(())
}

/// A `Rename`: moves the source entity to `to`, replacing whatever was there.
fn apply_rename(entities: &mut Entities<'_>, from: &str, to: &str) -> Result<(), DeriveError> {
  if from == to {
    return Ok(());
  }
  let source = entities
    .live_index(from)
    .or_else(|| materialize_base(entities, from))
    .ok_or_else(|| DeriveError::RenameMissingSource(from.to_owned()))?;
  let destination = resolve_rename_target(entities, to)?;
  let missing = || DeriveError::RenameMissingSource(from.to_owned());
  if let Some(dst) = destination {
    let (dst_origin, dst_base_len) = {
      let clobbered = entities.list.get(dst).ok_or_else(missing)?;
      (clobbered.origin.clone(), clobbered.base_len)
    };
    entities.clobber(dst);
    // Only a base file at its *own* path is a write-and-rename target: replacing it is a content
    // change to that base file. A base file that was itself renamed here is not a base path, so
    // the source is just a new file at `to`, and the clobbered file's original name is orphaned
    // (unlinked at seal by the survivor rule).
    let dst_at_own_base = dst_origin.as_deref() == Some(to);
    let moved = entities.list.get(source).ok_or_else(missing)?;
    if dst_at_own_base && moved.origin.is_none() {
      // A new file renamed over a base file: the destination's content is replaced (a delete of
      // the base and the new bytes), so re-root the source at the destination's base.
      entities.reroot(source, to, dst_base_len);
    }
    // A base file over a base file keeps its origin (emitting a `Rename` at seal); a new file
    // clobbered here simply cancels.
  }
  if entities.list.get(source).is_none() {
    return Err(missing());
  }
  entities.move_to(source, to);
  Ok(())
}

/// Resolves a rename's destination: a live entity there, or an untouched base file materialized
/// as the clobber target, or `None` when the path is free. A base path already consumed is a
/// typed refusal (owed).
fn resolve_rename_target(
  entities: &mut Entities<'_>,
  to: &str,
) -> Result<Option<usize>, DeriveError> {
  if let Some(index) = entities.live_index(to) {
    return Ok(Some(index));
  }
  if entities.base_len_of(to).is_some() {
    if entities.consumed(to) {
      return Err(DeriveError::Unsupported(to.to_owned()));
    }
    return Ok(materialize_base(entities, to));
  }
  Ok(None)
}

/// The directories present at seal: the base directories, with the journal's mkdir/rmdir applied,
/// and the root. (Validity — no mkdir over an existing directory, no rmdir of an absent one — is
/// checked by [`compose_directories`], which runs first.)
async fn present_directories(base: &Base, journal: &[VolumeOp], pace: &Ticks) -> BTreeSet<String> {
  let mut present: BTreeSet<String> = BTreeSet::new();
  for dir in &base.dirs {
    pace.tick().await;
    present.insert(dir.clone());
  }
  // The root, the empty path (the server keys a volume's `/` so), is always present: nothing makes or removes it, and
  // its mode and attributes compose as any directory's (A-618: a push's head is an attribute of the root, §4.17.1 "A
  // push is a work" item 3; and an agent's `chmod` or `setxattr` of a mount's root declares the same).
  present.insert(String::new());
  for op in journal {
    pace.tick().await;
    match op {
      VolumeOp::Mkdir { path } => {
        present.insert(path.clone());
      }
      VolumeOp::Rmdir { path } => {
        present.remove(path);
      }
      _ => {}
    }
  }
  present
}

/// Composes the `SetMode` operations into a mode per path (last one wins), emitting a `SetMode`
/// only where the mode differs from the base and the path is present at seal (a surviving file or
/// a present directory). A file's mode moves with its `Rename`, as POSIX `rename` keeps it: a mode set
/// before the rename, or a base file's own mode, is declared at the new path (2026-11-20: keyed by its
/// path alone, chmod then rename was refused and a renamed base file lost its mode). A mode left on a
/// renamed-away path is a typed `Unsupported` refusal; a mode on a path present nowhere is `SetModeMissing`.
async fn compose_modes(
  base: &BaseIndex<'_>,
  journal: &[VolumeOp],
  (survivors, renamed_away, present_dirs): (
    &BTreeSet<String>,
    &BTreeSet<String>,
    &BTreeSet<String>,
  ),
  pace: &Ticks,
) -> Result<Vec<(String, u32)>, DeriveError> {
  let mut final_mode: BTreeMap<String, u32> = BTreeMap::new();
  // Paths a rename moved a file from or over: a base mode there no longer names the base's file.
  let mut moved: BTreeSet<&str> = BTreeSet::new();
  for op in journal {
    pace.tick().await;
    match op {
      VolumeOp::SetMode { path, mode } => {
        final_mode.insert(path.clone(), *mode);
      }
      VolumeOp::Mknod { path, mode, .. } if survivors.contains(path) => {
        final_mode.insert(path.clone(), *mode);
      }
      VolumeOp::Rename { from, to } if from != to => {
        let carried = final_mode.remove(from).or_else(|| {
          (!moved.contains(from.as_str()))
            .then(|| base.mode_of(from))
            .flatten()
        });
        final_mode.remove(to);
        if let Some(mode) = carried {
          final_mode.insert(to.clone(), mode);
        }
        moved.insert(from);
        moved.insert(to);
      }
      _ => {}
    }
  }
  let mut emissions = Vec::new();
  for (path, mode) in final_mode {
    pace.tick().await;
    if renamed_away.contains(&path) {
      return Err(DeriveError::Unsupported(path));
    }
    if !survivors.contains(&path) && !present_dirs.contains(&path) {
      return Err(DeriveError::SetModeMissing(path));
    }
    if base.mode_of(&path) != Some(mode) {
      emissions.push((path, mode));
    }
  }
  Ok(emissions)
}

/// An xattr the increment declares.
enum XattrEmission {
  /// Set the xattr `name` on the path to `value`.
  Set(String, String, Vec<u8>),
  /// Remove the xattr `name` from the path.
  Removed(String, String),
}

/// Composes the xattr operations into a final state per `(path, name)`, emitting a `SetXattr` only
/// where the value differs from the base and a `RemoveXattr` only where a base xattr is removed,
/// and only where the path is present at seal (a surviving file or a present directory). An xattr
/// on a renamed-away path is the owed `Unsupported` refusal; one on a path present nowhere is
/// `XattrMissing`.
async fn compose_xattrs(
  base: &BaseIndex<'_>,
  journal: &[VolumeOp],
  (survivors, renamed_away, present_dirs): (
    &BTreeSet<String>,
    &BTreeSet<String>,
    &BTreeSet<String>,
  ),
  pace: &Ticks,
) -> Result<Vec<XattrEmission>, DeriveError> {
  // The final state of each attribute: `Some(value)` set, `None` removed. Keyed sorted so the
  // emissions (and the post-state layout of the values) are deterministic.
  let mut state: BTreeMap<(String, String), Option<Vec<u8>>> = BTreeMap::new();
  for op in journal {
    pace.tick().await;
    match op {
      VolumeOp::SetXattr { path, name, value } => {
        state.insert((path.clone(), name.clone()), Some(value.clone()));
      }
      VolumeOp::RemoveXattr { path, name } => {
        state.insert((path.clone(), name.clone()), None);
      }
      _ => {}
    }
  }
  let mut emissions = Vec::new();
  for ((path, name), final_value) in state {
    pace.tick().await;
    if renamed_away.contains(&path) {
      return Err(DeriveError::Unsupported(path));
    }
    if !survivors.contains(&path) && !present_dirs.contains(&path) {
      return Err(DeriveError::XattrMissing(path));
    }
    let base_value = base.xattr(&path, &name);
    match final_value {
      Some(value) => {
        if base_value != Some(value.as_slice()) {
          emissions.push(XattrEmission::Set(path, name, value));
        }
      }
      None => {
        if base_value.is_some() {
          emissions.push(XattrEmission::Removed(path, name));
        }
      }
    }
  }
  Ok(emissions)
}

/// A `SetXattr` op: `path` is the file's index, `at` the name's index into the table, and the
/// value is `len` bytes at post-state offset `src`.
fn set_xattr_op(path: u64, name: u64, value_offset: u64, value_len: u64) -> Op {
  Op {
    kind: OpKind::SetXattr,
    flags: 0,
    path,
    at: name,
    len: value_len,
    src: value_offset,
  }
}

/// A `RemoveXattr` op: `path` is the file's index, `at` the name's index into the table.
fn remove_xattr_op(path: u64, name: u64) -> Op {
  Op {
    kind: OpKind::RemoveXattr,
    flags: 0,
    path,
    at: name,
    len: 0,
    src: u64::MAX,
  }
}

/// A `SetMode` op: the new mode is carried in `len` (§4.16 `OpRecord`).
fn set_mode_op(path: u64, mode: u32) -> Op {
  Op {
    kind: OpKind::SetMode,
    flags: 0,
    path,
    at: 0,
    len: u64::from(mode),
    src: u64::MAX,
  }
}

/// A symlink the increment declares.
enum SymlinkEmission {
  /// A symlink at this path pointing at this target (a new or retargeted symlink).
  Made(String, String),
  /// A base symlink removed at this path.
  Removed(String),
}

/// The paths this increment treats as symlinks: those a `Symlink` op names, and the base symlinks.
async fn symlink_paths(base: &Base, journal: &[VolumeOp], pace: &Ticks) -> BTreeSet<String> {
  let mut paths: BTreeSet<String> = BTreeSet::new();
  for (path, _) in &base.symlinks {
    pace.tick().await;
    paths.insert(path.clone());
  }
  for op in journal {
    pace.tick().await;
    if let VolumeOp::Symlink { path, .. } = op {
      paths.insert(path.clone());
    }
  }
  paths
}

/// Composes the symlink operations — `Symlink`, and `Unlink` on a symlink path — per symlink path.
/// A new or retargeted symlink is one `Symlink`; a base symlink removed is one `Unlink`; a symlink
/// created then unlinked, or a base symlink removed then recreated to the same target, is nothing.
/// A symlink over a present symlink, or an unlink of an absent one, is a typed refusal.
async fn compose_symlinks(
  base: &BaseIndex<'_>,
  journal: &[VolumeOp],
  paths: &BTreeSet<String>,
  pace: &Ticks,
) -> Result<Vec<SymlinkEmission>, DeriveError> {
  // Per symlink path: whether it is present and, if so, its target.
  let mut state: BTreeMap<String, (bool, String)> = BTreeMap::new();
  let initial = |path: &str| -> (bool, String) {
    match base.symlink_target(path) {
      Some(target) => (true, target.to_owned()),
      None => (false, String::new()),
    }
  };
  for op in journal {
    pace.tick().await;
    match op {
      VolumeOp::Symlink { path, target } => {
        let here = state.entry(path.clone()).or_insert_with(|| initial(path));
        if here.0 {
          return Err(DeriveError::SymlinkOverExisting(path.clone()));
        }
        here.0 = true;
        here.1 = target.clone();
      }
      VolumeOp::Unlink { path } if paths.contains(path) => {
        let here = state.entry(path.clone()).or_insert_with(|| initial(path));
        if !here.0 {
          return Err(DeriveError::UnlinkMissing(path.clone()));
        }
        here.0 = false;
      }
      _ => {}
    }
  }
  let mut emissions = Vec::new();
  for (path, (here, target)) in state {
    pace.tick().await;
    let base_target = base.symlink_target(&path);
    if here {
      if base_target != Some(target.as_str()) {
        emissions.push(SymlinkEmission::Made(path, target));
      }
    } else if base_target.is_some() {
      emissions.push(SymlinkEmission::Removed(path));
    }
  }
  Ok(emissions)
}

/// A `Symlink` op: `path` is the link's index, `src` the target string's index into the table.
fn symlink_op(link: u64, target: u64) -> Op {
  Op {
    kind: OpKind::Symlink,
    flags: 0,
    path: link,
    at: 0,
    len: 0,
    src: target,
  }
}

/// A hard link the increment declares.
enum LinkEmission {
  /// A hard link at this path to the node at this target, the one at the base or the one the
  /// increment makes there (A-646): a new or retargeted link.
  Made(String, String, NodeOrigin),
  /// A base hard link removed at this path.
  Removed(String),
}

/// A link path's state as the journal composes it: whether a link stands there, the target it
/// names, and where that target's node is (A-646).
type LinkState = (bool, String, NodeOrigin);

/// Notes the paths a journal operation makes (A-646): a file created, an IPC node made, a rename's
/// destination, a link made; and forgets a path removed or renamed away.
fn note_made(op: &VolumeOp, made: &mut std::collections::BTreeSet<String>) {
  match op {
    VolumeOp::Create { path } | VolumeOp::Mknod { path, .. } | VolumeOp::Link { path, .. } => {
      made.insert(path.clone());
    }
    VolumeOp::Rename { from, to } => {
      made.remove(from);
      made.insert(to.clone());
    }
    VolumeOp::Unlink { path } => {
      made.remove(path);
    }
    _ => {}
  }
}

/// The path a journal operation removes or moves, or moves something onto, when a link the
/// increment made earlier names the node the increment made there (A-646): that link's node record
/// names the path, so its later move is a composition owed (`DeriveError::Unsupported`).
fn made_target_moved<'a>(
  op: &'a VolumeOp,
  linked_made: &std::collections::BTreeSet<String>,
) -> Option<&'a str> {
  match op {
    VolumeOp::Unlink { path } => linked_made.contains(path).then_some(path.as_str()),
    VolumeOp::Rename { from, to } => [from, to]
      .into_iter()
      .find(|path| linked_made.contains(*path))
      .map(String::as_str),
    _ => None,
  }
}

/// The paths this increment treats as hard links: those a `Link` op names, and the base hard
/// links.
async fn hardlink_paths(base: &Base, journal: &[VolumeOp], pace: &Ticks) -> BTreeSet<String> {
  let mut paths: BTreeSet<String> = BTreeSet::new();
  for (path, _) in &base.hardlinks {
    pace.tick().await;
    paths.insert(path.clone());
  }
  for op in journal {
    pace.tick().await;
    if let VolumeOp::Link { path, .. } = op {
      paths.insert(path.clone());
    }
  }
  paths
}

/// Composes the hard link operations — `Link`, and `Unlink` on a hard link path — per link path,
/// exactly as [`compose_symlinks`] does for symlinks: a new or retargeted link is one `Link`; a
/// base link removed is one `Unlink`; a link created then unlinked, or a base link removed then
/// recreated to the same target, is nothing ([`link_emissions`] over these states). Whether the
/// shared content survives an unlink is the merge's concern at apply time, not the composition's;
/// the writes a removed file leaves to a link are the seal's ([`heirs_of_writes`], A-669). The
/// states are each link path the journal touched, from its base state.
async fn link_states(
  base: &BaseIndex<'_>,
  journal: &[VolumeOp],
  paths: &BTreeSet<String>,
  pace: &Ticks,
) -> Result<BTreeMap<String, LinkState>, DeriveError> {
  let mut state: BTreeMap<String, LinkState> = BTreeMap::new();
  let initial = |path: &str| -> LinkState {
    match base.hardlink_target(path) {
      Some(target) => (true, target.to_owned(), NodeOrigin::Base),
      None => (false, String::new(), NodeOrigin::Base),
    }
  };
  // The paths the increment has made so far, and the targets of the links it made to a node it
  // made (A-646): a link names the node at its target when it is made, the increment's own or the
  // base's.
  let mut made = BTreeSet::new();
  let mut linked_made = BTreeSet::new();
  for op in journal {
    pace.tick().await;
    if let Some(moved) = made_target_moved(op, &linked_made) {
      return Err(DeriveError::Unsupported(moved.to_owned()));
    }
    match op {
      VolumeOp::Link { path, target } => {
        let here = state.entry(path.clone()).or_insert_with(|| initial(path));
        if here.0 {
          return Err(DeriveError::LinkOverExisting(path.clone()));
        }
        let origin = if made.contains(target) {
          linked_made.insert(target.clone());
          NodeOrigin::Made
        } else {
          NodeOrigin::Base
        };
        *here = (true, target.clone(), origin);
      }
      VolumeOp::Unlink { path } if paths.contains(path) => {
        let here = state.entry(path.clone()).or_insert_with(|| initial(path));
        if !here.0 {
          return Err(DeriveError::UnlinkMissing(path.clone()));
        }
        here.0 = false;
      }
      _ => {}
    }
    note_made(op, &mut made);
  }
  Ok(state)
}

/// Where the writes of a removed or replaced base file go, by its entity's index (A-669; A-646's decision: a removed
/// or replaced home leaves its node to its least surviving link, and the node is as the work left it). A base file the
/// journal wrote to and then removed, or replaced by renaming another file onto it, keeps its node while a hard link
/// still names it, and a volume reads the writes at that link. So the seal declares them at the link ([`classify`]),
/// where the seal's reader finds the node's bytes, beside the file's removal, and the engine makes the link the node's
/// home with them (`Green::merge_handed`). The heir is the least of the base's links to the file that the journal left
/// as they were. Refused, typed (`Unsupported`, owed), never accepted without the writes: writes whose node only links
/// the journal made still name, and writes to a file the journal made again at its own name, whose new node is
/// composed as an edit of the old one (GAPS). Until A-669 the writes were dropped with the file, and the green handed
/// the link the head's bytes.
async fn heirs_of_writes(
  base: &BaseIndex<'_>,
  entities: &[Entity],
  (links, nodes): (&BTreeMap<String, LinkState>, &LinkedNodes),
  pace: &Ticks,
) -> Result<BTreeMap<usize, String>, DeriveError> {
  let (live_origins, by_node) = heir_indexes(entities, nodes, pace).await;
  let mut heirs = BTreeMap::new();
  for (index, entity) in entities.iter().enumerate() {
    pace.tick().await;
    let Some(origin) = entity.origin.as_deref().filter(|_| !entity.live) else {
      continue;
    };
    let (writes, _) = compose_content_sized(entity.base_len, &entity.content);
    if writes.is_empty() {
      continue;
    }
    let named = by_node.get(origin).map(Vec::as_slice).unwrap_or_default();
    let (kept, made) = node_names(base, links, named, origin);
    if kept.is_empty() && made.is_empty() {
      // The node goes with its last name, and its writes with it.
      continue;
    }
    let made_again = live_origins.contains(origin);
    match kept.into_iter().next() {
      Some(heir) if !made_again => {
        heirs.insert(index, heir);
      }
      _ => return Err(DeriveError::Unsupported(origin.to_owned())),
    }
  }
  Ok(heirs)
}

/// [`heirs_of_writes`]'s indexes, built once (A-752) so no entity's unit scans every entity or every link: the base
/// paths a live entity came from, and each base node's standing links.
async fn heir_indexes<'e>(
  entities: &'e [Entity],
  nodes: &'e LinkedNodes,
  pace: &Ticks,
) -> (BTreeSet<&'e str>, BTreeMap<&'e str, Vec<&'e String>>) {
  let mut live_origins: BTreeSet<&str> = BTreeSet::new();
  for entity in entities {
    pace.tick().await;
    if entity.live
      && let Some(origin) = entity.origin.as_deref()
    {
      live_origins.insert(origin);
    }
  }
  let mut by_node: BTreeMap<&str, Vec<&String>> = BTreeMap::new();
  for (path, node) in nodes {
    pace.tick().await;
    if let Some(node) = node.as_deref() {
      by_node.entry(node).or_default().push(path);
    }
  }
  (live_origins, by_node)
}

/// The link paths standing at the seal that name `origin`'s base node (`nodes`, [`linked_nodes`]): those the base had
/// to the file and the journal left as they were (`links`, each link path the journal touched), the first set, and the
/// others.
fn node_names(
  base: &BaseIndex<'_>,
  links: &BTreeMap<String, LinkState>,
  named: &[&String],
  origin: &str,
) -> (BTreeSet<String>, BTreeSet<String>) {
  let as_based = |path: &&String| {
    base.hardlink_target(path) == Some(origin)
      && links
        .get(path.as_str())
        .is_none_or(|(here, target, node)| *here && target == origin && *node == NodeOrigin::Base)
  };
  let (kept, made): (Vec<&String>, Vec<&String>) = named.iter().copied().partition(as_based);
  (
    kept.into_iter().cloned().collect(),
    made.into_iter().cloned().collect(),
  )
}

/// Each hard link standing at the seal and the base node it names, by link path: `None` for a node the journal made
/// or moved.
type LinkedNodes = std::collections::BTreeMap<String, Option<String>>;

/// The node each hard link standing at the seal names (A-646: a link names the node at its target when it is made, so
/// a link made to a link names that link's node then, whatever becomes of that link after). A link the base had names
/// its base target's node until the journal removes it; a link the journal makes names what its target named then:
/// the node a standing link named, a base file's own, or a node the journal made or moved there (`None`, whose later
/// move or removal the deriver refuses, `made_target_moved`). A-669's first green run found a link resolved through
/// the links standing at the seal: `ln h l; rm h` lost `l`'s node, and with it a write handed on to `l`.
async fn linked_nodes(base: &Base, journal: &[VolumeOp], pace: &Ticks) -> LinkedNodes {
  let mut nodes: LinkedNodes = BTreeMap::new();
  for (link, target) in &base.hardlinks {
    pace.tick().await;
    nodes.insert(link.clone(), Some(target.clone()));
  }
  let mut made = BTreeSet::new();
  for op in journal {
    pace.tick().await;
    match op {
      VolumeOp::Link { path, target } => {
        let node = match nodes.get(target) {
          Some(named) => named.clone(),
          None if made.contains(target) => None,
          None => Some(target.clone()),
        };
        nodes.insert(path.clone(), node);
      }
      VolumeOp::Unlink { path } => {
        nodes.remove(path);
      }
      _ => {}
    }
    note_made(op, &mut made);
  }
  nodes
}

/// The links a composed state declares: a link standing where the base had none, or another, or
/// one naming a node the increment made (another node, whatever its path); a base link no longer
/// standing, removed.
async fn link_emissions(
  base: &BaseIndex<'_>,
  state: BTreeMap<String, LinkState>,
  pace: &Ticks,
) -> Vec<LinkEmission> {
  let mut emissions = Vec::new();
  for (path, (here, target, origin)) in state {
    pace.tick().await;
    let base_target = base.hardlink_target(&path);
    if here {
      let unchanged = origin == NodeOrigin::Base && base_target == Some(target.as_str());
      if !unchanged {
        emissions.push(LinkEmission::Made(path, target, origin));
      }
    } else if base_target.is_some() {
      emissions.push(LinkEmission::Removed(path));
    }
  }
  emissions
}

/// A `Link` op: `path` is the link's index, `src` the index of the node it names in the node table
/// (A-646).
fn link_op(link: u64, node: u64) -> Op {
  Op {
    kind: OpKind::Link,
    flags: 0,
    path: link,
    at: 0,
    len: 0,
    src: node,
  }
}

/// A namespace op with a path index and empty coordinates.
fn namespace_op(kind: OpKind, path: u64) -> Op {
  Op {
    kind,
    flags: 0,
    path,
    at: 0,
    len: 0,
    src: u64::MAX,
  }
}

/// A `Rename` op: `path` is the destination index, `src` the source path index into the table.
fn rename_op(destination: u64, source: u64) -> Op {
  Op {
    kind: OpKind::Rename,
    flags: 0,
    path: destination,
    at: 0,
    len: 0,
    src: source,
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
pub fn compose_volume(base: &Base, journal: &[VolumeOp]) -> Result<OpsDoc, DeriveError> {
  compose_volume_planned(base, journal).map(|(doc, _)| doc)
}

/// Where each run of an increment's post-state comes from: for the post-state offset (`src`) of each content op that
/// adds bytes, the offset of those bytes in the work's file at the seal, under the op's path. The seal copies each run
/// from there ([`compose_volume_planned`]).
pub type PostStatePlan = std::collections::BTreeMap<u64, u64>;

/// [`compose_volume`], with the plan the seal fills the post-state by (W2). The post-state is laid out compactly: each
/// added run at the next offset, in the document's file order, then the xattr values and the special nodes, so an
/// increment carries the bytes its ops add and nothing else. Until 2026-11-07 each edited file's whole final content
/// was a region of it, zero-filled but for the runs (a 64-byte overwrite at offset 640 KiB carried 640 KiB), and the
/// seal read each run at its op's offset in base coordinates rather than where the work's file holds it, so a run
/// after an earlier length change in the same file merged zeros into the green (`green_post_state.rs`).
pub fn compose_volume_planned(
  base: &Base,
  journal: &[VolumeOp],
) -> Result<(OpsDoc, PostStatePlan), DeriveError> {
  crate::pace::finish(compose_paced(base, journal, &Ticks))
}

/// [`compose_volume_planned`] a unit at a time under `pace` (A-752; §4.3's step budget): an entry of the base, an
/// operation of the journal, an entity, an emission or a path a unit, its sorts paced merge sorts, so a push's work
/// over any number of files composes over turns, no unit O(files). The same document and plan, whatever the pace.
pub async fn compose_paced(
  base: &Base,
  journal: &[VolumeOp],
  pace: &Ticks,
) -> Result<(OpsDoc, PostStatePlan), DeriveError> {
  let index = BaseIndex::of(base, pace).await;
  let checked = Checked::of(base, &index, journal, pace).await?;
  let entities = replay(&index, journal, &checked, pace).await?;
  let (survivors, renamed_away) =
    present_at_seal(base, &entities, &checked.specials.survivors, pace).await;
  let present_dirs = present_directories(base, journal, pace).await;
  let present = (&survivors, &renamed_away, &present_dirs);
  let (modes, xattrs) = attributes(&index, journal, present, pace).await?;
  let nodes = linked_nodes(base, journal, pace).await;
  let heirs = heirs_of_writes(&index, &entities, (&checked.link_state, &nodes), pace).await?;
  Ok(
    sealed(
      &index,
      journal,
      (entities, &heirs),
      (checked, modes, xattrs),
      pace,
    )
    .await,
  )
}

/// The document and plan of what was composed: the hard links' emissions, the seal, the IPC nodes' and the copies'
/// operations.
async fn sealed(
  index: &BaseIndex<'_>,
  journal: &[VolumeOp],
  files: (Vec<Entity>, &BTreeMap<usize, String>),
  (checked, modes, xattrs): (Checked, Vec<(String, u32)>, Vec<XattrEmission>),
  pace: &Ticks,
) -> (OpsDoc, PostStatePlan) {
  let Checked {
    specials,
    directories,
    symlinks,
    link_state,
    ..
  } = checked;
  let hardlinks = link_emissions(index, link_state, pace).await;
  let mut plan = PostStatePlan::new();
  let mut doc = seal(
    files,
    (directories, modes, symlinks, xattrs, hardlinks),
    &mut plan,
    pace,
  )
  .await;
  specials.emit(&mut doc, pace).await;
  emit_copies(&mut doc, journal, pace).await;
  (doc, plan)
}

/// The modes and xattrs composed ([`compose_modes`], [`compose_xattrs`]) over what is present at the seal.
async fn attributes(
  index: &BaseIndex<'_>,
  journal: &[VolumeOp],
  present: (&BTreeSet<String>, &BTreeSet<String>, &BTreeSet<String>),
  pace: &Ticks,
) -> Result<(Vec<(String, u32)>, Vec<XattrEmission>), DeriveError> {
  let modes = compose_modes(index, journal, present, pace).await?;
  let xattrs = compose_xattrs(index, journal, present, pace).await?;
  Ok((modes, xattrs))
}

/// What a composition checks and composes before it replays the files: the IPC nodes, the paths that are symlinks and
/// hard links, the directories, symlinks and links composed, each refusal typed.
struct Checked {
  /// The IPC nodes' composition.
  specials: crate::special::Composition,
  /// The paths the increment treats as symlinks.
  sym_paths: BTreeSet<String>,
  /// The paths it treats as hard links.
  link_paths: BTreeSet<String>,
  /// The directories it declares.
  directories: Vec<DirectoryEmission>,
  /// The symlinks it declares.
  symlinks: Vec<SymlinkEmission>,
  /// Each link path's composed state.
  link_state: BTreeMap<String, LinkState>,
}

impl Checked {
  /// `journal` checked and composed over `base`, a unit an entry or an operation.
  async fn of(
    base: &Base,
    index: &BaseIndex<'_>,
    journal: &[VolumeOp],
    pace: &Ticks,
  ) -> Result<Checked, DeriveError> {
    let specials =
      crate::special::Composition::of(base, (&index.link_targets, journal), pace).await?;
    check_copies(journal, pace).await?;
    let sym_paths = symlink_paths(base, journal, pace).await;
    let link_paths = hardlink_paths(base, journal, pace).await;
    check_no_file_directory_collision(
      index,
      journal,
      (&sym_paths, &link_paths, &specials.paths),
      pace,
    )
    .await?;
    let directories = compose_directories(index, journal, pace).await?;
    let symlinks = compose_symlinks(index, journal, &sym_paths, pace).await?;
    let link_state = link_states(index, journal, &link_paths, pace).await?;
    Ok(Checked {
      specials,
      sym_paths,
      link_paths,
      directories,
      symlinks,
      link_state,
    })
  }
}

/// The journal's file operations replayed over the base's files, an operation a unit: the entities at the seal.
async fn replay(
  index: &BaseIndex<'_>,
  journal: &[VolumeOp],
  checked: &Checked,
  pace: &Ticks,
) -> Result<Vec<Entity>, DeriveError> {
  let mut indexed = Entities::over(&index.files);
  for op in journal {
    pace.tick().await;
    if replays(op, checked) {
      apply_op(&mut indexed, op)?;
    }
  }
  Ok(indexed.list)
}

/// Whether `op` is the files' to replay: not a directory, symlink, link, node or copy, nor an unlink of a path one of
/// those compositions removes (done there).
fn replays(op: &VolumeOp, checked: &Checked) -> bool {
  if op.is_directory()
    || op.is_symlink()
    || op.is_link()
    || matches!(op, VolumeOp::Mknod { .. } | VolumeOp::Copy { .. })
  {
    return false;
  }
  !matches!(op, VolumeOp::Unlink { path }
    if checked.sym_paths.contains(path.as_str())
      || checked.link_paths.contains(path.as_str())
      || checked.specials.paths.contains(path))
}

/// The files present at seal, a unit an entity or a base file: the live entities' final paths, base files no entity
/// ever touched (still present, e.g. a base file that was only chmod'd) and the surviving IPC nodes; and the base paths
/// live files were renamed away from.
async fn present_at_seal(
  base: &Base,
  entities: &[Entity],
  special_survivors: &BTreeSet<String>,
  pace: &Ticks,
) -> (BTreeSet<String>, BTreeSet<String>) {
  let mut entity_origins: BTreeSet<&str> = BTreeSet::new();
  let mut survivors: BTreeSet<String> = BTreeSet::new();
  let mut renamed_away: BTreeSet<String> = BTreeSet::new();
  for entity in entities {
    pace.tick().await;
    if let Some(origin) = entity.origin.as_deref() {
      entity_origins.insert(origin);
    }
    if !entity.live {
      continue;
    }
    survivors.insert(entity.final_path.clone());
    if let Some(origin) = &entity.origin
      && origin != &entity.final_path
    {
      renamed_away.insert(origin.clone());
    }
  }
  untouched(
    base,
    &entity_origins,
    special_survivors,
    (&mut survivors, pace),
  )
  .await;
  (survivors, renamed_away)
}

/// The base files no entity came from and the surviving IPC nodes, added to `survivors`, a unit a path.
async fn untouched(
  base: &Base,
  entity_origins: &BTreeSet<&str>,
  special_survivors: &BTreeSet<String>,
  (survivors, pace): (&mut BTreeSet<String>, &Ticks),
) {
  for (path, _) in &base.files {
    pace.tick().await;
    if !entity_origins.contains(path.as_str()) {
      survivors.insert(path.clone());
    }
  }
  for path in special_survivors {
    pace.tick().await;
    survivors.insert(path.clone());
  }
}

/// Whether `path` lies in `root`'s tree: `root` itself or beneath it; every path, for the root (the empty path).
fn within(path: &str, root: &str) -> bool {
  root.is_empty()
    || path == root
    || path
      .strip_prefix(root)
      .is_some_and(|rest| rest.starts_with('/'))
}

/// Refuses a journal that names a path in a copy's destination tree before the copy (A-626 part 4). The base a
/// journal holding copies is composed against holds each copied tree where its copy puts it (`Green::base_for`), so an
/// earlier operation there would be composed against what the copy brings, not what it changed: owed
/// (`DeriveError::Unsupported`), as the deriver's other compositions over a reused name are.
async fn check_copies(journal: &[VolumeOp], pace: &Ticks) -> Result<(), DeriveError> {
  // Each name the journal has named so far, and where it was first named: a copy's refusal names the earliest
  // operation's name in its tree, as a scan of the operations before it found it, each lookup a range of the names.
  let mut named: BTreeMap<&str, usize> = BTreeMap::new();
  for (index, op) in journal.iter().enumerate() {
    pace.tick().await;
    if let VolumeOp::Copy { to, .. } = op {
      let mut earliest: Option<(usize, &str)> = None;
      for (name, at) in named.range(to.as_str()..) {
        pace.tick().await;
        if !name.starts_with(to.as_str()) {
          break;
        }
        if within(name, to) && earliest.is_none_or(|(first, _)| *at < first) {
          earliest = Some((*at, name));
        }
      }
      if let Some((_, name)) = earliest {
        return Err(DeriveError::Unsupported((*name).to_owned()));
      }
    }
    for (slot, name) in op.names().into_iter().enumerate() {
      if let Some(name) = name {
        named
          .entry(name)
          .or_insert(index.saturating_mul(2).saturating_add(slot));
      }
    }
  }
  Ok(())
}

/// Emits the journal's copies (A-626 part 4) by destination, so their order is not the journal's: each one `Copy`,
/// its destination the op's path, its source the `src`, the source's version the `at`, and `REPLACE` where it replaces
/// what its destination holds.
async fn emit_copies(doc: &mut OpsDoc, journal: &[VolumeOp], pace: &Ticks) {
  let mut copies: Vec<(&str, &str, u64, bool)> = Vec::new();
  for op in journal {
    pace.tick().await;
    if let VolumeOp::Copy {
      from,
      to,
      version,
      replace,
    } = op
    {
      copies.push((to.as_str(), from.as_str(), *version, *replace));
    }
  }
  let copies = crate::pace::sort_by(copies, pace, Ord::cmp).await;
  for (to, from, version, replace) in copies {
    pace.tick().await;
    let path = doc.paths.intern(to);
    let src = doc.paths.intern(from);
    doc.ops.push(Op {
      kind: OpKind::Copy,
      flags: if replace { crate::ops_doc::REPLACE } else { 0 },
      path,
      at: version,
      len: 0,
      src,
    });
  }
}

/// A directory the increment declares: a create or a remove.
enum DirectoryEmission {
  /// A new directory at this path.
  Made(String),
  /// A base directory removed at this path.
  Removed(String),
}

/// Refuses when any path is used as both a file and a directory this increment (including against
/// the base's kinds): a file/directory transition at a path, whose composition is owed.
async fn check_no_file_directory_collision(
  base: &BaseIndex<'_>,
  journal: &[VolumeOp],
  removals: (&BTreeSet<String>, &BTreeSet<String>, &BTreeSet<String>),
  pace: &Ticks,
) -> Result<(), DeriveError> {
  let special_paths = removals.2;
  let intents = Intents::of(journal, removals, pace).await;
  let Intents {
    files,
    dirs,
    syms,
    links,
  } = &intents;
  let base_file = |path: &str| base.files.contains_key(path);
  let base_dir = |path: &str| base.dirs.contains(path);
  let base_sym = |path: &str| base.symlinks.contains_key(path);
  let base_link = |path: &str| base.hardlinks.contains_key(path);
  let any_kind = |path: &str| {
    files.contains(path)
      || dirs.contains(path)
      || syms.contains(path)
      || links.contains(path)
      || base_file(path)
      || base_dir(path)
      || base_sym(path)
      || base_link(path)
  };
  let kind_conflict = DeriveError::PathKindConflict;
  let file_and_dir = DeriveError::PathIsFileAndDirectory;
  let specials = special_paths.iter().map(String::as_str);
  refuse_any(specials, any_kind, kind_conflict, pace).await?;
  // A file operation must not fall on a base directory or symlink.
  let on_dir =
    |path: &str| dirs.contains(path) || base_dir(path) || base_sym(path) || base_link(path);
  refuse_any(files.iter().copied(), on_dir, file_and_dir, pace).await?;
  // A directory operation must not fall on a base file or symlink, nor share a path with one.
  let on_file = |path: &str| files.contains(path) || base_file(path) || base_sym(path);
  refuse_any(dirs.iter().copied(), on_file, file_and_dir, pace).await?;
  // A symlink create must not fall on a base file or directory, nor share a path with a file or
  // directory operation.
  let sym_clash = |path: &str| {
    files.contains(path)
      || dirs.contains(path)
      || links.contains(path)
      || base_file(path)
      || base_dir(path)
      || base_link(path)
  };
  refuse_any(syms.iter().copied(), sym_clash, kind_conflict, pace).await?;
  let link_clash = |path: &str| {
    files.contains(path)
      || dirs.contains(path)
      || base_file(path)
      || base_dir(path)
      || base_sym(path)
  };
  refuse_any(links.iter().copied(), link_clash, kind_conflict, pace).await
}

/// The first of `paths` that `clashes`, refused as `refusal` names it, a unit a path.
async fn refuse_any<'p>(
  paths: impl Iterator<Item = &'p str>,
  clashes: impl Fn(&str) -> bool,
  refusal: fn(String) -> DeriveError,
  pace: &Ticks,
) -> Result<(), DeriveError> {
  for path in paths {
    pace.tick().await;
    if clashes(path) {
      return Err(refusal(path.to_owned()));
    }
  }
  Ok(())
}

/// The paths a journal's operations use as each kind ([`check_no_file_directory_collision`]).
struct Intents<'j> {
  /// The paths a file operation names.
  files: BTreeSet<&'j str>,
  /// The paths a directory is made or removed at.
  dirs: BTreeSet<&'j str>,
  /// The paths a symlink is made at.
  syms: BTreeSet<&'j str>,
  /// The paths a hard link is made at.
  links: BTreeSet<&'j str>,
}

impl<'j> Intents<'j> {
  /// `journal`'s paths by kind, an operation a unit: an unlink of a path a symlink, hard link or IPC node composition
  /// removes (`removals`) is that removal, not a file operation.
  async fn of(
    journal: &'j [VolumeOp],
    (sym_paths, link_paths, special_paths): (
      &BTreeSet<String>,
      &BTreeSet<String>,
      &BTreeSet<String>,
    ),
    pace: &Ticks,
  ) -> Intents<'j> {
    let mut intents = Intents {
      files: BTreeSet::new(),
      dirs: BTreeSet::new(),
      syms: BTreeSet::new(),
      links: BTreeSet::new(),
    };
    for op in journal {
      pace.tick().await;
      match op {
        VolumeOp::Mkdir { path } | VolumeOp::Rmdir { path } => {
          intents.dirs.insert(path);
        }
        VolumeOp::Symlink { path, .. } => {
          intents.syms.insert(path);
        }
        VolumeOp::Link { path, .. } => {
          intents.links.insert(path);
        }
        VolumeOp::Rename { from, to } => {
          intents.files.insert(from);
          intents.files.insert(to);
        }
        VolumeOp::Unlink { path }
          if sym_paths.contains(path.as_str())
            || link_paths.contains(path.as_str())
            || special_paths.contains(path) => {}
        other => {
          intents.files.extend(other.single_path());
        }
      }
    }
    intents
  }
}

/// Composes the directory operations into a create or remove per touched directory path: a base
/// directory removed is a `Rmdir`; a new directory that survives is a `Mkdir`; a mkdir then an
/// rmdir cancels, and a base directory removed then recreated is unchanged (directories have no
/// content). A mkdir over a present directory, or an rmdir of an absent one, is a typed refusal.
async fn compose_directories(
  base: &BaseIndex<'_>,
  journal: &[VolumeOp],
  pace: &Ticks,
) -> Result<Vec<DirectoryEmission>, DeriveError> {
  let mut present: BTreeMap<String, bool> = BTreeMap::new();
  for op in journal {
    pace.tick().await;
    let (path, making) = match op {
      VolumeOp::Mkdir { path } => (path, true),
      VolumeOp::Rmdir { path } => (path, false),
      _ => continue,
    };
    let base_is_dir = base.dirs.contains(path.as_str());
    let here = present.entry(path.clone()).or_insert(base_is_dir);
    if making {
      if *here {
        return Err(DeriveError::MkdirOverExisting(path.clone()));
      }
      *here = true;
    } else {
      if !*here {
        return Err(DeriveError::RmdirMissing(path.clone()));
      }
      *here = false;
    }
  }
  let mut emissions = Vec::new();
  for (path, here) in present {
    pace.tick().await;
    let base_is_dir = base.dirs.contains(path.as_str());
    if here && !base_is_dir {
      emissions.push(DirectoryEmission::Made(path));
    } else if !here && base_is_dir {
      emissions.push(DirectoryEmission::Removed(path));
    }
  }
  Ok(emissions)
}

/// One surviving file's emission: what it declares and the bytes it contributes to the post-state.
struct Emission {
  /// The file's final path.
  final_path: String,
  /// The rename source, when this file was renamed from another path.
  source: Option<String>,
  /// Whether this file was created this increment (declares a `Create`).
  created: bool,
  /// The composed content net ops (their `src` the run's offset in the final file until laid into the post-state at
  /// emit, [`push_content`]).
  ops: Vec<Op>,
}

/// Emits the ops document from the sealed entity set. A surviving file that was created, renamed,
/// or has content net ops is one emission; a base file edited to no net change is not in the
/// increment at all (keeping the identity minimal); a removed base file is an `Unlink`.
/// Classifies the sealed entity set into the surviving files' emissions and the base paths to
/// unlink. A base file edited to no net change is dropped (so a touched-then-reverted file leaves
/// no trace and the identity stays minimal); a gone base path is unlinked only when no surviving
/// file occupies it (a rename or content replacement onto that path covers its removal). A gone
/// base file whose writes a link inherits (`heirs`, by entity index, [`heirs_of_writes`]) is also an
/// emission at that link: the writes, which the seal reads there (A-669).
async fn classify(
  entities: Vec<Entity>,
  heirs: &BTreeMap<usize, String>,
  pace: &Ticks,
) -> (Vec<Emission>, Vec<String>) {
  let mut emissions: Vec<Emission> = Vec::new();
  let mut survivors: BTreeSet<String> = BTreeSet::new();
  let mut gone: Vec<String> = Vec::new();
  for (index, entity) in entities.into_iter().enumerate() {
    pace.tick().await;
    if let Some(heir) = heirs.get(&index) {
      let (ops, _) = compose_content_sized(entity.base_len, &entity.content);
      emissions.push(Emission {
        final_path: heir.clone(),
        source: None,
        created: false,
        ops,
      });
    }
    if entity.live {
      survivors.insert(entity.final_path.clone());
      emissions.extend(survivor_emission(entity));
    } else if let Some(origin) = entity.origin {
      gone.push(origin);
    }
  }
  let mut removed: Vec<String> = Vec::new();
  for origin in gone {
    pace.tick().await;
    if !survivors.contains(&origin) {
      removed.push(origin);
    }
  }
  let removed = sorted_names(removed, pace).await;
  (emissions, removed)
}

/// A live entity's emission: none for a base file edited to no net change.
fn survivor_emission(entity: Entity) -> Option<Emission> {
  let (ops, _) = compose_content_sized(entity.base_len, &entity.content);
  let created = entity.origin.is_none();
  let renamed = matches!(&entity.origin, Some(origin) if origin != &entity.final_path);
  if !created && !renamed && ops.is_empty() {
    return None;
  }
  let source = if renamed { entity.origin } else { None };
  Some(Emission {
    final_path: entity.final_path,
    source,
    created,
    ops,
  })
}

/// `names` sorted and deduplicated, a unit a name (A-752): what `sort_unstable` then `dedup` gave.
async fn sorted_names(names: Vec<String>, pace: &Ticks) -> Vec<String> {
  let sorted = crate::pace::sort_by(names, pace, Ord::cmp).await;
  let mut unique: Vec<String> = Vec::with_capacity(sorted.len());
  for name in sorted {
    pace.tick().await;
    if unique.last() != Some(&name) {
      unique.push(name);
    }
  }
  unique
}

/// Every path the document names — the emissions' final paths, their rename sources, the removed
/// paths, and the directory paths — sorted and deduplicated, so the path table is sorted and the
/// indices are stable (a rename's source index is a table index, which the sorted table keeps
/// valid).
async fn document_names(
  (emissions, removed): (&[Emission], &[String]),
  (directories, modes, symlinks, xattrs, hardlinks): NamespaceRefs<'_>,
  pace: &Ticks,
) -> Vec<String> {
  let mut names: Vec<String> = Vec::new();
  for emission in emissions {
    pace.tick().await;
    names.push(emission.final_path.clone());
    names.extend(emission.source.iter().cloned());
  }
  for path in removed {
    pace.tick().await;
    names.push(path.clone());
  }
  directory_names((directories, modes), &mut names, pace).await;
  link_names((symlinks, xattrs, hardlinks), &mut names, pace).await;
  sorted_names(names, pace).await
}

/// The names the directories and modes declare, added to `names`, a unit an emission.
async fn directory_names(
  (directories, modes): (&[DirectoryEmission], &[(String, u32)]),
  names: &mut Vec<String>,
  pace: &Ticks,
) {
  for directory in directories {
    pace.tick().await;
    let (DirectoryEmission::Made(path) | DirectoryEmission::Removed(path)) = directory;
    names.push(path.clone());
  }
  for (path, _) in modes {
    pace.tick().await;
    names.push(path.clone());
  }
}

/// The names the symlinks, xattrs and hard links declare, added to `names`, a unit an emission.
async fn link_names(
  (symlinks, xattrs, hardlinks): (&[SymlinkEmission], &[XattrEmission], &[LinkEmission]),
  names: &mut Vec<String>,
  pace: &Ticks,
) {
  for symlink in symlinks {
    pace.tick().await;
    match symlink {
      SymlinkEmission::Made(path, target) => names.extend([path.clone(), target.clone()]),
      SymlinkEmission::Removed(path) => names.push(path.clone()),
    }
  }
  for xattr in xattrs {
    pace.tick().await;
    let (XattrEmission::Set(path, name, _) | XattrEmission::Removed(path, name)) = xattr;
    names.extend([path.clone(), name.clone()]);
  }
  for link in hardlinks {
    pace.tick().await;
    match link {
      LinkEmission::Made(path, target, _) => names.extend([path.clone(), target.clone()]),
      LinkEmission::Removed(path) => names.push(path.clone()),
    }
  }
}

/// The namespace emissions a document names, borrowed: directories, modes, symlinks, xattrs and hard links.
type NamespaceRefs<'n> = (
  &'n [DirectoryEmission],
  &'n [(String, u32)],
  &'n [SymlinkEmission],
  &'n [XattrEmission],
  &'n [LinkEmission],
);

/// The namespace operations [`emit_namespace`] lays: the removed base files, directories, modes, symlinks and hard
/// links.
type NamespaceOps = (
  Vec<String>,
  Vec<DirectoryEmission>,
  Vec<(String, u32)>,
  Vec<SymlinkEmission>,
  Vec<LinkEmission>,
);

/// Emits the namespace operations — unlinks of removed base files, directory create/remove, mode
/// changes, and symlink create/remove — into the document, resolving each path to its table index.
async fn emit_namespace(
  doc: &mut OpsDoc,
  names: &[String],
  (removed, directories, modes, symlinks, hardlinks): NamespaceOps,
  pace: &Ticks,
) {
  for origin in removed {
    pace.tick().await;
    doc
      .ops
      .push(namespace_op(OpKind::Unlink, index_in(names, &origin)));
  }
  for directory in directories {
    pace.tick().await;
    let op = match directory {
      DirectoryEmission::Made(path) => namespace_op(OpKind::Mkdir, index_in(names, &path)),
      DirectoryEmission::Removed(path) => namespace_op(OpKind::Rmdir, index_in(names, &path)),
    };
    doc.ops.push(op);
  }
  for (path, mode) in modes {
    pace.tick().await;
    doc.ops.push(set_mode_op(index_in(names, &path), mode));
  }
  emit_links(doc, names, (symlinks, hardlinks), pace).await;
}

/// The symlink and hard link operations laid into `doc`, a unit an emission.
async fn emit_links(
  doc: &mut OpsDoc,
  names: &[String],
  (symlinks, hardlinks): (Vec<SymlinkEmission>, Vec<LinkEmission>),
  pace: &Ticks,
) {
  for symlink in symlinks {
    pace.tick().await;
    let op = match symlink {
      SymlinkEmission::Made(path, target) => {
        symlink_op(index_in(names, &path), index_in(names, &target))
      }
      SymlinkEmission::Removed(path) => namespace_op(OpKind::Unlink, index_in(names, &path)),
    };
    doc.ops.push(op);
  }
  for link in hardlinks {
    pace.tick().await;
    let op = match link {
      LinkEmission::Made(path, target, origin) => {
        let node = doc.nodes.intern(NodeRef {
          path: index_in(names, &target),
          origin,
        });
        link_op(index_in(names, &path), node)
      }
      LinkEmission::Removed(path) => namespace_op(OpKind::Unlink, index_in(names, &path)),
    };
    doc.ops.push(op);
  }
}

/// A name's index: its place among the sorted `names`; one not among them is `u64::MAX`, which names no path, never
/// another's (A-621: a 16-bit index answered `u16::MAX`, the 65,536th name's, for both).
fn index_in(names: &[String], name: &str) -> u64 {
  names
    .binary_search_by(|candidate| candidate.as_str().cmp(name))
    .map_or(u64::MAX, |position| {
      u64::try_from(position).unwrap_or(u64::MAX)
    })
}

/// The namespace emissions a seal lays out besides the files: directories, modes, symlinks, xattrs and hard links.
type Namespace = (
  Vec<DirectoryEmission>,
  Vec<(String, u32)>,
  Vec<SymlinkEmission>,
  Vec<XattrEmission>,
  Vec<LinkEmission>,
);

async fn seal(
  (entities, heirs): (Vec<Entity>, &BTreeMap<usize, String>),
  (directories, modes, symlinks, xattrs, hardlinks): Namespace,
  plan: &mut PostStatePlan,
  pace: &Ticks,
) -> OpsDoc {
  let (emissions, removed) = classify(entities, heirs, pace).await;
  let names = document_names(
    (&emissions, &removed),
    (&directories, &modes, &symlinks, &xattrs, &hardlinks),
    pace,
  )
  .await;
  let mut doc = OpsDoc::new();
  for name in &names {
    pace.tick().await;
    doc.paths.intern(name);
  }
  let region_offset = emit_files(&mut doc, &names, (emissions, plan), pace).await;
  emit_xattrs(&mut doc, &names, (xattrs, region_offset), pace).await;
  emit_namespace(
    &mut doc,
    &names,
    (removed, directories, modes, symlinks, hardlinks),
    pace,
  )
  .await;
  doc.canonicalize_paced(pace).await;
  doc
}

/// The files' operations laid into `doc`, a unit an emission, in sorted final-path order: the post-state's runs follow
/// it, each at the next offset, planned in `plan`. The offset past the last run.
async fn emit_files(
  doc: &mut OpsDoc,
  names: &[String],
  (emissions, plan): (Vec<Emission>, &mut PostStatePlan),
  pace: &Ticks,
) -> u64 {
  let emissions =
    crate::pace::sort_by(emissions, pace, |a, b| a.final_path.cmp(&b.final_path)).await;
  let mut region_offset = 0u64;
  for emission in emissions {
    pace.tick().await;
    let destination = index_in(names, &emission.final_path);
    if let Some(source) = &emission.source {
      doc
        .ops
        .push(rename_op(destination, index_in(names, source)));
    } else if emission.created {
      doc.ops.push(namespace_op(OpKind::Create, destination));
    }
    push_content(doc, emission.ops, destination, (&mut region_offset, plan));
  }
  region_offset
}

/// The xattr operations laid into `doc`, a unit an emission: a set's value in the post-state after the file content,
/// from `region_offset`, in sorted order.
async fn emit_xattrs(
  doc: &mut OpsDoc,
  names: &[String],
  (xattrs, mut region_offset): (Vec<XattrEmission>, u64),
  pace: &Ticks,
) {
  for xattr in xattrs {
    pace.tick().await;
    let op = match xattr {
      XattrEmission::Set(path, name, value) => {
        let value_len = value.len() as u64;
        let op = set_xattr_op(
          index_in(names, &path),
          index_in(names, &name),
          region_offset,
          value_len,
        );
        region_offset = region_offset.saturating_add(value_len);
        op
      }
      XattrEmission::Removed(path, name) => {
        remove_xattr_op(index_in(names, &path), index_in(names, &name))
      }
    };
    doc.ops.push(op);
  }
}

/// Pushes a file's content ops into the document, setting their path index and giving each content-adding op the
/// next post-state offset at `cursor`, its offset in the work's file (the deriver's `src`) planned under it.
fn push_content(
  doc: &mut OpsDoc,
  ops: Vec<Op>,
  index: u64,
  (cursor, plan): (&mut u64, &mut PostStatePlan),
) {
  for mut op in ops {
    op.path = index;
    if adds_content(op.kind) {
      plan.insert(*cursor, op.src);
      op.src = *cursor;
      *cursor = cursor.saturating_add(op.len);
    }
    doc.ops.push(op);
  }
}

#[cfg(test)]
mod tests;
