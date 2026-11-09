//! Windows symbolic links on the drive (A-TBD): a volume symlink is the Windows one. Windows asks a file system for a
//! link as a reparse point, an `IO_REPARSE_TAG_SYMLINK` `REPARSE_DATA_BUFFER` (MS-FSCC 2.1.2.4) naming a substitute
//! name, a print name and whether the substitute is relative to the link's directory. The volume keeps one target
//! string per link, the form every platform reads (`readlink`). This module is the map between them, host-buildable and
//! tested on any host; the mount host (`crate::host`) answers WinFsp's reparse callbacks with it.
//!
//! **The stored form.** A relative link keeps its substitute with `/` for `\`, so Linux and macOS follow it too: a
//! Windows `..\lib\x` is the volume's `../lib/x`, and a link Linux made to `../lib/x` reads on Windows as `..\lib\x`.
//! A device-relative substitute (`\usr\bin`, relative to the drive's root) becomes `/usr/bin`, which is what a Linux
//! link to `/usr/bin` reads as on Windows. An absolute link keeps the Win32 path its NT substitute names: `\??\C:\x`
//! is kept as `C:\x`, and `\??\UNC\srv\share\x` as `\\srv\share\x`, so the stored target says which it was by its
//! first characters (a drive letter and `:\`, or `\\`), and a relative target never starts so since its separators are
//! `/`. An absolute substitute in any other NT form (`\??\Volume{…}\`, `\Device\…`) has no such Win32 path and is
//! refused (`STATUS_IO_REPARSE_DATA_INVALID`).
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

/// The target the volume keeps for a symbolic link reparse buffer Windows set (`FSCTL_SET_REPARSE_POINT`, as
/// `CreateSymbolicLink` sends it). The print name is checked to lie within the buffer but not kept (the module's
/// "What is not kept").
pub fn target(data: &[u8]) -> Result<String, Refusal> {
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
    return Ok(substitute.replace('\\', "/"));
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

  /// The three fields winfstest's `t/reparse/02.t` reads back from a buffer: substitute, print and flags.
  fn read_back(data: &[u8]) -> (String, String, u32) {
    let flags = u32::from_le_bytes(data[16..20].try_into().unwrap());
    (name(data, 8).unwrap(), name(data, 12).unwrap(), flags)
  }

  #[test]
  fn a_relative_link_is_kept_with_slashes_and_reads_back_as_windows_set_it() {
    let set = windows_buffer("dir\\name", "dir\\name", 1);
    assert_eq!(target(&set), Ok("dir/name".to_owned()));
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
    assert_eq!(target(&set), Ok("X:\\work\\b".to_owned()));
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
    assert_eq!(target(&set), Ok("\\\\srv\\share\\x".to_owned()));
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

  #[test]
  fn a_link_made_on_linux_reads_on_windows_relative_to_the_drive() {
    // `../lib` relative to the link's directory; `/usr/bin` relative to the drive's root (a device-relative name,
    // which WinFsp's resolver splices in from the root).
    assert_eq!(
      read_back(&reparse_data("../lib").unwrap()),
      ("..\\lib".into(), "..\\lib".into(), 1)
    );
    assert_eq!(
      read_back(&reparse_data("/usr/bin").unwrap()),
      ("\\usr\\bin".into(), "\\usr\\bin".into(), 1)
    );
    assert_eq!(
      target(&windows_buffer("\\usr\\bin", "\\usr\\bin", 1)),
      Ok("/usr/bin".to_owned())
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
        target(&windows_buffer(substitute, "p", 0)),
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
    assert_eq!(target(&mount_point), Err(Refusal::Tag));
    assert_eq!(Refusal::Tag.status(), Ntstatus(0xC000_0010));
  }

  #[test]
  fn a_malformed_buffer_is_refused_not_read_past() {
    let good = windows_buffer("a", "a", 1);
    assert_eq!(target(&good[..3]), Err(Refusal::Malformed));
    assert_eq!(
      target(&good[..good.len() - 1]),
      Err(Refusal::Malformed),
      "length disagrees"
    );
    let mut far = good.clone();
    far[8..10].copy_from_slice(&200u16.to_le_bytes());
    assert_eq!(
      target(&far),
      Err(Refusal::Malformed),
      "a name past the buffer"
    );
    let mut odd = good.clone();
    odd[10..12].copy_from_slice(&1u16.to_le_bytes());
    assert_eq!(target(&odd), Err(Refusal::Malformed), "half a UTF-16 unit");
    assert_eq!(
      target(&windows_buffer("", "", 1)),
      Err(Refusal::Malformed),
      "no target"
    );
    let mut lone = windows_buffer("ab", "ab", 1);
    lone[20..22].copy_from_slice(&0xD800u16.to_le_bytes());
    assert_eq!(
      target(&lone),
      Err(Refusal::Malformed),
      "an unpaired surrogate"
    );
  }

  #[test]
  fn a_target_too_long_for_the_buffer_is_refused() {
    let long = "a".repeat(20_000);
    assert_eq!(reparse_data(&long), Err(Refusal::TooLong));
  }
}
