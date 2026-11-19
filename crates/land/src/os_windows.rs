//! The write seam over Windows (§4.15 step 6, D-26; plan G-shards-windows step 5, where a slates write must reach a
//! Windows DAX map by the refill): the Windows form of the Unix seam (`os.rs`), with the same surface, so the engine,
//! the server's wiring and the oracle run unchanged over it. It is the only Windows code in the workspace that
//! creates, writes, renames, flushes or removes a host path, and it runs inside a granted landing only.
//!
//! Shape: every verb is relative to a handle the base host holds (`slates_base::OsHost`, whose root and target are
//! walked one component at a time with `FILE_OPEN_REPARSE_POINT`, refusing links and junctions); no verb takes a path.
//! - **Creation:** `NtCreateFile` with `FILE_CREATE` and the parent's handle as `RootDirectory`, one entry name, so an
//!   existing name, a link or a junction under the name is never written through: the create refuses
//!   (`STATUS_OBJECT_NAME_COLLISION`). Temporaries are named hidden siblings (Windows has no unnamed file); a crash's
//!   leftovers are the next landing's sweep's, as on macOS.
//! - **Moves:** `NtSetInformationFile(FileRenameInformationEx)` on the entry's own handle, with the target
//!   directory's handle as `RootDirectory` and `FILE_RENAME_FLAG_POSIX_SEMANTICS` and never `REPLACE_IF_EXISTS`: the
//!   rename that never replaces (AUD-29-04), refused when the name exists. (Win32's `SetFileInformationByHandle`
//!   refused the relative name, `ERROR_INVALID_PARAMETER`.) NTFS has no atomic exchange of two names, so
//!   [`LandFs::exchange`] is refused and the capabilities say so; the engine then moves the old entry aside first, as
//!   on the other unixes.
//! - **Removal:** `FileDispositionInfoEx` with POSIX semantics, so the name goes at once even while another process
//!   holds the file open, and with the read-only attribute ignored.
//! - **Durability:** a file's write-out does nothing ([`write_out_file`]); a batch's barrier is each file's
//!   `FlushFileBuffers` on ⌈√n⌉ threads (the Unix derivation), off the shard (A-132). A directory's sync and the media
//!   barrier are the directory's own `FlushFileBuffers`, through the directory opened again with `FILE_ADD_FILE` (an
//!   empty name relative to its own handle): `ReOpenFile` with `GENERIC_WRITE` was refused `ERROR_ACCESS_DENIED`.
//! - **Mode, attributes and times:** Windows keeps no POSIX mode. [`LandFs::set_mode`] records the one bit it can: a
//!   mode with no write bit makes the file read-only (`FILE_ATTRIBUTE_READONLY`), as A-262 reads a file's read-only
//!   state as its mode, and every other bit the file holds is kept. Since A-451 every other bit the volume keeps
//!   (hidden, system, archive, offline, not indexed, and a directory's read-only) is set on the file, directory or link
//!   the landing makes ([`LandFs::set_windows`], the `mkdir` and `symlink` verbs; `crate::windows_bits`), so what a
//!   person set through the drive is what the disk holds; NTFS sets `ARCHIVE` again on the rename that places a file.
//!   The word is read back after it is set, and a bit the target's file system did not take (FAT holds no `OFFLINE`
//!   and no `NOT_CONTENT_INDEXED`, and drops them without a word) refuses the entry `EOPNOTSUPP` (`give_bits`), so a
//!   bit is landed or refused, never dropped. The modification time is set through `FILE_BASIC_INFO` in the FILETIME
//!   domain.
//! - **Reflinks (A-445):** where the target's volume shares clusters between files (ReFS, a Dev Drive:
//!   `FILE_SUPPORTS_BLOCK_REFCOUNTING`), a file whose bytes its batch already staged is cloned from that temporary by
//!   `FSCTL_DUPLICATE_EXTENTS_TO_FILE`, in regions of whole clusters below 4 GiB laid out from the volume's own cluster
//!   (`crate::block_clone`), the new file's end first set to the length rounded up to a cluster and set back after.
//!   NTFS shares nothing, so its capability says so and the engine never asks. A pair the volume refuses to share (the
//!   cluster's 8,175 sharers, the two files' integrity streams, another volume) is refused `EOPNOTSUPP` with nothing
//!   left, and the engine writes its bytes.
//! - **Symbolic links (A-438):** a volume link is landed as the Windows symbolic link the WinFsp drive shows for it
//!   (A-275's form, `slates_vfs::host::windows_link`): a relative target with `\` and `SYMLINK_FLAG_RELATIVE`, a
//!   Windows-absolute one under `\??\`. Handle-relative, as every verb here: the empty object made at the landing's
//!   own hidden name (`staged`) with `FILE_OPEN_REPARSE_POINT`, its `IO_REPARSE_TAG_SYMLINK` buffer set
//!   (`FSCTL_SET_REPARSE_POINT`), then moved to its name without replacing, so the name never holds half a link and a
//!   crash between the steps leaves a hidden sibling the landing's sweep removes. NTFS sets a link's buffer only for a
//!   caller with `SeCreateSymbolicLinkPrivilege` enabled, unless Developer Mode lets it; an administrator's token holds
//!   it disabled, and `CreateSymbolicLinkW` enables it around its own call, so the set runs on this thread under a copy
//!   of the process's token with it enabled (`as_link_maker`), for that call alone. No privilege is asked for that the
//!   process does not hold (R10): where NTFS refuses the set (a standard account outside Developer Mode) the entry fails
//!   `EPERM`, typed, as every link did before A-438. Since A-451 a directory link Windows made in the volume (A-275's
//!   `LINK_DIRECTORY`) lands as a directory link: its object is made a directory, as `SYMBOLIC_LINK_FLAG_DIRECTORY`
//!   makes one.
//!
//! Refusals the engine tells apart are reported in its errno numbers (it was written over POSIX): a name that exists
//! as `EEXIST` (17), no room as `ENOSPC` (28), the missing exchange as `EOPNOTSUPP` (95).
//!
//! Containment (§4.15 step 4, §4.13): [`OsLand::open_target`] walks the target from its drive's root
//! (`slates_base::walk_dir`), and the target must be owned by this process's user (`TargetNotOwned`), compared by
//! security identifier. A target on one of this daemon's own WinFsp drives is refused before any open (`OwnMount`,
//! A-227), as the Unix walk refuses a component on one of its mounts.

use std::collections::{BTreeMap, BTreeSet};
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::path::Path;

use slates_base::{OsHost, WalkRefusal, walk_dir};
use slates_vfs::host::{
  Barrier, BaseEntry, Hint, HostDir, HostError, HostFacts, HostFile, HostFs, LandCapabilities,
  LandFs, WatchState,
};
use slates_vfs::inode::Fingerprint;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem as nt;
use windows_sys::Win32::Foundation::{
  ERROR_FILENAME_EXCED_RANGE, ERROR_IO_DEVICE, GetLastError, HANDLE, NTSTATUS,
  OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, STATUS_DELETE_PENDING, STATUS_FILE_IS_A_DIRECTORY,
  STATUS_NO_SUCH_FILE, STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_COLLISION,
  STATUS_OBJECT_NAME_INVALID, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND,
  UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
  DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_TAG_INFO,
  FILE_BASIC_INFO, FILE_GENERIC_WRITE, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_READ_DATA,
  FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FILE_WRITE_ATTRIBUTES,
  FILE_WRITE_DATA, FileAttributeTagInfo, FileBasicInfo, FileDispositionInfoEx, FlushFileBuffers,
  GetFileInformationByHandleEx, SYNCHRONIZE, SetFileInformationByHandle,
};

use crate::engine::LandingTarget;
// The kinds the landing tells apart, named by the writer's one rule for a Win32 code (A-527, A-539).
use crate::windows_codes::{ALREADY_EXISTS, DISK_FULL, NOT_SUPPORTED, unsupported};
use slates_vfs::host::OsCode;
/// Format: every share mode, so a handle the writer holds never blocks a user's read, write, rename or delete.
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
/// Format: a temporary's access: written, its attributes and times set, moved and flushed (`DELETE` to rename it), and
/// read as the source of a clone (`FILE_READ_DATA`, which `FSCTL_DUPLICATE_EXTENTS_TO_FILE` asks of its source, A-445).
const TEMP_ACCESS: u32 =
  FILE_GENERIC_WRITE | FILE_READ_DATA | FILE_READ_ATTRIBUTES | DELETE | SYNCHRONIZE;
