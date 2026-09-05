//! The landing manifest (§4.15 "Plan"): every diverged entry of the snapshot with its action,
//! its witnessed base and its overlay identity, in a canonical order with a canonical
//! encoding, so the BLAKE3 of the encoding names exactly what a grant allows to be written.
//! Planning walks the overlay's loaded nodes only (the diverged set), never the base.

use std::collections::BTreeMap;

use slates_vfs::base::{Diverged, Divergence};
use slates_vfs::dir::Child;
use slates_vfs::error::VfsError;
use slates_vfs::host::HostFs;
use slates_vfs::inode::{Fingerprint, Kind, Witness};
use slates_vfs::volume::{Store, Volume};

/// Whether `path` is `prefix` or lies beneath it.
fn under(path: &str, prefix: &str) -> bool {
  path == prefix || path.starts_with(&format!("{}/", prefix.trim_end_matches('/')))
}

/// Format: the manifest encoding's magic and version.
const MAGIC: &[u8; 4] = b"SLMF";
/// Format: the encoding version; bumped with any change to the layout.
const VERSION: u16 = 1;
/// Format: the action tags of the encoding, in the order the design lists the actions.
const TAG_CREATE: u8 = 0;
/// Format: see `TAG_CREATE`.
const TAG_REPLACE: u8 = 1;
/// Format: see `TAG_CREATE`.
const TAG_DELETE: u8 = 2;
/// Format: see `TAG_CREATE`.
const TAG_RENAME: u8 = 3;
/// Format: see `TAG_CREATE`.
const TAG_MKDIR: u8 = 4;
/// Format: see `TAG_CREATE`.
const TAG_RMDIR: u8 = 5;
/// Format: see `TAG_CREATE`.
const TAG_SYMLINK: u8 = 6;
/// Format: see `TAG_CREATE`.
const TAG_CLEAR: u8 = 7;
/// Shape: the landing order of §4.15 step 5, lowest first: directories top-down, then creates,
/// then replacements, then directory renames, then file deletes, then directory removals
/// bottom-up, so a tool reading the tree mid-landing never sees a referenced entry missing.
const CLASS_MKDIR: u8 = 0;
/// Shape: see `CLASS_MKDIR`.
const CLASS_CREATE: u8 = 1;
/// Shape: see `CLASS_MKDIR`.
const CLASS_REPLACE: u8 = 2;
/// Shape: see `CLASS_MKDIR`.
const CLASS_RENAME: u8 = 3;
/// Shape: see `CLASS_MKDIR`.
const CLASS_DELETE: u8 = 4;
/// Shape: see `CLASS_MKDIR`.
const CLASS_RMDIR: u8 = 5;

/// What the landing does to one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
  /// A file the base did not have.
  Create,
  /// A file the base had, replaced.
  Replace,
  /// A file the base had, removed.
  Delete,
  /// A directory renamed on the base, as one rename from its origin.
  Rename {
    /// The origin path on the base.
    from: Box<str>,
  },
  /// A directory created.
  Mkdir,
  /// A directory removed with everything beneath it (a whiteout over a base directory).
  Rmdir,
  /// A symlink created or replaced.
  Symlink {
    /// The target.
    target: Box<str>,
  },
  /// A base directory the overlay removed and recreated opaque: everything beneath it on the
  /// disk goes, the directory itself stays (T-1.12's "one recursive removal").
  Clear,
}

/// What the overlay holds for a file: the identity of its bytes, its size, mode and mtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlayIdentity {
  /// BLAKE3 of the bytes.
  pub hash: [u8; 32],
  /// Size in bytes.
  pub size: u64,
  /// Mode bits.
  pub mode: u32,
  /// Modification time, nanoseconds.
  pub mtime_ns: i64,
}

