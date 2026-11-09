//! The Windows WinFsp bridge (§4.6, D-2; Phase 4).
//!
//! WinFsp is Windows' user-mode filesystem framework: a kernel FSD and a user DLL where the user side
//! answers requests through an `FSP_FILE_SYSTEM_INTERFACE` of callbacks and mounts at a drive letter.
//! slates realizes it in two parts, both here:
//!
//! - The **refusal taxonomy** ([`ntstatus`]), host-buildable and tested on any host: the map from the
//!   volume core's [`VfsError`] to the `NTSTATUS` WinFsp returns to the kernel, the exact analogue of
//!   the FUSE bridge's `VfsError`→errno edge and the NFS bridge's `VfsError`→`nfsstat3` edge. The
//!   common `NTSTATUS` codes are mapped precisely; a rare or slates-specific refusal falls to
//!   `STATUS_UNSUCCESSFUL` rather than a guessed code.
//!
//! - The **mount host** (the `host` module, Windows-only): the `FSP_FILE_SYSTEM_INTERFACE` vtable and
//!   the `FspFileSystem*` FFI, **hand-transcribed from winfsp's `winfsp.h`/`fsctl.h`** — the same
//!   discipline the rt AFD reactor uses over the WDK — so it is a faithful model, not a guess (the
//!   structs carry the header's own `static_assert` sizes), and it cross-lints on the Windows target
//!   from any host. Each callback hands its request to the volume's owner shard over a bounded channel
//!   (D-7's "sharing is a move over a bounded channel"; no `Mutex`, no `Arc`), where the daemon serves it
//!   through the shared `Bridge` (`slates-bridge-core`). The live mount — `slates mount` at a drive letter,
//!   files read and written through the Windows kernel, another path's change seen — runs on the native
//!   Windows CI runner (`crates/cli/tests/winfsp.rs`).
//!
//! - **What Windows is told** ([`notify`]): the names Windows holds open and the `FspFileSystemNotify`
//!   records a change made through another path owes them, host-buildable and tested on any host.
//!
//! - **What a mapped view is purged of** ([`purge`]): the range an unbuffered write through the drive must cover so
//!   WinFsp's coherent purge invalidates a process's view of a file another path changed (A-190, an experiment),
//!   host-buildable and tested on any host.

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds
// are exempt. Once a crate is clean this holds it there; out-of-bounds indexing and slicing are denied
// workspace-wide.
// The panic rule: a print macro panics on a refused write; lines go through `slates_machine::log_line!` (A-213).
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

use slates_vfs::error::VfsError;
use slates_vfs::inode::Kind;

// The WinFsp host — the `FSP_FILE_SYSTEM_INTERFACE` FFI and the mount driver, hand-transcribed from
// winfsp's headers and dispatching onto the shared `Bridge`. Windows-only (it links `winfsp-x64.dll`);
// it cross-lints on the Windows target from any host, and the live mount runs on the Windows runner.
#[cfg(windows)]
mod ffi;
#[cfg(windows)]
pub mod host;
// What a mount tells Windows of another path's change: the names Windows holds open and the notification records
// (§4.6 "Windows"). Pure, so it is tested on every host.
#[cfg(windows)]
mod dispatcher;
// Which WinFsp DLL this host may drive: its version, against the one its layout was written from. Pure, so it is tested
// on every host.
pub mod dll_version;
pub mod held;
pub mod inflight;
pub mod notify;
pub mod opens;
pub mod purge;
#[cfg(windows)]
pub mod purge_client;
#[cfg(windows)]
mod purge_intents;
pub mod purge_slot;
#[cfg(windows)]
mod purge_token;
// How the receive thread waits for a shard's reply: spin for what a wake costs, then park (A-212).
#[cfg(windows)]
mod reply;
// The purges Windows did not take, made again on a doubling wait until each takes or the volume's request timeout
// passes (A-190 as amended 2026-11-01). Pure, so it is tested on every host.
pub mod retry;
// Windows symbolic links: the map between the volume's link target and the reparse buffer Windows reads and sets
// (A-275). Pure, so it is tested on every host.
pub mod symlink;