/// Format: an entry opened to be moved or removed: nothing of its data, its attributes, and `DELETE`.
const MOVE_ACCESS: u32 = DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
/// Format: a directory opened to have its attributes set (a base directory's metadata entry, A-501): its attributes read
/// and written (`FILE_BASIC_INFO` asks `FILE_WRITE_ATTRIBUTES`), and `FILE_WRITE_DATA`, a directory's `FILE_ADD_FILE`,
/// which its batch's barrier needs to flush it (A-472: `FlushFileBuffers` "writes all the buffered information for a
/// specified file", through a handle that "must have the GENERIC_WRITE access right"); nothing in it is written. MS-FSA
/// 2.1.5.1.2.1 refuses those rights to a read-only data file alone, never a directory. A file's metadata entry opens as
/// `crate::windows_bits::meta_open` says (A-519).
const CHMOD_ACCESS: u32 =
  FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES | FILE_WRITE_DATA | SYNCHRONIZE;
/// Format: a directory this writer makes: listed, traversed and its attributes read, as the host retains one.
const MADE_DIR_ACCESS: u32 =
  FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
/// Format: `FILE_RENAME_FLAG_POSIX_SEMANTICS` (winbase.h): the rename takes effect at once, as a POSIX rename does,
/// even while another handle holds the destination's old file open. `REPLACE_IF_EXISTS` (1) is never set.
const RENAME_POSIX: u32 = 0x2;
/// Format: `FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS |
/// FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE` (winbase.h): the name goes at once, whoever holds the file, and a
/// read-only file goes too, as `unlink` removes one.
const DISPOSE_POSIX: u32 = 0x1 | 0x2 | 0x10;
/// Format: the FILETIME epoch (1601) precedes the Unix epoch by this many 100 ns intervals.
const FILETIME_UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;
/// Format: nanoseconds in a FILETIME interval.
const HUNDRED_NS: i64 = 100;
/// Format: a symbolic link's object as this writer makes it (A-438): its reparse buffer set (`FSCTL_SET_REPARSE_POINT`
/// asks `FILE_WRITE_DATA` or `FILE_WRITE_ATTRIBUTES`, MS-FSA 2.1.5.10.36), moved to its name and, refused, removed
/// (`DELETE`), its attributes read and waited on synchronously.
const LINK_ACCESS: u32 =
  FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES | FILE_READ_ATTRIBUTES | DELETE | SYNCHRONIZE;
/// Format: `FSCTL_SET_REPARSE_POINT` (winioctl.h): `CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 41, METHOD_BUFFERED,
/// FILE_SPECIAL_ACCESS)`, which sets the object's reparse buffer.
const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
/// Format: the access a directory's flush is made through: adding entries (`FILE_ADD_FILE`, the directory's
/// `FILE_WRITE_DATA`) and waiting synchronously; `FlushFileBuffers` needs a handle that may write.
const DIR_FLUSH_ACCESS: u32 = windows_sys::Win32::Storage::FileSystem::FILE_ADD_FILE | SYNCHRONIZE;

/// Why a target path was refused before the lease (§4.15 step 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetRefusal {
  /// The path is not absolute.
  NotAbsolute,
  /// A component is a link, a junction, a mount point or `..`: the path would escape the directory the human named.
  EscapesTarget,
  /// The target is owned by another user.
  TargetNotOwned,
  /// The target is on one of this daemon's own WinFsp volumes (§4.6: no daemon thread opens a path on a volume the
  /// daemon serves; A-227): refused before any open, so the landing wrote nothing and waits on nothing of its own.
  OwnMount,
  /// The host refused.
  Unavailable(HostError),
}

/// A temporary's bookkeeping: where it was created and under which name.
#[derive(Clone, Debug)]
struct Temp {
  dir: HostDir,
  name: Box<str>,
}

/// The operating-system writer.
pub struct OsLand {
  host: OsHost,
  temps: BTreeMap<u64, Temp>,
  /// The metadata entries' files opened write-through (A-519): read-only files, each set through them stored as it
  /// returns, which no barrier flushes, since their handles may not write.
  through: BTreeSet<u64>,
}

/// A writer's own state while it has lent its host away ([`OsLand::lend`]).
#[derive(Debug)]
pub struct Away {
  temps: BTreeMap<u64, Temp>,
  through: BTreeSet<u64>,
}

impl std::fmt::Debug for OsLand {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("OsLand")
      .field("host", &self.host)
      .field("temps", &self.temps.len())
      .field("through", &self.through.len())
      .finish()
  }
}

/// A Win32 error code as the seam's refusal (A-527, `crate::windows_codes::refusal_of`): the errno of a kind the engine
/// tells apart, else the Win32 code it is.
fn code_refusal(code: u32) -> HostError {
  crate::windows_codes::refusal_of(code)
}

/// An I/O error of the standard library's as the seam's refusal: its OS code is a Win32 code here.
fn io_refusal(error: &std::io::Error) -> HostError {
  code_refusal(
    error
      .raw_os_error()
      .and_then(|code| u32::try_from(code).ok())
      .unwrap_or(0),
  )
}

/// The calling thread's last Win32 error as the seam's refusal; read before any other call.
fn last_refusal() -> HostError {
  // SAFETY: reads this thread's last error, which the call that just failed set.
  code_refusal(unsafe { GetLastError() })
}

/// An `NtCreateFile` status as the seam's refusal.
fn status_refusal(status: NTSTATUS) -> HostError {
  match status {
    STATUS_OBJECT_NAME_NOT_FOUND
    | STATUS_OBJECT_PATH_NOT_FOUND
    | STATUS_NO_SUCH_FILE
    | STATUS_DELETE_PENDING
    | STATUS_OBJECT_NAME_INVALID => HostError::NotFound,
    STATUS_NOT_A_DIRECTORY => HostError::NotDirectory,
    STATUS_FILE_IS_A_DIRECTORY => HostError::NotFile,
    STATUS_OBJECT_NAME_COLLISION => HostError::Exists(OsCode::Win32(ALREADY_EXISTS)),
    other => {
      // SAFETY: a pure translation of a status value; it reads and writes no memory of ours.
      code_refusal(unsafe { RtlNtStatusToDosError(other) })
    }
  }
}

/// `name` as one entry's UTF-16 units: never empty, never `.` or `..`, never a separator or a stream.
fn entry_units(name: &str) -> Result<Vec<u16>, HostError> {
  if name.is_empty() || name == "." || name == ".." || name.contains(['\\', '/', ':', '\0']) {
    return Err(HostError::NotFound);
  }
  Ok(name.encode_utf16().collect())
}

/// `NtCreateFile` of the entry `name` of `parent` with `access`, `disposition` (`FILE_OPEN` or `FILE_CREATE`) and
/// `options`, case-insensitive as Win32 opens are; `attributes` are a created file's.
fn nt_create(
  parent: BorrowedHandle<'_>,
  name: &str,
  access: u32,
  disposition: u32,
  options: u32,
  attributes: u32,
) -> Result<OwnedHandle, HostError> {
  let units = entry_units(name)?;
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
  let object = OBJECT_ATTRIBUTES {
    Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).unwrap_or(u32::MAX),
    RootDirectory: parent.as_raw_handle().cast(),
    ObjectName: &object_name,
    Attributes: OBJ_CASE_INSENSITIVE,
    ..Default::default()
  };
  let mut handle: HANDLE = std::ptr::null_mut();
  let mut status_block = windows_sys::Win32::System::IO::IO_STATUS_BLOCK::default();
  // SAFETY: `object` names a live parent handle and a counted name whose buffer (`units`) outlives the call; `handle`
  // and `status_block` are writable records of the types the call fills; no allocation size and no extended
  // attributes.
  let status = unsafe {
    nt::NtCreateFile(
      &mut handle,
      access,
      &object,
      &mut status_block,
      std::ptr::null(),
      attributes,
      SHARE_ALL,
      disposition,
      options | nt::FILE_SYNCHRONOUS_IO_NONALERT,
      std::ptr::null(),
      0,
    )
  };
  if status < 0 {
    return Err(status_refusal(status));
  }
  // SAFETY: the handle `NtCreateFile` just returned on success; nothing else owns it.
  Ok(unsafe { OwnedHandle::from_raw_handle(handle.cast()) })
}

