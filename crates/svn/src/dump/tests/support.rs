//! What the dump codec's tests share: events with their bytes owned, a reader driven as a streaming caller drives it,
//! the writer run over a list of events, and the pieces hand-made dumps are built from.

use crate::dump::{
  Admission, DumpError, Event, Format, Md5, NodeHeader, Property, Read, Reader, RevisionHeader,
  Sha1, Uuid, Writer,
};
use crate::svndiff::{Version, Window};

/// An admission no test dump comes near.
pub(super) const ROOMY: Admission = Admission {
  buffer_bytes: usize::MAX,
};

/// A version record of format 2, as `svnadmin` writes it.
pub(super) const V2: &[u8] = b"SVN-fs-dump-format-version: 2\n\n";

/// A version record of format 3, as `svnadmin` writes it.
pub(super) const V3: &[u8] = b"SVN-fs-dump-format-version: 3\n\n";

/// An event with its bytes owned, so a stream's events can be kept, compared and written again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Owned {
  Format(Format),
  Uuid(Uuid),
  Revision(RevisionHeader),
  Node(NodeHeader),
  Set { name: Vec<u8>, value: Vec<u8> },
  Delete { name: Vec<u8> },
  PropertiesEnd,
  Text(Vec<u8>),
  Delta(Version),
  Window(Window),
  TextEnd,
}

impl Owned {
  /// The event `event` names, its bytes copied.
  pub(super) fn of(event: Event<'_>) -> Self {
    match event {
      Event::Format(format) => Self::Format(format),
      Event::Uuid(uuid) => Self::Uuid(uuid),
      Event::Revision(header) => Self::Revision(header),
      Event::Node(header) => Self::Node(header),
      Event::Property(Property::Set { name, value }) => Self::Set {
        name: name.to_vec(),
        value: value.to_vec(),
      },
      Event::Property(Property::Delete { name }) => Self::Delete {
        name: name.to_vec(),
      },
      Event::PropertiesEnd => Self::PropertiesEnd,
      Event::Text(bytes) => Self::Text(bytes.to_vec()),
      Event::Delta(version) => Self::Delta(version),
      Event::Window(window) => Self::Window(window),
      Event::TextEnd => Self::TextEnd,
    }
  }

  /// The event, borrowing its bytes.
  pub(super) fn event(&self) -> Event<'_> {
    match self {
      Self::Format(format) => Event::Format(*format),
      Self::Uuid(uuid) => Event::Uuid(*uuid),
      Self::Revision(header) => Event::Revision(header.clone()),
      Self::Node(header) => Event::Node(header.clone()),
      Self::Set { name, value } => Event::Property(Property::Set { name, value }),
      Self::Delete { name } => Event::Property(Property::Delete { name }),
      Self::PropertiesEnd => Event::PropertiesEnd,
      Self::Text(bytes) => Event::Text(bytes),
      Self::Delta(version) => Event::Delta(*version),
      Self::Window(window) => Event::Window(window.clone()),
      Self::TextEnd => Event::TextEnd,
    }
  }
}

/// Appends `event` to `events`, a text piece joined to a text piece before it: a text arrives in pieces of whatever the
/// caller held, so streams read in different pieces compare equal once joined.
pub(super) fn push(events: &mut Vec<Owned>, event: Owned) {
  if let (Some(Owned::Text(previous)), Owned::Text(piece)) = (events.last_mut(), &event) {
    previous.extend_from_slice(piece);
    return;
  }
  events.push(event);
}

/// What reading a dump made.
#[derive(Clone, Debug, Default)]
pub(super) struct Reading {
  /// The events, text pieces joined.
  pub(super) events: Vec<Owned>,
  /// The most bytes the reader asked its caller to hold at once.
  pub(super) most_needed: usize,
  /// Where in the dump each whole text's bytes lie, `[start, end)`, every piece of them.
  pub(super) text_spans: Vec<(usize, usize)>,
}

