//! The refusal a Win32 error code is at the write seam (A-527): the Windows writer gives the few codes whose kinds the
//! landing tells apart the errnos of those kinds — a name that exists (`EEXIST`), a full disk (`ENOSPC`) — and keeps
//! every other code as the Win32 code it is (`HostError::Win32`). The engine was written over POSIX and tells refusals
//! apart by an errno's number, and Win32 numbers its codes on its own: five of them carry a number the engine reads as an
//! errno's kind (`ERROR_NOT_SAME_DEVICE` 17 as `EEXIST`, `ERROR_BAD_COMMAND` 22 as `EINVAL`, `ERROR_OUT_OF_PAPER` 28 as
//! `ENOSPC`, `ERROR_TOO_MANY_SESS` 69 as macOS's `EDQUOT`, `ERROR_INSUFFICIENT_BUFFER` 122 as Linux's `EDQUOT`), so a
//! code passed on as a bare number could read as a refusal it is not. Pure, so it is tested on every host; the Windows
//! writer (`os_windows.rs`) is its only user.

use slates_vfs::host::HostError;

/// Format: `EEXIST` (Linux and macOS): the name exists, the errno the engine tells that kind apart by.
pub(crate) const ERRNO_EXIST: i32 = 17;
/// Format: `ENOSPC` (Linux and macOS): no room, the errno the engine fails an entry alone by.
pub(crate) const ERRNO_NOSPC: i32 = 28;
/// Format: `ERROR_FILE_EXISTS` (winerror.h): a file of that name exists.
const FILE_EXISTS: u32 = 80;
/// Format: `ERROR_ALREADY_EXISTS` (winerror.h): an object of that name exists.
const ALREADY_EXISTS: u32 = 183;
/// Format: `ERROR_HANDLE_DISK_FULL` (winerror.h): the disk is full.
const HANDLE_DISK_FULL: u32 = 39;
/// Format: `ERROR_DISK_FULL` (winerror.h): there is not enough space on the disk.
const DISK_FULL: u32 = 112;

/// The seam's refusal for the Win32 error code `code`: a name that exists or a full disk as the errno of that kind,
/// which the engine tells apart; any other code as the Win32 code it is, which no reader takes for an errno.
pub(crate) fn refusal_of(code: u32) -> HostError {
  match code {
    FILE_EXISTS | ALREADY_EXISTS => HostError::Unavailable(ERRNO_EXIST),
    HANDLE_DISK_FULL | DISK_FULL => HostError::Unavailable(ERRNO_NOSPC),
    other => HostError::Win32(other),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_vfs::host::OsCode;

  /// Format: the Win32 codes whose numbers the engine reads as errnos' kinds (winerror.h): `ERROR_NOT_SAME_DEVICE`
  /// (17, `EEXIST`), `ERROR_BAD_COMMAND` (22, `EINVAL`, a filesystem without an atomic exchange), `ERROR_OUT_OF_PAPER`
  /// (28, `ENOSPC`), `ERROR_TOO_MANY_SESS` (69, macOS's `EDQUOT`) and `ERROR_INSUFFICIENT_BUFFER` (122, Linux's `EDQUOT`).
  const COLLIDING: [u32; 5] = [17, 22, 28, 69, 122];
  /// Shape: every code Win32's system error codes reach (winerror.h numbers them below 16,000), and past it.
  const CODES: u32 = 1 << 16;

  /// A-527 (the seam's numbering is typed). Do: map through the writer's rule every Win32 code up to 65,535, the five
  /// whose numbers the engine reads as errnos' kinds among them, and the four the writer gives kinds. Expect a name that
  /// exists (`ERROR_FILE_EXISTS`, `ERROR_ALREADY_EXISTS`) as `EEXIST` and a full disk (`ERROR_HANDLE_DISK_FULL`,
  /// `ERROR_DISK_FULL`) as `ENOSPC`, and every other code kept as the Win32 code it is: `ERROR_NOT_SAME_DEVICE` is never
  /// the name that exists its number names in POSIX. Before A-527 every other code passed on as a bare number.
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
    for code in COLLIDING {
      assert_eq!(refusal_of(code), HostError::Win32(code), "{code}");
    }
    for code in 0..CODES {
      if ![FILE_EXISTS, ALREADY_EXISTS, HANDLE_DISK_FULL, DISK_FULL].contains(&code) {
        assert_eq!(refusal_of(code), HostError::Win32(code), "{code}");
      }
    }
  }
}