/// Opens the entry `name` of `parent` itself (never through a link) to move or remove it.
fn open_entry(parent: BorrowedHandle<'_>, name: &str) -> Result<OwnedHandle, HostError> {
  nt_create(
    parent,
    name,
    MOVE_ACCESS,
    nt::FILE_OPEN,
    nt::FILE_OPEN_REPARSE_POINT,
    0,
  )
}

/// The opened entry's attributes.
fn attributes(handle: BorrowedHandle<'_>) -> Result<u32, HostError> {
  let mut info = FILE_ATTRIBUTE_TAG_INFO {
    FileAttributes: 0,
    ReparseTag: 0,
  };
  let size = u32::try_from(size_of::<FILE_ATTRIBUTE_TAG_INFO>()).unwrap_or(0);
  // SAFETY: `handle` is open for the call; `info` is a writable record of the class named, of the size passed.
  let ok = unsafe {
    GetFileInformationByHandleEx(
      handle.as_raw_handle().cast(),
      FileAttributeTagInfo,
      (&raw mut info).cast(),
      size,
    )
  };
  if ok == 0 {
    return Err(last_refusal());
  }
  Ok(info.FileAttributes)
}

/// Renames the entry `entry` holds to `name` under `to_dir`, never replacing (POSIX semantics, no
/// `REPLACE_IF_EXISTS`): refused `EEXIST` when the name exists.
fn rename_handle(
  entry: BorrowedHandle<'_>,
  to_dir: BorrowedHandle<'_>,
  name: &str,
) -> Result<(), HostError> {
  let units = entry_units(name)?;
  let name_bytes = units
    .len()
    .checked_mul(size_of::<u16>())
    .ok_or(HostError::NotFound)?;
  let name_at = std::mem::offset_of!(nt::FILE_RENAME_INFORMATION, FileName);
  let record_bytes = name_at
    .checked_add(name_bytes)
    .and_then(|bytes| bytes.checked_add(size_of::<u16>()))
    .ok_or(HostError::NotFound)?;
  // A buffer aligned for the record (its handle member), zeroed, the record and the name written into it. Win32's
  // `SetFileInformationByHandle(FileRenameInfoEx)` refused a name relative to `RootDirectory` here
  // (`ERROR_INVALID_PARAMETER`, measured on jupiter 2026-10-26), so the record goes to the NT call beneath it, which
  // takes one.
  let mut record = vec![0u64; record_bytes.div_ceil(size_of::<u64>())];
  let base = record.as_mut_ptr().cast::<u8>();
  let length = u32::try_from(name_bytes).map_err(|_| HostError::NotFound)?;
  // SAFETY: `record` holds `record_bytes` bytes, aligned for `FILE_RENAME_INFORMATION`; the flags word sits at the
  // record's start, the root handle and the name's length at their offsets, and the name's units at `FileName`'s
  // offset, all inside the buffer (its length was summed from those offsets with checked arithmetic).
  unsafe {
    let info = base.cast::<nt::FILE_RENAME_INFORMATION>();
    (*info).Anonymous.Flags = RENAME_POSIX;
    (*info).RootDirectory = to_dir.as_raw_handle().cast();
    (*info).FileNameLength = length;
    std::ptr::copy_nonoverlapping(units.as_ptr().cast::<u8>(), base.add(name_at), name_bytes);
  }
  let size = u32::try_from(record_bytes).map_err(|_| HostError::NotFound)?;
  let mut status_block = windows_sys::Win32::System::IO::IO_STATUS_BLOCK::default();
  // SAFETY: `entry` is open with `DELETE`; `record` is a filled `FILE_RENAME_INFORMATION` of `size` bytes and
  // `status_block` a writable record, both alive for the call.
  let status = unsafe {
    nt::NtSetInformationFile(
      entry.as_raw_handle().cast(),
      &mut status_block,
      record.as_ptr().cast(),
      size,
      nt::FileRenameInformationEx,
    )
  };
  if status < 0 {
    return Err(status_refusal(status));
  }
  Ok(())
}

/// Removes the name of the entry `entry` holds at once (POSIX semantics, the read-only attribute ignored).
fn dispose(entry: BorrowedHandle<'_>) -> Result<(), HostError> {
  let info = windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO_EX {
    Flags: DISPOSE_POSIX,
  };
  let size = u32::try_from(size_of_val(&info)).unwrap_or(0);
  // SAFETY: `entry` is open with `DELETE`; `info` is a filled record of the class named, of the size passed.
  let ok = unsafe {
    SetFileInformationByHandle(
      entry.as_raw_handle().cast(),
      FileDispositionInfoEx,
      (&raw const info).cast(),
      size,
    )
  };
  if ok == 0 {
    return Err(last_refusal());
  }
  Ok(())
}

/// Format: `FILE_SUPPORTS_BLOCK_REFCOUNTING` (winnt.h): the volume shares logical clusters between files on it (ReFS's
/// block cloning, Microsoft Learn, `GetVolumeInformationByHandleW`).
const BLOCK_REFCOUNTING: u32 =
  windows_sys::Win32::System::SystemServices::FILE_SUPPORTS_BLOCK_REFCOUNTING;

/// Whether the volume the directory `dir` is on shares clusters between files (`FILE_SUPPORTS_BLOCK_REFCOUNTING`): a
/// landing's reflinks there (A-445).
fn shares_clusters(dir: BorrowedHandle<'_>) -> Result<bool, HostError> {
  let mut flags = 0u32;
  // SAFETY: `dir` is open for the call; only the file system's flags are asked for (a writable local), every buffer
  // left null with its length zero.
  let read = unsafe {
    windows_sys::Win32::Storage::FileSystem::GetVolumeInformationByHandleW(
      dir.as_raw_handle().cast(),
      std::ptr::null_mut(),
      0,
      std::ptr::null_mut(),
      std::ptr::null_mut(),
      &mut flags,
      std::ptr::null_mut(),
      0,
    )
  };
  if read == 0 {
    return Err(last_refusal());
  }
  Ok(flags & BLOCK_REFCOUNTING != 0)
}

/// The volume's cluster in bytes (its allocation unit: sectors per unit times bytes per sector, `FileFsSizeInformation`),
/// asked of an open handle on it: what every clone region aligns to (A-445).
fn cluster_bytes(on: BorrowedHandle<'_>) -> Result<u64, HostError> {
  let mut size = windows_sys::Wdk::System::SystemServices::FILE_FS_SIZE_INFORMATION::default();
  let mut status_block = windows_sys::Win32::System::IO::IO_STATUS_BLOCK::default();
  let length =
    u32::try_from(size_of_val(&size)).map_err(|_| code_refusal(ERROR_FILENAME_EXCED_RANGE))?;
  // SAFETY: `on` is open for the call; `size` is a writable record of the class asked, of the length passed, and
  // `status_block` a writable status record.
  let status = unsafe {
    nt::NtQueryVolumeInformationFile(
      on.as_raw_handle().cast(),
      &mut status_block,
      (&raw mut size).cast(),
      length,
      nt::FileFsSizeInformation,
    )
  };
  if status < 0 {
    return Err(status_refusal(status));
  }
  u64::from(size.SectorsPerAllocationUnit)
    .checked_mul(u64::from(size.BytesPerSector))
    .ok_or(unsupported(NOT_SUPPORTED))
}

/// Sets the end of the file `file` to `end` bytes (`FileEndOfFileInfo`): grown with zeros, or cut.
fn set_end(file: BorrowedHandle<'_>, end: u64) -> Result<(), HostError> {
  let info = windows_sys::Win32::Storage::FileSystem::FILE_END_OF_FILE_INFO {
    EndOfFile: i64::try_from(end).map_err(|_| HostError::NoSpace(OsCode::Win32(DISK_FULL)))?,
  };
  let size = u32::try_from(size_of_val(&info)).unwrap_or(0);
  // SAFETY: `file` is open with `FILE_WRITE_DATA`; `info` is a filled record of the class named, of the size passed.
  let set = unsafe {
    SetFileInformationByHandle(
      file.as_raw_handle().cast(),
      windows_sys::Win32::Storage::FileSystem::FileEndOfFileInfo,
      (&raw const info).cast(),
      size,
    )
  };
  if set == 0 {
    return Err(last_refusal());
  }
  Ok(())
}

