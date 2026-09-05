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
//! Scope: content, create, unlink and rename of regular files, and directory create and remove
//! (composed independently; a mkdir then rmdir cancels, a base directory removed is one `Rmdir`).
//! Hard links, symlinks and metadata (mode, xattrs), a file/directory transition at one path, and
//! the rare rename onto a base path already consumed this increment are the deriver's remaining
//! piece (owed; GAPS §8f), each a typed [`DeriveError`]. An operation a valid volume could not
//! have produced — content on a missing file, a create over an existing one, an unlink, rename or
//! rmdir of a missing one, a mkdir over an existing directory — is a typed [`DeriveError`] too,
//! never a panic. This module is pure: no I/O, no clock, no randomness.

use crate::derive::{ContentOp, compose_content_sized};
use crate::ops_doc::{Op, OpKind, OpsDoc};

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
      | VolumeOp::Rmdir { .. } => None,
    }
  }

  /// Whether this is a directory operation (`Mkdir` or `Rmdir`).
  fn is_directory(&self) -> bool {
    matches!(self, VolumeOp::Mkdir { .. } | VolumeOp::Rmdir { .. })
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
      VolumeOp::Rename { .. } | VolumeOp::Mkdir { .. } | VolumeOp::Rmdir { .. } => None,
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
  /// The base directory paths.
  pub dirs: Vec<String>,
}

