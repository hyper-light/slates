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
//!   from any host. Each callback dispatches onto the shared `Bridge` (`slates-bridge-core`) over a
//!   single owner thread that holds the `!Send` volume (D-7's "sharing is a move over a bounded
//!   channel"; no `Mutex`, no `Arc`). The live mount — mount a drive letter, create/write/read/list/
//!   delete through the Windows kernel, unmount — runs on the native Windows CI runner, the way the
//!   FSKit handler's live mount runs on macOS.

use slates_vfs::error::VfsError;
use slates_vfs::inode::Kind;

// The WinFsp host — the `FSP_FILE_SYSTEM_INTERFACE` FFI and the mount driver, hand-transcribed from
// winfsp's headers and dispatching onto the shared `Bridge`. Windows-only (it links `winfsp-x64.dll`);
// it cross-lints on the Windows target from any host, and the live mount runs on the Windows runner.
#[cfg(windows)]
mod ffi;
#[cfg(windows)]
pub mod host;

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
/// Format: `STATUS_OBJECT_NAME_NOT_FOUND` — no such name (the POSIX `ENOENT` analogue).
const STATUS_OBJECT_NAME_NOT_FOUND: Ntstatus = Ntstatus(0xC000_0034);
/// Format: `STATUS_OBJECT_NAME_COLLISION` — the name already exists (`EEXIST`).
const STATUS_OBJECT_NAME_COLLISION: Ntstatus = Ntstatus(0xC000_0035);
/// Format: `STATUS_DISK_FULL` — the volume is out of space (`ENOSPC`).
const STATUS_DISK_FULL: Ntstatus = Ntstatus(0xC000_007F);
/// Format: `STATUS_MEDIA_WRITE_PROTECTED` — the target is read-only (a pinned view refuses a write).
const STATUS_MEDIA_WRITE_PROTECTED: Ntstatus = Ntstatus(0xC000_00A2);
/// Format: `STATUS_FILE_IS_A_DIRECTORY` — a directory was found where a file was expected (`EISDIR`).
const STATUS_FILE_IS_A_DIRECTORY: Ntstatus = Ntstatus(0xC000_00BA);
/// Format: `STATUS_NOT_SAME_DEVICE` — a rename would cross volumes (`EXDEV`).
const STATUS_NOT_SAME_DEVICE: Ntstatus = Ntstatus(0xC000_00D4);
/// Format: `STATUS_DIRECTORY_NOT_EMPTY` — the directory still has entries (`ENOTEMPTY`).
const STATUS_DIRECTORY_NOT_EMPTY: Ntstatus = Ntstatus(0xC000_0101);
/// Format: `STATUS_NOT_A_DIRECTORY` — a file was found where a directory was expected (`ENOTDIR`).
const STATUS_NOT_A_DIRECTORY: Ntstatus = Ntstatus(0xC000_0103);

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

/// Converts a Unix time in nanoseconds to a Windows `FILETIME` (100-ns ticks since 1601), the form
/// WinFsp reports file times in. A negative time is clamped to the Unix epoch (WinFsp takes an unsigned
/// tick count), exactly as the FUSE bridge clamps a negative time to zero.
pub fn filetime_from_unix_ns(ns: i64) -> u64 {
  let ns = u64::try_from(ns).unwrap_or(0);
  ns / FILETIME_TICK_NS + FILETIME_UNIX_EPOCH_OFFSET
}

/// The Windows file-attribute bits for a node kind (§4.6) — a directory, an ordinary file, or a symlink
/// (a reparse point, the form WinFsp represents links in). The FUSE bridge makes the same choice with
/// the `DT_*` d_type; this is its Windows analogue.
pub fn file_attributes(kind: Kind) -> u32 {
  match kind {
    Kind::Dir => FILE_ATTRIBUTE_DIRECTORY,
    Kind::File => FILE_ATTRIBUTE_NORMAL,
    Kind::Symlink => FILE_ATTRIBUTE_REPARSE_POINT,
  }
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
    VfsError::NotPermitted => STATUS_ACCESS_DENIED,
    VfsError::Invalid | VfsError::InvalidName => STATUS_INVALID_PARAMETER,
    VfsError::CrossVolumeMove => STATUS_NOT_SAME_DEVICE,
    VfsError::StaleHandle => STATUS_INVALID_HANDLE,
    VfsError::Pinned => STATUS_MEDIA_WRITE_PROTECTED,
    _ => STATUS_UNSUCCESSFUL,
  }
}

/// The generic-failure status as the FFI `i32`, for the host to return where no volume refusal applies
/// (a broken owner channel, an unexpected reply). Windows-only, used by the mount host.
#[cfg(windows)]
pub(crate) fn status_unsuccessful() -> i32 {
  STATUS_UNSUCCESSFUL.as_i32()
}

/// `STATUS_OBJECT_NAME_NOT_FOUND` as the FFI `i32` (a lookup of a name that does not exist).
#[cfg(windows)]
pub(crate) fn status_object_name_not_found() -> i32 {
  STATUS_OBJECT_NAME_NOT_FOUND.as_i32()
}

/// `STATUS_INVALID_PARAMETER` as the FFI `i32` (a malformed path with no final component).
#[cfg(windows)]
pub(crate) fn status_invalid_parameter() -> i32 {
  STATUS_INVALID_PARAMETER.as_i32()
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
      (VfsError::Invalid, STATUS_INVALID_PARAMETER),
      (VfsError::InvalidName, STATUS_INVALID_PARAMETER),
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

  /// Each node kind maps to its Windows file-attribute bit — a directory, an ordinary file, and a
  /// symlink (a reparse point).
  #[test]
  fn each_kind_maps_to_its_file_attribute() {
    assert_eq!(file_attributes(Kind::Dir), FILE_ATTRIBUTE_DIRECTORY);
    assert_eq!(file_attributes(Kind::File), FILE_ATTRIBUTE_NORMAL);
    assert_eq!(file_attributes(Kind::Symlink), FILE_ATTRIBUTE_REPARSE_POINT);
  }
}