/// Clones `length` bytes at `offset` of `source` to the same offset of `target` (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`, a
/// region of whole clusters below 4 GiB, `crate::block_clone`): refused `EOPNOTSUPP` where the volume will not share
/// this pair ([`clone_refusal`]).
fn duplicate_extents(
  source: BorrowedHandle<'_>,
  target: BorrowedHandle<'_>,
  offset: u64,
  length: u64,
) -> Result<(), HostError> {
  use windows_sys::Win32::System::Ioctl::{
    DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE,
  };
  let at = |value: u64| i64::try_from(value).map_err(|_| unsupported(NOT_SUPPORTED));
  let region = DUPLICATE_EXTENTS_DATA {
    FileHandle: source.as_raw_handle().cast(),
    SourceFileOffset: at(offset)?,
    TargetFileOffset: at(offset)?,
    ByteCount: at(length)?,
  };
  let size = u32::try_from(size_of_val(&region)).unwrap_or(0);
  let mut returned = 0u32;
  // SAFETY: `target` is open with `FILE_WRITE_DATA` and synchronous (`FILE_SYNCHRONOUS_IO_NONALERT`), so no overlapped
  // record is passed; `region` names `source`, open with `FILE_READ_DATA` for the call, and is readable for `size`
  // bytes; nothing is written back but `returned`, a writable local.
  let cloned = unsafe {
    windows_sys::Win32::System::IO::DeviceIoControl(
      target.as_raw_handle().cast(),
      FSCTL_DUPLICATE_EXTENTS_TO_FILE,
      (&raw const region).cast(),
      size,
      std::ptr::null_mut(),
      0,
      &mut returned,
      std::ptr::null_mut(),
    )
  };
  if cloned != 0 {
    return Ok(());
  }
  let code = std::io::Error::last_os_error()
    .raw_os_error()
    .and_then(|code| u32::try_from(code).ok())
    .unwrap_or(0);
  Err(clone_refusal(code))
}

/// A clone's refusal as the seam reports it (A-445): the volume's refusals to share this pair, each with nothing
/// written, are `EOPNOTSUPP`, after which the engine writes the bytes: no sharing at all (`ERROR_NOT_SUPPORTED`,
/// `ERROR_INVALID_FUNCTION`), a cluster already shared by its limit of regions (`ERROR_BLOCK_TOO_MANY_REFERENCES`),
/// another volume (`ERROR_NOT_SAME_DEVICE`), and a pair whose integrity streams or sparseness differ
/// (`ERROR_INVALID_PARAMETER`, which ReFS answers for every precondition Microsoft lists). Every other code is the
/// clone's failure in Windows' own number, as a write's.
fn clone_refusal(code: u32) -> HostError {
  use windows_sys::Win32::Foundation::{
    ERROR_BLOCK_TOO_MANY_REFERENCES, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER,
    ERROR_NOT_SAME_DEVICE, ERROR_NOT_SUPPORTED,
  };
  match code {
    ERROR_NOT_SUPPORTED
    | ERROR_INVALID_FUNCTION
    | ERROR_BLOCK_TOO_MANY_REFERENCES
    | ERROR_NOT_SAME_DEVICE
    | ERROR_INVALID_PARAMETER => unsupported(code),
    other => code_refusal(other),
  }
}

/// Sets `buffer` (an `IO_REPARSE_TAG_SYMLINK` `REPARSE_DATA_BUFFER`) as the reparse point of the empty object `object`,
/// opened as itself (`FILE_OPEN_REPARSE_POINT`) with [`LINK_ACCESS`]: refused in Windows' own code, the link privilege's
/// `ERROR_PRIVILEGE_NOT_HELD` among them (A-539: as the Win32 code it is, never an errno's).
fn set_reparse(object: BorrowedHandle<'_>, buffer: &[u8]) -> Result<(), HostError> {
  let length = u32::try_from(buffer.len()).map_err(|_| code_refusal(ERROR_FILENAME_EXCED_RANGE))?;
  let mut returned = 0u32;
  // SAFETY: `object` is open for the call, synchronous (`FILE_SYNCHRONOUS_IO_NONALERT`), so no overlapped record is
  // passed; `buffer` is readable for `length` bytes; nothing is written back (no output buffer), and `returned` is a
  // writable local.
  let set = unsafe {
    windows_sys::Win32::System::IO::DeviceIoControl(
      object.as_raw_handle().cast(),
      FSCTL_SET_REPARSE_POINT,
      buffer.as_ptr().cast(),
      length,
      std::ptr::null_mut(),
      0,
      &mut returned,
      std::ptr::null_mut(),
    )
  };
  if set != 0 {
    return Ok(());
  }
  let code = std::io::Error::last_os_error()
    .raw_os_error()
    .and_then(|code| u32::try_from(code).ok())
    .unwrap_or(0);
  Err(code_refusal(code))
}

/// Runs `make` (a link's buffer set) on this thread under a copy of the process's token with
/// `SeCreateSymbolicLinkPrivilege` enabled, where the process holds the privilege ([`link_token`]), and reverts the
/// thread to the process's own token after: NTFS checks the privilege enabled at the set, an administrator's token holds
/// it disabled, and `CreateSymbolicLinkW` enables it around its own call the same way. Only this thread, only for
/// `make`: the shard's other work and every other thread run as they did. Where the process does not hold it, or the
/// thread cannot take the copy, `make` runs as the process is and NTFS decides (Developer Mode).
fn as_link_maker<T>(make: impl FnOnce() -> T) -> T {
  let Some(token) = link_token() else {
    return make();
  };
  // SAFETY: `token` is an impersonation token this process made and keeps for its life ([`link_token`]); a null thread
  // handle names this thread; the call reads both and writes nothing of ours.
  let impersonating = unsafe {
    windows_sys::Win32::System::Threading::SetThreadToken(
      std::ptr::null(),
      token.as_raw_handle().cast(),
    )
  };
  if impersonating == 0 {
    return make();
  }
  let _back = Reverted;
  make()
}

/// The thread's own token again when dropped ([`as_link_maker`]), whatever `make` returned.
struct Reverted;

impl Drop for Reverted {
  fn drop(&mut self) {
    // SAFETY: no arguments; it ends this thread's impersonation, which `as_link_maker` began on this thread.
    let reverted = unsafe { windows_sys::Win32::Security::RevertToSelf() };
    if reverted == 0 {
      slates_machine::log_line!(
        "slates-land: a thread could not revert from the link-making token: {}",
        std::io::Error::last_os_error()
      );
    }
  }
}

/// The impersonation token a link's set runs under ([`as_link_maker`]): a copy of this process's token with
/// `SeCreateSymbolicLinkPrivilege` enabled, made once for the process; `None` where the process does not hold the
/// privilege (a standard account) or a step refused, logged once.
fn link_token() -> Option<BorrowedHandle<'static>> {
  static TOKEN: std::sync::OnceLock<Option<OwnedHandle>> = std::sync::OnceLock::new();
  TOKEN
    .get_or_init(|| {
      enabled_link_token()
        .map_err(|why| {
          slates_machine::log_line!("slates-land: links are set as the process is: {why}")
        })
        .ok()
    })
    .as_ref()
    .map(AsHandle::as_handle)
}

