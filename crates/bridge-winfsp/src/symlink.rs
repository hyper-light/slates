//! Windows symbolic links on the drive (A-275): a volume symlink is the Windows one. Windows asks a file system for a
//! link as a reparse point, an `IO_REPARSE_TAG_SYMLINK` `REPARSE_DATA_BUFFER` (MS-FSCC 2.1.2.4) naming a substitute
//! name, a print name and whether the substitute is relative to the link's directory. The volume keeps one target
//! string per link, the form every platform reads (`readlink`). This module is the map between them, host-buildable and
//! tested on any host; the mount host (`crate::host`) answers WinFsp's reparse callbacks with it.
//!
//! **The stored form (A-275's separator rule).** A relative link means the same object on every platform (R8): it is
//! stored with `/` and presented to Windows with `\`, as Git for Windows writes a link (`mingw_create_symlink`) and reads
//! one back (`normalize_ntpath`). A Windows `..\lib\x` is the volume's `../lib/x`, and a link Linux made to `../lib/x`
//! reads on Windows as `..\lib\x`. A Windows link relative to the drive's root (`\x\y`, a device-relative substitute)
//! names an object in the volume, so it is stored as the path to it from the link's own directory (`../../x/y` for a
//! link at `\a\b\link`), which every platform resolves to the same object; Windows then reads it back in that form.
//! Every other target is presented as given and resolves only where it means something. A POSIX-absolute target
//! (`/usr/bin`, made on Linux or macOS) names a path outside the volume: Windows is shown it unchanged and absolute, and
//! it resolves to nothing on the drive. A Windows-absolute link keeps the Win32 path its NT substitute names
//! (`\??\C:\x` is kept as `C:\x`, `\??\UNC\srv\share\x` as `\\srv\share\x`), which Linux and macOS read unchanged and
//! do not resolve. An absolute substitute in any other NT form (`\??\Volume{…}\`, `\Device\…`) has no Win32 path and is
//! refused (`STATUS_IO_REPARSE_DATA_INVALID`).
//!
//! **The lossy case.** A POSIX name may hold `\`, which Windows takes for a separator. A Linux target `a\b` (one name)
//! reads on Windows as two components, and one that starts with a drive letter and `:\` (`C:\x`) or with `\\` reads as
//! a Windows-absolute path. Neither resolves to what Linux meant; Windows cannot name such a file in any case.
//!
//! **What is not kept.** The print name: it is derived from the substitute, as `CreateSymbolicLink` writes it (the
//! same name for a relative link; the Win32 path for an absolute one, its UNC form with one leading `\` dropped, which
//! is what winfstest's `t/reparse/02.t` expects of Windows). A link set with any other print name reads back with the
//! derived one.

use crate::Ntstatus;

/// Format: `IO_REPARSE_TAG_SYMLINK` (`ntifs.h`; MS-FSCC 2.1.2.1), the tag of a symbolic link's reparse buffer.
pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
/// Format: `SYMLINK_FLAG_RELATIVE` (MS-FSCC 2.1.2.4): the substitute name is relative to the link's directory.
const SYMLINK_FLAG_RELATIVE: u32 = 0x0000_0001;
/// Format: the bytes of a `REPARSE_DATA_BUFFER`'s header before its tag-specific data: `ReparseTag` (4),
/// `ReparseDataLength` (2) and `Reserved` (2) (MS-FSCC 2.1.2.2).
const HEADER: usize = 8;
/// Format: the bytes of a symbolic link's fields before its path buffer: the substitute name's offset and length, the
/// print name's offset and length (2 each) and `Flags` (4) (MS-FSCC 2.1.2.4).
const SYMLINK_FIELDS: usize = 12;
/// Derived: where a symbolic link's path buffer starts, the offset its name offsets count from.
const PATH_BUFFER: usize = HEADER + SYMLINK_FIELDS;
/// Format: where the substitute name's offset and length (two 16-bit fields) sit: first after the header.
const SUBSTITUTE_AT: usize = HEADER;
/// Format: where the print name's offset and length sit: after the substitute name's two 16-bit fields.
const PRINT_AT: usize = SUBSTITUTE_AT + 2 * size_of::<u16>();
/// Format: where `Flags` sits: after the print name's two 16-bit fields, and just before the path buffer.
const FLAGS_AT: usize = PRINT_AT + 2 * size_of::<u16>();
/// Format: the NT prefix of a Win32 path in the object manager's DOS-device directory, what an absolute link's
/// substitute name starts with.
const NT_PREFIX: &str = "\\??\\";
/// Format: the DOS-device name a UNC path is reached by under [`NT_PREFIX`] (`\??\UNC\server\share`).
const UNC_DEVICE: &str = "UNC\\";
/// Format: the Win32 prefix of a UNC path (`\\server\share`).
const UNC_PREFIX: &str = "\\\\";

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

