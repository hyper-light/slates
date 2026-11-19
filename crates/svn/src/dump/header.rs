//! A record's header block: its header names, one header line read into its typed value, and a whole block made into
//! the record it names (the module documentation's format; `load.c` `read_header_block`, `load-fs-vtable.c`
//! `make_node_baton`).

use super::{
  Contradiction, CopyFrom, DeltaBase, DumpError, Format, Md5, NUMBER_DIGITS, NUMBER_LIMIT,
  NodeAction, NodeHeader, PropertySection, RevisionHeader, Sha1, TextSection, Uuid, check_node,
  check_revision, content_length,
};
use crate::index::{NodeKind, Revision};

/// A header the format names (`svn_repos.h` 3084-3125). Any other header is read and passed over, as `load.c` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Header {
  /// `SVN-fs-dump-format-version`: a version record's format.
  FormatVersion,
  /// `UUID`: a UUID record's UUID.
  Uuid,
  /// `Revision-number`: a revision record's number.
  RevisionNumber,
  /// `Node-path`: a node record's path.
  NodePath,
  /// `Node-kind`: `file` or `dir`.
  NodeKind,
  /// `Node-action`: `change`, `add`, `delete` or `replace`.
  NodeAction,
  /// `Node-copyfrom-rev`: a copy source's revision.
  NodeCopyfromRev,
  /// `Node-copyfrom-path`: a copy source's path.
  NodeCopyfromPath,
  /// `Text-copy-source-md5`.
  TextCopySourceMd5,
  /// `Text-copy-source-sha1`.
  TextCopySourceSha1,
  /// `Prop-delta`: whether the property section is a delta.
  PropDelta,
  /// `Text-delta`: whether the text section is a delta.
  TextDelta,
  /// `Text-delta-base-md5`.
  TextDeltaBaseMd5,
  /// `Text-delta-base-sha1`.
  TextDeltaBaseSha1,
  /// `Text-content-md5`.
  TextContentMd5,
  /// `Text-content-sha1`.
  TextContentSha1,
  /// `Prop-content-length`.
  PropContentLength,
  /// `Text-content-length`.
  TextContentLength,
  /// `Content-length`.
  ContentLength,
}

/// Every header the format names.
const HEADERS: [Header; 19] = [
  Header::FormatVersion,
  Header::Uuid,
  Header::RevisionNumber,
  Header::NodePath,
  Header::NodeKind,
  Header::NodeAction,
  Header::NodeCopyfromRev,
  Header::NodeCopyfromPath,
  Header::TextCopySourceMd5,
  Header::TextCopySourceSha1,
  Header::PropDelta,
  Header::TextDelta,
  Header::TextDeltaBaseMd5,
  Header::TextDeltaBaseSha1,
  Header::TextContentMd5,
  Header::TextContentSha1,
  Header::PropContentLength,
  Header::TextContentLength,
  Header::ContentLength,
];

impl Header {
  /// The header's name, as a line carries it before its `: `.
  /// Format: `svn_repos.h` 3085-3121, each `SVN_REPOS_DUMPFILE_*` name.
  pub fn name(self) -> &'static [u8] {
    match self {
      Self::FormatVersion => b"SVN-fs-dump-format-version",
      Self::Uuid => b"UUID",
      Self::RevisionNumber => b"Revision-number",
      Self::NodePath => b"Node-path",
      Self::NodeKind => b"Node-kind",
      Self::NodeAction => b"Node-action",
      Self::NodeCopyfromRev => b"Node-copyfrom-rev",
      Self::NodeCopyfromPath => b"Node-copyfrom-path",
      Self::TextCopySourceMd5 => b"Text-copy-source-md5",
      Self::TextCopySourceSha1 => b"Text-copy-source-sha1",
      Self::PropDelta => b"Prop-delta",
      Self::TextDelta => b"Text-delta",
      Self::TextDeltaBaseMd5 => b"Text-delta-base-md5",
      Self::TextDeltaBaseSha1 => b"Text-delta-base-sha1",
      Self::TextContentMd5 => b"Text-content-md5",
      Self::TextContentSha1 => b"Text-content-sha1",
      Self::PropContentLength => b"Prop-content-length",
      Self::TextContentLength => b"Text-content-length",
      Self::ContentLength => b"Content-length",
    }
  }

  /// The header a name names, if the format names it (case and all, as `svn_hash_gets` matches).
  pub fn from_name(name: &[u8]) -> Option<Self> {
    HEADERS.into_iter().find(|header| header.name() == name)
  }
}