/// A copy of this process's token, for impersonation, with `SeCreateSymbolicLinkPrivilege` enabled in it; refused, in
/// words, where the process does not hold the privilege or a call refused.
fn enabled_link_token() -> Result<OwnedHandle, String> {
  use windows_sys::Win32::Foundation::{ERROR_NOT_ALL_ASSIGNED, LUID};
  use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
    SecurityImpersonation, TOKEN_ADJUST_PRIVILEGES, TOKEN_DUPLICATE, TOKEN_IMPERSONATE,
    TOKEN_PRIVILEGES, TOKEN_QUERY, TokenImpersonation,
  };
  use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
  let refused = |call: &str| format!("{call}: {}", std::io::Error::last_os_error());
  let mut process: HANDLE = std::ptr::null_mut();
  // SAFETY: the current process's pseudo-handle and a writable handle out-parameter; the handle the call returns on
  // success is owned at once, and nothing else owns it.
  let process = unsafe {
    (OpenProcessToken(
      GetCurrentProcess(),
      TOKEN_DUPLICATE | TOKEN_QUERY,
      &mut process,
    ) != 0)
      .then(|| OwnedHandle::from_raw_handle(process.cast()))
  }
  .ok_or_else(|| refused("OpenProcessToken"))?;
  let mut copy: HANDLE = std::ptr::null_mut();
  // SAFETY: `process` is open with `TOKEN_DUPLICATE`; no security attributes; `copy` is a writable out-parameter, and the
  // handle the call returns on success is owned at once, and nothing else owns it.
  let copy = unsafe {
    (DuplicateTokenEx(
      process.as_raw_handle().cast(),
      TOKEN_ADJUST_PRIVILEGES | TOKEN_IMPERSONATE | TOKEN_QUERY,
      std::ptr::null(),
      SecurityImpersonation,
      TokenImpersonation,
      &mut copy,
    ) != 0)
      .then(|| OwnedHandle::from_raw_handle(copy.cast()))
  }
  .ok_or_else(|| refused("DuplicateTokenEx"))?;
  let link = LUID {
    LowPart: u32::try_from(
      windows_sys::Wdk::System::SystemServices::SE_CREATE_SYMBOLIC_LINK_PRIVILEGE,
    )
    .map_err(|_| "the link privilege's number".to_owned())?,
    HighPart: 0,
  };
  let enabled = TOKEN_PRIVILEGES {
    PrivilegeCount: 1,
    Privileges: [LUID_AND_ATTRIBUTES {
      Luid: link,
      Attributes: SE_PRIVILEGE_ENABLED,
    }],
  };
  // SAFETY: `copy` is open with `TOKEN_ADJUST_PRIVILEGES`; `enabled` is a filled one-entry record; no previous state is
  // asked for.
  let adjusted = unsafe {
    AdjustTokenPrivileges(
      copy.as_raw_handle().cast(),
      0,
      &enabled,
      0,
      std::ptr::null_mut(),
      std::ptr::null_mut(),
    )
  };
  // The call succeeds with `ERROR_NOT_ALL_ASSIGNED` where the token does not hold the privilege at all.
  let code = std::io::Error::last_os_error()
    .raw_os_error()
    .and_then(|code| u32::try_from(code).ok());
  if adjusted == 0 {
    return Err(refused("AdjustTokenPrivileges"));
  }
  if code == Some(ERROR_NOT_ALL_ASSIGNED) {
    return Err("this process does not hold SeCreateSymbolicLinkPrivilege".to_owned());
  }
  Ok(copy)
}

/// Sets the attribute word `word` on the object `object` and leaves its times as they are (`FILE_BASIC_INFO` with
/// every time zero).
fn set_attributes(object: BorrowedHandle<'_>, word: u32) -> Result<(), HostError> {
  set_basic(
    object,
    &FILE_BASIC_INFO {
      CreationTime: 0,
      LastAccessTime: 0,
      LastWriteTime: 0,
      ChangeTime: 0,
      FileAttributes: word,
    },
  )
}

/// Gives the object `object`, made as `made`, the Windows bits the volume keeps for it (`volume`, A-451:
/// `crate::windows_bits::set_word`), then reads its attributes back: a file system that holds fewer bits than NTFS
/// drops the others without a word (FAT, `crate::windows_bits`), so a bit that did not take refuses the entry
/// `EOPNOTSUPP` rather than land without it (`crate::windows_bits::held`).
fn give_bits(
  object: BorrowedHandle<'_>,
  volume: u16,
  made: crate::windows_bits::Made,
) -> Result<(), HostError> {
  let current = attributes(object)?;
  set_attributes(object, crate::windows_bits::set_word(current, volume, made))?;
  if crate::windows_bits::held(attributes(object)?, volume, made) {
    Ok(())
  } else {
    Err(unsupported(NOT_SUPPORTED))
  }
}

/// Writes `info` as the file's basic information: a zero member is left as it is.
fn set_basic(file: BorrowedHandle<'_>, info: &FILE_BASIC_INFO) -> Result<(), HostError> {
  let size = u32::try_from(size_of::<FILE_BASIC_INFO>()).unwrap_or(0);
  // SAFETY: `file` is open with `FILE_WRITE_ATTRIBUTES`; `info` is a filled record of the class named, of the size
  // passed.
  let ok = unsafe {
    SetFileInformationByHandle(
      file.as_raw_handle().cast(),
      FileBasicInfo,
      std::ptr::from_ref(info).cast(),
      size,
    )
  };
  if ok == 0 {
    return Err(last_refusal());
  }
  Ok(())
}

/// Makes everything written through `handle` durable (`FlushFileBuffers`).
fn flush(handle: BorrowedHandle<'_>) -> Result<(), HostError> {
  // SAFETY: `handle` is open for writing for the call's duration.
  if unsafe { FlushFileBuffers(handle.as_raw_handle().cast()) } == 0 {
    return Err(last_refusal());
  }
  Ok(())
}

/// A directory's entries made durable: the directory itself opened again with write access (an empty name relative
/// to the handle the host retains read-only, which `NtCreateFile` takes as the directory itself), flushed.
fn flush_dir(dir: BorrowedHandle<'_>) -> Result<(), HostError> {
  let object_name = UNICODE_STRING {
    Length: 0,
    MaximumLength: 0,
    Buffer: std::ptr::null_mut(),
  };
  let object = OBJECT_ATTRIBUTES {
    Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>()).unwrap_or(u32::MAX),
    RootDirectory: dir.as_raw_handle().cast(),
    ObjectName: &object_name,
    Attributes: OBJ_CASE_INSENSITIVE,
    ..Default::default()
  };
  let mut handle: HANDLE = std::ptr::null_mut();
  let mut status_block = windows_sys::Win32::System::IO::IO_STATUS_BLOCK::default();
  // SAFETY: `object` names the live directory handle and an empty name, which opens that directory; `handle` and
  // `status_block` are writable records of the types the call fills.
  let status = unsafe {
    nt::NtCreateFile(
      &mut handle,
      DIR_FLUSH_ACCESS,
      &object,
      &mut status_block,
      std::ptr::null(),
      0,
      SHARE_ALL,
      nt::FILE_OPEN,
      nt::FILE_DIRECTORY_FILE | nt::FILE_SYNCHRONOUS_IO_NONALERT,
      std::ptr::null(),
      0,
    )
  };
  if status < 0 {
    return Err(status_refusal(status));
  }
  // SAFETY: the handle `NtCreateFile` just returned on success; nothing else owns it.
  let reopened = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };
  flush(reopened.as_handle())
}

/// A Unix time in nanoseconds as a FILETIME's 100 ns intervals since 1601.
fn ticks_of_unix_ns(ns: i64) -> i64 {
  ns.div_euclid(HUNDRED_NS)
    .saturating_add(FILETIME_UNIX_EPOCH_TICKS)
}

/// Format: the bytes of the largest record `GetTokenInformation` fills here, `TOKEN_USER` and its SID (a `TOKEN_OWNER`
/// is smaller), rounded up to whole words so the buffer is aligned for either: the SID is at most
/// `SECURITY_MAX_SID_SIZE` bytes (winnt.h).
const TOKEN_SID_WORDS: usize = (size_of::<windows_sys::Win32::Security::TOKEN_USER>()
  + windows_sys::Win32::Security::SECURITY_MAX_SID_SIZE as usize)
  .div_ceil(size_of::<u64>());

