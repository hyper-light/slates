//! The records the Windows file APIs fill, parsed as untrusted bytes (§4.5, §4.15, AUD-29-62). The Windows
//! host lists a directory through its retained handle (`GetFileInformationByHandleEx` with
//! `FileFullDirectoryInfo`, a chain of `FILE_FULL_DIR_INFO` records) and reads a link through the link's
//! own handle (`FSCTL_GET_REPARSE_POINT`, a `REPARSE_DATA_BUFFER`). Both records carry their own offsets and
//! lengths, and the standard library's reader trusts them (`std::sys::fs::windows::File::readlink` slices
//! the path buffer at the record's offsets without checking them against the bytes returned). Here every
//! offset and length is checked against the bytes actually filled before anything is read, a chain must
//! advance past each record, and a malformed record is a typed refusal, never a panic or an out-of-bounds
//! read. The parsers are cfg-free so their golden and hostile-input tests run on every host; the Windows
//! host is their only caller.
//!
//! Layouts: Microsoft Learn, `FILE_FULL_DIR_INFO` (winbase.h) and `REPARSE_DATA_BUFFER` (ntifs.h). Windows
//! runs little-endian on every architecture it supports, so the fields are read little-endian.

/// Format: `FILE_FULL_DIR_INFO`'s fixed head — `NextEntryOffset` and `FileIndex` (4 bytes each), six 8-byte
/// times and sizes, then `FileAttributes`, `FileNameLength` and `EaSize` (4 bytes each): 68 bytes, the name
/// after it.
const DIRECTORY_RECORD_HEAD: usize = 68;
/// Format: `FILE_FULL_DIR_INFO.NextEntryOffset`, the byte distance to the next record (0 on the last).
const NEXT_RECORD_AT: usize = 0;
/// Format: `FILE_FULL_DIR_INFO.FileNameLength`, the name's length in bytes.
const NAME_BYTES_AT: usize = 60;

/// Format: `REPARSE_DATA_BUFFER`'s head — `ReparseTag` (4 bytes), `ReparseDataLength` and `Reserved` (2 bytes
/// each); the tag's own data follows.
const REPARSE_HEAD: usize = 8;
/// Format: `REPARSE_DATA_BUFFER.ReparseDataLength`, the data's length in bytes after the head.
const REPARSE_DATA_BYTES_AT: usize = 4;
/// Format: the four name fields both link layouts open their data with — `SubstituteNameOffset`,
/// `SubstituteNameLength`, `PrintNameOffset`, `PrintNameLength` (2 bytes each), offsets into the path buffer.
const SUBSTITUTE_OFFSET_AT: usize = REPARSE_HEAD;
/// Format: see [`SUBSTITUTE_OFFSET_AT`].
const SUBSTITUTE_BYTES_AT: usize = REPARSE_HEAD + 2;
/// Format: see [`SUBSTITUTE_OFFSET_AT`].
const PRINT_OFFSET_AT: usize = REPARSE_HEAD + 4;
/// Format: see [`SUBSTITUTE_OFFSET_AT`].
const PRINT_BYTES_AT: usize = REPARSE_HEAD + 6;
/// Format: a mount point's (junction's) path buffer follows the four name fields.
const MOUNT_POINT_PATHS_AT: usize = REPARSE_HEAD + 8;
/// Format: a symbolic link's path buffer follows the four name fields and its 4-byte `Flags`.
const SYMLINK_PATHS_AT: usize = REPARSE_HEAD + 12;

/// Format: `IO_REPARSE_TAG_MOUNT_POINT`, a junction or volume mount point (ntifs.h).
pub(crate) const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
/// Format: `IO_REPARSE_TAG_SYMLINK`, a symbolic link (ntifs.h).
pub(crate) const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
/// Format: the name-surrogate bit of a reparse tag (`IsReparseTagNameSurrogate`, ntifs.h): the reparse point
/// stands for another named entity — it redirects the namespace — as symbolic links, junctions and mount
/// points do. A tag without it (deduplication, cloud placeholders, WOF compression) is the entry's own data.
const NAME_SURROGATE_BIT: u32 = 0x2000_0000;

