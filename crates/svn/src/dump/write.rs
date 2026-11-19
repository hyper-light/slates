//! The dump writer (the module documentation's writing rules): each event's bytes as `svnadmin dump` writes them, every
//! event checked against where the stream stands, and every check made before a byte is appended.
//!
//! The bytes, from `libsvn_repos/dump.c` (Subversion's `1.14.x` branch at `d434dc14`): a version record
//! `SVN-fs-dump-format-version: N` and an empty line (516-524); a UUID record the same way (527-537); a revision record's
//! `Revision-number`, then `Prop-content-length` and `Content-length` when it has properties, an empty line, the
//! properties, and one more empty line (540-598); a node record's headers in the order `dump_node` pushes them
//! (1119-1545): `Node-path`, `Node-kind`, `Node-action`, `Node-copyfrom-rev`, `Node-copyfrom-path`,
//! `Text-copy-source-md5`, `Text-copy-source-sha1`, `Prop-delta`, `Text-delta`, `Text-delta-base-md5`,
//! `Text-delta-base-sha1`, `Text-content-md5`, `Text-content-sha1`, then `Prop-content-length`, `Text-content-length`
//! and `Content-length` (`svn_repos__dump_node_record`, 601-645), an empty line, the sections and two line feeds, or,
//! for a record with no sections, one line feed after its empty line (1366-1376); and a replacement by a copy as a
//! deletion record ended by its empty line alone, then an addition (`dump_node_delete`, 1087-1104, and 1225-1247).

use super::digest::{Digests, check_text, checks_anything};
use super::header::{FLAG_SET, kind_word};
use super::{
  Contradiction, CopyFrom, DumpError, Event, Format, Header, LINE_FEED, Md5, NodeAction, NodeHeader, PROPS_END_LINE,
  Property, PropertyFault, RevisionHeader, Sha1, TextSection, Uuid, check_node, check_revision, content_length, to_u64,
};
use crate::index::Revision;
use crate::svndiff::{Compression, Encoder, Version, Window};

/// What lies between a header's name and its value.
/// Format: `dump.c` 507 and 405 write `"%s: %s\n"`.
const HEADER_SEPARATOR: &[u8] = b": ";

/// The digits of lowercase hex, as a checksum is written.
/// Format: `svn_checksum_to_cstring` writes lowercase hex (`checksum.c`); every vector's checksums are lowercase.
const HEX_DIGITS: &[u8] = b"0123456789abcdef";

/// The bits of a byte's high hex digit.
/// Format: a byte is two hex digits, the high one its top four bits.
const NIBBLE_BITS: u32 = 4;

/// The bits of a byte's low hex digit.
/// Format: a byte's low four bits.
const LOW_NIBBLE: u8 = 0x0f;

/// Writes a dump an event at a time, as `svnadmin dump` writes it (the module documentation).
#[derive(Clone, Debug, Default)]
pub struct Writer {
  format: Option<Format>,
  revision: Option<Revision>,
  state: WriteState,
}

/// Where the stream being written stands.
#[derive(Clone, Debug, Default)]
enum WriteState {
  /// Nothing written: a version record is due.
  #[default]
  Start,
  /// Between records.
  Between,
  /// In a property section.
  Properties(OpenSection),
  /// In a whole text.
  Text(Box<OpenText>),
  /// A text delta's svndiff header is due: the delta's bytes left.
  DeltaHeader(u64),
  /// In a text delta: its bytes left and its encoder.
  Delta { left: u64, encoder: Encoder },
}

/// A property section being written.
#[derive(Clone, Copy, Debug)]
struct OpenSection {
  /// Its bytes left, `PROPS-END` included.
  left: u64,
  /// Whether `D` entries may stand in it.
  deletions: bool,
  /// What follows it.
  then: Then,
}

/// What follows a property section.
#[derive(Clone, Copy, Debug)]
enum Then {
  /// A revision record's end.
  Revision,
  /// A node record's text, when it has one, then its end.
  Node(Option<TextSection>),
}

/// A whole text being written.
#[derive(Clone, Debug)]
struct OpenText {
  left: u64,
  expected_md5: Option<Md5>,
  expected_sha1: Option<Sha1>,
  digests: Option<Digests>,
}

impl Writer {
  /// A writer that has written nothing.
  pub fn new() -> Self {
    Self::default()
  }