/// Whether the directory `target` is owned by this process's user, by security identifier: its owner is the token's
/// user, or the token's default owner (`TokenOwner`), the SID Windows makes the owner of every object this process
/// creates. An elevated administrator's default owner is the Administrators group, so a directory it made is owned by
/// that group, never by the user; checking the user alone refused every target such a process made itself.
fn owned_by_this_user(target: BorrowedHandle<'_>) -> Result<bool, HostError> {
  use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
  use windows_sys::Win32::Security::{
    EqualSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY,
  };
  use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
  let mut owner: PSID = std::ptr::null_mut();
  let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
  // SAFETY: `target` is open with `READ_CONTROL`; the out-pointers are locals; the descriptor the call allocates is
  // freed below, and `owner` points into it.
  let asked = unsafe {
    GetSecurityInfo(
      target.as_raw_handle().cast(),
      SE_FILE_OBJECT,
      OWNER_SECURITY_INFORMATION,
      &mut owner,
      std::ptr::null_mut(),
      std::ptr::null_mut(),
      std::ptr::null_mut(),
      &mut descriptor,
    )
  };
  if asked != 0 {
    return Err(code_refusal(asked));
  }
  let mut token: HANDLE = std::ptr::null_mut();
  // SAFETY: this process's pseudo-handle and an out-pointer to a local; the token is owned below.
  let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
  let owned = if opened == 0 {
    Err(last_refusal())
  } else {
    // SAFETY: the token just opened, owned here alone and closed on drop.
    let token = unsafe { OwnedHandle::from_raw_handle(token.cast()) };
    [TokenIdentity::User, TokenIdentity::DefaultOwner]
      .into_iter()
      .try_fold(false, |found, identity| {
        if found {
          return Ok(true);
        }
        let mut buffer = [0u64; TOKEN_SID_WORDS];
        let sid = token_sid(token.as_handle(), identity, &mut buffer)?;
        // SAFETY: `sid` points into `buffer`, which the token's record filled and which outlives this comparison;
        // `owner` points into the descriptor, alive until it is freed below.
        Ok(unsafe { EqualSid(sid, owner) } != 0)
      })
  };
  // SAFETY: the descriptor `GetSecurityInfo` allocated, freed once; nothing reads `owner` after.
  unsafe { windows_sys::Win32::Foundation::LocalFree(descriptor) };
  owned
}

/// Which of a token's identities a target's owner is compared with.
#[derive(Clone, Copy)]
enum TokenIdentity {
  /// `TokenUser`: the user the process runs as.
  User,
  /// `TokenOwner`: the SID the process's new objects are owned by.
  DefaultOwner,
}

/// The SID of the token's `identity`, read into `buffer`, which holds the record and the SID it points to.
fn token_sid(
  token: BorrowedHandle<'_>,
  identity: TokenIdentity,
  buffer: &mut [u64; TOKEN_SID_WORDS],
) -> Result<windows_sys::Win32::Security::PSID, HostError> {
  use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_OWNER, TOKEN_USER, TokenOwner, TokenUser,
  };
  let class = match identity {
    TokenIdentity::User => TokenUser,
    TokenIdentity::DefaultOwner => TokenOwner,
  };
  let mut needed = 0u32;
  let size = u32::try_from(size_of_val(buffer)).unwrap_or(0);
  // SAFETY: the token is open with `TOKEN_QUERY`; `buffer` is writable for `size` bytes and aligned for either record;
  // `needed` is a local.
  let got = unsafe {
    GetTokenInformation(
      token.as_raw_handle().cast(),
      class,
      buffer.as_mut_ptr().cast(),
      size,
      &mut needed,
    )
  };
  if got == 0 {
    return Err(last_refusal());
  }
  // SAFETY: the call filled the class's record at the buffer's start; each holds the SID's address, inside the buffer.
  Ok(unsafe {
    match identity {
      TokenIdentity::User => (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid,
      TokenIdentity::DefaultOwner => (*buffer.as_ptr().cast::<TOKEN_OWNER>()).Owner,
    }
  })
}

impl OsLand {
  /// Opens an absolute target path with containment and the ownership check, and returns the writer with the target.
  pub fn open_target(path: &Path) -> Result<(Self, LandingTarget), TargetRefusal> {
    let current = walk_target(path)?;
    let root = Path::new(path.ancestors().last().unwrap_or(path));
    let (host, _) = OsHost::open_root(root).map_err(TargetRefusal::Unavailable)?;
    Self::adopt_target(host, current, key_of(path)).map_err(|(_, refusal)| refusal)
  }

  /// [`OsLand::open_target`] inside an existing host — the base host an overlay volume's slot holds — so the target
  /// and every handle the volume's base names live in one host. A refusal hands the host back.
  pub fn open_target_in(
    host: OsHost,
    path: &Path,
  ) -> Result<(Self, LandingTarget), (OsHost, TargetRefusal)> {
    match walk_target(path) {
      Ok(current) => Self::adopt_target(host, current, key_of(path)),
      Err(refusal) => Err((host, refusal)),
    }
  }

  /// The writer over `host`, with the walked target `current` adopted into it under `key`.
  fn adopt_target(
    mut host: OsHost,
    current: OwnedHandle,
    key: String,
  ) -> Result<(Self, LandingTarget), (OsHost, TargetRefusal)> {
    let dir = match host.adopt_dir(current) {
      Ok(dir) => dir,
      Err(error) => return Err((host, TargetRefusal::Unavailable(error))),
    };
    Ok((
      Self {
        host,
        temps: BTreeMap::new(),
        through: BTreeSet::new(),
      },
      LandingTarget {
        dir,
        key: key.into(),
      },
    ))
  }

  /// The base host beneath, for the volume's read verbs.
  pub fn host(&mut self) -> &mut OsHost {
    &mut self.host
  }

  /// The base host beneath, kept after the landing (see the Unix seam's `into_host`).
  pub fn into_host(self) -> OsHost {
    self.host
  }

  /// Lends the host back between a sliced landing's slices (AUD-29-25); [`OsLand::resume`] takes it back.
  pub fn lend(self) -> (OsHost, Away) {
    (
      self.host,
      Away {
        temps: self.temps,
        through: self.through,
      },
    )
  }

  /// The writer again, over the host it lent and its own state.
  pub fn resume(host: OsHost, away: Away) -> Self {
    Self {
      host,
      temps: away.temps,
      through: away.through,
    }
  }

  fn dir(&self, dir: HostDir) -> Result<BorrowedHandle<'_>, HostError> {
    self.host.dir_handle(dir)
  }

  fn file(&self, file: HostFile) -> Result<BorrowedHandle<'_>, HostError> {
    self.host.file_of(file).map(AsHandle::as_handle)
  }
}

/// The key a target lands under: its path as given, separators as `/`, which a lease and a grant name it by.
fn key_of(path: &Path) -> String {
  path.to_string_lossy().replace('\\', "/")
}

/// Walks the target and checks its owner.
fn walk_target(path: &Path) -> Result<OwnedHandle, TargetRefusal> {
  let current = walk_dir(path).map_err(|refusal| match refusal {
    WalkRefusal::NotAbsolute => TargetRefusal::NotAbsolute,
    WalkRefusal::Escapes => TargetRefusal::EscapesTarget,
    WalkRefusal::OwnMount => TargetRefusal::OwnMount,
    WalkRefusal::Host(error) => TargetRefusal::Unavailable(error),
  })?;
  if !owned_by_this_user(current.as_handle()).map_err(TargetRefusal::Unavailable)? {
    return Err(TargetRefusal::TargetNotOwned);
  }
  Ok(current)
}

/// A file's write-out: nothing reaches the device here. Windows' only call that starts a file's writeback also waits
/// for it (`FlushFileBuffers`), so it belongs to the batch's barrier, off the shard (A-132).
fn write_out_file(_file: BorrowedHandle<'_>) -> Result<(), HostError> {
  Ok(())
}

/// A batch's barrier away from its host (A-132): its own handle of each file, flushed on ⌈√n⌉ streams.
struct OsBarrier {
  files: Vec<OwnedHandle>,
}

impl Barrier for OsBarrier {
  fn run(self: Box<Self>) -> Result<(), HostError> {
    let handles: Vec<BorrowedHandle<'_>> = self.files.iter().map(AsHandle::as_handle).collect();
    let root = handles.len().isqrt();
    let square = root.checked_mul(root) == Some(handles.len());
    let streams = root
      .saturating_add(usize::from(!square))
      .clamp(1, handles.len().max(1));
    std::thread::scope(|scope| {
      let mut started = Vec::with_capacity(streams.saturating_sub(1));
      let mut refused = Vec::new();
      for stream in 1..streams {
        let handles = &handles;
        match std::thread::Builder::new()
          .spawn_scoped(scope, move || flush_stream(handles, stream, streams))
        {
          Ok(thread) => started.push(thread),
          Err(_) => refused.push(stream),
        }
      }
      let mut flushed = flush_stream(&handles, 0, streams);
      for stream in refused {
        flushed = flushed.and(flush_stream(&handles, stream, streams));
      }
      // A thread gone without its outcome (it cannot be: shipped code does not panic, and a panic aborts) reads as a
      // failed I/O, Windows' `ERROR_IO_DEVICE`, as the Unix seam reads it `EIO`.
      for thread in started {
        flushed = flushed.and(thread.join().unwrap_or(Err(code_refusal(ERROR_IO_DEVICE))));
      }
      flushed
    })
  }
}

