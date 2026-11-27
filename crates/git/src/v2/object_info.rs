//! `object-info` (A-624; gitprotocol-v2 "object-info" \[B\]; `protocol-caps.c` `cap_object_info` and `send_info`,
//! `serve.c` \[C\]): its arguments, and its answer, the size and type of each object asked about.
//!
//! The arguments are `size` (asking for each object's uncompressed size), `type` (asking for each object's Git type:
//! `blob`, `tree`, `commit`, or `tag`), and `oid <id>`, naming an object, each a whole line, in any order, any again.
//! A request that names no object is answered by the flush alone. Otherwise the answer is one line per requested
//! attribute (`size`, then `type`, in stock Git 2.56.1's fixed order even if the client sent `type` first); then a line
//! for each object, in the order named: its id followed by a space and each requested value separated by a space, or
//! the trailing space alone (`<oid> `) when the server holds no such visible object; then a flush.
//!
//! ```text
//! output = info flush-pkt
//! info = *PKT-LINE(attr LF)
//!        *PKT-LINE(obj-info LF)
//! attr = "size" | "type"
//! obj-size = 1*DIGIT
//! obj-type = "blob" | "tree" | "commit" | "tag"
//! obj-val = obj-size | obj-type
//! obj-info = obj-id [SP [obj-val *(SP obj-val)]]
//! ```
//!
//! **As stock Git 2.56.1 answers** (`send_info`): no attribute line when neither `size` nor `type`
//! was asked for, writing each present object as its bare `<oid>` and each missing or hidden object
//! as `<oid> ` (trailing space), and every line without an LF (`packet_writer_write`), where the
//! document's grammar ends each with one. A reader takes either ending and records it, as every v2
//! line does, so a decoded answer writes back to its bytes.
//!
//! **Where git departs, and why this does.** git answers an argument it does not know with an `ERR` line and goes on,
//! and an id it cannot read with another; a request with either is refused here by kind before any answer, as `fetch`'s
//! and `ls-refs`'s are. git echoes each id as its client wrote it; the answer here writes it as `oid_to_hex` does, in
//! lower case (A-624's rule): the same bytes for every client git ships, each of which writes ids in lower case.
//!
//! **Answered over a trait.** The answer asks a repository one thing, an object's size and type ([`ObjectInfo`]);
//! A-614's store answers both from the object's index row ([`Stored`]). Nothing is allocated to answer: each line is
//! written as it is made, once measured and once written, so a refused answer writes nothing.

use slates_vcs::ContentStore;

use super::Refusal;
use super::line::{self, Ending, Line, Packets, Part, SP};
use crate::Admission;
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::out::{Measure, Out};
use crate::pktline::Packet;
use crate::store::Repository;

/// Format: the argument asking for each object's size, and the attribute line naming it [gitprotocol-v2
/// "object-info": `attr = "size"`].
const SIZE: &[u8] = b"size";

/// Format: the argument asking for each object's Git type, and the attribute line naming it [gitprotocol-v2
/// "object-info": `attr = "type"`].
const TYPE: &[u8] = b"type";

/// Format: the word before an argument's object id \[gitprotocol-v2 "object-info": `oid <oid>`\].
const OID: &[u8] = b"oid";

/// Format: the most parts an answer line is written in: an id, a space, a size, a space, a type and its ending.
const ANSWER_LINE_PARTS: usize = 6;

/// One `object-info` argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Argument {
  /// `size`: answer each object's size.
  Size,
  /// `type`: answer each object's Git type (`blob`, `tree`, `commit`, or `tag`).
  Type,
  /// `oid <id>`: an object to answer about.
  Oid(ObjectId),
}

impl Argument {
  /// The argument a line's text writes, its id in `format`: refused when it writes none (`cap_object_info`'s
  /// "unexpected line"), or an id that is none.
  pub(crate) fn parse(text: &[u8], format: ObjectFormat, at: u64) -> Result<Argument, Refusal> {
    if text == SIZE {
      return Ok(Argument::Size);
    }
    if text == TYPE {
      return Ok(Argument::Type);
    }
    match text
      .strip_prefix(OID)
      .and_then(|rest| rest.strip_prefix(&[SP]))
    {
      Some(hex) => line::object_id(format, hex, at).map(Argument::Oid),
      None => Err(Refusal::UnknownArgument { at }),
    }
  }