/// What lies between a header's name and its value.
/// Format: `dump.c` 506 writes `"%s: %s\n"`; `load.c` 112 takes the value from two bytes past the colon.
const HEADER_SEPARATOR: &[u8] = b": ";

/// The `Node-kind` word of a file.
/// Format: `dump.c` 1170; `load-fs-vtable.c` 402.
const KIND_FILE: &[u8] = b"file";

/// The `Node-kind` word of a directory.
/// Format: `dump.c` 1173; `load-fs-vtable.c` 404.
const KIND_DIRECTORY: &[u8] = b"dir";

/// The value of a delta flag that is set.
/// Format: `dump.c` 1445 and 1471 write `true`; `load.c` 709 and 734 read nothing else as set.
pub(super) const FLAG_SET: &[u8] = b"true";

/// The value of a delta flag that is not set.
/// Format: `notes/dump-load-format.txt`: "[Text-delta: true|false]", "The default value for the boolean headers is
/// "false"".
const FLAG_UNSET: &[u8] = b"false";

/// The `Node-kind` word of `kind`.
pub(super) fn kind_word(kind: NodeKind) -> &'static [u8] {
  match kind {
    NodeKind::File => KIND_FILE,
    NodeKind::Directory => KIND_DIRECTORY,
  }
}

/// The kind a `Node-kind` word names, if it is one.
fn parse_kind(word: &[u8]) -> Option<NodeKind> {
  [NodeKind::File, NodeKind::Directory]
    .into_iter()
    .find(|&kind| kind_word(kind) == word)
}

/// The flag a delta header's value names, if it is `true` or `false`.
fn parse_flag(word: &[u8]) -> Option<bool> {
  match word {
    FLAG_SET => Some(true),
    FLAG_UNSET => Some(false),
    _ => None,
  }
}

/// The number `digits` holds, if it is one to nineteen decimal digits and at most 2^63 - 1: stricter than
/// Subversion's `atol` and `apr_atoi64`, which stop at the first other byte and read nothing as 0.
pub(super) fn parse_number(digits: &[u8]) -> Option<u64> {
  if digits.is_empty() || digits.len() > NUMBER_DIGITS {
    return None;
  }
  let mut number: u64 = 0;
  for &digit in digits {
    if !digit.is_ascii_digit() {
      return None;
    }
    number = number
      .checked_mul(DECIMAL)?
      .checked_add(u64::from(digit.checked_sub(b'0')?))?;
  }
  (number <= NUMBER_LIMIT).then_some(number)
}

/// The base numbers are written in.
/// Format: every length and revision is `%ld` or `APR_SIZE_T_FMT`, decimal (`dump.c`, `hash.c`).
const DECIMAL: u64 = 10;

/// The digest `hex` holds, if it is exactly twice `N` hex digits of either case (`svn_checksum_parse_hex` takes either
/// and ignores what follows them; this takes nothing more).
pub(super) fn parse_hex<const N: usize>(hex: &[u8]) -> Option<[u8; N]> {
  let mut digest = [0u8; N];
  if hex.len() != N.checked_mul(HEX_DIGITS_PER_BYTE)? {
    return None;
  }
  for (byte, &[high, low]) in digest.iter_mut().zip(hex.as_chunks::<HEX_DIGITS_PER_BYTE>().0) {
    *byte = hex_value(high)?
      .checked_mul(HEX_BASE)?
      .checked_add(hex_value(low)?)?;
  }
  Some(digest)
}

/// The hex digits that write one byte.
/// Format: `svn_checksum_to_cstring` writes two digits a byte (`checksum.c`).
const HEX_DIGITS_PER_BYTE: usize = 2;

/// The base hex digits are read in.
/// Format: hexadecimal.
const HEX_BASE: u8 = 16;