  /// Appends `event`'s bytes to `out`.
  ///
  /// # Errors
  /// [`DumpError::OutOfOrder`] for an event the stream cannot take where it stands; the refusals of
  /// [`DumpError::Contradiction`], [`DumpError::MalformedValue`], [`DumpError::MissingHeader`] and
  /// [`DumpError::Property`] for what the reader would refuse; [`DumpError::ChecksumMismatch`] for a whole text whose
  /// bytes are not the ones its digests name; [`DumpError::Svndiff`] for a window svndiff refuses. Nothing is appended
  /// then.
  pub fn write(&mut self, event: &Event<'_>, out: &mut Vec<u8>) -> Result<(), DumpError> {
    match event {
      Event::Format(format) => self.format_record(*format, out),
      Event::Uuid(uuid) => self.uuid_record(uuid, out),
      Event::Revision(header) => self.revision_record(header, out),
      Event::Node(header) => self.node_record(header, out),
      Event::Property(property) => self.property(property, out),
      Event::PropertiesEnd => self.properties_end(out),
      Event::Text(bytes) => self.text(bytes, out),
      Event::Delta(version) => self.delta_header(*version, out),
      Event::Window(window) => self.window(window, out),
      Event::TextEnd => self.text_end(out),
    }
  }

  /// Ends the stream.
  ///
  /// # Errors
  /// [`DumpError::OutOfOrder`] when nothing was written or the stream would end inside a record.
  pub fn finish(&self) -> Result<(), DumpError> {
    match self.state {
      WriteState::Between => Ok(()),
      _ => Err(DumpError::OutOfOrder),
    }
  }

  /// Refuses a record anywhere but between records.
  fn between(&self) -> Result<(), DumpError> {
    match self.state {
      WriteState::Between => Ok(()),
      _ => Err(DumpError::OutOfOrder),
    }
  }

  /// A version record, at the start or between records.
  fn format_record(&mut self, format: Format, out: &mut Vec<u8>) -> Result<(), DumpError> {
    if !matches!(self.state, WriteState::Start | WriteState::Between) {
      return Err(DumpError::OutOfOrder);
    }
    header_line(out, Header::FormatVersion, format.number().to_string().as_bytes());
    out.push(LINE_FEED);
    self.format = Some(format);
    self.state = WriteState::Between;
    Ok(())
  }

  /// A UUID record.
  fn uuid_record(&mut self, uuid: &Uuid, out: &mut Vec<u8>) -> Result<(), DumpError> {
    self.between()?;
    header_line(out, Header::Uuid, uuid.as_bytes());
    out.push(LINE_FEED);
    Ok(())
  }

  /// A revision record's header, and its end when it has no properties.
  fn revision_record(&mut self, header: &RevisionHeader, out: &mut Vec<u8>) -> Result<(), DumpError> {
    self.between()?;
    check_revision(header)?;
    header_line(out, Header::RevisionNumber, header.number.to_string().as_bytes());
    if let Some(length) = header.properties {
      header_line(out, Header::PropContentLength, length.to_string().as_bytes());
      header_line(out, Header::ContentLength, length.to_string().as_bytes());
    }
    out.push(LINE_FEED);
    self.revision = Some(header.number);
    self.state = match header.properties {
      Some(left) => WriteState::Properties(OpenSection {
        left,
        deletions: false,
        then: Then::Revision,
      }),
      None => {
        out.push(LINE_FEED);
        WriteState::Between
      }
    };
    Ok(())
  }

  /// A node record's headers, and its end when it has no sections.
  fn node_record(&mut self, header: &NodeHeader, out: &mut Vec<u8>) -> Result<(), DumpError> {
    self.between()?;
    let fault = match self.revision {
      None => Some(Contradiction::NodeOutsideRevision),
      Some(0) => Some(Contradiction::NodeInRevisionZero),
      Some(_) => None,
    };
    if let Some(fault) = fault {
      return Err(DumpError::Contradiction { fault });
    }
    let format = self.format.ok_or(DumpError::OutOfOrder)?;
    check_node(header, format)?;
    let length = content_length(header)?;
    let next = match header.properties {
      Some(section) => WriteState::Properties(OpenSection {
        left: section.length,
        deletions: section.delta,
        then: Then::Node(header.text),
      }),
      None => text_state(header.text)?,
    };
    node_headers(header, length, out);
    if matches!(next, WriteState::Between) {
      out.push(LINE_FEED);
    }
    self.state = next;
    Ok(())
  }