  /// Refused when the argument's id is not in `format`.
  pub(crate) fn check(&self, format: ObjectFormat, at: u64) -> Result<(), Refusal> {
    match self {
      Argument::Size | Argument::Type => Ok(()),
      Argument::Oid(id) => line::check_format(id, format, at),
    }
  }

  /// The line's text in parts: the word, a space where there is an id, and the id.
  pub(super) fn parts(&self) -> [Part<'_>; 3] {
    match self {
      Argument::Size => [Part::Bytes(SIZE), Part::Bytes(b""), Part::Bytes(b"")],
      Argument::Type => [Part::Bytes(TYPE), Part::Bytes(b""), Part::Bytes(b"")],
      Argument::Oid(id) => [Part::Bytes(OID), Part::Bytes(&[SP]), Part::Id(id)],
    }
  }
}

/// One attribute header announced before `object-info` object lines (`attr = "size" | "type"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attr {
  /// `size`: each object line carries the object's size in bytes.
  Size,
  /// `type`: each object line carries the object's Git type (`blob`, `tree`, `commit`, or `tag`).
  Type,
}

impl Attr {
  /// Wire bytes of this attribute header.
  fn bytes(self) -> &'static [u8] {
    match self {
      Attr::Size => SIZE,
      Attr::Type => TYPE,
    }
  }
}

/// What an answer's line about an object reports for the attribute headers preceding it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Values {
  /// The request asked for no attributes: the line is the id alone.
  NotAsked,
  /// The server holds no such visible object: the id and a single trailing space.
  Unknown,
  /// `size` alone: the object's size in bytes.
  Size(u64),
  /// `type` alone: the object's Git type.
  Type(Kind),
  /// `size`, then `type`.
  SizeType(u64, Kind),
  /// `type`, then `size`.
  TypeSize(Kind, u64),
}

/// One line of an `object-info` answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Info {
  /// An attribute header line (`size` or `type`): every object line after the headers carries these columns in order.
  Attr(Attr),
  /// An object's line.
  Object {
    /// The object.
    id: ObjectId,
    /// What the line reports for the attributes announced above it.
    values: Values,
  },
}

/// Uncompressed size and Git type of one object returned by [`ObjectInfo`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectMeta {
  /// Uncompressed size in bytes.
  pub size: u64,
  /// Git object type (`blob`, `tree`, `commit`, or `tag`).
  pub kind: Kind,
}

/// What a repository answers `object-info` from.
pub trait ObjectInfo {
  /// The uncompressed size and Git type of the object `id` names, or `None` when the repository holds no such visible
  /// object.
  fn info(&self, id: &ObjectId) -> Option<ObjectMeta>;
}

/// A-614's store, read for `object-info`: an object's size and type come from its index row, and an object with no row
/// (none in the repository, or one a push holds until its verdict) is unknown, as git's quarantine hides a push's
/// objects.
#[derive(Clone, Copy)]
pub struct Stored<'a> {
  /// The repository.
  pub repository: &'a Repository,
  /// The content store its index lives in.
  pub content: &'a ContentStore,
}

impl ObjectInfo for Stored<'_> {
  fn info(&self, id: &ObjectId) -> Option<ObjectMeta> {
    self
      .repository
      .object(self.content, id)
      .map(|row| ObjectMeta {
        size: row.len,
        kind: row.kind,
      })
  }
}

/// Where an answer stands, enforcing that attribute headers appear at most once each before any object line and that
/// every object line matches the announced columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
  /// No line yet: `size`, `type`, a bare object line, or the flush.
  Start,
  /// Header announced `size`.
  HeaderSize,
  /// Header announced `type`.
  HeaderType,
  /// Header announced `size`, then `type`.
  HeaderSizeType,
  /// Header announced `type`, then `size`.
  HeaderTypeSize,
  /// Object lines after no attribute header: bare ids for present objects, `<oid> ` for missing or hidden.
  ObjectsBare,
  /// Object lines after `size`.
  ObjectsSize,
  /// Object lines after `type`.
  ObjectsType,
  /// Object lines after `size`, `type`.
  ObjectsSizeType,
  /// Object lines after `type`, `size`.
  ObjectsTypeSize,
}

