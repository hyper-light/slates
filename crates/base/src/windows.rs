//! The Windows host, path-relative through the standard library: a directory handle form with
//! `FILE_FLAG_OPEN_REPARSE_POINT` opens relative to the base arrives with the Windows bridge
//! (Phase 4, GAPS); until then every access re-resolves the base path, symlinks and reparse
//! points are refused by a metadata check before the open, and there is no watcher
//! (`Unavailable`: fingerprints alone, the failure matrix's Masked cell).

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
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

/// Format: `FILE_FLAG_BACKUP_SEMANTICS`, which lets `CreateFileW` open a directory handle.
const BACKUP_SEMANTICS: u32 = 0x0200_0000;
/// Format: `FILE_FLAG_OPEN_REPARSE_POINT`, which opens a reparse point itself, never its target
/// (R1/D-3: a symlink is never followed).
const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
/// Format: the `FileBasicInfo` class of `GetFileInformationByHandleEx`.
const FILE_BASIC_INFO: i32 = 0;

/// The fingerprint of an open handle: the volume serial as the device, the file index as the
/// inode, the size, the last write and change times, and the attributes as the mode; the
/// stable Win32 calls, since the standard library's `volume_serial_number`, `file_index` and
/// `change_time` are unstable (`windows_by_handle`, `windows_change_time`).
fn fingerprint_of_handle(
  handle: std::os::windows::io::RawHandle,
) -> Result<Fingerprint, HostError> {
  use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, GetFileInformationByHandle,
    GetFileInformationByHandleEx,
  };
  // SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is a valid value for the call to fill.
  let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
  // SAFETY: `handle` is an open handle this host holds for the call's duration; `info` is a
  // writable record of the type the call fills.
  let ok = unsafe { GetFileInformationByHandle(handle.cast(), &mut info) };
  if ok == 0 {
    return Err(HostError::Unavailable(
      std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
    ));
  }
  // SAFETY: an all-zero FILE_BASIC_INFO is a valid value for the call to fill.
  let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
  let basic_len = u32::try_from(size_of::<FILE_BASIC_INFO>()).unwrap_or(0);
  // SAFETY: `handle` is open for the call's duration; `basic` is a writable record of the
  // class named, of the length passed.
  let ok = unsafe {
    GetFileInformationByHandleEx(
      handle.cast(),
      FILE_BASIC_INFO,
      (&raw mut basic).cast(),
      basic_len,
    )
  };
  let change_time = if ok == 0 {
    filetime_ns(
      info.ftLastWriteTime.dwHighDateTime,
      info.ftLastWriteTime.dwLowDateTime,
    )
  } else {
    basic.ChangeTime.saturating_mul(HUNDRED_NS)
  };
  Ok(Fingerprint {
    dev: u64::from(info.dwVolumeSerialNumber),
    ino: (u64::from(info.nFileIndexHigh) << u32::BITS) | u64::from(info.nFileIndexLow),
    size: (u64::from(info.nFileSizeHigh) << u32::BITS) | u64::from(info.nFileSizeLow),
    mtime_ns: filetime_ns(
      info.ftLastWriteTime.dwHighDateTime,
      info.ftLastWriteTime.dwLowDateTime,
    ),
    ctime_ns: change_time,
    mode: info.dwFileAttributes,
  })
}

/// A `FILETIME` as nanoseconds.
fn filetime_ns(high: u32, low: u32) -> i64 {
  let ticks = (u64::from(high) << u32::BITS) | u64::from(low);
  i64::try_from(ticks)
    .unwrap_or(i64::MAX)
    .saturating_mul(HUNDRED_NS)
}

/// The fingerprint of the entry at `path` itself (a reparse point is not followed), through a
/// handle opened for attributes only.
fn fingerprint_of_path(path: &Path) -> Result<Fingerprint, HostError> {
  use std::os::windows::fs::OpenOptionsExt;
  use std::os::windows::io::AsRawHandle;
  // structural: allow — a read-only open for the entry's attributes; nothing is written (R1).
  let file = std::fs::OpenOptions::new()
    .read(true)
    .custom_flags(BACKUP_SEMANTICS | OPEN_REPARSE_POINT)
    .open(path)
    .map_err(|e| refusal(&e))?;
  fingerprint_of_handle(file.as_raw_handle())
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
    fingerprint_of_path(path)
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
      let fingerprint = match fingerprint_of_path(&entry.path()) {
        Ok(fp) => fp,
        Err(HostError::NotFound) => continue,
        Err(e) => return Err(e),
      };
      out.push(BaseEntry {
        name: name.into(),
        kind: kind_of(&meta),
        fingerprint,
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
    use std::os::windows::io::AsRawHandle;
    let f = self.files.get(&file.0).ok_or(HostError::StaleHandle)?;
    fingerprint_of_handle(f.as_raw_handle())
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