/// Whether a token's privileges (`GetTokenInformation(TokenPrivileges)`'s bytes) hold the privilege `luid` names, enabled
/// or not: a holder may enable it, as `CreateSymbolicLinkW` does for an administrator, whose token holds
/// `SeCreateSymbolicLinkPrivilege` disabled (A-275). A short or malformed buffer holds nothing.
pub fn holds_privilege(token_privileges: &[u8], luid: u32) -> bool {
  let Some((count, entries)) = token_privileges.split_first_chunk::<{ size_of::<u32>() }>() else {
    return false;
  };
  let count = usize::try_from(u32::from_le_bytes(*count)).unwrap_or(0);
  let (entries, _) = entries.as_chunks::<PRIVILEGE_ENTRY>();
  entries.iter().take(count).any(|entry| {
    let (luid_parts, _) = entry.as_chunks::<{ size_of::<u32>() }>();
    matches!(luid_parts, [low, high, _] if u32::from_le_bytes(*low) == luid && u32::from_le_bytes(*high) == 0)
  })
}

/// Why a link could not be mapped between the volume and Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// The buffer is short, its lengths disagree, a name lies outside it, or a name is not UTF-16.
  Malformed,
  /// The buffer carries a tag other than a symbolic link's.
  Tag,
  /// An absolute substitute with no Win32 path (`\??\Volume{…}\`, `\Device\…`).
  Foreign,
  /// A target too long for a reparse buffer's 16-bit name lengths.
  TooLong,
}

impl Refusal {
  /// The `NTSTATUS` Windows is answered with.
  pub fn status(self) -> Ntstatus {
    match self {
      Refusal::Malformed | Refusal::Foreign => STATUS_IO_REPARSE_DATA_INVALID,
      Refusal::Tag => STATUS_INVALID_DEVICE_REQUEST,
      Refusal::TooLong => STATUS_NAME_TOO_LONG,
    }
  }
}

/// The reparse tag a buffer names, its first little-endian word; `None` when it is shorter than one.
pub fn tag(data: &[u8]) -> Option<u32> {
  let word = data.get(..size_of::<u32>())?;
  Some(u32::from_le_bytes(word.try_into().ok()?))
}

/// Whether a stored target is an absolute Win32 path: a drive letter, `:` and `\`, or a UNC path's `\\`.
fn is_absolute(target: &str) -> bool {
  let bytes = target.as_bytes();
  target.starts_with(UNC_PREFIX)
    || matches!(bytes, [letter, b':', b'\\', ..] if letter.is_ascii_alphabetic())
}

/// The substitute name, print name and flags Windows is shown for a stored target.
fn names(target: &str) -> (String, String, u32) {
  if target.starts_with('/') {
    return (target.to_owned(), target.to_owned(), 0);
  }
  if is_absolute(target) {
    match target.strip_prefix(UNC_PREFIX) {
      Some(rest) => (
        format!("{NT_PREFIX}{UNC_DEVICE}{rest}"),
        target.get(1..).unwrap_or(target).to_owned(),
        0,
      ),
      None => (format!("{NT_PREFIX}{target}"), target.to_owned(), 0),
    }
  } else {
    let windows = target.replace('/', "\\");
    (windows.clone(), windows, SYMLINK_FLAG_RELATIVE)
  }
}