/// One entry of the manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandingEntry {
  /// The path in the target, absolute from the target root.
  pub path: Box<str>,
  /// The action.
  pub action: Action,
  /// The witnessed base the volume's copy was based on (a fingerprint for deletes and renames
  /// too), or none for a created entry.
  pub witnessed: Option<Witness>,
  /// The overlay's identity, for files.
  pub overlay: Option<OverlayIdentity>,
}

impl LandingEntry {
  /// The class the landing orders by (§4.15 step 5), lower first.
  pub fn class(&self) -> u8 {
    match self.action {
      Action::Mkdir | Action::Clear => CLASS_MKDIR,
      Action::Create | Action::Symlink { .. } => CLASS_CREATE,
      Action::Replace => CLASS_REPLACE,
      Action::Rename { .. } => CLASS_RENAME,
      Action::Delete => CLASS_DELETE,
      Action::Rmdir => CLASS_RMDIR,
    }
  }
}

/// Counts by action and by top-level directory, and the bytes to write.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
  /// Entries per action name.
  pub by_action: BTreeMap<Box<str>, usize>,
  /// Entries per top-level directory of the target.
  pub by_top_level: BTreeMap<Box<str>, usize>,
  /// Bytes the landing writes.
  pub bytes: u64,
  /// Entries the filter left out.
  pub filtered_out: usize,
}

/// The manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
  /// The entries, sorted by class then path (the writing order within a class is the path's).
  pub entries: Vec<LandingEntry>,
  /// The summary a human sees.
  pub summary: Summary,
  /// BLAKE3 of the canonical encoding.
  pub hash: [u8; 32],
}

/// The caller's filter: include and exclude prefixes; an entry is kept when it matches an
/// include (or there are none) and no exclude.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
  /// Path prefixes to keep.
  pub include: Vec<Box<str>>,
  /// Path prefixes to leave out.
  pub exclude: Vec<Box<str>>,
}

impl Filter {
  fn keeps(&self, path: &str) -> bool {
    (self.include.is_empty() || self.include.iter().any(|p| under(path, p)))
      && !self.exclude.iter().any(|p| under(path, p))
  }
}

impl Manifest {
  /// The canonical encoding: little-endian, length-prefixed strings, entries in order.
  pub fn encode(entries: &[LandingEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(
      &u32::try_from(entries.len())
        .unwrap_or(u32::MAX)
        .to_le_bytes(),
    );
    for e in entries {
      put_str(&mut out, &e.path);
      match &e.action {
        Action::Create => out.push(TAG_CREATE),
        Action::Replace => out.push(TAG_REPLACE),
        Action::Delete => out.push(TAG_DELETE),
        Action::Rename { from } => {
          out.push(TAG_RENAME);
          put_str(&mut out, from);
        }
        Action::Mkdir => out.push(TAG_MKDIR),
        Action::Rmdir => out.push(TAG_RMDIR),
        Action::Symlink { target } => {
          out.push(TAG_SYMLINK);
          put_str(&mut out, target);
        }
        Action::Clear => out.push(TAG_CLEAR),
      }
      match &e.witnessed {
        Some(w) => {
          out.push(1);
          put_fingerprint(&mut out, &w.fingerprint);
          out.extend_from_slice(&w.identity);
        }
        None => out.push(0),
      }
      match &e.overlay {
        Some(o) => {
          out.push(1);
          out.extend_from_slice(&o.hash);
          out.extend_from_slice(&o.size.to_le_bytes());
          out.extend_from_slice(&o.mode.to_le_bytes());
          out.extend_from_slice(&o.mtime_ns.to_le_bytes());
        }
        None => out.push(0),
      }
    }
    out
  }

  /// Builds a manifest from entries: sorts them into landing order, summarizes, hashes.
  pub fn from_entries(mut entries: Vec<LandingEntry>, filtered_out: usize) -> Self {
    entries.sort_by(|a, b| a.class().cmp(&b.class()).then_with(|| a.path.cmp(&b.path)));
    // Removals go bottom-up: deeper paths first within their class.
    let (mut keep, mut removals): (Vec<LandingEntry>, Vec<LandingEntry>) = entries
      .into_iter()
      .partition(|e| !matches!(e.action, Action::Delete | Action::Rmdir));
    removals.sort_by(|a, b| {
      a.class()
        .cmp(&b.class())
        .then_with(|| {
          b.path
            .matches('/')
            .count()
            .cmp(&a.path.matches('/').count())
        })
        .then_with(|| a.path.cmp(&b.path))
    });
    keep.append(&mut removals);
    let entries = keep;
    let mut summary = Summary {
      filtered_out,
      ..Summary::default()
    };
    for e in &entries {
      let action = match &e.action {
        Action::Create => "create",
        Action::Replace => "replace",
        Action::Delete => "delete",
        Action::Rename { .. } => "rename",
        Action::Mkdir => "mkdir",
        Action::Rmdir => "rmdir",
        Action::Symlink { .. } => "symlink",
        Action::Clear => "clear",
      };
      *summary.by_action.entry(action.into()).or_insert(0) += 1;
      let top = e
        .path
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or("");
      *summary.by_top_level.entry(top.into()).or_insert(0) += 1;
      summary.bytes += e.overlay.map_or(0, |o| o.size);
    }
    let hash = *blake3::hash(&Self::encode(&entries)).as_bytes();
    Self {
      entries,
      summary,
      hash,
    }
  }

  /// The hash as hex.
  pub fn hash_hex(&self) -> String {
    self.hash.iter().map(|b| format!("{b:02x}")).collect()
  }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
  out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(u16::MAX).to_le_bytes());
  out.extend_from_slice(s.as_bytes());
}

