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
//! - **Mode and times:** Windows keeps no POSIX mode. [`LandFs::set_mode`] records the one bit it can: a mode with no
//!   write bit makes the file read-only (`FILE_ATTRIBUTE_READONLY`). The modification time is set through
//!   `FILE_BASIC_INFO` in the FILETIME domain.
//! - **Symbolic links:** refused (`ERROR_PRIVILEGE_NOT_HELD`'s errno, `EPERM`). Windows makes one only through
//!   `CreateSymbolicLinkW`, which takes a path, or a reparse point the caller sets, which needs a privilege without
//!   developer mode; slates writes through handles only and requires no privilege (R10), so a landing holding a
//!   symbolic link fails that entry, typed.
//!
//! Refusals the engine tells apart are reported in its errno numbers (it was written over POSIX): a name that exists
//! as `EEXIST` (17), no room as `ENOSPC` (28), the missing exchange as `EOPNOTSUPP` (95).
//!
//! Containment (§4.15 step 4, §4.13): [`OsLand::open_target`] walks the target from its drive's root
//! (`slates_base::walk_dir`), and the target must be this process's user's (`TargetNotOwned`): its owner, by security
//! identifier, is the token's user or the token's default owner ([`owned_by_this_user`]).

use std::collections::BTreeMap;
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
  ERROR_ALREADY_EXISTS, ERROR_DISK_FULL, ERROR_FILE_EXISTS, ERROR_HANDLE_DISK_FULL, GetLastError,
  HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, STATUS_DELETE_PENDING,
  STATUS_FILE_IS_A_DIRECTORY, STATUS_NO_SUCH_FILE, STATUS_NOT_A_DIRECTORY,
  STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_INVALID, STATUS_OBJECT_NAME_NOT_FOUND,
  STATUS_OBJECT_PATH_NOT_FOUND, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
  DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY,
  FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO, FILE_GENERIC_WRITE, FILE_LIST_DIRECTORY,
  FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE,
  FileAttributeTagInfo, FileBasicInfo, FileDispositionInfoEx, FlushFileBuffers,
  GetFileInformationByHandleEx, SYNCHRONIZE, SetFileInformationByHandle,
};

use crate::engine::LandingTarget;

/// Format: the errno numbers the engine tells refusals apart by (it was written over POSIX; `engine.rs`).
const ERRNO_EXIST: i32 = 17;
/// Format: `ENOSPC`: no room.
const ERRNO_NOSPC: i32 = 28;
/// Format: `EOPNOTSUPP`: the verb does not exist here.
const ERRNO_NOTSUP: i32 = 95;
/// Format: `EPERM`: the verb would need a privilege slates never asks for (R10).
const ERRNO_PERM: i32 = 1;
/// Format: every share mode, so a handle the writer holds never blocks a user's read, write, rename or delete.
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
/// Format: a temporary's access: written, its attributes and times set, moved and flushed (`DELETE` to rename it).
const TEMP_ACCESS: u32 = FILE_GENERIC_WRITE | FILE_READ_ATTRIBUTES | DELETE | SYNCHRONIZE;
/// Format: an entry opened to be moved or removed: nothing of its data, its attributes, and `DELETE`.
const MOVE_ACCESS: u32 = DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
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
/// Format: the POSIX write bits of a mode; a file with none of them is made read-only.
const WRITE_BITS: u32 = 0o222;
/// Format: the FILETIME epoch (1601) precedes the Unix epoch by this many 100 ns intervals.
const FILETIME_UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;
/// Format: nanoseconds in a FILETIME interval.
const HUNDRED_NS: i64 = 100;
/// Format: the access a directory's flush is made through: adding entries (`FILE_ADD_FILE`, the directory's
/// `FILE_WRITE_DATA`) and waiting synchronously; `FlushFileBuffers` needs a handle that may write.
const DIR_FLUSH_ACCESS: u32 = windows_sys::Win32::Storage::FileSystem::FILE_ADD_FILE | SYNCHRONIZE;

/// Format: 64-bit words of a buffer for a token's `TOKEN_USER` or `TOKEN_OWNER` and its SID, aligned for the record: far
/// past both, since a SID is at most 68 bytes (`SECURITY_MAX_SID_SIZE`, winnt.h) and the record two pointers.
const TOKEN_SID_WORDS: usize = 64;

/// Why a target path was refused before the lease (§4.15 step 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetRefusal {
  /// The path is not absolute.
  NotAbsolute,
  /// A component is a link, a junction, a mount point or `..`: the path would escape the directory the human named.
  EscapesTarget,
  /// The target is owned by another user.
  TargetNotOwned,
  /// The target's drive is a mount this daemon serves (§4.6: no daemon thread opens a path on a volume the daemon
  /// serves; A-227): refused, so the landing wrote nothing.
  OwnMount,
  /// The host refused.
  Unavailable(HostError),
}