/// Format: the NT object-manager prefix an absolute link target is stored under (`\??\`), and the Win32
/// verbatim prefix (`\\?\`) a reader shows in its place, as the standard library's `read_link` does.
const NT_PREFIX: [u16; 4] = [0x5C, 0x3F, 0x3F, 0x5C];
/// Format: see [`NT_PREFIX`].
const VERBATIM_PREFIX: [u16; 4] = [0x5C, 0x5C, 0x3F, 0x5C];

/// Why a record was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordRefusal {
  /// A field or a name the record names ends past the bytes filled; `at` is the record's offset.
  Truncated {
    /// The record's offset in the buffer.
    at: usize,
  },
  /// A chained record's next offset does not advance past the record itself: a loop, or an overlap.
  Overlapping {
    /// The record's offset in the buffer.
    at: usize,
  },
  /// A name's byte length is odd: not UTF-16.
  OddNameLength {
    /// The record's offset in the buffer.
    at: usize,
  },
  /// A reparse tag this reader has no layout for.
  UnsupportedTag(u32),
}

/// Whether a reparse tag redirects the namespace (a symbolic link, a junction, a mount point).
pub(crate) fn is_name_surrogate(tag: u32) -> bool {
  tag & NAME_SURROGATE_BIT != 0
}

/// The little-endian `u32` at `at`, if the bytes hold it.
fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
  let end = at.checked_add(size_of::<u32>())?;
  let field: [u8; 4] = bytes.get(at..end)?.try_into().ok()?;
  Some(u32::from_le_bytes(field))
}

/// The little-endian `u16` at `at`, if the bytes hold it.
fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
  let end = at.checked_add(size_of::<u16>())?;
  let field: [u8; 2] = bytes.get(at..end)?.try_into().ok()?;
  Some(u16::from_le_bytes(field))
}

/// The UTF-16 units of `length` bytes at `at`, if the bytes hold them and the length is even.
fn units_at(
  bytes: &[u8],
  at: usize,
  length: usize,
  record: usize,
) -> Result<Vec<u16>, RecordRefusal> {
  if !length.is_multiple_of(size_of::<u16>()) {
    return Err(RecordRefusal::OddNameLength { at: record });
  }
  let end = at
    .checked_add(length)
    .ok_or(RecordRefusal::Truncated { at: record })?;
  let raw = bytes
    .get(at..end)
    .ok_or(RecordRefusal::Truncated { at: record })?;
  let (pairs, _) = raw.as_chunks::<2>();
  Ok(pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect())
}

/// The names in a `FILE_FULL_DIR_INFO` chain, in order, as UTF-16 units (`.` and `..` included; the caller
/// skips them). `bytes` is the whole buffer the call filled from its start; the chain ends at a record whose
/// next offset is 0. Every record must fit, every name must fit inside the buffer, and every next offset
/// must step past its own record — so the walk ends within `bytes.len() / 68` records.
pub(crate) fn directory_names(bytes: &[u8]) -> Result<Vec<Vec<u16>>, RecordRefusal> {
  let mut names = Vec::new();
  let mut at = 0usize;
  loop {
    let next =
      u32_at(bytes, at.saturating_add(NEXT_RECORD_AT)).ok_or(RecordRefusal::Truncated { at })?;
    let name_bytes =
      u32_at(bytes, at.saturating_add(NAME_BYTES_AT)).ok_or(RecordRefusal::Truncated { at })?;
    let name_bytes = usize::try_from(name_bytes).map_err(|_| RecordRefusal::Truncated { at })?;
    let head_end = at
      .checked_add(DIRECTORY_RECORD_HEAD)
      .ok_or(RecordRefusal::Truncated { at })?;
    if head_end > bytes.len() {
      return Err(RecordRefusal::Truncated { at });
    }
    names.push(units_at(bytes, head_end, name_bytes, at)?);
    if next == 0 {
      return Ok(names);
    }
    let next = usize::try_from(next).map_err(|_| RecordRefusal::Truncated { at })?;
    let record_bytes = DIRECTORY_RECORD_HEAD.saturating_add(name_bytes);
    if next < record_bytes {
      return Err(RecordRefusal::Overlapping { at });
    }
    at = at
      .checked_add(next)
      .ok_or(RecordRefusal::Truncated { at })?;
  }
}