/// An `NTSTATUS` code — the 32-bit status WinFsp hands the kernel for a request (§4.6). Held as the raw
/// `u32` the constants are written in; the Windows FFI casts it to the `NTSTATUS`/`LONG` (`i32`) the API
/// takes at the boundary, so no signed-hex casts live in this host-buildable core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ntstatus(pub u32);

#[cfg(windows)]
impl Ntstatus {
  /// The status as the signed `i32` WinFsp's ABI takes — a bit-preserving reinterpret (an `NTSTATUS`
  /// failure code such as `0xC000_0001` is meant to read as a negative `i32`), never a lossy cast.
  pub(crate) fn as_i32(self) -> i32 {
    i32::from_ne_bytes(self.0.to_ne_bytes())
  }
}

/// Format: `STATUS_SUCCESS` — the request succeeded (`0x0000_0000`).
pub const STATUS_SUCCESS: Ntstatus = Ntstatus(0x0000_0000);
/// Format: `STATUS_UNSUCCESSFUL` — the generic failure, used where no more specific code is mapped yet.
const STATUS_UNSUCCESSFUL: Ntstatus = Ntstatus(0xC000_0001);
/// Format: `STATUS_INVALID_HANDLE` — a stale or invalid file handle.
const STATUS_INVALID_HANDLE: Ntstatus = Ntstatus(0xC000_0008);
/// Format: `STATUS_INVALID_PARAMETER` — the request is not valid.
const STATUS_INVALID_PARAMETER: Ntstatus = Ntstatus(0xC000_000D);
/// Format: `STATUS_ACCESS_DENIED` — the context is not permitted the operation.
const STATUS_ACCESS_DENIED: Ntstatus = Ntstatus(0xC000_0022);
/// Format: `STATUS_CANNOT_DELETE` — a read-only file refuses a delete (Win32 `ERROR_ACCESS_DENIED`; A-262). Only the
/// Windows host answers it.
#[cfg(windows)]
const STATUS_CANNOT_DELETE: Ntstatus = Ntstatus(0xC000_0121);
/// Format: `STATUS_OBJECT_NAME_NOT_FOUND` — no such name (the POSIX `ENOENT` analogue).
const STATUS_OBJECT_NAME_NOT_FOUND: Ntstatus = Ntstatus(0xC000_0034);
/// Format: `STATUS_OBJECT_NAME_COLLISION` — the name already exists (`EEXIST`).
const STATUS_OBJECT_NAME_COLLISION: Ntstatus = Ntstatus(0xC000_0035);
/// Format: `STATUS_DISK_FULL` — the volume is out of space (`ENOSPC`).
const STATUS_DISK_FULL: Ntstatus = Ntstatus(0xC000_007F);
/// Format: `STATUS_SHARING_VIOLATION` (`0xC000_0043`): the file is in use; Windows callers retry, as they do when an
/// opportunistic lock is being broken. A delegated file awaiting its recall (A-79).
const STATUS_SHARING_VIOLATION: Ntstatus = Ntstatus(0xC000_0043);
/// Format: `STATUS_MEDIA_WRITE_PROTECTED` — the target is read-only (a pinned view refuses a write).
const STATUS_MEDIA_WRITE_PROTECTED: Ntstatus = Ntstatus(0xC000_00A2);
/// Format: `STATUS_FILE_IS_A_DIRECTORY` — a directory was found where a file was expected (`EISDIR`).
const STATUS_FILE_IS_A_DIRECTORY: Ntstatus = Ntstatus(0xC000_00BA);
/// Format: MS-ERREF `STATUS_NOT_SUPPORTED` — the adapter cannot perform this operation.
const STATUS_NOT_SUPPORTED: Ntstatus = Ntstatus(0xC000_00BB);
/// Format: `STATUS_NOT_SAME_DEVICE` — a rename would cross volumes (`EXDEV`).
const STATUS_NOT_SAME_DEVICE: Ntstatus = Ntstatus(0xC000_00D4);
/// Format: `STATUS_DIRECTORY_NOT_EMPTY` — the directory still has entries (`ENOTEMPTY`).
const STATUS_DIRECTORY_NOT_EMPTY: Ntstatus = Ntstatus(0xC000_0101);
/// Format: `STATUS_NOT_A_DIRECTORY` — a file was found where a directory was expected (`ENOTDIR`).
const STATUS_NOT_A_DIRECTORY: Ntstatus = Ntstatus(0xC000_0103);
/// Format: `STATUS_NAME_TOO_LONG` — a name longer than the call can carry (`ENAMETOOLONG`; Win32
/// `ERROR_FILENAME_EXCED_RANGE`): a component past `NAME_MAX` and a directory entry too long for its record.
const STATUS_NAME_TOO_LONG: Ntstatus = Ntstatus(0xC000_0106);

