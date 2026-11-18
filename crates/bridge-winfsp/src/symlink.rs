//! Windows symbolic links on the drive (A-275): a volume symlink is the Windows one. The map between the volume's target
//! and the `IO_REPARSE_TAG_SYMLINK` reparse buffer is `slates_vfs::host::windows_link` (moved there by A-438, so a
//! Windows landing writes the form this drive shows), re-exported here; this module keeps what is the drive's own: the
//! `NTSTATUS` each refusal answers Windows with ([`status_of`]) and NTFS's privilege rule read from a caller's token
//! ([`holds_enabled_privilege`]). The mount host (`crate::host`) answers WinFsp's reparse callbacks with them.

use crate::Ntstatus;

/// The shared form (A-275, A-438): the buffer a link is shown as, the target a set buffer keeps, the tag a buffer names.
pub use slates_vfs::host::windows_link::{IO_REPARSE_TAG_SYMLINK, Refusal, reparse_data, tag, target};

/// Format: `STATUS_IO_REPARSE_DATA_INVALID` (`0xC000_0278`): the buffer is not a well-formed symbolic link, or names
/// an absolute target that has no Win32 path.
const STATUS_IO_REPARSE_DATA_INVALID: Ntstatus = Ntstatus(0xC000_0278);
/// Format: `STATUS_INVALID_DEVICE_REQUEST` (`0xC000_0010`, Win32 `ERROR_INVALID_FUNCTION`): the drive keeps no reparse
/// data but a symbolic link's, and answers any other tag as it answered every reparse request before links were served.
const STATUS_INVALID_DEVICE_REQUEST: Ntstatus = Ntstatus(0xC000_0010);
/// Format: `STATUS_NAME_TOO_LONG` (`0xC000_0106`): a target whose names do not fit a reparse buffer's 16-bit lengths.
const STATUS_NAME_TOO_LONG: Ntstatus = Ntstatus(0xC000_0106);

/// Format: `STATUS_REPARSE` (`0x0000_0104`): `GetSecurityByName` found a link before the path's last component, and
/// its attribute out-parameter carries the link's index instead (WinFsp's contract, `winfsp.h` v2.1).
pub const STATUS_REPARSE: Ntstatus = Ntstatus(0x0000_0104);
/// Format: `STATUS_NOT_A_REPARSE_POINT` (`0xC000_0275`): the object is no link.
pub const STATUS_NOT_A_REPARSE_POINT: Ntstatus = Ntstatus(0xC000_0275);
/// Format: `STATUS_IO_REPARSE_TAG_MISMATCH` (`0xC000_0277`): a delete named another tag than the link's.
pub const STATUS_IO_REPARSE_TAG_MISMATCH: Ntstatus = Ntstatus(0xC000_0277);
/// Format: `STATUS_BUFFER_TOO_SMALL` (`0xC000_0023`): the caller's buffer cannot hold the link's reparse buffer.
pub const STATUS_BUFFER_TOO_SMALL: Ntstatus = Ntstatus(0xC000_0023);

/// Format: `STATUS_PRIVILEGE_NOT_HELD` (`0xC000_0061`, Win32 `ERROR_PRIVILEGE_NOT_HELD`): NTFS's answer to a symbolic link
/// set by a caller without `SeCreateSymbolicLinkPrivilege`, outside Developer Mode (A-275).
pub const STATUS_PRIVILEGE_NOT_HELD: Ntstatus = Ntstatus(0xC000_0061);
/// Format: a `TOKEN_PRIVILEGES` (`winnt.h`): its `PrivilegeCount` (four bytes), then that many `LUID_AND_ATTRIBUTES`, each
/// a `LUID` (`LowPart`, four bytes; `HighPart`, four) and its `Attributes` (four).
const PRIVILEGE_ENTRY: usize = 3 * size_of::<u32>();

/// Format: `SE_PRIVILEGE_ENABLED` (`winnt.h`), the attribute of a privilege enabled in the token.
const SE_PRIVILEGE_ENABLED: u32 = 0x0000_0002;

