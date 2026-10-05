//! The Windows host: every access is relative to a directory handle this host retains (§4.5, §4.15, R1;
//! AUD-29-62), the Windows form of the Unix host's descriptors. The base root is the one path it resolves;
//! every later lookup is `NtCreateFile` with the parent's retained handle as `RootDirectory` and a single
//! entry name (`one_entry`: never `..`, a separator or a stream), with `FILE_OPEN_REPARSE_POINT` so a final
//! symbolic link, junction or mount point is opened as itself and never traversed. The reparse check is
//! then made on the object actually opened, never on a path looked at beforehand: a name surrogate (a link,
//! a junction, a mount point) is refused as the wrong kind; a reparse point that is the entry's own data
//! (deduplication, a cloud placeholder, WOF compression) is reopened through its filter and kept only if it
//! is the same object (volume serial and file index) the contained open found. A listing enumerates the
//! retained handle (`FileFullDirectoryInfo`) and fingerprints each entry through its own contained open, as
//! the Unix host's `getdents` and `statat` do. So a directory renamed or replaced after it was opened, an
//! intermediate swapped for a junction, or a final component swapped for a link never leads outside the
//! base: a retained handle keeps naming the directory it opened, and every new open starts from one.
//!
//! What this replaced (2026-10-01): handles were path strings; each access re-resolved the base path through
//! the standard library, and a file was checked by `symlink_metadata` and then opened by `File::open` — the
//! check and the open could name different objects, and any intermediate component could become a junction
//! between them. There is no watcher yet (`Unavailable`: fingerprints alone, the failure matrix's Masked
//! cell).

use std::collections::BTreeMap;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;

use slates_vfs::host::{
  BaseEntry, Hint, HostDir, HostError, HostFacts, HostFile, HostFs, HostKind, WatchState,
};
use slates_vfs::inode::Fingerprint;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem as nt;
use windows_sys::Win32::Foundation::{
  ERROR_INVALID_DATA, ERROR_NO_MORE_FILES, ERROR_NO_UNICODE_TRANSLATION, HANDLE, NTSTATUS,
  OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, STATUS_DELETE_PENDING, STATUS_FILE_IS_A_DIRECTORY,
  STATUS_NO_SUCH_FILE, STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_INVALID,
  STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
  FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
  FILE_GENERIC_READ, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
  FILE_SHARE_WRITE, FILE_TRAVERSE, FileAttributeTagInfo, FileFullDirectoryInfo,
  FileFullDirectoryRestartInfo, GetFileInformationByHandleEx, MAXIMUM_REPARSE_DATA_BUFFER_SIZE,
  SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK};

use crate::windows_records::{RecordRefusal, directory_names, is_name_surrogate, link_target};
use crate::{FsKind, WINDOWS_NAME_BREAKS, granularity_for, one_entry};

/// Format: Windows file times count 100 ns intervals since 1601.
const HUNDRED_NS: i64 = 100;
/// Format: the FILETIME epoch (1601-01-01) precedes the Unix epoch (1970-01-01) by 11,644,473,600
/// seconds, so a Unix time enters the FILETIME domain the fingerprints use by adding it.
const FILETIME_EPOCH_OFFSET_S: i64 = 11_644_473_600;
/// Format: nanoseconds per second.
const NS_PER_S: i64 = 1_000_000_000;

/// Format: `FILE_FLAG_BACKUP_SEMANTICS`, which lets `CreateFileW` open a directory handle.
const BACKUP_SEMANTICS: u32 = 0x0200_0000;
/// Format: `FILE_FLAG_OPEN_REPARSE_POINT`, which opens a reparse point itself, never its target
/// (R1/D-3: a symlink is never followed).
const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
/// Format: `FSCTL_GET_REPARSE_POINT` (winioctl.h: device 9, function 42, buffered, any access), which
/// returns a reparse point's `REPARSE_DATA_BUFFER`.
const FSCTL_GET_REPARSE_POINT: u32 = 0x0009_00A8;
/// Format: every share mode, so the base holding a handle never blocks a user's read, write, rename or
/// delete of the entry — the Unix descriptor's semantics.
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
/// Format: a retained directory's access — enumerate it, open entries beneath it, read its attributes,
/// wait on it synchronously. No write right of any kind (R1).
const DIRECTORY_ACCESS: u32 =
  FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