/// Splits `bytes` at its first ASCII space (`SP`).
fn split_sp(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
  let split = bytes.iter().position(|byte| *byte == SP)?;
  Some((
    bytes.get(..split).unwrap_or_default(),
    bytes.get(split.saturating_add(1)..).unwrap_or_default(),
  ))
}

/// Parses an `obj-size` decimal field at `at`.
fn parse_size(bytes: &[u8], at: u64) -> Result<u64, Refusal> {
  line::decimal(bytes).ok_or(Refusal::Number { at })
}

/// Parses an `obj-type` field (`blob`, `tree`, `commit`, or `tag`) at `at`.
fn parse_kind(bytes: &[u8], at: u64) -> Result<Kind, Refusal> {
  Kind::from_name(bytes).map_err(|_| Refusal::Line { at })
}

impl Stage {
  /// Whether `self` is still in the leading attribute-header block (before any object line).
  fn in_header(self) -> bool {
    matches!(
      self,
      Stage::Start
        | Stage::HeaderSize
        | Stage::HeaderType
        | Stage::HeaderSizeType
        | Stage::HeaderTypeSize
    )
  }

  /// The stage after `info`: refused when the line has no place where the answer stands.
  fn after(self, info: &Info, at: u64) -> Result<Stage, Refusal> {
    match (self, info) {
      (Stage::Start, Info::Attr(Attr::Size)) => Ok(Stage::HeaderSize),
      (Stage::Start, Info::Attr(Attr::Type)) => Ok(Stage::HeaderType),
      (Stage::HeaderSize, Info::Attr(Attr::Type)) => Ok(Stage::HeaderSizeType),
      (Stage::HeaderType, Info::Attr(Attr::Size)) => Ok(Stage::HeaderTypeSize),
      (
        Stage::Start | Stage::ObjectsBare,
        Info::Object {
          values: Values::NotAsked | Values::Unknown,
          ..
        },
      ) => Ok(Stage::ObjectsBare),
      (
        Stage::HeaderSize | Stage::ObjectsSize,
        Info::Object {
          values: Values::Size(_) | Values::Unknown,
          ..
        },
      ) => Ok(Stage::ObjectsSize),
      (
        Stage::HeaderType | Stage::ObjectsType,
        Info::Object {
          values: Values::Type(_) | Values::Unknown,
          ..
        },
      ) => Ok(Stage::ObjectsType),
      (
        Stage::HeaderSizeType | Stage::ObjectsSizeType,
        Info::Object {
          values: Values::SizeType(..) | Values::Unknown,
          ..
        },
      ) => Ok(Stage::ObjectsSizeType),
      (
        Stage::HeaderTypeSize | Stage::ObjectsTypeSize,
        Info::Object {
          values: Values::TypeSize(..) | Values::Unknown,
          ..
        },
      ) => Ok(Stage::ObjectsTypeSize),
      _ => Err(Refusal::Line { at }),
    }
  }

  /// Parses the post-`SP` attribute values on an object line for the columns announced in `self`.
  fn parse_values(self, rest: Option<&[u8]>, at: u64) -> Result<Values, Refusal> {
    let Some(rest) = rest else {
      return Ok(Values::NotAsked);
    };
    if rest.is_empty() {
      return Ok(Values::Unknown);
    }
    match self {
      Stage::Start | Stage::ObjectsBare => Err(Refusal::Line { at }),
      Stage::HeaderSize | Stage::ObjectsSize => Ok(Values::Size(parse_size(rest, at)?)),
      Stage::HeaderType | Stage::ObjectsType => Ok(Values::Type(parse_kind(rest, at)?)),
      Stage::HeaderSizeType | Stage::ObjectsSizeType => {
        let (digits, kind_bytes) = split_sp(rest).ok_or(Refusal::Line { at })?;
        Ok(Values::SizeType(
          parse_size(digits, at)?,
          parse_kind(kind_bytes, at)?,
        ))
      }
      Stage::HeaderTypeSize | Stage::ObjectsTypeSize => {
        let (kind_bytes, digits) = split_sp(rest).ok_or(Refusal::Line { at })?;
        Ok(Values::TypeSize(
          parse_kind(kind_bytes, at)?,
          parse_size(digits, at)?,
        ))
      }
    }
  }