/// Format: the offset from the Windows `FILETIME` epoch (1601-01-01) to the Unix epoch (1970-01-01),
/// in 100-nanosecond intervals — 11 644 473 600 seconds × 10^7. A `FILETIME` is 100-ns ticks since 1601,
/// so a Unix time converts by dividing to 100-ns ticks and adding this offset.
const FILETIME_UNIX_EPOCH_OFFSET: u64 = 116_444_736_000_000_000;
/// Format: the length of a `FILETIME` tick in nanoseconds (100 ns) — the unit a Unix nanosecond time is
/// divided by before the epoch offset is added.
const FILETIME_TICK_NS: u64 = 100;

/// Format: `FILE_ATTRIBUTE_DIRECTORY` — the object is a directory.
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
/// Format: `FILE_ATTRIBUTE_NORMAL` — an ordinary file with no other attributes.
const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
/// Format: `FILE_ATTRIBUTE_REPARSE_POINT` — the object is a reparse point; WinFsp represents a symlink
/// as one.
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
/// Format: `FILE_ATTRIBUTE_READONLY` — a file's is its mode's write bits (A-262), a directory's a kept bit.
const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
/// Format: `FILE_ATTRIBUTE_ARCHIVE` — NTFS sets it on every file it makes or rewrites, for backups.
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;
/// Format: `INVALID_FILE_ATTRIBUTES`, what `SetBasicInfo` is sent for "leave the attributes as they are".
pub const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
/// Format: the mode's owner write bit, which a file's `READONLY` is the absence of (A-262; WSL DrvFs's mapping).
const OWNER_WRITE: u32 = 0o200;
/// Format: every class's write bit, which setting a file's `READONLY` clears.
const ALL_WRITE: u32 = 0o222;

/// The Win32 attribute word the drive reports for an object (A-262): its kind's bit (a directory, a reparse point for a
/// symlink), the bits the volume keeps beside its mode, and `READONLY` for a file its owner may not write. A file with
/// none of them reports `FILE_ATTRIBUTE_NORMAL`, as Windows requires (`NORMAL` is valid only alone).
pub fn node_attributes(attr: &slates_bridge_core::NodeAttr) -> Result<u32, VfsError> {
  let kept = u32::from(attr.windows);
  let word = match attr.kind {
    Kind::Dir => FILE_ATTRIBUTE_DIRECTORY | kept,
    Kind::Symlink => FILE_ATTRIBUTE_REPARSE_POINT | kept,
    Kind::File if attr.mode & OWNER_WRITE == 0 => kept | FILE_ATTRIBUTE_READONLY,
    Kind::File => kept,
    Kind::Fifo | Kind::Socket => return Err(VfsError::SpecialFileOperation),
  };
  Ok(if word == 0 {
    FILE_ATTRIBUTE_NORMAL
  } else {
    word
  })
}