/// Format: a listed entry's or a link's access — its attributes only; no data is read through it.
const ENTRY_ACCESS: u32 = FILE_READ_ATTRIBUTES | SYNCHRONIZE;
/// Derived: one directory query's buffer, 64 KiB — the largest output a directory query may ask of an SMB
/// 2.0.2 server (MS-SMB2 §3.3.5.4: `MaxTransactSize` is 65,536 for that dialect; a larger request is
/// refused), so a base on a network share is never asked for more than the oldest dialect serves. It holds
/// over a hundred of the largest records (a 68-byte head plus NTFS's 255-unit name, 578 bytes).
const DIRECTORY_QUERY_BYTES: usize = 65_536;

/// The Windows host.
#[derive(Debug)]
pub struct OsHost {
  dirs: BTreeMap<u64, OwnedHandle>,
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

/// The refusal for an `NtCreateFile` status: absent (including an entry being deleted, and a name the
/// filesystem cannot hold), the wrong kind, or the host's own error as its Win32 code.
fn status_refusal(status: NTSTATUS) -> HostError {
  match status {
    STATUS_OBJECT_NAME_NOT_FOUND
    | STATUS_OBJECT_PATH_NOT_FOUND
    | STATUS_NO_SUCH_FILE
    | STATUS_DELETE_PENDING
    | STATUS_OBJECT_NAME_INVALID => HostError::NotFound,
    STATUS_NOT_A_DIRECTORY => HostError::NotDirectory,
    STATUS_FILE_IS_A_DIRECTORY => HostError::NotFile,
    other => {
      // SAFETY: a pure translation of a status value; it reads and writes no memory of ours.
      let code = unsafe { RtlNtStatusToDosError(other) };
      HostError::Unavailable(i32::try_from(code).unwrap_or(i32::MAX))
    }
  }
}

/// A Win32 error code as the host's refusal.
fn code_refusal(code: u32) -> HostError {
  HostError::Unavailable(i32::try_from(code).unwrap_or(i32::MAX))
}

/// A malformed record the OS filled: the host cannot serve it (`ERROR_INVALID_DATA`).
fn record_refusal(_refused: RecordRefusal) -> HostError {
  code_refusal(ERROR_INVALID_DATA)
}

/// Opens the entry `name` of the directory `parent` holds — one entry, never a path — with `access` and the
/// `NtCreateFile` `options` given; it opens what exists and creates nothing (`FILE_OPEN`). Case-insensitive,
/// as Win32 opens are.
fn open_relative(
  parent: &OwnedHandle,
  name: &str,
  access: u32,
  options: u32,
) -> Result<OwnedHandle, HostError> {
  let units: Vec<u16> = one_entry(name, WINDOWS_NAME_BREAKS)?
    .encode_utf16()
    .collect();
  // A name longer than a counted string holds is no entry of any directory.
  let name_bytes = units
    .len()
    .checked_mul(size_of::<u16>())
    .and_then(|bytes| u16::try_from(bytes).ok())
    .ok_or(HostError::NotFound)?;
  let object_name = UNICODE_STRING {
    Length: name_bytes,
    MaximumLength: name_bytes,
    Buffer: units.as_ptr().cast_mut(),
  };
  let attributes = OBJECT_ATTRIBUTES {
    Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).unwrap_or(u32::MAX),
    RootDirectory: parent.as_raw_handle().cast(),
    ObjectName: &object_name,
    Attributes: OBJ_CASE_INSENSITIVE,
    ..Default::default()
  };
  let mut handle: HANDLE = std::ptr::null_mut();
  let mut status_block = IO_STATUS_BLOCK::default();
  // SAFETY: `attributes` names a live parent handle and a counted name whose buffer (`units`) outlives the
  // call; `handle` and `status_block` are writable records of the types the call fills; the allocation size
  // and extended attributes are the documented nulls for an open.
  let status = unsafe {
    // structural: allow — FILE_OPEN with read-only access: opens an existing entry, creates and writes nothing (R1).
    nt::NtCreateFile(
      &mut handle,
      access,
      &attributes,
      &mut status_block,
      std::ptr::null(),
      0,
      SHARE_ALL,
      nt::FILE_OPEN,
      options,
      std::ptr::null(),
      0,
    )
  };
  if status != 0 {
    return Err(status_refusal(status));
  }
  // SAFETY: a handle `NtCreateFile` just returned on success; nothing else owns it.
  Ok(unsafe { OwnedHandle::from_raw_handle(handle.cast()) })
}