/// A UTF-16 name's length in bytes as a reparse buffer's 16-bit field.
fn length(units: &[u16]) -> Result<u16, Refusal> {
  units
    .len()
    .checked_mul(size_of::<u16>())
    .and_then(|bytes| u16::try_from(bytes).ok())
    .ok_or(Refusal::TooLong)
}

/// The symbolic link reparse buffer Windows is shown for the volume's link to `target` (`FSCTL_GET_REPARSE_POINT`, and
/// each link WinFsp follows): the substitute name, then the print name, neither terminated.
pub fn reparse_data(target: &str) -> Result<Vec<u8>, Refusal> {
  let (substitute, print, flags) = names(target);
  let substitute: Vec<u16> = substitute.encode_utf16().collect();
  let print: Vec<u16> = print.encode_utf16().collect();
  let substitute_length = length(&substitute)?;
  let print_length = length(&print)?;
  let data_length = usize::from(substitute_length)
    .checked_add(usize::from(print_length))
    .and_then(|names| names.checked_add(SYMLINK_FIELDS))
    .and_then(|data| u16::try_from(data).ok())
    .ok_or(Refusal::TooLong)?;
  let mut data = Vec::with_capacity(HEADER.saturating_add(usize::from(data_length)));
  data.extend_from_slice(&IO_REPARSE_TAG_SYMLINK.to_le_bytes());
  data.extend_from_slice(&data_length.to_le_bytes());
  data.extend_from_slice(&0u16.to_le_bytes());
  data.extend_from_slice(&0u16.to_le_bytes());
  data.extend_from_slice(&substitute_length.to_le_bytes());
  data.extend_from_slice(&substitute_length.to_le_bytes());
  data.extend_from_slice(&print_length.to_le_bytes());
  data.extend_from_slice(&flags.to_le_bytes());
  for unit in substitute.iter().chain(&print) {
    data.extend_from_slice(&unit.to_le_bytes());
  }
  Ok(data)
}

/// A little-endian 16-bit field at `at`.
fn field(data: &[u8], at: usize) -> Result<u16, Refusal> {
  let end = at.checked_add(size_of::<u16>()).ok_or(Refusal::Malformed)?;
  let bytes = data.get(at..end).ok_or(Refusal::Malformed)?;
  Ok(u16::from_le_bytes(
    bytes.try_into().map_err(|_| Refusal::Malformed)?,
  ))
}

/// The name a symbolic link's offset and length fields at `at` give, decoded from the path buffer.
fn name(data: &[u8], at: usize) -> Result<String, Refusal> {
  let offset = usize::from(field(data, at)?);
  let length = usize::from(field(
    data,
    at.checked_add(size_of::<u16>()).ok_or(Refusal::Malformed)?,
  )?);
  if offset % 2 != 0 || length % 2 != 0 {
    return Err(Refusal::Malformed);
  }
  let start = PATH_BUFFER.checked_add(offset).ok_or(Refusal::Malformed)?;
  let end = start.checked_add(length).ok_or(Refusal::Malformed)?;
  let bytes = data.get(start..end).ok_or(Refusal::Malformed)?;
  // The length is even (checked), so the pairs cover the name and nothing is left over.
  let (pairs, _) = bytes.as_chunks::<{ size_of::<u16>() }>();
  let units: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
  String::from_utf16(&units).map_err(|_| Refusal::Malformed)
}