  /// One property entry, within its section's length, `PROPS-END` left room.
  fn property(&mut self, property: &Property<'_>, out: &mut Vec<u8>) -> Result<(), DumpError> {
    let WriteState::Properties(section) = &mut self.state else {
      return Err(DumpError::OutOfOrder);
    };
    if matches!(property, Property::Delete { .. }) && !section.deletions {
      return Err(property_fault(PropertyFault::DeletionOutOfPlace));
    }
    if property.name().contains(&0) {
      return Err(property_fault(PropertyFault::NameNul));
    }
    let length = property.encoded_len().ok_or(property_fault(PropertyFault::PastSection))?;
    let room = section.left.saturating_sub(to_u64(PROPS_END_LINE.len()));
    if length > room {
      return Err(property_fault(PropertyFault::PastSection));
    }
    match property {
      Property::Set { name, value } => {
        length_entry(out, b'K', name);
        length_entry(out, b'V', value);
      }
      Property::Delete { name } => length_entry(out, b'D', name),
    }
    section.left = section.left.saturating_sub(length);
    Ok(())
  }

  /// `PROPS-END`, which must fill the section's length exactly, and what follows it.
  fn properties_end(&mut self, out: &mut Vec<u8>) -> Result<(), DumpError> {
    let WriteState::Properties(section) = self.state else {
      return Err(DumpError::OutOfOrder);
    };
    if section.left != to_u64(PROPS_END_LINE.len()) {
      return Err(DumpError::OutOfOrder);
    }
    let next = match section.then {
      Then::Revision => WriteState::Between,
      Then::Node(text) => text_state(text)?,
    };
    out.extend_from_slice(PROPS_END_LINE);
    match (section.then, &next) {
      (Then::Revision, _) => out.push(LINE_FEED),
      (Then::Node(_), WriteState::Between) => record_end(out),
      (Then::Node(_), _) => {}
    }
    self.state = next;
    Ok(())
  }