/// The change that gives `attr`'s object the Win32 attribute word `word` (A-262), as `SetFileAttributes` and a create's
/// or an overwrite's attributes ask: the volume keeps the bits it keeps for the object's kind, and a file's `READONLY`
/// becomes its mode, its write bits cleared when set and its owner's write bit given back when cleared. The mode is
/// left alone when it already says what was asked.
pub fn attribute_changes(
  attr: &slates_bridge_core::NodeAttr,
  word: u32,
) -> slates_bridge_core::SetAttr {
  let mode = (attr.kind == Kind::File)
    .then(|| {
      let read_only = attr.mode & OWNER_WRITE == 0;
      match (word & FILE_ATTRIBUTE_READONLY != 0, read_only) {
        (true, false) => Some(attr.mode & !ALL_WRITE),
        (false, true) => Some(attr.mode | OWNER_WRITE),
        _ => None,
      }
    })
    .flatten();
  slates_bridge_core::SetAttr {
    mode,
    windows: Some(word),
    ..slates_bridge_core::SetAttr::default()
  }
}

/// The attribute word a create leaves (A-262): what the caller asked, with `ARCHIVE` on a file, as NTFS and WinFsp's
/// memfs mark every file they make.
pub fn created_attributes(word: u32, is_dir: bool) -> u32 {
  if is_dir {
    word
  } else {
    word | FILE_ATTRIBUTE_ARCHIVE
  }
}

/// The attribute word an overwrite (`CREATE_ALWAYS`, `TRUNCATE_EXISTING` through `Overwrite`) leaves on a file that
/// reported `current` (A-262): with `replace` (`CREATE_ALWAYS`'s supersede), the caller's word alone; without, the
/// caller's added to what the file had. `ARCHIVE` either way, since the file was rewritten.
pub fn overwritten_attributes(current: u32, word: u32, replace: bool) -> u32 {
  let kept = if replace {
    word
  } else {
    (current & !FILE_ATTRIBUTE_NORMAL) | word
  };
  kept | FILE_ATTRIBUTE_ARCHIVE
}

/// Whether Windows refuses to delete or replace `attr`'s object for its attributes (A-262): a read-only file
/// (`STATUS_CANNOT_DELETE` for a delete, `STATUS_ACCESS_DENIED` for a rename onto it, `t/base/01.t` and `04.t`). A
/// directory's `READONLY` is no such refusal on Windows, and is none here.
pub fn refuses_replacement(attr: &slates_bridge_core::NodeAttr) -> bool {
  attr.kind == Kind::File && attr.mode & OWNER_WRITE == 0
}

/// Converts a Unix time in nanoseconds to a Windows `FILETIME` (100-ns ticks since 1601), the form
/// WinFsp reports file times in. A negative time is clamped to the Unix epoch (WinFsp takes an unsigned
/// tick count), exactly as the FUSE bridge clamps a negative time to zero.
pub fn filetime_from_unix_ns(ns: i64) -> u64 {
  let ns = u64::try_from(ns).unwrap_or(0);
  (ns / FILETIME_TICK_NS).saturating_add(FILETIME_UNIX_EPOCH_OFFSET)
}

/// Format: the `FILETIME` a `SetBasicInfo` call passes for a time it leaves alone (WinFsp's `SetBasicInfo`: "0 if
/// the time should not be changed"), and the all-ones value Windows' `SetFileTime` documents as "stop updating it for this
/// handle", which a volume that keeps no per-handle update policy also leaves alone.
const FILETIME_UNCHANGED: [u64; 2] = [0, u64::MAX];

