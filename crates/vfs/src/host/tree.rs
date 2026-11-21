//! A base whose tree is another structure the shard holds, read where it lies (A-774 part 2; §4.5 base plane, §4.16
//! "Role", D-5): a mounted work's base is its green's pinned version, served to the overlay through this seam so a
//! mount pays for what it changes, never for a copy of the green.
//!
//! **Why a lend.** The base plane holds its host for the volume's life and lends it to each request (the slot's host).
//! A green is not the host's to hold: it lives in the shard's merge state, beside the volume, and no shared-ownership
//! pointer may join them (R2). So the host here, [`TreeHost`], holds only what a host must keep between calls, its
//! handle tables, and answers nothing alone ([`HostFs`] on it refuses `BaseUnavailable`); the shard lends it together
//! with the tree for one request ([`TreeHost::lend`]), as a [`TreeLens`] over a [`TreeSource`], and the base plane reads
//! through that.
//!
//! **What a source is.** A read-only tree that does not change while a volume is based on it (a green's version stays
//! readable while a mount pins it): its directories' entries, its files' lengths and bytes, its symlinks' targets. A
//! file's fingerprint is fixed for the tree, so the base plane never sees drift: its device is the tree's identity, its
//! inode number a digest of its path, its size its length, its times one instant, its owner the volume's own.
//!
//! **Bounds.** The handle tables hold what the base plane has open: a listed directory's handle while its listing is
//! cached, a file's while its witness reads it. Each handle is a path; nothing of a file's bytes is held here.

use std::collections::BTreeMap;

use super::{
  BaseEntry, Hint, HostDir, HostError, HostFacts, HostFile, HostFs, HostKind, OsCode, WatchState,
};
use crate::inode::Fingerprint;

/// Format: `ENOTSUP`'s errno on Linux, the code a tree host lent without its tree refuses with.
#[cfg(target_os = "linux")]
const ENOTSUP: i32 = 95;
/// Format: `ENOTSUP`'s errno on macOS and the BSDs, the code a tree host lent without its tree refuses with.
#[cfg(not(target_os = "linux"))]
const ENOTSUP: i32 = 45;
/// Format: the `st_mode` type bits of a regular file, a directory and a symlink (POSIX `S_IFREG`, `S_IFDIR`, `S_IFLNK`).
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;
/// Format: the permission bits a tree's files and directories show (a green records no mode for most paths).
const FILE_PERMISSIONS: u32 = 0o644;
const DIR_PERMISSIONS: u32 = 0o755;
const LINK_PERMISSIONS: u32 = 0o777;
/// Format: the one instant every entry of a tree shows: a fixed time, never zero (hosts report zero for "no birth time
/// recorded", so a real stamp is never it); its value is the tree's, never compared with a clock's.
const TREE_INSTANT_NS: i64 = 1_000_000_000;
/// Format: the root directory's handle.
const ROOT: HostDir = HostDir(1);

/// The kind and length of one entry of a source's directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
  /// The entry's name in its directory.
  pub name: Box<str>,
  /// What it is.
  pub kind: HostKind,
  /// A file's length; zero for anything else.
  pub len: u64,
}

/// A read-only tree a [`TreeHost`] serves (the module doc). Paths are `/`-separated, relative to the root, `""` the root.
pub trait TreeSource {
  /// The entries of the directory at `dir`; `NotFound` or `NotDirectory` as the tree has it.
  fn entries(&self, dir: &str) -> Result<Vec<TreeEntry>, HostError>;
  /// The kind and length of the entry at `path`, or `None` when the tree has none.
  fn entry(&self, path: &str) -> Option<(HostKind, u64)>;
  /// Reads the file at `path` from `offset` into `into`; the bytes read (fewer at its end).
  fn read_at(&self, path: &str, offset: u64, into: &mut [u8]) -> Result<usize, HostError>;
  /// The target of the symlink at `path`.
  fn link_target(&self, path: &str) -> Result<Box<str>, HostError>;
}

/// The host a volume over a [`TreeSource`] keeps (the module doc): its handle tables and its fingerprints' constants.
#[derive(Debug)]
pub struct TreeHost {
  /// The tree's identity, every fingerprint's device.
  identity: u64,
  /// The owner every entry shows: the volume's.
  owner: (u32, u32),
  /// Each open directory's path, by handle.
  dirs: BTreeMap<u64, String>,
  /// Each open file's path, by handle.
  files: BTreeMap<u64, String>,
  /// The next handle given out.
  next: u64,
}