/// The value of one hex digit, if it is one.
fn hex_value(digit: u8) -> Option<u8> {
  char::from(digit)
    .to_digit(u32::from(HEX_BASE))
    .and_then(|value| u8::try_from(value).ok())
}

/// A record's header block as it is read: each header the format names, once at most, by its typed value.
#[derive(Clone, Debug, Default)]
pub(super) struct Block {
  version: Option<u64>,
  uuid: Option<Uuid>,
  revision: Option<Revision>,
  path: Option<Vec<u8>>,
  kind: Option<NodeKind>,
  action: Option<NodeAction>,
  copy_revision: Option<Revision>,
  copy_path: Option<Vec<u8>>,
  copy_md5: Option<Md5>,
  copy_sha1: Option<Sha1>,
  prop_delta: Option<bool>,
  text_delta: Option<bool>,
  base_md5: Option<Md5>,
  base_sha1: Option<Sha1>,
  text_md5: Option<Md5>,
  text_sha1: Option<Sha1>,
  prop_length: Option<u64>,
  text_length: Option<u64>,
  content_length: Option<u64>,
}

/// A record its header block names, with the bytes of its content block past its sections, which are passed over.
#[derive(Clone, Debug)]
pub(super) enum Record {
  /// A version record.
  Format(Format),
  /// A UUID record.
  Uuid(Uuid),
  /// A revision record.
  Revision {
    /// Its header.
    header: RevisionHeader,
    /// The bytes of its content block past its property section.
    rest: u64,
  },
  /// A node record.
  Node {
    /// Its header.
    header: NodeHeader,
    /// The bytes of its content block past its sections.
    rest: u64,
  },
}

/// The kind of record a header block names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
  Format,
  Uuid,
  Revision,
  Node,
}

impl Block {
  /// Reads one header line, its line feed excluded: a name, `: ` and a value. A header the format names is read into
  /// its typed value; any other is passed over.
  ///
  /// # Errors
  /// [`DumpError::MalformedHeader`] for a line without `: ` after a name, or with a NUL; then the header's own refusal.
  pub(super) fn add(&mut self, line: &[u8]) -> Result<(), DumpError> {
    if line.contains(&0) {
      return Err(DumpError::MalformedHeader);
    }
    let colon = line
      .iter()
      .position(|&byte| byte == b':')
      .filter(|&colon| colon > 0)
      .ok_or(DumpError::MalformedHeader)?;
    let (name, after) = line
      .split_at_checked(colon)
      .ok_or(DumpError::MalformedHeader)?;
    let value = after
      .strip_prefix(HEADER_SEPARATOR)
      .ok_or(DumpError::MalformedHeader)?;
    match Header::from_name(name) {
      Some(header) => self.set(header, value),
      None => Ok(()),
    }
  }

  /// Reads `value` as `header`'s.
  fn set(&mut self, header: Header, value: &[u8]) -> Result<(), DumpError> {
    match header {
      Header::FormatVersion => put(&mut self.version, parse_number(value), header),
      Header::Uuid => put(&mut self.uuid, Uuid::parse(value), header),
      Header::RevisionNumber => put(&mut self.revision, parse_number(value), header),
      Header::NodePath => put(&mut self.path, Some(value.to_vec()), header),
      Header::NodeKind => put(&mut self.kind, parse_kind(value), header),
      Header::NodeAction => put(&mut self.action, NodeAction::from_word(value), header),
      Header::NodeCopyfromRev => put(&mut self.copy_revision, parse_number(value), header),
      Header::NodeCopyfromPath => put(&mut self.copy_path, Some(value.to_vec()), header),
      Header::TextCopySourceMd5 => put(&mut self.copy_md5, parse_hex(value), header),
      Header::TextCopySourceSha1 => put(&mut self.copy_sha1, parse_hex(value), header),
      Header::PropDelta => put(&mut self.prop_delta, parse_flag(value), header),
      Header::TextDelta => put(&mut self.text_delta, parse_flag(value), header),
      Header::TextDeltaBaseMd5 => put(&mut self.base_md5, parse_hex(value), header),
      Header::TextDeltaBaseSha1 => put(&mut self.base_sha1, parse_hex(value), header),
      Header::TextContentMd5 => put(&mut self.text_md5, parse_hex(value), header),
      Header::TextContentSha1 => put(&mut self.text_sha1, parse_hex(value), header),
      Header::PropContentLength => put(&mut self.prop_length, parse_number(value), header),
      Header::TextContentLength => put(&mut self.text_length, parse_number(value), header),
      Header::ContentLength => put(&mut self.content_length, parse_number(value), header),
    }
  }