/// A substitute relative to the drive's root (`\x\y`) as the path to the same object from the directory of the link at
/// `link` (a WinFsp path, `\a\b\link`): one `..` for each of that directory's components, then the substitute's.
fn from_link_directory(link: &str, rooted: &str) -> String {
  let depth = link
    .split('\\')
    .filter(|part| !part.is_empty())
    .count()
    .saturating_sub(1);
  let parts: Vec<&str> = std::iter::repeat_n("..", depth)
    .chain(rooted.split('\\').filter(|part| !part.is_empty()))
    .collect();
  if parts.is_empty() {
    ".".to_owned()
  } else {
    parts.join("/")
  }
}

/// The target the volume keeps for a symbolic link reparse buffer Windows set (`FSCTL_SET_REPARSE_POINT`, as
/// `CreateSymbolicLink` sends it) on the link at `link`, a WinFsp path. The print name is checked to lie within the
/// buffer but not kept (the module's "What is not kept").
pub fn target(data: &[u8], link: &str) -> Result<String, Refusal> {
  if tag(data) != Some(IO_REPARSE_TAG_SYMLINK) {
    return match tag(data) {
      Some(_) => Err(Refusal::Tag),
      None => Err(Refusal::Malformed),
    };
  }
  let data_length = usize::from(field(data, size_of::<u32>())?);
  if data.len() < PATH_BUFFER || HEADER.checked_add(data_length) != Some(data.len()) {
    return Err(Refusal::Malformed);
  }
  let substitute = name(data, SUBSTITUTE_AT)?;
  name(data, PRINT_AT)?;
  let flags = data
    .get(FLAGS_AT..PATH_BUFFER)
    .and_then(|word| word.try_into().ok())
    .map(u32::from_le_bytes)
    .ok_or(Refusal::Malformed)?;
  if substitute.is_empty() {
    return Err(Refusal::Malformed);
  }
  if flags & SYMLINK_FLAG_RELATIVE != 0 {
    return Ok(match substitute.strip_prefix('\\') {
      Some(rooted) => from_link_directory(link, rooted),
      None => substitute.replace('\\', "/"),
    });
  }
  let path = substitute.strip_prefix(NT_PREFIX).ok_or(Refusal::Foreign)?;
  let target = match path.strip_prefix(UNC_DEVICE) {
    Some(rest) => format!("{UNC_PREFIX}{rest}"),
    None => path.to_owned(),
  };
  if is_absolute(&target) {
    Ok(target)
  } else {
    Err(Refusal::Foreign)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A symbolic link buffer as Windows lays it out, built field by field (MS-FSCC 2.1.2.4), so the tests do not use
  /// the encoder they check.
  fn windows_buffer(substitute: &str, print: &str, flags: u32) -> Vec<u8> {
    let substitute: Vec<u16> = substitute.encode_utf16().collect();
    let print: Vec<u16> = print.encode_utf16().collect();
    let (s_len, p_len) = (
      u16::try_from(substitute.len() * 2).unwrap(),
      u16::try_from(print.len() * 2).unwrap(),
    );
    let mut b = Vec::new();
    b.extend_from_slice(&0xA000_000Cu32.to_le_bytes());
    b.extend_from_slice(&(12 + s_len + p_len).to_le_bytes());
    b.extend_from_slice(&[0, 0]);
    // CreateSymbolicLinkW puts the print name second; offsets say where each lies.
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&s_len.to_le_bytes());
    b.extend_from_slice(&s_len.to_le_bytes());
    b.extend_from_slice(&p_len.to_le_bytes());
    b.extend_from_slice(&flags.to_le_bytes());
    for unit in substitute.iter().chain(&print) {
      b.extend_from_slice(&unit.to_le_bytes());
    }
    b
  }

  /// The target set on a link in the drive's root.
  fn target_at(data: &[u8]) -> Result<String, Refusal> {
    target(data, "\\link")
  }

  /// The three fields winfstest's `t/reparse/02.t` reads back from a buffer: substitute, print and flags.
  fn read_back(data: &[u8]) -> (String, String, u32) {
    let flags = u32::from_le_bytes(data[16..20].try_into().unwrap());
    (name(data, 8).unwrap(), name(data, 12).unwrap(), flags)
  }

  #[test]
  fn a_relative_link_is_kept_with_slashes_and_reads_back_as_windows_set_it() {
    let set = windows_buffer("dir\\name", "dir\\name", 1);
    assert_eq!(target_at(&set), Ok("dir/name".to_owned()));
    let shown = reparse_data("dir/name").unwrap();
    assert_eq!(tag(&shown), Some(IO_REPARSE_TAG_SYMLINK));
    assert_eq!(
      read_back(&shown),
      ("dir\\name".into(), "dir\\name".into(), 1)
    );
    assert_eq!(
      shown, set,
      "the buffer Windows set is the one it reads back"
    );
  }

  #[test]
  fn an_absolute_drive_link_keeps_its_win32_path() {
    // t/reparse/02.t: SubstituteName == "\\??\\" + dstpath, PrintName == dstpath, Flags == 0.
    let set = windows_buffer("\\??\\X:\\work\\b", "X:\\work\\b", 0);
    assert_eq!(target_at(&set), Ok("X:\\work\\b".to_owned()));
    let shown = reparse_data("X:\\work\\b").unwrap();
    assert_eq!(
      read_back(&shown),
      ("\\??\\X:\\work\\b".into(), "X:\\work\\b".into(), 0)
    );
  }

  #[test]
  fn a_unc_link_keeps_its_unc_path_and_prints_as_windows_does() {
    // t/reparse/02.t: SubstituteName == "\\??\\UNC" + dstpath[1:], PrintName == dstpath[1:].
    let set = windows_buffer("\\??\\UNC\\srv\\share\\x", "\\srv\\share\\x", 0);
    assert_eq!(target_at(&set), Ok("\\\\srv\\share\\x".to_owned()));
    let shown = reparse_data("\\\\srv\\share\\x").unwrap();
    assert_eq!(
      read_back(&shown),
      (
        "\\??\\UNC\\srv\\share\\x".into(),
        "\\srv\\share\\x".into(),
        0
      )
    );
  }

  /// A-275's separator rule. Do: read on Windows a relative link Linux made, and a POSIX-absolute one. Expect: the
  /// relative one with `\` and the relative flag; the absolute one as given and absolute (flags 0), since it names a
  /// path outside the volume.
  #[test]
  fn a_link_made_on_linux_reads_on_windows_with_its_meaning() {
    assert_eq!(
      read_back(&reparse_data("../lib/x").unwrap()),
      ("..\\lib\\x".into(), "..\\lib\\x".into(), 1)
    );
    assert_eq!(
      read_back(&reparse_data("/usr/bin").unwrap()),
      ("/usr/bin".into(), "/usr/bin".into(), 0)
    );
  }

  /// A-275. Do: set links relative to the drive's root (`\x\y`) on links at the root, one and two directories down.
  /// Expect: each stored as the path to `\x\y` from its own directory, so Linux and macOS reach the same object.
  #[test]
  fn a_link_relative_to_the_drives_root_is_kept_relative_to_its_directory() {
    let rooted = windows_buffer("\\x\\y", "\\x\\y", 1);
    assert_eq!(target(&rooted, "\\link"), Ok("x/y".to_owned()));
    assert_eq!(target(&rooted, "\\a\\link"), Ok("../x/y".to_owned()));
    assert_eq!(target(&rooted, "\\a\\b\\link"), Ok("../../x/y".to_owned()));
    let root = windows_buffer("\\", "\\", 1);
    assert_eq!(target(&root, "\\a\\link"), Ok("..".to_owned()));
    assert_eq!(target(&root, "\\link"), Ok(".".to_owned()));
  }

  /// A-275's lossy case. Do: read on Windows a link Linux made to a name holding `\`. Expect: Windows takes the `\` for
  /// a separator (two components), and a name that starts like a drive path reads as a Windows-absolute one.
  #[test]
  fn a_posix_name_holding_a_backslash_is_read_as_a_separator() {
    assert_eq!(
      read_back(&reparse_data("a\\b").unwrap()),
      ("a\\b".into(), "a\\b".into(), 1)
    );
    assert_eq!(
      read_back(&reparse_data("C:\\x").unwrap()).2,
      0,
      "taken for a drive path"
    );
  }

  #[test]
  fn an_absolute_name_with_no_win32_path_is_refused() {
    for substitute in [
      "\\??\\Volume{0}\\x",
      "\\Device\\HarddiskVolume1\\x",
      "C:\\x",
      "\\??\\rel",
    ] {
      assert_eq!(
        target_at(&windows_buffer(substitute, "p", 0)),
        Err(Refusal::Foreign),
        "{substitute}"
      );
    }
    assert_eq!(Refusal::Foreign.status(), Ntstatus(0xC000_0278));
  }

  #[test]
  fn another_tag_is_answered_as_before_links_were_served() {
    let mut mount_point = windows_buffer("\\??\\C:\\x", "C:\\x", 0);
    mount_point[..4].copy_from_slice(&0xA000_0003u32.to_le_bytes());
    assert_eq!(target_at(&mount_point), Err(Refusal::Tag));
    assert_eq!(Refusal::Tag.status(), Ntstatus(0xC000_0010));
  }

  #[test]
  fn a_malformed_buffer_is_refused_not_read_past() {
    let good = windows_buffer("a", "a", 1);
    assert_eq!(target_at(&good[..3]), Err(Refusal::Malformed));
    assert_eq!(
      target_at(&good[..good.len() - 1]),
      Err(Refusal::Malformed),
      "length disagrees"
    );
    let mut far = good.clone();
    far[8..10].copy_from_slice(&200u16.to_le_bytes());
    assert_eq!(
      target_at(&far),
      Err(Refusal::Malformed),
      "a name past the buffer"
    );
    let mut odd = good.clone();
    odd[10..12].copy_from_slice(&1u16.to_le_bytes());
    assert_eq!(
      target_at(&odd),
      Err(Refusal::Malformed),
      "half a UTF-16 unit"
    );
    assert_eq!(
      target_at(&windows_buffer("", "", 1)),
      Err(Refusal::Malformed),
      "no target"
    );
    let mut lone = windows_buffer("ab", "ab", 1);
    lone[20..22].copy_from_slice(&0xD800u16.to_le_bytes());
    assert_eq!(
      target_at(&lone),
      Err(Refusal::Malformed),
      "an unpaired surrogate"
    );
  }

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

  /// A-275 (who may make a link). Do: ask whether tokens hold `SeCreateSymbolicLinkPrivilege` (LUID 35): one holding it
  /// enabled, one holding it disabled (an administrator's, before `CreateSymbolicLinkW` enables it), one without it,
  /// one claiming more entries than it carries, and a truncated one. Expect: held, held, not, not, not.
  #[test]
  fn a_token_holds_the_link_privilege_enabled_or_not() {
    const LINK: u32 = 35;
    assert!(holds_privilege(
      &token_privileges(&[(23, 3), (LINK, 2)]),
      LINK
    ));
    assert!(holds_privilege(&token_privileges(&[(LINK, 0)]), LINK));
    assert!(!holds_privilege(
      &token_privileges(&[(23, 3), (19, 0)]),
      LINK
    ));
    let mut overclaimed = token_privileges(&[(23, 3)]);
    overclaimed[..4].copy_from_slice(&9u32.to_le_bytes());
    assert!(!holds_privilege(&overclaimed, LINK));
    assert!(!holds_privilege(
      &token_privileges(&[(LINK, 2)])[..10],
      LINK
    ));
    assert!(!holds_privilege(&[], LINK));
  }

  #[test]
  fn a_target_too_long_for_the_buffer_is_refused() {
    let long = "a".repeat(20_000);
    assert_eq!(reparse_data(&long), Err(Refusal::TooLong));
  }
}
