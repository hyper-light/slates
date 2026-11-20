//! The refusal a Win32 error code is at the write seam (A-527, A-539, A-568): the Windows writer names the kinds the
//! landing tells apart, a name that exists (`HostError::Exists`), a full disk (`HostError::NoSpace`) and a privilege this
//! process does not hold (`HostError::Unprivileged`, a link on an account without the link privilege), by the Win32 code
//! they came as, and keeps every other code as the Win32 code it is (`HostError::Unavailable`), each typed so
//! (`OsCode::Win32`); a verb NTFS lacks is named `HostError::Unsupported` where the writer meets one. Win32 numbers its
//! codes on its own, and five carry the number of an errno the engine once told apart (`ERROR_NOT_SAME_DEVICE` 17
//! `EEXIST`, `ERROR_BAD_COMMAND` 22 `EINVAL`, `ERROR_OUT_OF_PAPER` 28 `ENOSPC`, `ERROR_TOO_MANY_SESS` 69 macOS's
//! `EDQUOT`, `ERROR_INSUFFICIENT_BUFFER` 122 Linux's `EDQUOT`); since A-539 the engine reads kinds and compares no
//! number, and a code typed as Win32's is read as no errno anywhere. Pure, so it is tested on every host; the Windows
//! writer (`os_windows.rs`) is its only user.

use slates_vfs::host::{HostError, OsCode};

/// Format: `ERROR_FILE_EXISTS` (winerror.h): a file of that name exists.
const FILE_EXISTS: u32 = 80;
/// Format: `ERROR_ALREADY_EXISTS` (winerror.h): an object of that name exists; what NTFS's
/// `STATUS_OBJECT_NAME_COLLISION` is as a Win32 code.
pub(crate) const ALREADY_EXISTS: u32 = 183;
/// Format: `ERROR_HANDLE_DISK_FULL` (winerror.h): the disk is full.
const HANDLE_DISK_FULL: u32 = 39;
/// Format: `ERROR_DISK_FULL` (winerror.h): there is not enough space on the disk; the code the writer names a write
/// that took no byte, and a length past the file system's, by.
pub(crate) const DISK_FULL: u32 = 112;
/// Format: `ERROR_NOT_SUPPORTED` (winerror.h): the request is not supported; the code the writer names a verb NTFS
/// lacks by (an exchange, a clone it cannot make, a bit the file system drops).
pub(crate) const NOT_SUPPORTED: u32 = 50;
/// Format: `ERROR_PRIVILEGE_NOT_HELD` (winerror.h): a required privilege is not held by the client; what a symbolic
/// link's set (`FSCTL_SET_REPARSE_POINT`) answers on an account without `SeCreateSymbolicLinkPrivilege` (A-568).
const PRIVILEGE_NOT_HELD: u32 = 1314;

/// The seam's refusal for the Win32 error code `code`: a name that exists, a full disk, or a privilege this process does
/// not hold (A-568) named as its kind, by its code; any other code as the Win32 code it is, which no reader takes for an
/// errno.
pub(crate) fn refusal_of(code: u32) -> HostError {
  match code {
    FILE_EXISTS | ALREADY_EXISTS => HostError::Exists(OsCode::Win32(code)),
    HANDLE_DISK_FULL | DISK_FULL => HostError::NoSpace(OsCode::Win32(code)),
    PRIVILEGE_NOT_HELD => HostError::Unprivileged(OsCode::Win32(code)),
    other => HostError::Unavailable(OsCode::Win32(other)),
  }
}

/// A verb NTFS lacks, named as the kind the landing falls back on (A-539), by `ERROR_NOT_SUPPORTED` or the code the
/// call refused with.
pub(crate) fn unsupported(code: u32) -> HostError {
  HostError::Unsupported(OsCode::Win32(code))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Format: the Win32 codes whose numbers are errnos the engine once told apart (winerror.h):
  /// `ERROR_NOT_SAME_DEVICE` (17, `EEXIST`), `ERROR_BAD_COMMAND` (22, `EINVAL`, a filesystem without an atomic
  /// exchange), `ERROR_OUT_OF_PAPER` (28, `ENOSPC`), `ERROR_TOO_MANY_SESS` (69, macOS's `EDQUOT`) and
  /// `ERROR_INSUFFICIENT_BUFFER` (122, Linux's `EDQUOT`).
  const COLLIDING: [u32; 5] = [17, 22, 28, 69, 122];
  /// Shape: every code Win32's system error codes reach (winerror.h numbers them below 16,000), and past it.
  const CODES: u32 = 1 << 16;

  /// Every code the writer names no kind of, the colliding five among them, kept as the Win32 code it is.
  fn assert_every_other_code_kept() {
    let kept = |code: u32| HostError::Unavailable(OsCode::Win32(code));
    for code in COLLIDING {
      assert_eq!(refusal_of(code), kept(code), "{code}");
    }
    for code in 0..CODES {
      if ![
        FILE_EXISTS,
        ALREADY_EXISTS,
        HANDLE_DISK_FULL,
        DISK_FULL,
        PRIVILEGE_NOT_HELD,
      ]
      .contains(&code)
      {
        assert_eq!(refusal_of(code), kept(code), "{code}");
      }
    }
  }

  /// A-527, A-539 (the seam's numbering is typed). Do: map through the writer's rule every Win32 code up to 65,535, the
  /// five whose numbers are errnos the engine once told apart among them, and the four the writer names as kinds.
  /// Expect a name that exists (`ERROR_FILE_EXISTS`, `ERROR_ALREADY_EXISTS`) named `Exists` and a full disk
  /// (`ERROR_HANDLE_DISK_FULL`, `ERROR_DISK_FULL`) `NoSpace`, each by its own Win32 code, and every other code kept as
  /// the Win32 code it is: `ERROR_NOT_SAME_DEVICE` is never the name that exists its number names in POSIX. Before A-527
  /// every other code passed on as a bare number; before A-539 the two kinds passed on as errnos.
  #[test]
  fn a_win32_code_is_never_read_as_the_errno_of_its_number() {
    for code in [FILE_EXISTS, ALREADY_EXISTS] {
      assert_eq!(
        refusal_of(code),
        HostError::Exists(OsCode::Win32(code)),
        "{code}"
      );
    }
    for code in [HANDLE_DISK_FULL, DISK_FULL] {
      assert_eq!(
        refusal_of(code),
        HostError::NoSpace(OsCode::Win32(code)),
        "{code}"
      );
    }
    assert_every_other_code_kept();
    assert_eq!(
      unsupported(NOT_SUPPORTED),
      HostError::Unsupported(OsCode::Win32(NOT_SUPPORTED))
    );
  }

  /// A-568 (a link the account may not make fails alone). Do: read through the writer's rule the code a symbolic link's
  /// set answers on an account without the link privilege (`ERROR_PRIVILEGE_NOT_HELD`, measured on jupiter in Developer
  /// Mode, where `CreateSymbolicLinkW`'s unprivileged flag makes a link and this route may not). Expect the privilege
  /// named as its kind, `Unprivileged`, by its own Win32 code: the landing fails that entry alone by it and goes on.
  /// Before A-568 it was `Unavailable`, which ended the whole landing.
  #[test]
  fn a_privilege_the_process_lacks_is_named_as_its_kind() {
    assert_eq!(
      refusal_of(PRIVILEGE_NOT_HELD),
      HostError::Unprivileged(OsCode::Win32(PRIVILEGE_NOT_HELD))
    );
  }
}