  /// The record the block names, in a stream of `format` whose current revision is `revision`.
  ///
  /// # Errors
  /// [`DumpError::UnrecognizedRecord`] for a block that names no kind of record, and the refusals of the kind it names.
  pub(super) fn finish(
    self,
    format: Format,
    revision: Option<Revision>,
  ) -> Result<Record, DumpError> {
    match self.kind()? {
      Kind::Format => self.into_format(),
      Kind::Uuid => self.into_uuid(),
      Kind::Revision => self.into_revision(),
      Kind::Node => self.into_node(format, revision),
    }
  }

  /// The one kind of record the block names: by its version, UUID, revision number or node path.
  fn kind(&self) -> Result<Kind, DumpError> {
    let named = [
      (self.version.is_some(), Kind::Format),
      (self.uuid.is_some(), Kind::Uuid),
      (self.revision.is_some(), Kind::Revision),
      (self.path.is_some(), Kind::Node),
    ];
    let mut kinds = named
      .into_iter()
      .filter_map(|(present, kind)| present.then_some(kind));
    let kind = kinds.next().ok_or(DumpError::UnrecognizedRecord)?;
    if kinds.next().is_some() {
      return Err(contradiction(Contradiction::MixedRecord));
    }
    Ok(kind)
  }

  /// Whether the block holds a header only a node record carries.
  fn has_node_headers(&self) -> bool {
    self.kind.is_some()
      || self.action.is_some()
      || self.copy_revision.is_some()
      || self.copy_path.is_some()
      || self.copy_md5.is_some()
      || self.copy_sha1.is_some()
      || self.text_delta.is_some()
      || self.base_md5.is_some()
      || self.base_sha1.is_some()
      || self.text_md5.is_some()
      || self.text_sha1.is_some()
  }

  /// Whether the block holds a section header of a record with content.
  fn has_section_headers(&self) -> bool {
    self.prop_delta.is_some()
      || self.prop_length.is_some()
      || self.text_length.is_some()
      || self.content_length.is_some()
  }

  /// A version record: its format, and nothing but unknown headers beside it.
  fn into_format(self) -> Result<Record, DumpError> {
    if self.has_node_headers() || self.has_section_headers() {
      return Err(contradiction(Contradiction::HeaderOutOfPlace));
    }
    let version = self.version.ok_or(DumpError::UnrecognizedRecord)?;
    Format::from_number(version)
      .map(Record::Format)
      .ok_or(DumpError::UnsupportedVersion { version })
  }

  /// A UUID record: its UUID, and nothing but unknown headers beside it.
  fn into_uuid(self) -> Result<Record, DumpError> {
    if self.has_node_headers() || self.has_section_headers() {
      return Err(contradiction(Contradiction::HeaderOutOfPlace));
    }
    self
      .uuid
      .map(Record::Uuid)
      .ok_or(DumpError::UnrecognizedRecord)
  }

  /// A revision record: its number and property section, without a node's headers, a text or a delta.
  fn into_revision(self) -> Result<Record, DumpError> {
    if self.has_node_headers() || self.prop_delta.is_some() || self.text_length.is_some() {
      return Err(contradiction(Contradiction::HeaderOutOfPlace));
    }
    let number = self.revision.ok_or(DumpError::UnrecognizedRecord)?;
    let header = RevisionHeader {
      number,
      properties: self.prop_length,
    };
    check_revision(&header)?;
    let rest = rest_of(self.content_length, self.prop_length.unwrap_or(0))?;
    Ok(Record::Revision { header, rest })
  }