/// Converts a Windows `FILETIME` a caller set (`SetFileTime`, `SetFileInformationByHandle`) to a Unix time in
/// nanoseconds, the form the volume keeps (A-209): `None` for a time the call leaves alone ([`FILETIME_UNCHANGED`]),
/// and a time before 1970 negative, as the volume's signed times hold it. A time past what nanoseconds since 1970 hold
/// in an `i64` (the year 2262) is `None` too: the volume cannot keep it, and the call keeps the old one.
pub fn unix_ns_from_filetime(filetime: u64) -> Option<i64> {
  if FILETIME_UNCHANGED.contains(&filetime) {
    return None;
  }
  let ticks = i128::from(filetime).checked_sub(i128::from(FILETIME_UNIX_EPOCH_OFFSET))?;
  i64::try_from(ticks.checked_mul(i128::from(FILETIME_TICK_NS))?).ok()
}

/// The Windows file-attribute bits for a node kind (§4.6) — a directory, an ordinary file, or a symlink
/// (a reparse point, the form WinFsp represents links in). The FUSE bridge makes the same choice with
/// the `DT_*` d_type; this is its Windows analogue.
pub fn file_attributes(kind: Kind) -> Result<u32, VfsError> {
  Ok(match kind {
    Kind::Dir => FILE_ATTRIBUTE_DIRECTORY,
    Kind::File => FILE_ATTRIBUTE_NORMAL,
    Kind::Symlink => FILE_ATTRIBUTE_REPARSE_POINT,
    Kind::Fifo | Kind::Socket => return Err(VfsError::SpecialFileOperation),
  })
}

/// The `NTSTATUS` WinFsp returns for a volume refusal (§4.6). The common, well-defined codes map
/// precisely; the rare and slates-specific refusals (`TooManyLinks`, `FileTooLarge`, `Destroying`,
/// `BaseUnavailable`, `BaseDrift`, `NotOverlay`, `RecoveryIncomplete`, `Archived`, `PolicyMismatch`,
/// `Memory`) fall to the generic `STATUS_UNSUCCESSFUL` rather than a guessed code — their precise
/// `NTSTATUS` values are owed, verified against `ntstatus.h` on Windows.
pub fn ntstatus(error: &VfsError) -> Ntstatus {
  match error {
    VfsError::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
    VfsError::AlreadyExists => STATUS_OBJECT_NAME_COLLISION,
    VfsError::NotDirectory => STATUS_NOT_A_DIRECTORY,
    VfsError::IsDirectory => STATUS_FILE_IS_A_DIRECTORY,
    VfsError::NotEmpty => STATUS_DIRECTORY_NOT_EMPTY,
    VfsError::NoSpace => STATUS_DISK_FULL,
    VfsError::NotPermitted | VfsError::LinkProtected => STATUS_ACCESS_DENIED,
    VfsError::Invalid | VfsError::InvalidName => STATUS_INVALID_PARAMETER,
    VfsError::NameTooLong => STATUS_NAME_TOO_LONG,
    VfsError::CrossVolumeMove => STATUS_NOT_SAME_DEVICE,
    VfsError::StaleHandle => STATUS_INVALID_HANDLE,
    // A read-only attachment or a snapshot's view is a write-protected volume to Windows, as `Pinned` is.
    VfsError::Pinned | VfsError::ReadOnly => STATUS_MEDIA_WRITE_PROTECTED,
    VfsError::SpecialFileOperation => STATUS_NOT_SUPPORTED,
    VfsError::Delegated => STATUS_SHARING_VIOLATION,
    _ => STATUS_UNSUCCESSFUL,
  }
}

/// The generic-failure status as the FFI `i32`, for the host to return where no volume refusal applies
/// (a broken owner channel, an unexpected reply). Windows-only, used by the mount host.
#[cfg(windows)]
pub(crate) fn status_unsuccessful() -> i32 {
  STATUS_UNSUCCESSFUL.as_i32()
}

/// `STATUS_NAME_TOO_LONG` as the FFI `i32` (a directory entry whose record would not fit the size field
/// WinFsp's `FSP_FSCTL_DIR_INFO` gives it).
#[cfg(windows)]
pub(crate) fn status_name_too_long() -> i32 {
  STATUS_NAME_TOO_LONG.as_i32()
}