fn put_fingerprint(out: &mut Vec<u8>, fp: &Fingerprint) {
  out.extend_from_slice(&fp.dev.to_le_bytes());
  out.extend_from_slice(&fp.ino.to_le_bytes());
  out.extend_from_slice(&fp.size.to_le_bytes());
  out.extend_from_slice(&fp.mtime_ns.to_le_bytes());
  out.extend_from_slice(&fp.ctime_ns.to_le_bytes());
  out.extend_from_slice(&fp.mode.to_le_bytes());
}

/// Plans the manifest of a volume's head: the diverged entries with their witnesses and the
/// overlay's identities (the bytes are hashed here, once, proportional to the delta).
pub fn plan(
  vol: &mut Volume,
  store: &mut Store,
  host: &mut dyn HostFs,
  filter: &Filter,
) -> Result<Manifest, VfsError> {
  let diverged = vol.diverged(store);
  let mut entries = Vec::new();
  let mut filtered_out = 0usize;
  // A renamed base directory implies the whiteout at its origin; that whiteout is not a
  // separate removal.
  let origins: Vec<Box<str>> = diverged
    .iter()
    .filter(|d| d.kind == Divergence::Redirect)
    .filter_map(|d| origin_of(vol, store, &d.path))
    .collect();
  for d in &diverged {
    if !filter.keeps(&d.path) {
      filtered_out += 1;
      continue;
    }
    if d.kind == Divergence::Whiteout && origins.iter().any(|o| o.as_ref() == d.path.as_str()) {
      continue;
    }
    if let Some(entry) = entry_for(vol, store, host, d)? {
      entries.push(entry);
    }
  }
  Ok(Manifest::from_entries(entries, filtered_out))
}

fn origin_of(vol: &Volume, store: &Store, path: &str) -> Option<Box<str>> {
  match vol.resolve(store, path).ok()?.child {
    Child::Dir(h) => store.dirs.get(h).ok()?.origin.clone(),
    _ => None,
  }
}