/// The target a link's `REPARSE_DATA_BUFFER` names, as UTF-16 units: its print name — the form a user typed
/// and `dir` shows — or, when the creator left that empty, its substitute name with an absolute target's
/// `\??\` shown as the verbatim `\\?\`. `bytes` is exactly what `FSCTL_GET_REPARSE_POINT` returned. A tag
/// other than a symbolic link or a mount point is refused.
pub(crate) fn link_target(bytes: &[u8]) -> Result<Vec<u16>, RecordRefusal> {
  const RECORD: usize = 0;
  let truncated = RecordRefusal::Truncated { at: RECORD };
  let tag = u32_at(bytes, 0).ok_or(truncated)?;
  let paths_at = match tag {
    IO_REPARSE_TAG_SYMLINK => SYMLINK_PATHS_AT,
    IO_REPARSE_TAG_MOUNT_POINT => MOUNT_POINT_PATHS_AT,
    other => return Err(RecordRefusal::UnsupportedTag(other)),
  };
  let data_bytes = usize::from(u16_at(bytes, REPARSE_DATA_BYTES_AT).ok_or(truncated)?);
  let data_end = REPARSE_HEAD.checked_add(data_bytes).ok_or(truncated)?;
  // Everything the record names must lie inside its own declared data, and that inside the bytes returned.
  let data = bytes.get(..data_end).ok_or(truncated)?;
  if paths_at > data.len() {
    return Err(truncated);
  }
  let field = |at: usize| u16_at(data, at).map(usize::from).ok_or(truncated);
  let print = units_at(
    data,
    paths_at
      .checked_add(field(PRINT_OFFSET_AT)?)
      .ok_or(truncated)?,
    field(PRINT_BYTES_AT)?,
    RECORD,
  )?;
  if !print.is_empty() {
    return Ok(print);
  }
  let mut substitute = units_at(
    data,
    paths_at
      .checked_add(field(SUBSTITUTE_OFFSET_AT)?)
      .ok_or(truncated)?,
    field(SUBSTITUTE_BYTES_AT)?,
    RECORD,
  )?;
  if substitute.starts_with(&NT_PREFIX) {
    for (unit, verbatim) in substitute.iter_mut().zip(VERBATIM_PREFIX) {
      *unit = verbatim;
    }
  }
  Ok(substitute)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A `FILE_FULL_DIR_INFO` record for `name`, its next offset set to `next` (0 for the last).
  fn directory_record(name: &str, next: u32) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let mut record = vec![0u8; DIRECTORY_RECORD_HEAD];
    record[NEXT_RECORD_AT..NEXT_RECORD_AT + 4].copy_from_slice(&next.to_le_bytes());
    let name_bytes = u32::try_from(units.len() * 2).unwrap();
    record[NAME_BYTES_AT..NAME_BYTES_AT + 4].copy_from_slice(&name_bytes.to_le_bytes());
    for unit in units {
      record.extend_from_slice(&unit.to_le_bytes());
    }
    record
  }

  /// A chain of records as the call lays them out: each padded to the 8-byte record alignment, each next
  /// offset its padded length, the last 0.
  fn chain(names: &[&str]) -> Vec<u8> {
    /// Format: directory records start on 8-byte boundaries.
    const ALIGN: usize = 8;
    let mut out = Vec::new();
    for (position, name) in names.iter().enumerate() {
      let unpadded = DIRECTORY_RECORD_HEAD + name.encode_utf16().count() * 2;
      let padded = unpadded.div_ceil(ALIGN) * ALIGN;
      let next = if position + 1 == names.len() {
        0
      } else {
        u32::try_from(padded).unwrap()
      };
      let mut record = directory_record(name, next);
      record.resize(padded, 0);
      out.extend(record);
    }
    out
  }

  fn text(units: &[u16]) -> String {
    String::from_utf16(units).unwrap()
  }

  /// AUD-29-62 (golden). Do: parse a chain of three records laid out as the call fills them, then one with
  /// its tail of unused buffer. Expect: the three names in order, `.` and `..` included, the tail ignored.
  #[test]
  fn a_directory_chain_yields_its_names_in_order() {
    let mut bytes = chain(&[".", "..", "naïve file.txt"]);
    let names: Vec<String> = directory_names(&bytes)
      .unwrap()
      .iter()
      .map(|n| text(n))
      .collect();
    assert_eq!(names, [".", "..", "naïve file.txt"]);
    bytes.resize(bytes.len() + 4096, 0xAB);
    assert_eq!(directory_names(&bytes).unwrap().len(), 3);
  }

  /// AUD-29-62 (hostile input). Do: hand the directory parser an empty buffer, a cut head, a name longer
  /// than the buffer (and the largest even `u32` long), an odd name length, a next offset of the record's own head (a
  /// loop back into itself) and one past the buffer. Expect: each is a typed refusal naming the record, and
  /// nothing is read out of bounds.
  #[test]
  fn a_malformed_directory_chain_is_refused_typed() {
    assert_eq!(
      directory_names(&[]),
      Err(RecordRefusal::Truncated { at: 0 })
    );
    let whole = chain(&["entry"]);
    assert_eq!(
      directory_names(&whole[..DIRECTORY_RECORD_HEAD - 1]),
      Err(RecordRefusal::Truncated { at: 0 })
    );
    let mut long = whole.clone();
    long[NAME_BYTES_AT..NAME_BYTES_AT + 4].copy_from_slice(&64u32.to_le_bytes());
    assert_eq!(
      directory_names(&long),
      Err(RecordRefusal::Truncated { at: 0 })
    );
    // The largest even length (`u32::MAX` itself is odd, refused as not UTF-16 before its bounds).
    long[NAME_BYTES_AT..NAME_BYTES_AT + 4].copy_from_slice(&(u32::MAX - 1).to_le_bytes());
    assert_eq!(
      directory_names(&long),
      Err(RecordRefusal::Truncated { at: 0 })
    );
    let mut odd = whole.clone();
    odd[NAME_BYTES_AT..NAME_BYTES_AT + 4].copy_from_slice(&3u32.to_le_bytes());
    assert_eq!(
      directory_names(&odd),
      Err(RecordRefusal::OddNameLength { at: 0 })
    );
    let mut looping = chain(&["a", "b"]);
    looping[NEXT_RECORD_AT..NEXT_RECORD_AT + 4].copy_from_slice(&4u32.to_le_bytes());
    assert_eq!(
      directory_names(&looping),
      Err(RecordRefusal::Overlapping { at: 0 })
    );
    let mut past = chain(&["a", "b"]);
    past[NEXT_RECORD_AT..NEXT_RECORD_AT + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(
      directory_names(&past),
      Err(RecordRefusal::Truncated { .. })
    ));
  }

  /// A `REPARSE_DATA_BUFFER` for `tag` with the substitute and print names, laid out as the filesystem
  /// returns it: substitute first, print after.
  fn reparse(tag: u32, substitute: &str, print: &str, flags: u32) -> Vec<u8> {
    let substitute: Vec<u16> = substitute.encode_utf16().collect();
    let print: Vec<u16> = print.encode_utf16().collect();
    let paths_at = if tag == IO_REPARSE_TAG_SYMLINK {
      SYMLINK_PATHS_AT
    } else {
      MOUNT_POINT_PATHS_AT
    };
    let substitute_bytes = u16::try_from(substitute.len() * 2).unwrap();
    let print_bytes = u16::try_from(print.len() * 2).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&tag.to_le_bytes());
    let data_bytes =
      u16::try_from(paths_at - REPARSE_HEAD).unwrap() + substitute_bytes + print_bytes;
    out.extend_from_slice(&data_bytes.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&substitute_bytes.to_le_bytes());
    out.extend_from_slice(&substitute_bytes.to_le_bytes());
    out.extend_from_slice(&print_bytes.to_le_bytes());
    if tag == IO_REPARSE_TAG_SYMLINK {
      out.extend_from_slice(&flags.to_le_bytes());
    }
    for unit in substitute.iter().chain(&print) {
      out.extend_from_slice(&unit.to_le_bytes());
    }
    out
  }

  /// AUD-29-62 (golden). Do: read the target of a relative symbolic link, an absolute one, a junction whose
  /// creator set a print name, and one whose creator left it empty. Expect: the print name when present;
  /// otherwise the substitute name with `\??\` shown as `\\?\`.
  #[test]
  fn a_link_record_yields_the_target_a_user_sees() {
    /// Format: `SYMLINK_FLAG_RELATIVE`.
    const RELATIVE: u32 = 1;
    let relative = reparse(IO_REPARSE_TAG_SYMLINK, "sub", "sub", RELATIVE);
    assert_eq!(text(&link_target(&relative).unwrap()), "sub");
    let absolute = reparse(
      IO_REPARSE_TAG_SYMLINK,
      "\\??\\C:\\outside",
      "C:\\outside",
      0,
    );
    assert_eq!(text(&link_target(&absolute).unwrap()), "C:\\outside");
    let junction = reparse(
      IO_REPARSE_TAG_MOUNT_POINT,
      "\\??\\D:\\elsewhere",
      "D:\\elsewhere",
      0,
    );
    assert_eq!(text(&link_target(&junction).unwrap()), "D:\\elsewhere");
    let unprinted = reparse(IO_REPARSE_TAG_MOUNT_POINT, "\\??\\D:\\elsewhere", "", 0);
    assert_eq!(
      text(&link_target(&unprinted).unwrap()),
      "\\\\?\\D:\\elsewhere"
    );
    assert!(is_name_surrogate(IO_REPARSE_TAG_SYMLINK));
    assert!(is_name_surrogate(IO_REPARSE_TAG_MOUNT_POINT));
  }

  /// AUD-29-62 (hostile input). Do: hand the link parser an empty buffer, a cut head, a declared data length
  /// past the bytes returned, a name offset and a name length past the declared data, an odd name length,
  /// and a tag with no link layout (deduplication, `0x8000_0013`). Expect: each is a typed refusal; a name
  /// outside the record's own declared data is never read, even when the buffer holds more bytes.
  #[test]
  fn a_malformed_link_record_is_refused_typed() {
    let truncated = Err(RecordRefusal::Truncated { at: 0 });
    assert_eq!(link_target(&[]), truncated);
    let good = reparse(IO_REPARSE_TAG_SYMLINK, "sub", "sub", 1);
    assert_eq!(link_target(&good[..6]), truncated);
    let mut long_data = good.clone();
    long_data[REPARSE_DATA_BYTES_AT..REPARSE_DATA_BYTES_AT + 2]
      .copy_from_slice(&u16::MAX.to_le_bytes());
    assert_eq!(link_target(&long_data), truncated);
    let mut far_name = good.clone();
    far_name[PRINT_OFFSET_AT..PRINT_OFFSET_AT + 2].copy_from_slice(&u16::MAX.to_le_bytes());
    assert_eq!(link_target(&far_name), truncated);
    // The buffer holds bytes past the declared data; a name reaching into them is still refused.
    let mut beyond = good.clone();
    beyond.extend_from_slice(&[0x41, 0x00, 0x41, 0x00]);
    beyond[PRINT_BYTES_AT..PRINT_BYTES_AT + 2].copy_from_slice(&10u16.to_le_bytes());
    assert_eq!(link_target(&beyond), truncated);
    let mut odd = good.clone();
    odd[PRINT_BYTES_AT..PRINT_BYTES_AT + 2].copy_from_slice(&3u16.to_le_bytes());
    assert_eq!(
      link_target(&odd),
      Err(RecordRefusal::OddNameLength { at: 0 })
    );
    /// Format: `IO_REPARSE_TAG_DEDUP`, a tag with data but no link.
    const DEDUP: u32 = 0x8000_0013;
    let mut dedup = good;
    dedup[..4].copy_from_slice(&DEDUP.to_le_bytes());
    assert_eq!(
      link_target(&dedup),
      Err(RecordRefusal::UnsupportedTag(DEDUP))
    );
    assert!(!is_name_surrogate(DEDUP));
  }
}