/// `STATUS_OBJECT_NAME_NOT_FOUND` as the FFI `i32` (a lookup of a name that does not exist).
#[cfg(windows)]
pub(crate) fn status_object_name_not_found() -> i32 {
  STATUS_OBJECT_NAME_NOT_FOUND.as_i32()
}

/// `STATUS_CANNOT_DELETE` as the FFI `i32`: a delete of a read-only file (A-262).
#[cfg(windows)]
pub(crate) fn status_cannot_delete() -> i32 {
  STATUS_CANNOT_DELETE.as_i32()
}

/// `STATUS_INVALID_PARAMETER` as the FFI `i32` (a malformed path with no final component).
#[cfg(windows)]
pub(crate) fn status_invalid_parameter() -> i32 {
  STATUS_INVALID_PARAMETER.as_i32()
}

/// Format: `STATUS_UNEXPECTED_IO_ERROR` (`0xC000_00E9`) — the request's effect is in the volume but the publication that
/// makes it survive a restart was refused, so the caller is told rather than promised survival (the FUSE mount's `EIO`
/// in the same place).
#[cfg(windows)]
const STATUS_UNEXPECTED_IO_ERROR: Ntstatus = Ntstatus(0xC000_00E9);

/// [`STATUS_UNEXPECTED_IO_ERROR`] as the FFI `i32`.
#[cfg(windows)]
pub(crate) fn status_unexpected_io_error() -> i32 {
  STATUS_UNEXPECTED_IO_ERROR.as_i32()
}