/// The opened object's attributes and, for a reparse point, its tag.
fn attribute_tag(handle: &OwnedHandle) -> Result<FILE_ATTRIBUTE_TAG_INFO, HostError> {
  let mut info = FILE_ATTRIBUTE_TAG_INFO {
    FileAttributes: 0,
    ReparseTag: 0,
  };
  let size = u32::try_from(size_of::<FILE_ATTRIBUTE_TAG_INFO>()).unwrap_or(0);
  // SAFETY: `handle` is open for the call; `info` is a writable record of the class named, of the size
  // passed.
  let ok = unsafe {
    GetFileInformationByHandleEx(
      handle.as_raw_handle().cast(),
      FileAttributeTagInfo,
      (&raw mut info).cast(),
      size,
    )
  };
  if ok == 0 {
    return Err(refusal(&std::io::Error::last_os_error()));
  }
  Ok(info)
}

/// Whether the opened object redirects the namespace (a symbolic link, a junction, a mount point).
fn redirects(info: &FILE_ATTRIBUTE_TAG_INFO) -> bool {
  info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && is_name_surrogate(info.ReparseTag)
}

/// The object identity a reopen is checked against: the volume serial and the file index.
fn identity(handle: &OwnedHandle) -> Result<(u64, u64), HostError> {
  let fingerprint = fingerprint_of_handle(handle.as_raw_handle())?;
  Ok((fingerprint.dev, fingerprint.ino))
}

/// What a lookup expects to find.
#[derive(Clone, Copy)]
enum Lookup {
  Dir,
  File,
}

impl Lookup {
  fn access(self) -> u32 {
    match self {
      Lookup::Dir => DIRECTORY_ACCESS,
      Lookup::File => FILE_GENERIC_READ,
    }
  }

  fn options(self) -> u32 {
    let kind = match self {
      Lookup::Dir => nt::FILE_DIRECTORY_FILE,
      Lookup::File => nt::FILE_NON_DIRECTORY_FILE,
    };
    kind | nt::FILE_SYNCHRONOUS_IO_NONALERT
  }

  fn wrong_kind(self) -> HostError {
    match self {
      Lookup::Dir => HostError::NotDirectory,
      Lookup::File => HostError::NotFile,
    }
  }
}

/// Keeps a contained open by the reparse check on the object it opened: a plain entry is kept; a name
/// surrogate is the wrong kind; an entry whose reparse point is its own data is reopened through its filter
/// (`reopen`) and kept only if that is the same object — else the name now leads elsewhere, and the
/// contained entry is gone from it.
fn settle(
  contained: OwnedHandle,
  lookup: Lookup,
  reopen: impl FnOnce() -> Result<OwnedHandle, HostError>,
) -> Result<OwnedHandle, HostError> {
  let info = attribute_tag(&contained)?;
  if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
    return Ok(contained);
  }
  if redirects(&info) {
    return Err(lookup.wrong_kind());
  }
  let through = reopen()?;
  if identity(&through)? != identity(&contained)? {
    return Err(HostError::NotFound);
  }
  Ok(through)
}

/// Opens the entry `name` of `parent` as `lookup` expects, contained (see the module docs).
fn open_contained(
  parent: &OwnedHandle,
  name: &str,
  lookup: Lookup,
) -> Result<OwnedHandle, HostError> {
  let contained = open_relative(
    parent,
    name,
    lookup.access(),
    lookup.options() | nt::FILE_OPEN_REPARSE_POINT,
  )?;
  settle(contained, lookup, || {
    open_relative(parent, name, lookup.access(), lookup.options())
  })
}

