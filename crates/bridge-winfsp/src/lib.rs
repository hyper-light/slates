//! The Windows WinFsp bridge (§4.6, D-2; Phase 4) — the host-buildable pieces first.
//!
//! WinFsp is Windows' user-mode filesystem framework: a kernel FSD and a user DLL, where the user side
//! blocks in `FSP_FSCTL_TRANSACT` to fetch IRPs and answers them through the `FSP_FILE_SYSTEM_INTERFACE`
//! callbacks (roughly 25 of them). Unlike the FSKit bridge — whose shim wire is *ours* to define, so its
//! whole codec is built and tested on any host — WinFsp's transact request/response layout is winfsp's
//! own header-defined protocol (`fsctl.h`), so the marshalling and the `winfsp-rs` FFI are the Windows
//! half, written against those headers and cross-linted on the native Windows runner (they cannot be
//! faithfully modelled here without guessing the struct layout).
//!
//! What *is* host-buildable — and is built and tested here — is the **refusal taxonomy**: the map from
//! the volume core's [`VfsError`] to the `NTSTATUS` WinFsp returns to the kernel, the exact analogue of
//! the FUSE bridge's `VfsError`→errno edge and the NFS bridge's `VfsError`→`nfsstat3` edge (§4.6 "the
//! volume core's own `VfsError`, which each transport maps to its wire error"). The common, well-defined
//! `NTSTATUS` codes are mapped precisely; the rare and slates-specific refusals fall to the generic
//! `STATUS_UNSUCCESSFUL` rather than a guessed-at code — their precise `NTSTATUS` values are owed,
//! pending verification against `ntstatus.h` on Windows.

use slates_vfs::error::VfsError;

/// An `NTSTATUS` code — the 32-bit status WinFsp hands the kernel for a request (§4.6). Held as the raw
/// `u32` the constants are written in; the Windows FFI casts it to the `NTSTATUS`/`LONG` (`i32`) the API
/// takes at the boundary, so no signed-hex casts live in this host-buildable core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ntstatus(pub u32);

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
}