/// Format: `STATUS_BUFFER_OVERFLOW` (`0x8000_0005`) as the FFI `i32` — the security-descriptor buffer
/// WinFsp offered was smaller than the descriptor; the host reports the needed size and this status.
#[cfg(windows)]
pub(crate) fn status_buffer_overflow() -> i32 {
  Ntstatus(0x8000_0005).as_i32()
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The attributes of a `kind` object with `mode` and the kept `windows` bits.
  fn node(kind: Kind, mode: u32, windows: u16) -> slates_bridge_core::NodeAttr {
    slates_bridge_core::NodeAttr {
      ino: 2,
      generation: 0,
      kind,
      mode,
      nlink: 1,
      uid: 0,
      gid: 0,
      size: 0,
      atime: 0,
      mtime: 0,
      ctime: 0,
      btime: 0,
      windows,
      change: 0,
    }
  }

  /// A-262. Do: report a file with no kept bits, one with `ARCHIVE`, one its owner may not write, a directory with a
  /// kept `READONLY | HIDDEN`, and a symlink. Expect: `NORMAL` alone for the first (Windows' rule), `ARCHIVE`, `READONLY`
  /// from the mode, the directory's bits beside `DIRECTORY`, and a reparse point.
  #[test]
  fn the_drive_reports_kept_bits_and_a_files_read_only_from_its_mode() {
    assert_eq!(node_attributes(&node(Kind::File, 0o644, 0)), Ok(0x80));
    assert_eq!(node_attributes(&node(Kind::File, 0o644, 0x20)), Ok(0x20));
    assert_eq!(node_attributes(&node(Kind::File, 0o444, 0x20)), Ok(0x21));
    assert_eq!(node_attributes(&node(Kind::Dir, 0o755, 0x3)), Ok(0x13));
    assert_eq!(node_attributes(&node(Kind::Symlink, 0o777, 0)), Ok(0x400));
  }

  /// A-262, `t/base/01.t` and `07.t`. Do: set `READONLY` on a writable file, clear it on a read-only one, and set
  /// `READONLY | HIDDEN` on a directory. Expect: the file's write bits cleared and then its owner's given back (the mode
  /// is the file's read-only state), the bits passed for the volume to keep, and a directory's mode never touched.
  #[test]
  fn a_files_read_only_is_its_mode_and_a_directorys_is_kept() {
    let set = attribute_changes(&node(Kind::File, 0o664, 0x20), 0x1);
    assert_eq!((set.mode, set.windows), (Some(0o444), Some(0x1)));
    let cleared = attribute_changes(&node(Kind::File, 0o444, 0x20), 0x20);
    assert_eq!((cleared.mode, cleared.windows), (Some(0o644), Some(0x20)));
    let same = attribute_changes(&node(Kind::File, 0o644, 0), 0x2);
    assert_eq!(
      same.mode, None,
      "the mode is left as it is when it says what was asked"
    );
    let dir = attribute_changes(&node(Kind::Dir, 0o755, 0), 0x3);
    assert_eq!((dir.mode, dir.windows), (None, Some(0x3)));
  }

  /// A-262. Do: create a file and a directory with `NORMAL`, then overwrite a `HIDDEN` file with `READONLY`, superseding
  /// it and not. Expect: a new file is `ARCHIVE` and a new directory has no bits; a supersede takes the caller's word and
  /// a plain overwrite adds it to the file's, each with `ARCHIVE` (NTFS's rules, which WinFsp's memfs follows).
  #[test]
  fn creates_and_overwrites_mark_a_file_archived() {
    assert_eq!(created_attributes(0x80, false) & !0x80, 0x20);
    assert_eq!(created_attributes(0, true), 0);
    assert_eq!(overwritten_attributes(0x22, 0x1, true), 0x21);
    assert_eq!(overwritten_attributes(0x22, 0x1, false), 0x23);
    assert_eq!(overwritten_attributes(0x80, 0, false), 0x20);
  }

  /// A-262, `t/base/01.t` case 13 and `04.t` case 10. Do: ask whether a read-only file, a writable file and a read-only
  /// directory refuse a delete or a replacement. Expect: only the file whose owner may not write it.
  #[test]
  fn only_a_read_only_file_refuses_its_deletion() {
    assert!(refuses_replacement(&node(Kind::File, 0o444, 0)));
    assert!(!refuses_replacement(&node(Kind::File, 0o644, 0)));
    assert!(!refuses_replacement(&node(Kind::Dir, 0o555, 0x1)));
  }

  /// AC-3.10 / A-26: an IPC inode is refused as unsupported, never projected as a disk file.
  #[test]
  fn ipc_names_report_unsupported() {
    for kind in [Kind::Fifo, Kind::Socket] {
      let error = file_attributes(kind).unwrap_err();
      // Format: MS-ERREF STATUS_NOT_SUPPORTED.
      assert_eq!(ntstatus(&error), Ntstatus(0xC000_00BB));
    }
  }

  /// Each mapped refusal returns its documented `NTSTATUS`, and an unmapped one falls to the generic
  /// failure — the refusal taxonomy WinFsp will hand the kernel (§4.6), confirmable with no Windows.
  #[test]
  fn each_refusal_maps_to_its_ntstatus() {
    let cases = [
      (VfsError::NotFound, STATUS_OBJECT_NAME_NOT_FOUND),
      (VfsError::AlreadyExists, STATUS_OBJECT_NAME_COLLISION),
      (VfsError::NotDirectory, STATUS_NOT_A_DIRECTORY),
      (VfsError::IsDirectory, STATUS_FILE_IS_A_DIRECTORY),
      (VfsError::NotEmpty, STATUS_DIRECTORY_NOT_EMPTY),
      (VfsError::NoSpace, STATUS_DISK_FULL),
      (VfsError::NotPermitted, STATUS_ACCESS_DENIED),
      (VfsError::ReadOnly, STATUS_MEDIA_WRITE_PROTECTED),
      (VfsError::Invalid, STATUS_INVALID_PARAMETER),
      (VfsError::InvalidName, STATUS_INVALID_PARAMETER),
      (VfsError::NameTooLong, STATUS_NAME_TOO_LONG),
      (VfsError::CrossVolumeMove, STATUS_NOT_SAME_DEVICE),
      (VfsError::StaleHandle, STATUS_INVALID_HANDLE),
      (VfsError::Pinned, STATUS_MEDIA_WRITE_PROTECTED),
      // A refusal without a precise code yet falls to the generic failure, never a wrong code.
      (VfsError::Archived, STATUS_UNSUCCESSFUL),
      (VfsError::RecoveryIncomplete, STATUS_UNSUCCESSFUL),
    ];
    for (error, expected) in cases {
      assert_eq!(ntstatus(&error), expected, "{error:?}");
    }
  }

  /// Every mapped `NTSTATUS` is a failure code (the high `0xC000_0000` class), so a refusal can never be
  /// mistaken for `STATUS_SUCCESS` at the kernel boundary.
  #[test]
  fn a_refusal_is_never_success() {
    let refusals = [
      VfsError::NotFound,
      VfsError::AlreadyExists,
      VfsError::Invalid,
      VfsError::Pinned,
      VfsError::Archived,
    ];
    for refusal in refusals {
      let status = ntstatus(&refusal);
      assert_ne!(
        status, STATUS_SUCCESS,
        "a refusal is not success: {status:?}"
      );
      assert!(
        status.0 & 0xC000_0000 == 0xC000_0000,
        "a refusal is an error class: {status:?}"
      );
    }
  }

  /// A Unix time converts to the Windows `FILETIME` the kernel expects: the Unix epoch is the offset
  /// itself, one second later is ten million ticks past it, and a negative time clamps to the epoch.
  #[test]
  fn unix_time_converts_to_filetime() {
    assert_eq!(
      filetime_from_unix_ns(0),
      FILETIME_UNIX_EPOCH_OFFSET,
      "the Unix epoch is the offset"
    );
    assert_eq!(
      filetime_from_unix_ns(1_000_000_000),
      FILETIME_UNIX_EPOCH_OFFSET + 10_000_000,
      "one second is ten million 100-ns ticks past the epoch"
    );
    assert_eq!(
      filetime_from_unix_ns(-42),
      FILETIME_UNIX_EPOCH_OFFSET,
      "a negative time clamps to the epoch"
    );
  }

  /// A-209: a `FILETIME` a caller set converts back to the volume's Unix nanoseconds and round-trips through
  /// [`filetime_from_unix_ns`]; the two "leave it" values convert to nothing, a time before 1970 is negative, and one past
  /// what the volume's `i64` holds is nothing rather than a wrapped time.
  #[test]
  fn a_set_filetime_converts_to_the_volumes_time() {
    assert_eq!(unix_ns_from_filetime(0), None, "zero leaves the time alone");
    assert_eq!(
      unix_ns_from_filetime(u64::MAX),
      None,
      "all ones leaves it alone"
    );
    assert_eq!(unix_ns_from_filetime(FILETIME_UNIX_EPOCH_OFFSET), Some(0));
    let set = 4_102_444_800_000_000_000_i64;
    assert_eq!(unix_ns_from_filetime(filetime_from_unix_ns(set)), Some(set));
    assert_eq!(
      unix_ns_from_filetime(FILETIME_UNIX_EPOCH_OFFSET - 10_000_000),
      Some(-1_000_000_000),
      "a second before 1970 is negative"
    );
    assert_eq!(unix_ns_from_filetime(u64::MAX - 1), None, "past 2262");
  }

  /// Each node kind maps to its Windows file-attribute bit — a directory, an ordinary file, and a
  /// symlink (a reparse point).
  #[test]
  fn each_kind_maps_to_its_file_attribute() {
    assert_eq!(
      file_attributes(Kind::Dir).unwrap(),
      FILE_ATTRIBUTE_DIRECTORY
    );
    assert_eq!(file_attributes(Kind::File).unwrap(), FILE_ATTRIBUTE_NORMAL);
    assert_eq!(
      file_attributes(Kind::Symlink).unwrap(),
      FILE_ATTRIBUTE_REPARSE_POINT
    );
  }
}