/// One stream's share of a batch: files `stream`, `stream + streams`, and so on, each flushed; the first refusal.
fn flush_stream(
  handles: &[BorrowedHandle<'_>],
  stream: usize,
  streams: usize,
) -> Result<(), HostError> {
  let mut flushed = Ok(());
  for handle in handles.iter().skip(stream).step_by(streams.max(1)) {
    flushed = flushed.and(flush(*handle));
  }
  flushed
}

impl HostFs for OsLand {
  fn facts(&mut self, dir: HostDir) -> Result<HostFacts, HostError> {
    self.host.facts(dir)
  }

  fn now_ns(&mut self) -> i64 {
    self.host.now_ns()
  }

  fn fingerprint_dir(&mut self, dir: HostDir) -> Result<Fingerprint, HostError> {
    self.host.fingerprint_dir(dir)
  }

  fn list(&mut self, dir: HostDir) -> Result<Vec<BaseEntry>, HostError> {
    self.host.list(dir)
  }

  fn open_dir(&mut self, parent: HostDir, name: &str) -> Result<HostDir, HostError> {
    self.host.open_dir(parent, name)
  }

  fn open_file(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    self.host.open_file(dir, name)
  }

  fn fstat(&mut self, file: HostFile) -> Result<Fingerprint, HostError> {
    self.host.fstat(file)
  }

  fn read_at(&mut self, file: HostFile, off: u64, buf: &mut [u8]) -> Result<usize, HostError> {
    self.host.read_at(file, off, buf)
  }

  fn read_link(&mut self, dir: HostDir, name: &str) -> Result<Box<str>, HostError> {
    self.host.read_link(dir, name)
  }

  fn close_file(&mut self, file: HostFile) {
    self.temps.remove(&file.0);
    self.through.remove(&file.0);
    self.host.close_file(file);
  }

  fn close_dir(&mut self, dir: HostDir) {
    self.host.close_dir(dir);
  }

  fn watch(&mut self, dir: HostDir) -> WatchState {
    self.host.watch(dir)
  }

  fn hints(&mut self) -> Vec<Hint> {
    self.host.hints()
  }
}

impl LandFs for OsLand {
  /// NTFS has no exchange and no unnamed temporary; reflinks where the target's volume shares clusters between files
  /// (`shares_clusters`: ReFS, a Dev Drive).
  fn capabilities(&mut self, dir: HostDir) -> Result<LandCapabilities, HostError> {
    let reflink = shares_clusters(self.dir(dir)?)?;
    Ok(LandCapabilities {
      exchange: false,
      reflink,
      unnamed_temporaries: false,
    })
  }

  /// A new temporary at `name` holding the first `len` bytes of the temporary `from`, shared by ReFS's block cloning
  /// (the module's "Reflinks"): its end set to the length rounded up to a whole cluster of the volume
  /// (`crate::block_clone`), each region cloned, the end set to `len`. A refusal to share this pair answers
  /// `EOPNOTSUPP` (`clone_refusal`) and whatever refused, the new file is removed: nothing is left behind.
  fn clone_temp(
    &mut self,
    dir: HostDir,
    name: &str,
    from: HostFile,
    len: u64,
  ) -> Result<HostFile, HostError> {
    let parent = self.dir(dir)?;
    let cluster = cluster_bytes(parent)?;
    let unshareable = |_| unsupported(NOT_SUPPORTED);
    let end = crate::block_clone::clone_end(len, cluster).map_err(unshareable)?;
    let regions = crate::block_clone::clone_regions(len, cluster).map_err(unshareable)?;
    let source = self.file(from)?;
    let made = nt_create(
      parent,
      name,
      TEMP_ACCESS,
      nt::FILE_CREATE,
      nt::FILE_NON_DIRECTORY_FILE,
      FILE_ATTRIBUTE_NORMAL,
    )?;
    let shared = set_end(made.as_handle(), end)
      .and_then(|()| {
        regions.iter().try_for_each(|&(offset, length)| {
          duplicate_extents(source, made.as_handle(), offset, length)
        })
      })
      .and_then(|()| set_end(made.as_handle(), len));
    if let Err(refused) = shared {
      if let Err(left) = dispose(made.as_handle()) {
        slates_machine::log_line!(
          "slates-land: a refused clone's temporary {name} stays for the sweep: {left:?}"
        );
      }
      return Err(refused);
    }
    let file = self.host.adopt_file(made)?;
    self.temps.insert(
      file.0,
      Temp {
        dir,
        name: name.into(),
      },
    );
    Ok(file)
  }

  fn create_temp(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    let made = nt_create(
      self.dir(dir)?,
      name,
      TEMP_ACCESS,
      nt::FILE_CREATE,
      nt::FILE_NON_DIRECTORY_FILE,
      FILE_ATTRIBUTE_NORMAL,
    )?;
    let file = self.host.adopt_file(made)?;
    self.temps.insert(
      file.0,
      Temp {
        dir,
        name: name.into(),
      },
    );
    Ok(file)
  }

  /// Writes through the temporary's own file (`seek_write`, `WriteFile` at the offset) until every byte is written; a
  /// write that takes none is no room (`ENOSPC`).
  fn write_at(&mut self, file: HostFile, off: u64, bytes: &[u8]) -> Result<(), HostError> {
    use std::os::windows::fs::FileExt;
    let writer = self.host.file_of(file)?;
    let mut written = 0usize;
    while written < bytes.len() {
      let at = off.saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
      let n = writer
        .seek_write(bytes.get(written..).unwrap_or_default(), at)
        .map_err(|error| io_refusal(&error))?;
      if n == 0 {
        return Err(HostError::NoSpace(OsCode::Win32(DISK_FULL)));
      }
      written = written.saturating_add(n);
    }
    Ok(())
  }

  fn write_out(&mut self, file: HostFile) -> Result<(), HostError> {
    write_out_file(self.file(file)?)
  }

  /// The batch's barrier over its own handle of each file but a read-only file's metadata entry, which its write-through
  /// handle stored as it was set and which may not be flushed (A-519).
  fn begin_barrier(&mut self, files: &[HostFile]) -> Result<Box<dyn Barrier>, HostError> {
    let files = files
      .iter()
      .filter(|file| !self.through.contains(&file.0))
      .map(|file| {
        self
          .file(*file)?
          .try_clone_to_owned()
          .map_err(|error| io_refusal(&error))
      })
      .collect::<Result<Vec<_>, _>>()?;
    Ok(Box::new(OsBarrier { files }))
  }

  fn end_barrier(
    &mut self,
    _files: &[HostFile],
    ran: Result<(), HostError>,
  ) -> Result<(), HostError> {
    ran
  }

  /// The file opened to have its attributes set (A-458) and its batch's barrier flush it (A-472), relative to its
  /// directory's handle, as a file and never through a link (`crate::windows_bits::meta_open`): a reparse point is
  /// refused `NotFile`, closed again. NTFS refuses a read-only file the handle that may write (MS-FSA 2.1.5.1.2.1), so
  /// one is opened again to write its attributes alone, write-through: each set through it is stored as it returns, and
  /// its barrier leaves it out (A-519). Any other refusal of the first open stands.
  fn open_to_chmod(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    let parent = self.dir(dir)?;
    let flushed = crate::windows_bits::meta_open(false);
    let (opened, through) = match nt_create(
      parent,
      name,
      flushed.access,
      nt::FILE_OPEN,
      flushed.options,
      0,
    ) {
      Ok(opened) => (opened, false),
      Err(refused) => {
        let stored = crate::windows_bits::meta_open(true);
        let Ok(opened) = nt_create(
          parent,
          name,
          stored.access,
          nt::FILE_OPEN,
          stored.options,
          0,
        ) else {
          return Err(refused);
        };
        match attributes(opened.as_handle()) {
          Ok(held) if crate::windows_bits::read_only(held) => (opened, true),
          _ => return Err(refused),
        }
      }
    };
    if attributes(opened.as_handle())?
      & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
      != 0
    {
      return Err(HostError::NotFile);
    }
    let file = self.host.adopt_file(opened)?;
    if through {
      self.through.insert(file.0);
    }
    Ok(file)
  }

  /// The directory opened to have its attributes set and its batch's barrier flush it (A-501): `CHMOD_ACCESS`, whose
  /// `FILE_WRITE_DATA` is a directory's `FILE_ADD_FILE`, the access `NtFlushBuffersFile` asks of a directory too; as a
  /// directory (`FILE_DIRECTORY_FILE`) and never through a link: opened with `FILE_OPEN_REPARSE_POINT`, a reparse point is
  /// refused `NotDirectory`, closed again.
  fn open_dir_to_chmod(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    let opened = nt_create(
      self.dir(dir)?,
      name,
      CHMOD_ACCESS,
      nt::FILE_OPEN,
      nt::FILE_DIRECTORY_FILE | nt::FILE_OPEN_REPARSE_POINT,
      0,
    )?;
    if attributes(opened.as_handle())?
      & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
      != 0
    {
      return Err(HostError::NotDirectory);
    }
    self.host.adopt_file(opened)
  }

  /// The one bit a mode decides on Windows, `READONLY`, with every other bit the temporary holds kept
  /// (`crate::windows_bits::mode_word`). A directory's read-only is among its Windows bits (A-451), and Windows keeps no
  /// POSIX mode, so a directory's mode decides nothing here (A-501), as at its `mkdir`.
  fn set_mode(&mut self, file: HostFile, mode: u32) -> Result<(), HostError> {
    let handle = self.file(file)?;
    let held = attributes(handle)?;
    if held & FILE_ATTRIBUTE_DIRECTORY != 0 {
      return Ok(());
    }
    set_attributes(handle, crate::windows_bits::mode_word(held, mode))
  }

  /// The volume's Windows bits on the temporary (A-451, `give_bits`): every bit the volume keeps, set or cleared and
  /// read back, the read-only state its mode gave it kept. The rename that places it sets `ARCHIVE` again (NTFS's own
  /// rule for a renamed file, measured by A-185), so a landed file is `ARCHIVE` whatever the volume held: the truth that
  /// it changed, which a backup reads. A base directory's metadata entry (A-501) gives a directory's bits, its read-only
  /// among them.
  fn set_windows(&mut self, file: HostFile, windows: u16) -> Result<(), HostError> {
    let handle = self.file(file)?;
    let made = if attributes(handle)? & FILE_ATTRIBUTE_DIRECTORY != 0 {
      crate::windows_bits::Made::Dir
    } else {
      crate::windows_bits::Made::File
    };
    give_bits(handle, windows, made)
  }

  fn set_mtime(&mut self, file: HostFile, mtime_ns: i64) -> Result<(), HostError> {
    set_basic(
      self.file(file)?,
      &FILE_BASIC_INFO {
        CreationTime: 0,
        LastAccessTime: 0,
        LastWriteTime: ticks_of_unix_ns(mtime_ns),
        ChangeTime: 0,
        FileAttributes: 0,
      },
    )
  }

  fn place(&mut self, file: HostFile, dir: HostDir, name: &str) -> Result<(), HostError> {
    let temp = self
      .temps
      .get(&file.0)
      .cloned()
      .ok_or(HostError::StaleHandle)?;
    if temp.dir == dir && temp.name.as_ref() == name {
      return Ok(());
    }
    rename_handle(self.file(file)?, self.dir(dir)?, name)?;
    self.temps.insert(
      file.0,
      Temp {
        dir,
        name: name.into(),
      },
    );
    Ok(())
  }

  fn exchange(&mut self, dir: HostDir, _a: &str, _b: &str) -> Result<(), HostError> {
    self.dir(dir)?;
    Err(unsupported(NOT_SUPPORTED))
  }

  fn rename_noreplace(
    &mut self,
    dir: HostDir,
    from: &str,
    to_dir: HostDir,
    to: &str,
  ) -> Result<(), HostError> {
    let entry = open_entry(self.dir(dir)?, from)?;
    rename_handle(entry.as_handle(), self.dir(to_dir)?, to)
  }

  fn entry_fingerprint(&mut self, dir: HostDir, name: &str) -> Result<Fingerprint, HostError> {
    self.host.entry_fingerprint(dir, name)
  }

  fn unlink(&mut self, dir: HostDir, name: &str) -> Result<(), HostError> {
    let entry = open_entry(self.dir(dir)?, name)?;
    let attributes = attributes(entry.as_handle())?;
    let link =
      attributes & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT != 0;
    if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 && !link {
      return Err(HostError::NotFile);
    }
    dispose(entry.as_handle())
  }

  /// A directory made with `FILE_CREATE` and given the volume's Windows bits (A-451, `give_bits`: its read-only,
  /// hidden, system, archive, offline and not-indexed bits); Windows keeps no POSIX mode, so `mode` decides nothing here.
  /// Where its bits are refused, the empty directory is removed, so nothing of the entry is left. The bits are given
  /// after the directory is made, as a Unix directory's mode is (`fchmod` after `mkdirat`), so a crash between the two
  /// leaves it without them, and a resumed landing takes it as already there (GAPS, A-451).
  fn mkdir(&mut self, dir: HostDir, name: &str, _mode: u32, windows: u16) -> Result<(), HostError> {
    let made = nt_create(
      self.dir(dir)?,
      name,
      MADE_DIR_ACCESS | FILE_WRITE_ATTRIBUTES | DELETE,
      nt::FILE_CREATE,
      nt::FILE_DIRECTORY_FILE,
      FILE_ATTRIBUTE_NORMAL,
    )?;
    let given = give_bits(made.as_handle(), windows, crate::windows_bits::Made::Dir);
    if let Err(refused) = given {
      if let Err(left) = dispose(made.as_handle()) {
        slates_machine::log_line!(
          "slates-land: a directory whose attributes were refused stays at {name}: {left:?}"
        );
      }
      return Err(refused);
    }
    Ok(())
  }

  fn rmdir(&mut self, dir: HostDir, name: &str) -> Result<(), HostError> {
    let entry = open_entry(self.dir(dir)?, name)?;
    if attributes(entry.as_handle())? & FILE_ATTRIBUTE_DIRECTORY == 0 {
      return Err(HostError::NotDirectory);
    }
    dispose(entry.as_handle())
  }

  /// The volume's link to `target` as a Windows symbolic link in A-275's form (the module's "Symbolic links"): made at
  /// `staged` (a directory object for a directory link, A-451), its buffer set with the link privilege where the process
  /// holds it (`as_link_maker`), given the volume's bits (`give_bits`), then moved to `name` without replacing;
  /// whatever refuses a step, the staged object is removed, and a crash between them leaves it at the landing's hidden
  /// name for its sweep.
  fn symlink(
    &mut self,
    dir: HostDir,
    name: &str,
    target: &str,
    staged: &str,
    windows: u16,
  ) -> Result<(), HostError> {
    let buffer = slates_vfs::host::windows_link::reparse_data(target)
      .map_err(|_| code_refusal(ERROR_FILENAME_EXCED_RANGE))?;
    entry_units(name)?;
    let parent = self.dir(dir)?;
    // A directory link is a directory object (A-275's `LINK_DIRECTORY`, A-451), a file link a file.
    let kind = if crate::windows_bits::directory_link(windows) {
      nt::FILE_DIRECTORY_FILE
    } else {
      nt::FILE_NON_DIRECTORY_FILE
    };
    let object = nt_create(
      parent,
      staged,
      LINK_ACCESS,
      nt::FILE_CREATE,
      kind | nt::FILE_OPEN_REPARSE_POINT,
      FILE_ATTRIBUTE_NORMAL,
    )?;
    let placed = as_link_maker(|| set_reparse(object.as_handle(), &buffer))
      .and_then(|()| give_bits(object.as_handle(), windows, crate::windows_bits::Made::Link))
      .and_then(|()| {
        if staged == name {
          Ok(())
        } else {
          rename_handle(object.as_handle(), parent, name)
        }
      });
    if let Err(refused) = placed {
      // The step's own refusal is what the landing reports; a removal refused too leaves the object at the hidden name,
      // where this landing's sweep removes it (`engine::sweep_parent`).
      if let Err(left) = dispose(object.as_handle()) {
        slates_machine::log_line!(
          "slates-land: a refused link's staged object {staged} stays for the sweep: {left:?}"
        );
      }
      return Err(refused);
    }
    Ok(())
  }

  fn sync_dir(&mut self, dir: HostDir) -> Result<(), HostError> {
    flush_dir(self.dir(dir)?)
  }

  fn sync_media(&mut self, dir: HostDir) -> Result<(), HostError> {
    flush_dir(self.dir(dir)?)
  }
}