/// The kind of an opened entry: a name surrogate is a link (never followed), else its directory bit.
fn kind_of(info: &FILE_ATTRIBUTE_TAG_INFO) -> HostKind {
  if redirects(info) {
    HostKind::Symlink
  } else if info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
    HostKind::Dir
  } else {
    HostKind::File
  }
}

/// A zeroed buffer of at least `bytes` bytes, 8-byte aligned (the alignment directory and reparse records
/// are filled at), as words.
fn aligned_words(bytes: usize) -> Vec<u64> {
  vec![0u64; bytes.div_ceil(size_of::<u64>())]
}

/// The bytes of an aligned buffer as the OS filled them.
fn filled_bytes(words: &[u64]) -> Vec<u8> {
  words.iter().flat_map(|word| word.to_ne_bytes()).collect()
}

/// The byte length of an aligned buffer, as the Win32 calls take it.
fn byte_length(words: &[u64]) -> Result<u32, HostError> {
  words
    .len()
    .checked_mul(size_of::<u64>())
    .and_then(|bytes| u32::try_from(bytes).ok())
    .ok_or(code_refusal(ERROR_INVALID_DATA))
}

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
  let never = windows_sys::Win32::Foundation::FILETIME {
    dwLowDateTime: 0,
    dwHighDateTime: 0,
  };
  let mut info = BY_HANDLE_FILE_INFORMATION {
    dwFileAttributes: 0,
    ftCreationTime: never,
    ftLastAccessTime: never,
    ftLastWriteTime: never,
    dwVolumeSerialNumber: 0,
    nFileSizeHigh: 0,
    nFileSizeLow: 0,
    nNumberOfLinks: 0,
    nFileIndexHigh: 0,
    nFileIndexLow: 0,
  };
  // SAFETY: `handle` is an open handle this host holds for the call's duration; `info` is a
  // writable record of the type the call fills.
  let ok = unsafe { GetFileInformationByHandle(handle.cast(), &mut info) };
  if ok == 0 {
    return Err(HostError::Unavailable(
      std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
    ));
  }
  let mut basic = FILE_BASIC_INFO {
    CreationTime: 0,
    LastAccessTime: 0,
    LastWriteTime: 0,
    ChangeTime: 0,
    FileAttributes: 0,
  };
  let basic_len = u32::try_from(size_of::<FILE_BASIC_INFO>()).unwrap_or(0);
  // SAFETY: `handle` is open for the call's duration; `basic` is a writable record of the
  // class named, of the length passed.
  let ok = unsafe {
    GetFileInformationByHandleEx(
      handle.cast(),
      windows_sys::Win32::Storage::FileSystem::FileBasicInfo,
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
    // Windows has no POSIX owner; a base entry reports the root's, as the WinFsp bridge maps every file.
    uid: 0,
    gid: 0,
  })
}

/// A `FILETIME` as nanoseconds.
fn filetime_ns(high: u32, low: u32) -> i64 {
  let ticks = (u64::from(high) << u32::BITS) | u64::from(low);
  i64::try_from(ticks)
    .unwrap_or(i64::MAX)
    .saturating_mul(HUNDRED_NS)
}