/// The manifest entry for one diverged path.
fn entry_for(
  vol: &mut Volume,
  store: &mut Store,
  host: &mut dyn HostFs,
  d: &Diverged,
) -> Result<Option<LandingEntry>, VfsError> {
  let path: Box<str> = d.path.as_str().into();
  match d.kind {
    Divergence::Whiteout => {
      let (dir, name) = split(&d.path);
      let witnessed = vol.whiteout_witness(store, dir, name).map(|fp| Witness {
        fingerprint: fp,
        identity: [0; 32],
        witnessed_at: 0,
        racy: false,
      });
      let action = if witnessed.is_some_and(|w| kind_is_dir(w.fingerprint.mode)) {
        Action::Rmdir
      } else {
        Action::Delete
      };
      Ok(Some(LandingEntry {
        path,
        action,
        witnessed,
        overlay: None,
      }))
    }
    Divergence::Redirect => {
      let from = origin_of(vol, store, &d.path).ok_or(VfsError::Invalid)?;
      let witnessed = vol.redirect_witness(store, &d.path).map(|fp| Witness {
        fingerprint: fp,
        identity: [0; 32],
        witnessed_at: 0,
        racy: false,
      });
      Ok(Some(LandingEntry {
        path,
        action: Action::Rename { from },
        witnessed,
        overlay: None,
      }))
    }
    Divergence::Created | Divergence::Witnessed => {
      let located = vol.resolve(store, &d.path)?;
      match located.child {
        Child::Dir(_) => {
          let attrs = vol.stat(store, located.inode)?;
          // An opaque directory over a whiteout of a base directory clears the disk's copy.
          let (dir, name) = split(&d.path);
          let cleared = vol.whiteout_witness(store, dir, name).map(|fp| Witness {
            fingerprint: fp,
            identity: [0; 32],
            witnessed_at: 0,
            racy: false,
          });
          Ok(Some(LandingEntry {
            path,
            action: if cleared.is_some() {
              Action::Clear
            } else {
              Action::Mkdir
            },
            witnessed: cleared,
            overlay: Some(OverlayIdentity {
              hash: [0; 32],
              size: 0,
              mode: attrs.mode,
              mtime_ns: attrs.mtime,
            }),
          }))
        }
        Child::Symlink(no) => Ok(Some(LandingEntry {
          path,
          action: Action::Symlink {
            target: vol.readlink(store, no)?,
          },
          witnessed: None,
          overlay: None,
        })),
        Child::File(no) => {
          let attrs = vol.stat(store, no)?;
          let mut bytes =
            vec![0u8; usize::try_from(attrs.size).map_err(|_| VfsError::FileTooLarge)?];
          let n = vol.with_host(host).read(store, no, 0, &mut bytes)?;
          bytes.truncate(n);
          let witnessed = vol.base_plane().and_then(|b| b.witness(no));
          Ok(Some(LandingEntry {
            path,
            action: if witnessed.is_some() {
              Action::Replace
            } else {
              Action::Create
            },
            witnessed,
            overlay: Some(OverlayIdentity {
              hash: *blake3::hash(&bytes).as_bytes(),
              size: attrs.size,
              mode: attrs.mode,
              mtime_ns: attrs.mtime,
            }),
          }))
        }
        Child::Whiteout => Ok(None),
      }
    }
  }
}

fn split(path: &str) -> (&str, &str) {
  match path.rfind('/') {
    Some(0) => ("/", &path[1..]),
    Some(i) => (&path[..i], &path[i + 1..]),
    None => ("/", path),
  }
}

/// Format: the directory bit of a POSIX mode.
const S_IFMT: u32 = 0o170_000;
/// Format: the directory type in a POSIX mode.
const S_IFDIR: u32 = 0o040_000;

fn kind_is_dir(mode: u32) -> bool {
  mode & S_IFMT == S_IFDIR
}

/// The kind a fingerprint's mode names, for the verdict.
pub fn kind_of_mode(mode: u32) -> Kind {
  if kind_is_dir(mode) {
    Kind::Dir
  } else {
    Kind::File
  }
}