  /// A node record, in a stream of `format` whose current revision is `revision`.
  fn into_node(self, format: Format, revision: Option<Revision>) -> Result<Record, DumpError> {
    match revision {
      None => return Err(contradiction(Contradiction::NodeOutsideRevision)),
      Some(0) => return Err(contradiction(Contradiction::NodeInRevisionZero)),
      Some(_) => {}
    }
    let header = NodeHeader {
      path: self.path.clone().ok_or(DumpError::UnrecognizedRecord)?,
      kind: self.kind,
      action: self.action.ok_or(DumpError::MissingHeader {
        header: Header::NodeAction,
      })?,
      copy_from: self.copy_from()?,
      properties: self.property_section()?,
      text: self.text_section()?,
    };
    if header.action == NodeAction::Delete && self.content_length.is_some() {
      return Err(contradiction(Contradiction::ContentOnDelete));
    }
    check_node(&header, format)?;
    let sections = content_length(&header)?.unwrap_or(0);
    let rest = rest_of(self.content_length, sections)?;
    Ok(Record::Node { header, rest })
  }

  /// The copy source: both of `Node-copyfrom-rev` and `Node-copyfrom-path` or neither, and its text's checksums only
  /// with it.
  fn copy_from(&self) -> Result<Option<CopyFrom>, DumpError> {
    match (self.copy_revision, &self.copy_path) {
      (Some(revision), Some(path)) => Ok(Some(CopyFrom {
        revision,
        path: path.clone(),
        md5: self.copy_md5,
        sha1: self.copy_sha1,
      })),
      (None, None) if self.copy_md5.is_none() && self.copy_sha1.is_none() => Ok(None),
      (None, None) => Err(contradiction(Contradiction::CopyChecksumWithoutCopy)),
      (Some(_), None) | (None, Some(_)) => Err(contradiction(Contradiction::CopySourceIncomplete)),
    }
  }

  /// The property section: `Prop-content-length`, with `Prop-delta` only beside it.
  fn property_section(&self) -> Result<Option<PropertySection>, DumpError> {
    match (self.prop_length, self.prop_delta) {
      (Some(length), delta) => Ok(Some(PropertySection {
        delta: delta.unwrap_or(false),
        length,
      })),
      (None, Some(_)) => Err(contradiction(Contradiction::DeltaWithoutSection)),
      (None, None) => Ok(None),
    }
  }

  /// The text section: `Text-content-length`, with its checksums and `Text-delta` only beside it, and the delta's
  /// base checksums only beside `Text-delta: true`.
  fn text_section(&self) -> Result<Option<TextSection>, DumpError> {
    let delta = self.text_delta.unwrap_or(false);
    let has_base = self.base_md5.is_some() || self.base_sha1.is_some();
    if has_base && !delta {
      return Err(contradiction(Contradiction::DeltaBaseWithoutDelta));
    }
    let Some(length) = self.text_length else {
      if self.text_delta.is_some() {
        return Err(contradiction(Contradiction::DeltaWithoutSection));
      }
      if self.text_md5.is_some() || self.text_sha1.is_some() {
        return Err(contradiction(Contradiction::TextChecksumWithoutText));
      }
      return Ok(None);
    };
    Ok(Some(TextSection {
      delta: delta.then_some(DeltaBase {
        md5: self.base_md5,
        sha1: self.base_sha1,
      }),
      md5: self.text_md5,
      sha1: self.text_sha1,
      length,
    }))
  }
}

/// Puts `value`, `header`'s parsed value, in its slot, refusing a second one and a value that did not parse.
fn put<T>(slot: &mut Option<T>, value: Option<T>, header: Header) -> Result<(), DumpError> {
  if slot.is_some() {
    return Err(DumpError::DuplicateHeader { header });
  }
  *slot = Some(value.ok_or(DumpError::MalformedValue { header })?);
  Ok(())
}

/// The bytes of a content block of `content_length` past `sections` bytes of sections, which are passed over as
/// `load.c` passes them over (`load.c` 783-811); refused when the content block is shorter than its sections.
fn rest_of(content_length: Option<u64>, sections: u64) -> Result<u64, DumpError> {
  match content_length {
    None => Ok(0),
    Some(length) => length
      .checked_sub(sections)
      .ok_or(contradiction(Contradiction::ContentLengthShort)),
  }
}

/// A contradiction's refusal.
fn contradiction(fault: Contradiction) -> DumpError {
  DumpError::Contradiction { fault }
}