impl OsHost {
  /// Opens a base directory: the one path this host ever resolves, opened as itself (a name surrogate at
  /// the root is refused, as on Unix with `O_NOFOLLOW`) and retained, so every later access is relative to
  /// it and a rename or replacement of the path changes nothing.
  pub fn open_root(path: &Path) -> Result<(Self, HostDir), HostError> {
    let open = |flags: u32| -> Result<OwnedHandle, HostError> {
      // structural: allow — a read-only open of the base directory; nothing is written (R1).
      std::fs::OpenOptions::new()
        .access_mode(DIRECTORY_ACCESS)
        .share_mode(SHARE_ALL)
        .custom_flags(flags)
        .open(path)
        .map(OwnedHandle::from)
        .map_err(|e| refusal(&e))
    };
    let contained = open(BACKUP_SEMANTICS | OPEN_REPARSE_POINT)?;
    if attribute_tag(&contained)?.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
      return Err(HostError::NotDirectory);
    }
    let root = settle(contained, Lookup::Dir, || open(BACKUP_SEMANTICS))?;
    let mut host = Self {
      dirs: BTreeMap::new(),
      files: BTreeMap::new(),
      next: 1,
    };
    let h = host.allocate()?;
    host.dirs.insert(h, root);
    Ok((host, HostDir(h)))
  }

  /// The next handle id. Ids are never reused, so one is never recycled onto a handle still held; once the
  /// id space is spent the host refuses as it would past its handle limit (`ERROR_TOO_MANY_OPEN_FILES`).
  /// (At one open a nanosecond, a `u64` lasts 584 years.)
  fn allocate(&mut self) -> Result<u64, HostError> {
    let h = self.next;
    self.next = h.checked_add(1).ok_or(code_refusal(
      windows_sys::Win32::Foundation::ERROR_TOO_MANY_OPEN_FILES,
    ))?;
    Ok(h)
  }

  fn dir(&self, dir: HostDir) -> Result<&OwnedHandle, HostError> {
    self.dirs.get(&dir.0).ok_or(HostError::StaleHandle)
  }

  /// Open handles, for leak checks.
  pub fn open_handles(&self) -> usize {
    self.dirs.len().saturating_add(self.files.len())
  }
}

/// The entry `units` of `parent` as a listing shows it: fingerprinted and kinded through its own contained
/// open; `None` when it is gone between the enumeration and the open (the disk is the truth). A name that is
/// not valid UTF-16 is listed as something the volume never serves, as the Unix host lists a non-UTF-8 name.
fn listed_entry(parent: &OwnedHandle, units: &[u16]) -> Result<Option<BaseEntry>, HostError> {
  let Ok(name) = String::from_utf16(units) else {
    return Ok(Some(BaseEntry {
      name: String::from_utf16_lossy(units).into(),
      kind: HostKind::Other,
      fingerprint: Fingerprint::default(),
    }));
  };
  let entry = match open_relative(
    parent,
    &name,
    ENTRY_ACCESS,
    nt::FILE_OPEN_REPARSE_POINT | nt::FILE_SYNCHRONOUS_IO_NONALERT,
  ) {
    Ok(entry) => entry,
    Err(HostError::NotFound) => return Ok(None),
    Err(e) => return Err(e),
  };
  let info = attribute_tag(&entry)?;
  Ok(Some(BaseEntry {
    name: name.into(),
    kind: kind_of(&info),
    fingerprint: fingerprint_of_handle(entry.as_raw_handle())?,
  }))
}

impl HostFs for OsHost {
  fn facts(&mut self, dir: HostDir) -> Result<HostFacts, HostError> {
    self.dir(dir)?;
    // The coarsest class of the table until the volume's filesystem is queried through the handle: the
    // racy rule then re-hashes more often, never less.
    Ok(HostFacts {
      timestamp_granularity_ns: granularity_for(FsKind::Unknown),
    })
  }

  fn now_ns(&mut self) -> i64 {
    // The fingerprints' timestamps are FILETIME nanoseconds (`filetime_ns`), so "now" is too.
    let since_unix = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap_or(std::time::Duration::ZERO);
    i64::try_from(since_unix.as_secs())
      .unwrap_or(i64::MAX)
      .saturating_add(FILETIME_EPOCH_OFFSET_S)
      .saturating_mul(NS_PER_S)
      .saturating_add(i64::from(since_unix.subsec_nanos()))
  }

  fn fingerprint_dir(&mut self, dir: HostDir) -> Result<Fingerprint, HostError> {
    fingerprint_of_handle(self.dir(dir)?.as_raw_handle())
  }