/// A host's refusal of the target as the writer's: a mount this daemon serves is [`TargetRefusal::OwnMount`].
fn host_refusal(error: HostError) -> TargetRefusal {
  match error {
    HostError::OwnMount(_) => TargetRefusal::OwnMount,
    other => TargetRefusal::Unavailable(other),
  }
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
}

/// A writer's own state while it has lent its host away ([`OsLand::lend`]).
#[derive(Debug)]
pub struct Away {
  temps: BTreeMap<u64, Temp>,
}

impl std::fmt::Debug for OsLand {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("OsLand")
      .field("host", &self.host)
      .field("temps", &self.temps.len())
      .finish()
  }
}

/// A Win32 error code as the seam's refusal, in the engine's errno numbers where it tells them apart.
fn code_refusal(code: u32) -> HostError {
  match code {
    ERROR_ALREADY_EXISTS | ERROR_FILE_EXISTS => HostError::Unavailable(ERRNO_EXIST),
    ERROR_DISK_FULL | ERROR_HANDLE_DISK_FULL => HostError::Unavailable(ERRNO_NOSPC),
    other => HostError::Unavailable(i32::try_from(other).unwrap_or(i32::MAX)),
  }
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
    STATUS_OBJECT_NAME_COLLISION => HostError::Unavailable(ERRNO_EXIST),
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

/// Reads the token's `class` record (one holding a SID) into `buffer`, aligned for it.
fn token_information(
  token: BorrowedHandle<'_>,
  class: windows_sys::Win32::Security::TOKEN_INFORMATION_CLASS,
  buffer: &mut [u64; TOKEN_SID_WORDS],
) -> Result<(), HostError> {
  let mut needed = 0u32;
  let size = u32::try_from(size_of_val(buffer)).unwrap_or(0);
  // SAFETY: the token is open with `TOKEN_QUERY`; `buffer` is writable for `size` bytes and aligned for the record;
  // `needed` is a local.
  let got = unsafe {
    windows_sys::Win32::Security::GetTokenInformation(
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
  Ok(())
}

/// Whether the directory `target` is this process's user's, by security identifier: its owner is the token's user, or
/// the token's default owner, the owner this process's own creations get. The second is what Unix's `st_uid == geteuid`
/// asks, and they differ on an elevated token, whose default owner is `BUILTIN\Administrators` (CI's runners, an elevated
/// shell; measured 2026-11-06). A process without that group's rights has a default owner of its user, so it still
/// refuses a directory the administrators own.
fn owned_by_this_user(target: BorrowedHandle<'_>) -> Result<bool, HostError> {
  use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
  use windows_sys::Win32::Security::{
    EqualSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_OWNER, TOKEN_QUERY,
    TOKEN_USER, TokenOwner, TokenUser,
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
  let user = if opened == 0 {
    Err(last_refusal())
  } else {
    // SAFETY: the token just opened, owned here alone and closed on drop.
    let token = unsafe { OwnedHandle::from_raw_handle(token.cast()) };
    let mut user = [0u64; TOKEN_SID_WORDS];
    let mut default_owner = [0u64; TOKEN_SID_WORDS];
    token_information(token.as_handle(), TokenUser, &mut user)
      .and_then(|()| token_information(token.as_handle(), TokenOwner, &mut default_owner))
      .map(|()| {
        // SAFETY: the call filled a `TOKEN_USER` at the buffer's start, whose SID points inside the buffer, which
        // outlives this comparison; `owner` points into the descriptor, alive until it is freed below.
        let is_user = unsafe { EqualSid((*user.as_ptr().cast::<TOKEN_USER>()).User.Sid, owner) };
        // SAFETY: as above, for the `TOKEN_OWNER` the call filled at its buffer's start.
        let is_default_owner =
          unsafe { EqualSid((*default_owner.as_ptr().cast::<TOKEN_OWNER>()).Owner, owner) };
        is_user != 0 || is_default_owner != 0
      })
  };
  // SAFETY: the descriptor `GetSecurityInfo` allocated, freed once; nothing reads `owner` after.
  unsafe { windows_sys::Win32::Foundation::LocalFree(descriptor) };
  user
}

impl OsLand {
  /// Opens an absolute target path with containment and the ownership check, and returns the writer with the target.
  pub fn open_target(path: &Path) -> Result<(Self, LandingTarget), TargetRefusal> {
    let current = walk_target(path)?;
    let root = Path::new(path.ancestors().last().unwrap_or(path));
    let (host, _) = OsHost::open_root(root).map_err(host_refusal)?;
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
    (self.host, Away { temps: self.temps })
  }

  /// The writer again, over the host it lent and its own state.
  pub fn resume(host: OsHost, away: Away) -> Self {
    Self {
      host,
      temps: away.temps,
    }
  }

  fn dir(&self, dir: HostDir) -> Result<BorrowedHandle<'_>, HostError> {
    self.host.dir_handle(dir)
  }

  fn file(&self, file: HostFile) -> Result<BorrowedHandle<'_>, HostError> {
    self.host.file_handle(file)
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
    WalkRefusal::Host(error) => host_refusal(error),
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
      for thread in started {
        flushed = flushed.and(
          thread
            .join()
            .unwrap_or(Err(HostError::Unavailable(ERRNO_NOTSUP))),
        );
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
  fn capabilities(&mut self, dir: HostDir) -> Result<LandCapabilities, HostError> {
    self.dir(dir)?;
    Ok(LandCapabilities {
      exchange: false,
      reflink: false,
      unnamed_temporaries: false,
    })
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

  fn write_at(&mut self, file: HostFile, off: u64, bytes: &[u8]) -> Result<(), HostError> {
    use std::os::windows::fs::FileExt;
    let handle = self
      .file(file)?
      .try_clone_to_owned()
      .map_err(|e| HostError::Unavailable(e.raw_os_error().unwrap_or(0)))?;
    let writer = std::fs::File::from(handle);
    let mut written = 0usize;
    while written < bytes.len() {
      let at = off.saturating_add(u64::try_from(written).unwrap_or(u64::MAX));
      let n = writer
        .seek_write(bytes.get(written..).unwrap_or_default(), at)
        .map_err(|e| code_refusal(u32::try_from(e.raw_os_error().unwrap_or(0)).unwrap_or(0)))?;
      if n == 0 {
        return Err(HostError::Unavailable(ERRNO_NOSPC));
      }
      written = written.saturating_add(n);
    }
    Ok(())
  }

  fn write_out(&mut self, file: HostFile) -> Result<(), HostError> {
    write_out_file(self.file(file)?)
  }

  fn begin_barrier(&mut self, files: &[HostFile]) -> Result<Box<dyn Barrier>, HostError> {
    let files = files
      .iter()
      .map(|file| {
        self
          .file(*file)?
          .try_clone_to_owned()
          .map_err(|error| HostError::Unavailable(error.raw_os_error().unwrap_or(0)))
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

  fn set_mode(&mut self, file: HostFile, mode: u32) -> Result<(), HostError> {
    let attributes = if mode & WRITE_BITS == 0 {
      FILE_ATTRIBUTE_READONLY
    } else {
      FILE_ATTRIBUTE_NORMAL
    };
    set_basic(
      self.file(file)?,
      &FILE_BASIC_INFO {
        CreationTime: 0,
        LastAccessTime: 0,
        LastWriteTime: 0,
        ChangeTime: 0,
        FileAttributes: attributes,
      },
    )
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
    Err(HostError::Unavailable(ERRNO_NOTSUP))
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

  /// A directory made with `FILE_CREATE`; Windows keeps no POSIX mode, so `mode` decides nothing here.
  fn mkdir(&mut self, dir: HostDir, name: &str, _mode: u32) -> Result<(), HostError> {
    nt_create(
      self.dir(dir)?,
      name,
      MADE_DIR_ACCESS,
      nt::FILE_CREATE,
      nt::FILE_DIRECTORY_FILE,
      FILE_ATTRIBUTE_NORMAL,
    )
    .map(drop)
  }

  fn rmdir(&mut self, dir: HostDir, name: &str) -> Result<(), HostError> {
    let entry = open_entry(self.dir(dir)?, name)?;
    if attributes(entry.as_handle())? & FILE_ATTRIBUTE_DIRECTORY == 0 {
      return Err(HostError::NotDirectory);
    }
    dispose(entry.as_handle())
  }

  fn symlink(&mut self, dir: HostDir, _name: &str, _target: &str) -> Result<(), HostError> {
    self.dir(dir)?;
    Err(HostError::Unavailable(ERRNO_PERM))
  }

  fn sync_dir(&mut self, dir: HostDir) -> Result<(), HostError> {
    flush_dir(self.dir(dir)?)
  }

  fn sync_media(&mut self, dir: HostDir) -> Result<(), HostError> {
    flush_dir(self.dir(dir)?)
  }
}