impl TreeHost {
  /// A host of the tree `identity` names, its entries owned by `uid`:`gid`; its root open ([`TreeHost::root`]).
  pub fn new(identity: u64, (uid, gid): (u32, u32)) -> TreeHost {
    let mut dirs = BTreeMap::new();
    dirs.insert(ROOT.0, String::new());
    TreeHost {
      identity,
      owner: (uid, gid),
      dirs,
      files: BTreeMap::new(),
      next: ROOT.0.saturating_add(1),
    }
  }

  /// The root directory's handle.
  pub fn root(&self) -> HostDir {
    ROOT
  }

  /// The facts a tree's base plane is created with: its times never move, so any granularity is exact; one nanosecond.
  pub fn tree_facts(&self) -> HostFacts {
    HostFacts {
      timestamp_granularity_ns: 1,
    }
  }

  /// The host lent with its tree for one request.
  pub fn lend<'a>(&'a mut self, source: &'a dyn TreeSource) -> TreeLens<'a> {
    TreeLens { host: self, source }
  }

  /// The handles open now, directories and files: the tables' size.
  pub fn open_handles(&self) -> usize {
    self.dirs.len().saturating_add(self.files.len())
  }

  fn give(&mut self) -> u64 {
    let handle = self.next;
    self.next = self.next.saturating_add(1);
    handle
  }

  fn dir_path(&self, dir: HostDir) -> Result<&str, HostError> {
    self
      .dirs
      .get(&dir.0)
      .map(String::as_str)
      .ok_or(HostError::StaleHandle)
  }

  fn fingerprint(&self, path: &str, kind: HostKind, len: u64) -> Fingerprint {
    let type_bits = match kind {
      HostKind::Dir => S_IFDIR | DIR_PERMISSIONS,
      HostKind::Symlink => S_IFLNK | LINK_PERMISSIONS,
      HostKind::File | HostKind::Other => S_IFREG | FILE_PERMISSIONS,
    };
    Fingerprint {
      dev: self.identity,
      ino: path_number(path),
      size: len,
      mtime_ns: TREE_INSTANT_NS,
      ctime_ns: TREE_INSTANT_NS,
      mode: type_bits,
      uid: self.owner.0,
      gid: self.owner.1,
      birth_ns: TREE_INSTANT_NS,
      windows: 0,
    }
  }
}

/// A path's inode number: FNV-1a over its bytes, never zero.
fn path_number(path: &str) -> u64 {
  /// Format: FNV-1a's 64-bit offset basis and prime.
  const BASIS: u64 = 0xcbf2_9ce4_8422_2325;
  const PRIME: u64 = 0x0000_0100_0000_01b3;
  let hash = path.bytes().fold(BASIS, |hash, byte| {
    (hash ^ u64::from(byte)).wrapping_mul(PRIME)
  });
  hash.max(1)
}

/// The child `name` of the directory at `dir`.
fn child(dir: &str, name: &str) -> String {
  if dir.is_empty() {
    name.to_owned()
  } else {
    format!("{dir}/{name}")
  }
}

/// A tree host lent without its tree answers nothing: a request reached it through a seam that does not lend the tree.
fn unlent() -> HostError {
  HostError::Unavailable(OsCode::Errno(ENOTSUP))
}

impl HostFs for TreeHost {
  fn facts(&mut self, _dir: HostDir) -> Result<HostFacts, HostError> {
    Ok(self.tree_facts())
  }

  /// A directory's fingerprint is fixed by its path and the tree's identity, so it is answered without the tree: what a
  /// publication's image of the base plane records for each listed directory (`BasePlane::image`).
  fn fingerprint_dir(&mut self, dir: HostDir) -> Result<Fingerprint, HostError> {
    let path = self.dir_path(dir)?;
    Ok(self.fingerprint(path, HostKind::Dir, 0))
  }

  fn list(&mut self, _dir: HostDir) -> Result<Vec<BaseEntry>, HostError> {
    Err(unlent())
  }

  fn open_dir(&mut self, _parent: HostDir, _name: &str) -> Result<HostDir, HostError> {
    Err(unlent())
  }

  fn open_file(&mut self, _dir: HostDir, _name: &str) -> Result<HostFile, HostError> {
    Err(unlent())
  }