  /// Reads the line `text` writes where the answer stands, its id in `format`.
  fn read(self, text: &[u8], format: ObjectFormat, at: u64) -> Result<Info, Refusal> {
    if self.in_header() {
      if text == SIZE {
        return Ok(Info::Attr(Attr::Size));
      }
      if text == TYPE {
        return Ok(Info::Attr(Attr::Type));
      }
    }
    let (hex, rest) = match split_sp(text) {
      Some((head, tail)) => (head, Some(tail)),
      None => (text, None),
    };
    let id = line::object_id(format, hex, at)?;
    let values = self.parse_values(rest, at)?;
    Ok(Info::Object { id, values })
  }
}

/// The parts `info` is written in, its id in `format`, then `ending`: refused when the id is in another format.
fn parts<'a>(
  info: &'a Info,
  ending: Ending,
  format: ObjectFormat,
  at: u64,
) -> Result<[Part<'a>; ANSWER_LINE_PARTS], Refusal> {
  let none = Part::Bytes(b"");
  let end = Part::Bytes(ending.bytes());
  match info {
    Info::Attr(attr) => Ok([Part::Bytes(attr.bytes()), end, none, none, none, none]),
    Info::Object { id, values } => {
      line::check_format(id, format, at)?;
      let space = Part::Bytes(&[SP]);
      Ok(match values {
        Values::NotAsked => [Part::Id(id), end, none, none, none, none],
        Values::Unknown => [Part::Id(id), space, end, none, none, none],
        Values::Size(bytes) => [Part::Id(id), space, Part::Decimal(*bytes), end, none, none],
        Values::Type(kind) => [
          Part::Id(id),
          space,
          Part::Bytes(kind.name()),
          end,
          none,
          none,
        ],
        Values::SizeType(bytes, kind) => [
          Part::Id(id),
          space,
          Part::Decimal(*bytes),
          space,
          Part::Bytes(kind.name()),
          end,
        ],
        Values::TypeSize(kind, bytes) => [
          Part::Id(id),
          space,
          Part::Bytes(kind.name()),
          space,
          Part::Decimal(*bytes),
          end,
        ],
      })
    }
  }
}

/// Walks the answer in `bytes`, handing each line to `each`: how many there were.
fn walk(
  bytes: &[u8],
  format: ObjectFormat,
  mut each: impl FnMut(Line<Info>),
) -> Result<usize, Refusal> {
  let mut packets = Packets::new(bytes);
  let mut stage = Stage::Start;
  let mut count = 0usize;
  while let Some((text, ending, at)) = packets.next_line()? {
    let info = stage.read(text, format, at)?;
    stage = stage.after(&info, at)?;
    each(Line {
      value: info,
      ending,
    });
    count = count.saturating_add(1);
  }
  packets.finish()?;
  Ok(count)
}

/// Decodes an `object-info` answer, whole, its ids in `format`. Refused when a line has no place where it stands (an
/// object line with attributes and no header before it, duplicate attribute headers, an object line missing or
/// mismatching announced columns), an id, size, or type is invalid, a packet comes where the answer has none, the
/// bytes end before its flush or go on after it; or when its lines would take more than `admission` has left, charged
/// before the list is made.
pub fn decode_answer(
  bytes: &[u8],
  format: ObjectFormat,
  admission: &mut Admission,
) -> Result<Vec<Line<Info>>, Refusal> {
  let count = walk(bytes, format, |_| {})?;
  admission
    .charge_items::<Line<Info>>(count)
    .map_err(Refusal::PastAdmission)?;
  let mut lines = Vec::with_capacity(count);
  walk(bytes, format, |line| lines.push(line))?;
  Ok(lines)
}