  /// A piece of a whole text, within its length.
  fn text(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> Result<(), DumpError> {
    let WriteState::Text(text) = &mut self.state else {
      return Err(DumpError::OutOfOrder);
    };
    let length = to_u64(bytes.len());
    if length > text.left {
      return Err(DumpError::OutOfOrder);
    }
    if let Some(digests) = text.digests.as_mut() {
      digests.update(bytes)?;
    }
    out.extend_from_slice(bytes);
    text.left = text.left.saturating_sub(length);
    Ok(())
  }

  /// A text delta's svndiff header.
  fn delta_header(&mut self, version: Version, out: &mut Vec<u8>) -> Result<(), DumpError> {
    let WriteState::DeltaHeader(left) = self.state else {
      return Err(DumpError::OutOfOrder);
    };
    let encoder = Encoder::new(version);
    let header = encoder.header();
    let length = to_u64(header.len());
    if length > left {
      return Err(DumpError::OutOfOrder);
    }
    out.extend_from_slice(&header);
    self.state = WriteState::Delta {
      left: left.saturating_sub(length),
      encoder,
    };
    Ok(())
  }

  /// One window of a text delta, within the delta's length, encoded as Subversion's encoder encodes it.
  fn window(&mut self, window: &Window, out: &mut Vec<u8>) -> Result<(), DumpError> {
    let WriteState::Delta { left, encoder } = &mut self.state else {
      return Err(DumpError::OutOfOrder);
    };
    let mut trial = encoder.clone();
    let mut bytes = Vec::new();
    trial
      .window(window, Compression::Lz4, &mut bytes)
      .map_err(DumpError::Svndiff)?;
    let length = to_u64(bytes.len());
    if length > *left {
      return Err(DumpError::OutOfOrder);
    }
    out.extend_from_slice(&bytes);
    *left = left.saturating_sub(length);
    *encoder = trial;
    Ok(())
  }

  /// The end of a text: a whole text's digests checked; the delta's bytes all written.
  fn text_end(&mut self, out: &mut Vec<u8>) -> Result<(), DumpError> {
    match &self.state {
      WriteState::Text(text) if text.left == 0 => text.check()?,
      WriteState::Delta { left: 0, .. } => {}
      _ => return Err(DumpError::OutOfOrder),
    }
    record_end(out);
    self.state = WriteState::Between;
    Ok(())
  }
}

impl OpenText {
  /// Refuses a text whose digests are not the ones its headers name.
  fn check(&self) -> Result<(), DumpError> {
    match self.digests.clone() {
      Some(digests) => check_text(self.expected_md5, self.expected_sha1, digests.finish()?),
      None => Ok(()),
    }
  }
}

/// The state a node record's text section opens, or the end of the record when it has none.
fn text_state(text: Option<TextSection>) -> Result<WriteState, DumpError> {
  Ok(match text {
    None => WriteState::Between,
    Some(section) if section.delta.is_some() => WriteState::DeltaHeader(section.length),
    Some(section) => {
      let digests = if checks_anything(section.md5, section.sha1) {
        Some(Digests::new()?)
      } else {
        None
      };
      WriteState::Text(Box::new(OpenText {
        left: section.length,
        expected_md5: section.md5,
        expected_sha1: section.sha1,
        digests,
      }))
    }
  })
}

/// A node record's headers and their empty line: a replacement by a copy as a deletion record first.
fn node_headers(header: &NodeHeader, content_length: Option<u64>, out: &mut Vec<u8>) {
  let mut action = header.action;
  if header.action == NodeAction::Replace && header.copy_from.is_some() {
    header_line(out, Header::NodePath, &header.path);
    header_line(out, Header::NodeAction, NodeAction::Delete.word());
    out.push(LINE_FEED);
    action = NodeAction::Add;
  }
  header_line(out, Header::NodePath, &header.path);
  if let Some(kind) = header.kind {
    header_line(out, Header::NodeKind, kind_word(kind));
  }
  header_line(out, Header::NodeAction, action.word());
  if let Some(copy) = &header.copy_from {
    copy_headers(copy, out);
  }
  if header.properties.is_some_and(|section| section.delta) {
    header_line(out, Header::PropDelta, FLAG_SET);
  }
  if let Some(text) = &header.text {
    text_headers(text, out);
  }
  length_headers(header, content_length, out);
  out.push(LINE_FEED);
}

/// A copy source's headers.
fn copy_headers(copy: &CopyFrom, out: &mut Vec<u8>) {
  header_line(out, Header::NodeCopyfromRev, copy.revision.to_string().as_bytes());
  header_line(out, Header::NodeCopyfromPath, &copy.path);
  digest_line(out, Header::TextCopySourceMd5, copy.md5.as_ref().map(|digest| digest.as_slice()));
  digest_line(out, Header::TextCopySourceSha1, copy.sha1.as_ref().map(|digest| digest.as_slice()));
}

/// A text section's headers but its length.
fn text_headers(text: &TextSection, out: &mut Vec<u8>) {
  if let Some(base) = &text.delta {
    header_line(out, Header::TextDelta, FLAG_SET);
    digest_line(out, Header::TextDeltaBaseMd5, base.md5.as_ref().map(|digest| digest.as_slice()));
    digest_line(out, Header::TextDeltaBaseSha1, base.sha1.as_ref().map(|digest| digest.as_slice()));
  }
  digest_line(out, Header::TextContentMd5, text.md5.as_ref().map(|digest| digest.as_slice()));
  digest_line(out, Header::TextContentSha1, text.sha1.as_ref().map(|digest| digest.as_slice()));
}

/// The sections' lengths and the content block's.
fn length_headers(header: &NodeHeader, content_length: Option<u64>, out: &mut Vec<u8>) {
  if let Some(section) = header.properties {
    header_line(out, Header::PropContentLength, section.length.to_string().as_bytes());
  }
  if let Some(text) = header.text {
    header_line(out, Header::TextContentLength, text.length.to_string().as_bytes());
  }
  if let Some(length) = content_length {
    header_line(out, Header::ContentLength, length.to_string().as_bytes());
  }
}

/// One header line.
fn header_line(out: &mut Vec<u8>, header: Header, value: &[u8]) {
  out.extend_from_slice(header.name());
  out.extend_from_slice(HEADER_SEPARATOR);
  out.extend_from_slice(value);
  out.push(LINE_FEED);
}

/// One digest's header line, in lowercase hex, when there is a digest.
fn digest_line(out: &mut Vec<u8>, header: Header, digest: Option<&[u8]>) {
  let Some(digest) = digest else {
    return;
  };
  let hex: Vec<u8> = digest
    .iter()
    .flat_map(|&byte| [hex_digit(byte.checked_shr(NIBBLE_BITS).unwrap_or(0)), hex_digit(byte & LOW_NIBBLE)])
    .collect();
  header_line(out, header, &hex);
}

/// The lowercase hex digit of a nibble.
fn hex_digit(nibble: u8) -> u8 {
  HEX_DIGITS.get(usize::from(nibble)).copied().unwrap_or(b'0')
}

/// One length entry of a property section: its letter, a space, the length, a line feed, the bytes and a line feed.
fn length_entry(out: &mut Vec<u8>, letter: u8, bytes: &[u8]) {
  out.push(letter);
  out.push(b' ');
  out.extend_from_slice(bytes.len().to_string().as_bytes());
  out.push(LINE_FEED);
  out.extend_from_slice(bytes);
  out.push(LINE_FEED);
}

/// The two line feeds that end a record with content (`dump.c` 1544).
fn record_end(out: &mut Vec<u8>) {
  out.push(LINE_FEED);
  out.push(LINE_FEED);
}

/// A property section's refusal.
fn property_fault(fault: PropertyFault) -> DumpError {
  DumpError::Property { fault }
}