impl Base {
  /// A base with the given files and no directories (the common file-only case).
  pub fn of_files(files: Vec<(String, u64)>) -> Base {
    Base {
      files,
      dirs: Vec::new(),
    }
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

/// The index of the live entity at `path`, if any.
fn live_index(entities: &[Entity], path: &str) -> Option<usize> {
  entities.iter().position(|e| e.live && e.final_path == path)
}

/// The base content length of `path`, if it existed at base.
fn base_len_of(base: &Base, path: &str) -> Option<u64> {
  base
    .files
    .iter()
    .find(|(candidate, _)| candidate == path)
    .map(|(_, len)| *len)
}

/// Whether `path`'s base file has already been materialized into an entity (so it cannot be
/// materialized again).
fn consumed(entities: &[Entity], path: &str) -> bool {
  entities.iter().any(|e| e.origin.as_deref() == Some(path))
}

/// Materializes an untouched base file into a live entity and returns its index, or `None` when
/// `path` is not an untouched base file (not at base, or already consumed).
fn materialize_base(entities: &mut Vec<Entity>, base: &Base, path: &str) -> Option<usize> {
  if consumed(entities, path) {
    return None;
  }
  let base_len = base_len_of(base, path)?;
  entities.push(Entity {
    origin: Some(path.to_owned()),
    base_len,
    content: Vec::new(),
    live: true,
    final_path: path.to_owned(),
    clobbered: false,
  });
  Some(entities.len() - 1)
}

/// Applies one operation to the entity set.
fn apply_op(entities: &mut Vec<Entity>, base: &Base, op: &VolumeOp) -> Result<(), DeriveError> {
  if let Some(content) = op.content() {
    let path = op.single_path().unwrap_or("");
    let index = live_index(entities, path)
      .or_else(|| materialize_base(entities, base, path))
      .ok_or_else(|| DeriveError::ContentOnMissing(path.to_owned()))?;
    entities[index].content.push(content);
    return Ok(());
  }
  match op {
    VolumeOp::Create { path } => apply_create(entities, base, path),
    VolumeOp::Unlink { path } => apply_unlink(entities, base, path),
    VolumeOp::Rename { from, to } => apply_rename(entities, base, from, to),
    _ => Ok(()),
  }
}

/// A `Create`: refuses over a live or untouched-base file; recreates an in-place-unlinked base
/// file as a content replacement; otherwise makes a fresh new file.
fn apply_create(entities: &mut Vec<Entity>, base: &Base, path: &str) -> Result<(), DeriveError> {
  if live_index(entities, path).is_some() {
    return Err(DeriveError::CreateOverExisting(path.to_owned()));
  }
  // An in-place unlink of this base path (dead, not clobbered, still named here): recreating it
  // replaces the base content.
  let replaced = entities.iter().position(|e| {
    !e.live && !e.clobbered && e.final_path == path && e.origin.as_deref() == Some(path)
  });
  if let Some(index) = replaced {
    let base_len = entities[index].base_len;
    // The recreation supersedes the unlink: mark the dead entity clobbered so seal emits no
    // stale `Unlink` for a path that is present again.
    entities[index].clobbered = true;
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
  if base_len_of(base, path).is_some() {
    // A base path here: an untouched one is still present (create over it fails); a consumed one
    // (its base was renamed away — the in-place-unlink case was handled above) is the owed
    // recreate-at-a-renamed-away-base-path case.
    return if consumed(entities, path) {
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

/// An `Unlink`: kills the live (or materialized-base) entity at `path`.
fn apply_unlink(entities: &mut Vec<Entity>, base: &Base, path: &str) -> Result<(), DeriveError> {
  let index = live_index(entities, path)
    .or_else(|| materialize_base(entities, base, path))
    .ok_or_else(|| DeriveError::UnlinkMissing(path.to_owned()))?;
  entities[index].live = false;
  entities[index].content.clear();
  Ok(())
}

/// A `Rename`: moves the source entity to `to`, replacing whatever was there.
fn apply_rename(
  entities: &mut Vec<Entity>,
  base: &Base,
  from: &str,
  to: &str,
) -> Result<(), DeriveError> {
  if from == to {
    return Ok(());
  }
  let source = live_index(entities, from)
    .or_else(|| materialize_base(entities, base, from))
    .ok_or_else(|| DeriveError::RenameMissingSource(from.to_owned()))?;
  let destination = resolve_rename_target(entities, base, to)?;
  if let Some(dst) = destination {
    entities[dst].live = false;
    entities[dst].clobbered = true;
    let dst_is_base = entities[dst].origin.is_some();
    let dst_base_len = entities[dst].base_len;
    if dst_is_base && entities[source].origin.is_none() {
      // A new file renamed over a base file: the destination's content is replaced (a delete of
      // the base and the new bytes), so re-root the source at the destination's base.
      entities[source].origin = Some(to.to_owned());
      entities[source].base_len = dst_base_len;
      entities[source]
        .content
        .insert(0, ContentOp::Truncate { len: 0 });
    }
    // A base file over a base file keeps its origin (emitting a `Rename` at seal); a new file
    // clobbered here simply cancels.
  }
  entities[source].final_path = to.to_owned();
  Ok(())
}

/// Resolves a rename's destination: a live entity there, or an untouched base file materialized
/// as the clobber target, or `None` when the path is free. A base path already consumed is a
/// typed refusal (owed).
fn resolve_rename_target(
  entities: &mut Vec<Entity>,
  base: &Base,
  to: &str,
) -> Result<Option<usize>, DeriveError> {
  if let Some(index) = live_index(entities, to) {
    return Ok(Some(index));
  }
  if base_len_of(base, to).is_some() {
    if consumed(entities, to) {
      return Err(DeriveError::Unsupported(to.to_owned()));
    }
    return Ok(materialize_base(entities, base, to));
  }
  Ok(None)
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

/// A `Rename` op: `path` is the destination index, `src` the source path index into the table.
fn rename_op(destination: u16, source: u16) -> Op {
  Op {
    kind: OpKind::Rename,
    flags: 0,
    path: destination,
    at: 0,
    len: 0,
    src: u64::from(source),
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
  check_no_file_directory_collision(base, journal)?;
  let directories = compose_directories(base, journal)?;
  let mut entities: Vec<Entity> = Vec::new();
  for op in journal {
    if op.is_directory() {
      continue;
    }
    apply_op(&mut entities, base, op)?;
  }
  Ok(seal(entities, directories))
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
fn check_no_file_directory_collision(base: &Base, journal: &[VolumeOp]) -> Result<(), DeriveError> {
  let mut file_paths: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
  let mut dir_paths: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
  for op in journal {
    match op {
      VolumeOp::Mkdir { path } | VolumeOp::Rmdir { path } => {
        dir_paths.insert(path);
      }
      VolumeOp::Rename { from, to } => {
        file_paths.insert(from);
        file_paths.insert(to);
      }
      other => {
        if let Some(path) = other.single_path() {
          file_paths.insert(path);
        }
      }
    }
  }
  for path in &dir_paths {
    if file_paths.contains(path) || base.files.iter().any(|(file, _)| file == path) {
      return Err(DeriveError::PathIsFileAndDirectory((*path).to_owned()));
    }
  }
  for path in &file_paths {
    if base.dirs.iter().any(|dir| dir == path) {
      return Err(DeriveError::PathIsFileAndDirectory((*path).to_owned()));
    }
  }
  Ok(())
}

/// Composes the directory operations into a create or remove per touched directory path: a base
/// directory removed is a `Rmdir`; a new directory that survives is a `Mkdir`; a mkdir then an
/// rmdir cancels, and a base directory removed then recreated is unchanged (directories have no
/// content). A mkdir over a present directory, or an rmdir of an absent one, is a typed refusal.
fn compose_directories(
  base: &Base,
  journal: &[VolumeOp],
) -> Result<Vec<DirectoryEmission>, DeriveError> {
  let mut present: std::collections::BTreeMap<String, bool> = std::collections::BTreeMap::new();
  for op in journal {
    let (path, making) = match op {
      VolumeOp::Mkdir { path } => (path, true),
      VolumeOp::Rmdir { path } => (path, false),
      _ => continue,
    };
    let base_is_dir = base.dirs.iter().any(|dir| dir == path);
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
    let base_is_dir = base.dirs.iter().any(|dir| dir == &path);
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
  /// The composed content net ops (their `src` per-file until shifted into the region at emit).
  ops: Vec<Op>,
  /// The file's final content length (its post-state region size).
  final_len: u64,
}

/// Emits the ops document from the sealed entity set. A surviving file that was created, renamed,
/// or has content net ops is one emission; a base file edited to no net change is not in the
/// increment at all (keeping the identity minimal); a removed base file is an `Unlink`.
/// Classifies the sealed entity set into the surviving files' emissions and the base paths to
/// unlink. A base file edited to no net change is dropped (so a touched-then-reverted file leaves
/// no trace and the identity stays minimal); a gone base path is unlinked only when no surviving
/// file occupies it (a rename or content replacement onto that path covers its removal).
fn classify(entities: Vec<Entity>) -> (Vec<Emission>, Vec<String>) {
  let mut emissions: Vec<Emission> = Vec::new();
  let mut survivors: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
  let mut gone: Vec<String> = Vec::new();
  for entity in entities {
    if entity.live {
      survivors.insert(entity.final_path.clone());
      let (ops, final_len) = compose_content_sized(entity.base_len, &entity.content);
      let created = entity.origin.is_none();
      let renamed = matches!(&entity.origin, Some(origin) if origin != &entity.final_path);
      if !created && !renamed && ops.is_empty() {
        continue;
      }
      let source = if renamed { entity.origin.clone() } else { None };
      emissions.push(Emission {
        final_path: entity.final_path,
        source,
        created,
        ops,
        final_len,
      });
    } else if let Some(origin) = entity.origin {
      gone.push(origin);
    }
  }
  let mut removed: Vec<String> = gone
    .into_iter()
    .filter(|origin| !survivors.contains(origin))
    .collect();
  removed.sort_unstable();
  removed.dedup();
  (emissions, removed)
}

/// Every path the document names — the emissions' final paths, their rename sources, the removed
/// paths, and the directory paths — sorted and deduplicated, so the path table is sorted and the
/// indices are stable (a rename's source index is a table index, which the sorted table keeps
/// valid).
fn document_names(
  emissions: &[Emission],
  removed: &[String],
  directories: &[DirectoryEmission],
) -> Vec<String> {
  let mut names: Vec<String> = Vec::new();
  for emission in emissions {
    names.push(emission.final_path.clone());
    if let Some(source) = &emission.source {
      names.push(source.clone());
    }
  }
  names.extend(removed.iter().cloned());
  for directory in directories {
    match directory {
      DirectoryEmission::Made(path) | DirectoryEmission::Removed(path) => names.push(path.clone()),
    }
  }
  names.sort_unstable();
  names.dedup();
  names
}

fn seal(entities: Vec<Entity>, directories: Vec<DirectoryEmission>) -> OpsDoc {
  let (mut emissions, removed) = classify(entities);
  let names = document_names(&emissions, &removed, &directories);

  let mut doc = OpsDoc::new();
  for name in &names {
    doc.paths.intern(name);
  }
  let index_of = |name: &str| -> u16 {
    names
      .binary_search_by(|candidate| candidate.as_str().cmp(name))
      .map_or(u16::MAX, |position| {
        u16::try_from(position).unwrap_or(u16::MAX)
      })
  };

  // The emissions in sorted final-path order: this is the post-state region layout.
  emissions.sort_by(|a, b| a.final_path.cmp(&b.final_path));
  let mut region_offset = 0u64;
  for emission in emissions {
    let destination = index_of(&emission.final_path);
    if let Some(source) = &emission.source {
      doc.ops.push(rename_op(destination, index_of(source)));
    } else if emission.created {
      doc.ops.push(namespace_op(OpKind::Create, destination));
    }
    push_content(&mut doc, emission.ops, destination, region_offset);
    region_offset = region_offset.saturating_add(emission.final_len);
  }
  for origin in removed {
    let index = index_of(&origin);
    doc.ops.push(namespace_op(OpKind::Unlink, index));
  }
  for directory in directories {
    match directory {
      DirectoryEmission::Made(path) => {
        doc.ops.push(namespace_op(OpKind::Mkdir, index_of(&path)));
      }
      DirectoryEmission::Removed(path) => {
        doc.ops.push(namespace_op(OpKind::Rmdir, index_of(&path)));
      }
    }
  }
  doc.canonicalize();
  doc
}

/// Pushes a file's content ops into the document, setting their path index and shifting the
/// source offset of content-adding ops into the file's post-state region.
fn push_content(doc: &mut OpsDoc, ops: Vec<Op>, index: u16, region_offset: u64) {
  for mut op in ops {
    op.path = index;
    if adds_content(op.kind) {
      op.src = op.src.saturating_add(region_offset);
    }
    doc.ops.push(op);
  }
}
