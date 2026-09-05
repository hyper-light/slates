//! The Windows host, path-relative through the standard library: a directory handle form with
//! `FILE_FLAG_OPEN_REPARSE_POINT` opens relative to the base arrives with the Windows bridge
//! (Phase 4, GAPS); until then every access re-resolves the base path, symlinks and reparse
//! points are refused by a metadata check before the open, and there is no watcher
//! (`Unavailable`: fingerprints alone, the failure matrix's Masked cell).

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use slates_vfs::host::{
  BaseEntry, Hint, HostDir, HostError, HostFacts, HostFile, HostFs, HostKind, WatchState,
};
use slates_vfs::inode::Fingerprint;

use crate::{FsKind, granularity_for};

/// Format: Windows file times count 100 ns intervals since 1601.
const HUNDRED_NS: i64 = 100;

/// The Windows host.
#[derive(Debug)]
pub struct OsHost {
  dirs: BTreeMap<u64, PathBuf>,
  files: BTreeMap<u64, std::fs::File>,
  next: u64,
}

fn refusal(e: &std::io::Error) -> HostError {
  match e.kind() {
    std::io::ErrorKind::NotFound => HostError::NotFound,
    std::io::ErrorKind::NotADirectory => HostError::NotDirectory,
    std::io::ErrorKind::IsADirectory => HostError::NotFile,
    _ => HostError::Unavailable(e.raw_os_error().unwrap_or(0)),
  }
}

fn fingerprint(meta: &std::fs::Metadata) -> Fingerprint {
  Fingerprint {
    dev: u64::from(meta.volume_serial_number().unwrap_or(0)),
    ino: meta.file_index().unwrap_or(0),
    size: meta.file_size(),
    mtime_ns: i64::try_from(meta.last_write_time())
      .unwrap_or(i64::MAX)
      .saturating_mul(HUNDRED_NS),
    ctime_ns: i64::try_from(meta.change_time().unwrap_or(0))
      .unwrap_or(i64::MAX)
      .saturating_mul(HUNDRED_NS),
    mode: meta.file_attributes(),
  }
}

fn kind_of(meta: &std::fs::Metadata) -> HostKind {
  let ft = meta.file_type();
  if ft.is_symlink() {
    HostKind::Symlink
  } else if ft.is_dir() {
    HostKind::Dir
  } else if ft.is_file() {
    HostKind::File
  } else {
    HostKind::Other
  }
}

impl OsHost {
  /// Opens a base directory: the path is recorded after a check that it is a directory and
  /// not a reparse point.
  pub fn open_root(path: &Path) -> Result<(Self, HostDir), HostError> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| refusal(&e))?;
    if !meta.is_dir() {
      return Err(HostError::NotDirectory);
    }
    let mut host = Self {
      dirs: BTreeMap::new(),
      files: BTreeMap::new(),
      next: 1,
    };
    let h = host.next;
    host.next += 1;
    host.dirs.insert(h, path.to_path_buf());
    Ok((host, HostDir(h)))
  }

  fn dir(&self, dir: HostDir) -> Result<&PathBuf, HostError> {
    self.dirs.get(&dir.0).ok_or(HostError::StaleHandle)
  }

  /// Open handles, for leak checks.
  pub fn open_handles(&self) -> usize {
    self.dirs.len() + self.files.len()
  }
}

impl HostFs for OsHost {
  fn facts(&mut self, dir: HostDir) -> Result<HostFacts, HostError> {
    self.dir(dir)?;
    // The coarsest class of the table until the volume is queried through the bridge's handle
    // (Phase 4): the racy rule then re-hashes more often, never less.
    Ok(HostFacts {
      timestamp_granularity_ns: granularity_for(FsKind::Unknown),
    })
  }

  fn fingerprint_dir(&mut self, dir: HostDir) -> Result<Fingerprint, HostError> {
    let path = self.dir(dir)?;
    std::fs::symlink_metadata(path)
      .map(|m| fingerprint(&m))
      .map_err(|e| refusal(&e))
  }

  fn list(&mut self, dir: HostDir) -> Result<Vec<BaseEntry>, HostError> {
    let path = self.dir(dir)?;
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path).map_err(|e| refusal(&e))? {
      let entry = entry.map_err(|e| refusal(&e))?;
      let meta = match std::fs::symlink_metadata(entry.path()) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
        Err(e) => return Err(refusal(&e)),
      };
      let name = entry.file_name().to_string_lossy().into_owned();
      out.push(BaseEntry {
        name: name.into(),
        kind: kind_of(&meta),
        fingerprint: fingerprint(&meta),
      });
    }
    Ok(out)
  }

  fn open_dir(&mut self, parent: HostDir, name: &str) -> Result<HostDir, HostError> {
    let path = self.dir(parent)?.join(name);
    let meta = std::fs::symlink_metadata(&path).map_err(|e| refusal(&e))?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
      return Err(HostError::NotDirectory);
    }
    let h = self.next;
    self.next += 1;
    self.dirs.insert(h, path);
    Ok(HostDir(h))
  }

  fn open_file(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    let path = self.dir(dir)?.join(name);
    let meta = std::fs::symlink_metadata(&path).map_err(|e| refusal(&e))?;
    if !meta.is_file() {
      return Err(HostError::NotFile);
    }
    let file = std::fs::File::open(&path).map_err(|e| refusal(&e))?;
    let h = self.next;
    self.next += 1;
    self.files.insert(h, file);
    Ok(HostFile(h))
  }

  fn fstat(&mut self, file: HostFile) -> Result<Fingerprint, HostError> {
    let f = self.files.get(&file.0).ok_or(HostError::StaleHandle)?;
    f.metadata()
      .map(|m| fingerprint(&m))
      .map_err(|e| refusal(&e))
  }

  fn read_at(&mut self, file: HostFile, off: u64, buf: &mut [u8]) -> Result<usize, HostError> {
    let f = self.files.get_mut(&file.0).ok_or(HostError::StaleHandle)?;
    f.seek(SeekFrom::Start(off)).map_err(|e| refusal(&e))?;
    f.read(buf).map_err(|e| refusal(&e))
  }

  fn read_link(&mut self, dir: HostDir, name: &str) -> Result<Box<str>, HostError> {
    let path = self.dir(dir)?.join(name);
    std::fs::read_link(&path)
      .map(|p| p.to_string_lossy().into_owned().into())
      .map_err(|e| refusal(&e))
  }

  fn close_file(&mut self, file: HostFile) {
    self.files.remove(&file.0);
  }

  fn close_dir(&mut self, dir: HostDir) {
    self.dirs.remove(&dir.0);
  }

  fn watch(&mut self, _dir: HostDir) -> WatchState {
    WatchState::Unavailable
  }

  fn hints(&mut self) -> Vec<Hint> {
    Vec::new()
  }
}