/// Reads `dump` as a streaming caller reads it: handed over `piece` bytes at a time (everything at once for
/// `usize::MAX`), each call given what the reader has not consumed, within `admission`. Checks the reader's promises on
/// every call: it never asks for more than it was admitted, never for bytes it already holds, never for more once the
/// stream has ended, and ends only on the stream's end with everything consumed.
pub(super) fn read(dump: &[u8], piece: usize, admission: Admission) -> Result<Reading, DumpError> {
  let mut reader = Reader::new(admission);
  let mut held: Vec<u8> = Vec::new();
  let mut handed = 0usize;
  let mut dropped = 0usize;
  let mut reading = Reading::default();
  loop {
    let ended = handed == dump.len();
    match reader.read(&held, ended)? {
      Read::Event { event, consumed } => {
        if let Event::Text(bytes) = &event {
          let end = dropped + consumed;
          reading.text_spans.push((end - bytes.len(), end));
        }
        push(&mut reading.events, Owned::of(event));
        held.drain(..consumed);
        dropped += consumed;
      }
      Read::NeedMore { consumed, needed } => {
        kept_its_promises(ended, needed, admission, held.len() - consumed);
        reading.most_needed = reading.most_needed.max(needed);
        held.drain(..consumed);
        dropped += consumed;
        let take = piece.min(dump.len() - handed);
        held.extend_from_slice(&dump[handed..handed + take]);
        handed += take;
      }
      Read::End => {
        ended_with_the_stream(ended);
        return Ok(reading);
      }
    }
  }
}

/// Holds a reader's request for more to its promises: never after the stream's end, never past the admission, never
/// for bytes it already holds (`unconsumed`).
fn kept_its_promises(ended: bool, needed: usize, admission: Admission, unconsumed: usize) {
  assert!(!ended, "the reader asked for more after the stream's end");
  assert!(
    needed <= admission.buffer_bytes,
    "asked for {needed} past the admission"
  );
  assert!(needed > unconsumed, "asked for bytes it already held");
}

/// Holds a reader's end to the stream's.
fn ended_with_the_stream(ended: bool) {
  assert!(ended, "the reader ended before the stream did");
}

/// Reads `dump` handed over whole.
pub(super) fn read_whole(dump: &[u8]) -> Result<Vec<Owned>, DumpError> {
  read(dump, usize::MAX, ROOMY).map(|reading| reading.events)
}

/// The refusal reading `dump` whole meets; panics when it reads cleanly.
pub(super) fn refusal(dump: &[u8]) -> DumpError {
  match read_whole(dump) {
    Ok(events) => panic!("read cleanly: {events:?}"),
    Err(refusal) => refusal,
  }
}

/// Writes `events` as one stream and ends it.
pub(super) fn write(events: &[Owned]) -> Result<Vec<u8>, DumpError> {
  let mut writer = Writer::new();
  let mut out = Vec::new();
  for event in events {
    writer.write(&event.event(), &mut out)?;
  }
  writer.finish()?;
  Ok(out)
}

/// The bytes of `parts`, one after another.
pub(super) fn dump(parts: &[&[u8]]) -> Vec<u8> {
  parts.concat()
}

/// A revision record with an empty property section, as `svnadmin` writes one.
pub(super) fn revision(number: u64) -> Vec<u8> {
  format!("Revision-number: {number}\nProp-content-length: 10\nContent-length: 10\n\nPROPS-END\n\n")
    .into_bytes()
}

/// A node record from its header lines and its content block, the blank line between them and two after a content
/// block, as `svnadmin` writes one.
pub(super) fn node(headers: &str, content: &[u8]) -> Vec<u8> {
  let mut bytes = format!("{headers}\n").into_bytes();
  bytes.extend_from_slice(content);
  bytes.extend_from_slice(b"\n\n");
  bytes
}

/// The lowercase hex of `bytes`.
pub(super) fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The MD5 and SHA-1 of `text`.
pub(super) fn digests(text: &[u8]) -> (Md5, Sha1) {
  let mut digests = crate::dump::Digests::new().unwrap();
  digests.update(text).unwrap();
  digests.finish().unwrap()
}

/// The MD5 and SHA-1 of `text`, in hex.
pub(super) fn digests_hex(text: &[u8]) -> (String, String) {
  let (md5, sha1) = digests(text);
  (hex(&md5), hex(&sha1))
}

/// A file record adding `path` with `text` whole and an empty property section, its checksums right.
pub(super) fn file_add(path: &str, text: &[u8]) -> Vec<u8> {
  let (md5, sha1) = digests_hex(text);
  let length = text.len();
  let headers = format!(
    "Node-path: {path}\nNode-kind: file\nNode-action: add\nText-content-md5: {md5}\nText-content-sha1: {sha1}\n\
     Prop-content-length: 10\nText-content-length: {length}\nContent-length: {}\n",
    10 + length
  );
  let mut content = b"PROPS-END\n".to_vec();
  content.extend_from_slice(text);
  node(&headers, &content)
}