  fn fstat(&mut self, _file: HostFile) -> Result<Fingerprint, HostError> {
    Err(unlent())
  }

  fn read_at(&mut self, _file: HostFile, _off: u64, _buf: &mut [u8]) -> Result<usize, HostError> {
    Err(unlent())
  }

  fn read_link(&mut self, _dir: HostDir, _name: &str) -> Result<Box<str>, HostError> {
    Err(unlent())
  }

  fn close_file(&mut self, file: HostFile) {
    self.files.remove(&file.0);
  }

  fn close_dir(&mut self, dir: HostDir) {
    if dir != ROOT {
      self.dirs.remove(&dir.0);
    }
  }

  fn watch(&mut self, _dir: HostDir) -> WatchState {
    WatchState::Unavailable
  }

  fn hints(&mut self) -> Vec<Hint> {
    Vec::new()
  }

  fn now_ns(&mut self) -> i64 {
    TREE_INSTANT_NS
  }
}

/// A [`TreeHost`] lent with its tree for one request: the base plane's host for that request.
pub struct TreeLens<'a> {
  host: &'a mut TreeHost,
  source: &'a dyn TreeSource,
}

impl HostFs for TreeLens<'_> {
  fn facts(&mut self, _dir: HostDir) -> Result<HostFacts, HostError> {
    Ok(self.host.tree_facts())
  }

  fn fingerprint_dir(&mut self, dir: HostDir) -> Result<Fingerprint, HostError> {
    let path = self.host.dir_path(dir)?;
    Ok(self.host.fingerprint(path, HostKind::Dir, 0))
  }

  fn list(&mut self, dir: HostDir) -> Result<Vec<BaseEntry>, HostError> {
    let path = self.host.dir_path(dir)?.to_owned();
    let entries = self.source.entries(&path)?;
    Ok(
      entries
        .into_iter()
        .map(|entry| {
          let fingerprint =
            self
              .host
              .fingerprint(&child(&path, &entry.name), entry.kind, entry.len);
          BaseEntry {
            name: entry.name,
            kind: entry.kind,
            fingerprint,
          }
        })
        .collect(),
    )
  }

  fn open_dir(&mut self, parent: HostDir, name: &str) -> Result<HostDir, HostError> {
    let path = child(self.host.dir_path(parent)?, name);
    match self.source.entry(&path) {
      Some((HostKind::Dir, _)) => {}
      Some(_) => return Err(HostError::NotDirectory),
      None => return Err(HostError::NotFound),
    }
    let handle = self.host.give();
    self.host.dirs.insert(handle, path);
    Ok(HostDir(handle))
  }

  fn open_file(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    let path = child(self.host.dir_path(dir)?, name);
    match self.source.entry(&path) {
      Some((HostKind::File, _)) => {}
      Some(_) => return Err(HostError::NotFile),
      None => return Err(HostError::NotFound),
    }
    let handle = self.host.give();
    self.host.files.insert(handle, path);
    Ok(HostFile(handle))
  }

  fn fstat(&mut self, file: HostFile) -> Result<Fingerprint, HostError> {
    let path = self.host.files.get(&file.0).ok_or(HostError::StaleHandle)?;
    let (kind, len) = self.source.entry(path).ok_or(HostError::NotFound)?;
    Ok(self.host.fingerprint(path, kind, len))
  }

  fn read_at(&mut self, file: HostFile, off: u64, buf: &mut [u8]) -> Result<usize, HostError> {
    let path = self.host.files.get(&file.0).ok_or(HostError::StaleHandle)?;
    self.source.read_at(path, off, buf)
  }

  fn read_link(&mut self, dir: HostDir, name: &str) -> Result<Box<str>, HostError> {
    let path = child(self.host.dir_path(dir)?, name);
    self.source.link_target(&path)
  }

  fn close_file(&mut self, file: HostFile) {
    self.host.close_file(file);
  }

  fn close_dir(&mut self, dir: HostDir) {
    self.host.close_dir(dir);
  }

  fn watch(&mut self, _dir: HostDir) -> WatchState {
    WatchState::Unavailable
  }

  fn hints(&mut self) -> Vec<Hint> {
    Vec::new()
  }

  fn now_ns(&mut self) -> i64 {
    TREE_INSTANT_NS
  }
}