/// Whether a token's privileges (`GetTokenInformation(TokenPrivileges)`'s bytes) hold the privilege `luid` names enabled,
/// as NTFS's check asks (`SeSinglePrivilegeCheck`). `CreateSymbolicLinkW` enables `SeCreateSymbolicLinkPrivilege` around
/// its whole operation, before its create, so on that path the token read at the create shows it enabled; a holder that
/// never enables it is refused, as NTFS refuses it (A-275). A short or malformed buffer holds nothing.
pub fn holds_enabled_privilege(token_privileges: &[u8], luid: u32) -> bool {
  let Some((count, entries)) = token_privileges.split_first_chunk::<{ size_of::<u32>() }>() else {
    return false;
  };
  let count = usize::try_from(u32::from_le_bytes(*count)).unwrap_or(0);
  let (entries, _) = entries.as_chunks::<PRIVILEGE_ENTRY>();
  entries.iter().take(count).any(|entry| {
    let (luid_parts, _) = entry.as_chunks::<{ size_of::<u32>() }>();
    matches!(luid_parts, [low, high, attributes]
      if u32::from_le_bytes(*low) == luid
        && u32::from_le_bytes(*high) == 0
        && u32::from_le_bytes(*attributes) & SE_PRIVILEGE_ENABLED != 0)
  })
}

/// The `NTSTATUS` Windows is answered with for a link the drive could not map (A-275).
pub fn status_of(refusal: Refusal) -> Ntstatus {
  match refusal {
    Refusal::Malformed | Refusal::Foreign => STATUS_IO_REPARSE_DATA_INVALID,
    Refusal::Tag => STATUS_INVALID_DEVICE_REQUEST,
    Refusal::TooLong => STATUS_NAME_TOO_LONG,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A `TOKEN_PRIVILEGES` holding `privileges` (each a LUID's low part and its attributes), as Windows lays it out.
  fn token_privileges(privileges: &[(u32, u32)]) -> Vec<u8> {
    let mut bytes = u32::try_from(privileges.len())
      .unwrap()
      .to_le_bytes()
      .to_vec();
    for (low, attributes) in privileges {
      bytes.extend_from_slice(&low.to_le_bytes());
      bytes.extend_from_slice(&0u32.to_le_bytes());
      bytes.extend_from_slice(&attributes.to_le_bytes());
    }
    bytes
  }

  /// A-275 (who may make a link, NTFS's check). Do: ask whether tokens hold `SeCreateSymbolicLinkPrivilege` (LUID 35)
  /// enabled: one holding it enabled, one enabled by default too (attributes 3), one holding it disabled (an
  /// administrator's before `CreateSymbolicLinkW` enables it), one without it, one claiming more entries than it carries,
  /// and a truncated one. Expect: yes, yes, then no for each of the rest.
  #[test]
  fn a_token_holds_the_link_privilege_only_enabled() {
    const LINK: u32 = 35;
    assert!(holds_enabled_privilege(
      &token_privileges(&[(23, 3), (LINK, 2)]),
      LINK
    ));
    assert!(holds_enabled_privilege(
      &token_privileges(&[(LINK, 3)]),
      LINK
    ));
    assert!(
      !holds_enabled_privilege(&token_privileges(&[(LINK, 0)]), LINK),
      "held, disabled"
    );
    assert!(!holds_enabled_privilege(
      &token_privileges(&[(23, 3), (19, 2)]),
      LINK
    ));
    let mut overclaimed = token_privileges(&[(23, 3)]);
    overclaimed[..4].copy_from_slice(&9u32.to_le_bytes());
    assert!(!holds_enabled_privilege(&overclaimed, LINK));
    assert!(!holds_enabled_privilege(
      &token_privileges(&[(LINK, 2)])[..10],
      LINK
    ));
    assert!(!holds_enabled_privilege(&[], LINK));
  }

  /// A-275. Do: map each refusal of the shared form to the status the drive answers Windows with. Expect NTFS's: an
  /// invalid buffer or a foreign absolute name `STATUS_IO_REPARSE_DATA_INVALID`, another tag as every reparse request was
  /// answered before links were served, and a name too long for the buffer `STATUS_NAME_TOO_LONG`.
  #[test]
  fn each_refusal_answers_windows_with_ntfs_status() {
    assert_eq!(status_of(Refusal::Malformed), Ntstatus(0xC000_0278));
    assert_eq!(status_of(Refusal::Foreign), Ntstatus(0xC000_0278));
    assert_eq!(status_of(Refusal::Tag), Ntstatus(0xC000_0010));
    assert_eq!(status_of(Refusal::TooLong), Ntstatus(0xC000_0106));
  }
}