/// Writes an `object-info` answer of `lines`, each as it ended, its ids in `format`, then its flush. Refused, nothing
/// written, when a line has no place where it stands or an id is in another format.
pub fn encode_answer(
  lines: &[Line<Info>],
  format: ObjectFormat,
  out: &mut impl Out,
) -> Result<(), Refusal> {
  write_answer(lines, format, &mut Measure::default())?;
  write_answer(lines, format, out)
}

/// Writes the answer's lines to `out`, checking each before it writes it.
fn write_answer(
  lines: &[Line<Info>],
  format: ObjectFormat,
  out: &mut impl Out,
) -> Result<(), Refusal> {
  let mut stage = Stage::Start;
  let mut at = 0u64;
  for line in lines {
    stage = stage.after(&line.value, at)?;
    let parts = parts(&line.value, line.ending, format, at)?;
    at = at.saturating_add(line::put_line(None, &parts, at, out)?);
  }
  line::put_special(Packet::Flush, out);
  Ok(())
}

/// Resolves the response column values for `id` given whether `size` and/or `type` were requested.
fn object_values(
  asked_size: bool,
  asked_type: bool,
  id: &ObjectId,
  objects: &impl ObjectInfo,
) -> Values {
  let Some(meta) = objects.info(id) else {
    return Values::Unknown;
  };
  match (asked_size, asked_type) {
    (true, true) => Values::SizeType(meta.size, meta.kind),
    (true, false) => Values::Size(meta.size),
    (false, true) => Values::Type(meta.kind),
    (false, false) => Values::NotAsked,
  }
}

/// Answers a request's `arguments` as stock Git 2.56.1 does, each object's metadata from `objects`, its ids in
/// `format`: nothing but the flush when no object is named; otherwise `size` and/or `type` attribute headers (in that
/// order), then a line for each object in the order named, every line without an LF. Refused, nothing written, when an
/// id is in another format.
pub fn answer(
  arguments: &[Line<Argument>],
  format: ObjectFormat,
  objects: &impl ObjectInfo,
  out: &mut impl Out,
) -> Result<(), Refusal> {
  write_answer_to(arguments, format, objects, &mut Measure::default())?;
  write_answer_to(arguments, format, objects, out)
}

/// Writes one pass of the answer to `arguments` into `out`, checking each line before writing it.
/// Callers that already bracket emission with their own measuring pass (such as `upload::charged`)
/// call this directly so `ObjectInfo::info` runs once per pass instead of twice per pass.
pub(crate) fn write_answer_to(
  arguments: &[Line<Argument>],
  format: ObjectFormat,
  objects: &impl ObjectInfo,
  out: &mut impl Out,
) -> Result<(), Refusal> {
  let asked_size = arguments.iter().any(|line| line.value == Argument::Size);
  let asked_type = arguments.iter().any(|line| line.value == Argument::Type);
  let mut ids = arguments.iter().filter_map(|line| match line.value {
    Argument::Oid(id) => Some(id),
    Argument::Size | Argument::Type => None,
  });
  let Some(first) = ids.next() else {
    line::put_special(Packet::Flush, out);
    return Ok(());
  };
  let mut at = 0u64;
  if asked_size {
    at = at.saturating_add(line::put_line(None, &[Part::Bytes(SIZE)], at, out)?);
  }
  if asked_type {
    at = at.saturating_add(line::put_line(None, &[Part::Bytes(TYPE)], at, out)?);
  }
  for id in core::iter::once(first).chain(ids) {
    let values = object_values(asked_size, asked_type, &id, objects);
    let info = Info::Object { id, values };
    let parts = parts(&info, Ending::NoLf, format, at)?;
    at = at.saturating_add(line::put_line(None, &parts, at, out)?);
  }
  line::put_special(Packet::Flush, out);
  Ok(())
}

#[cfg(test)]
#[path = "tests/object_info.rs"]
mod tests;