  fn list(&mut self, dir: HostDir) -> Result<Vec<BaseEntry>, HostError> {
    let parent = self.dir(dir)?;
    let mut buffer = aligned_words(DIRECTORY_QUERY_BYTES);
    let length = byte_length(&buffer)?;
    let mut class = FileFullDirectoryRestartInfo;
    let mut out = Vec::new();
    // Each call returns at least one record or `ERROR_NO_MORE_FILES`, so the loop ends with the directory.
    loop {
      // SAFETY: `parent` is a retained directory handle opened for listing; `buffer` is writable, 8-byte
      // aligned, of the length passed.
      let ok = unsafe {
        GetFileInformationByHandleEx(
          parent.as_raw_handle().cast(),
          class,
          buffer.as_mut_ptr().cast(),
          length,
        )
      };
      if ok == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == i32::try_from(ERROR_NO_MORE_FILES).ok() {
          return Ok(out);
        }
        return Err(refusal(&error));
      }
      class = FileFullDirectoryInfo;
      for units in directory_names(&filled_bytes(&buffer)).map_err(record_refusal)? {
        if units == [u16::from(b'.')] || units == [u16::from(b'.'), u16::from(b'.')] {
          continue;
        }
        if let Some(entry) = listed_entry(parent, &units)? {
          out.push(entry);
        }
      }
      buffer.fill(0);
    }
  }

  fn open_dir(&mut self, parent: HostDir, name: &str) -> Result<HostDir, HostError> {
    let opened = open_contained(self.dir(parent)?, name, Lookup::Dir)?;
    let h = self.allocate()?;
    self.dirs.insert(h, opened);
    Ok(HostDir(h))
  }

  fn open_file(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    let opened = open_contained(self.dir(dir)?, name, Lookup::File)?;
    let h = self.allocate()?;
    self.files.insert(h, std::fs::File::from(opened));
    Ok(HostFile(h))
  }

  fn fstat(&mut self, file: HostFile) -> Result<Fingerprint, HostError> {
    let f = self.files.get(&file.0).ok_or(HostError::StaleHandle)?;
    fingerprint_of_handle(f.as_raw_handle())
  }

  fn read_at(&mut self, file: HostFile, off: u64, buf: &mut [u8]) -> Result<usize, HostError> {
    let f = self.files.get(&file.0).ok_or(HostError::StaleHandle)?;
    f.seek_read(buf, off).map_err(|e| refusal(&e))
  }

  fn read_link(&mut self, dir: HostDir, name: &str) -> Result<Box<str>, HostError> {
    let link = open_relative(
      self.dir(dir)?,
      name,
      ENTRY_ACCESS,
      nt::FILE_OPEN_REPARSE_POINT | nt::FILE_SYNCHRONOUS_IO_NONALERT,
    )?;
    let reparse_bytes = usize::try_from(MAXIMUM_REPARSE_DATA_BUFFER_SIZE)
      .map_err(|_| code_refusal(ERROR_INVALID_DATA))?;
    let mut buffer = aligned_words(reparse_bytes);
    let length = byte_length(&buffer)?;
    let mut returned = 0u32;
    // SAFETY: `link` is the entry's own handle, opened without following it; `buffer` is writable, 8-byte
    // aligned, of the length passed; `returned` is writable; no input buffer and no overlapped record (the
    // handle is synchronous).
    let ok = unsafe {
      DeviceIoControl(
        link.as_raw_handle().cast(),
        FSCTL_GET_REPARSE_POINT,
        std::ptr::null(),
        0,
        buffer.as_mut_ptr().cast(),
        length,
        &mut returned,
        std::ptr::null_mut(),
      )
    };
    if ok == 0 {
      return Err(refusal(&std::io::Error::last_os_error()));
    }
    let bytes = filled_bytes(&buffer);
    let returned = usize::try_from(returned).map_err(|_| code_refusal(ERROR_INVALID_DATA))?;
    let filled = bytes
      .get(..returned)
      .ok_or(code_refusal(ERROR_INVALID_DATA))?;
    let target = link_target(filled).map_err(record_refusal)?;
    String::from_utf16(&target)
      .map(Into::into)
      .map_err(|_| code_refusal(ERROR_NO_UNICODE_TRANSLATION))
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
