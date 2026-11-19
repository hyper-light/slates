//! `object-info` (A-624; gitprotocol-v2 "object-info" \[B\]; `protocol-caps.c` `cap_object_info` and `send_info`,
//! `serve.c` \[C\]): its arguments, and its answer, the size of each object asked about.
//!
//! The arguments are `size`, asking for each object's size, and `oid <id>`, naming an object, each a whole line, in any
//! order, either again. A request that names no object is answered by the flush alone. Otherwise the answer is a line
//! naming the attributes asked for, `size` (git 2.50.1's only one), when the request asked for it; then a line for each
//! object, in the order named: its id, and with `size` a space and its size in bytes, or the space alone when the
//! server holds no such object; then a flush.
//!
//! ```text
//! output = info flush-pkt
//! info = PKT-LINE(attrs) LF)
//!        *PKT-LINE(obj-info LF)
//! attrs = attr | attrs SP attrs
//! attr = "size"
//! obj-info = obj-id SP obj-size
//! ```
//!
//! **As git 2.50.1 answers** (`send_info`): no attribute line and each id alone when `size` was not asked for, and
//! every line without an LF (`packet_writer_write`), where the document's grammar ends each with one. A reader takes
//! either ending and records it, as every v2 line does, so a decoded answer writes back to its bytes.
//!
//! **Where git departs, and why this does.** git answers an argument it does not know with an `ERR` line and goes on,
//! and an id it cannot read with another; a request with either is refused here by kind before any answer, as `fetch`'s
//! and `ls-refs`'s are. git echoes each id as its client wrote it; the answer here writes it as `oid_to_hex` does, in
//! lower case (A-624's rule): the same bytes for every client git ships, each of which writes ids in lower case. git's
//! master, after 2.50.1, adds a `type` attribute; it is owed when a release ships it, with its golden vectors.
//!
//! **Answered over a trait.** The answer asks a repository one thing, an object's size ([`ObjectSizes`]); A-614's store
//! answers it from the object's row ([`Stored`]). Nothing is allocated to answer: each line is written as it is made,
//! once measured and once written, so a refused answer writes nothing.

use slates_vcs::ContentStore;

use super::Refusal;
use super::line::{self, Ending, Line, Packets, Part, SP};
use crate::Admission;
use crate::object_name::{ObjectFormat, ObjectId};
use crate::out::{Measure, Out};
use crate::pktline::Packet;
use crate::store::Repository;

/// Format: the argument asking for each object's size, and the attribute line naming it [gitprotocol-v2
/// "object-info": `attr = "size"`].
const SIZE: &[u8] = b"size";

/// Format: the word before an argument's object id [gitprotocol-v2 "object-info": `oid <oid>`].
const OID: &[u8] = b"oid";

/// Format: the most parts an answer line is written in: an id, a space, a size and its ending.
const ANSWER_LINE_PARTS: usize = 4;

/// One `object-info` argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Argument {
  /// `size`: answer each object's size.
  Size,
  /// `oid <id>`: an object to answer about.
  Oid(ObjectId),
}

impl Argument {
  /// The argument a line's text writes, its id in `format`: refused when it writes none (`cap_object_info`'s
  /// "unexpected line"), or an id that is none.
  pub(crate) fn parse(text: &[u8], format: ObjectFormat, at: u64) -> Result<Argument, Refusal> {
    let _ = (text, format);
    Err(Refusal::UnknownArgument { at })
  }

  /// Refused when the argument's id is not in `format`.
  pub(crate) fn check(&self, format: ObjectFormat, at: u64) -> Result<(), Refusal> {
    match self {
      Argument::Size => Ok(()),
      Argument::Oid(id) => line::check_format(id, format, at),
    }
  }

  /// The line's text in parts: the word, a space where there is an id, and the id.
  pub(super) fn parts(&self) -> [Part<'_>; 3] {
    match self {
      Argument::Size => [Part::Bytes(SIZE), Part::Bytes(b""), Part::Bytes(b"")],
      Argument::Oid(id) => [Part::Bytes(OID), Part::Bytes(&[SP]), Part::Id(id)],
    }
  }
}

/// What an answer's line about an object says of its size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
  /// The request asked for no size: the line is the id alone.
  NotAsked,
  /// The server holds no such object: the id and a space.
  Unknown,
  /// The object's size, in bytes.
  Bytes(u64),
}

/// One line of an `object-info` answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Info {
  /// The attribute line, `size`: every object line after it gives a size.
  Size,
  /// An object's line.
  Object {
    /// The object.
    id: ObjectId,
    /// What the line says of its size.
    size: Size,
  },
}

/// What a repository answers `object-info` from.
pub trait ObjectSizes {
  /// The size in bytes of the object `id` names, or `None` when the repository holds no such object.
  fn size(&self, id: &ObjectId) -> Option<u64>;
}

/// A-614's store, read for `object-info`: an object's size is its row's length, and an object with no row (none in
/// the repository, or one a push holds until its verdict) is unknown, as git's quarantine hides a push's objects.
#[derive(Clone, Copy)]
pub struct Stored<'a> {
  /// The repository.
  pub repository: &'a Repository,
  /// The content store its index lives in.
  pub content: &'a ContentStore,
}

impl ObjectSizes for Stored<'_> {
  fn size(&self, id: &ObjectId) -> Option<u64> {
    self.repository.object(self.content, id).map(|row| row.len)
  }
}

/// Where an answer stands, which decides what its next line may be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
  /// No line yet: the attribute line, an object's line alone, or the flush.
  Start,
  /// After `size`: object lines with a size, or with the space alone.
  Sized,
  /// After an object line with no attribute line: ids alone.
  Bare,
}

impl Stage {
  /// The stage after `info`: refused when the line has no place where the answer stands.
  fn after(self, info: &Info, at: u64) -> Result<Stage, Refusal> {
    match (self, info) {
      (Stage::Start, Info::Size) => Ok(Stage::Sized),
      (
        Stage::Start | Stage::Bare,
        Info::Object {
          size: Size::NotAsked,
          ..
        },
      ) => Ok(Stage::Bare),
      (Stage::Sized, Info::Object { size, .. }) if *size != Size::NotAsked => Ok(Stage::Sized),
      _ => Err(Refusal::Line { at }),
    }
  }

  /// Reads the line `text` writes where the answer stands, its id in `format`.
  fn read(self, text: &[u8], format: ObjectFormat, at: u64) -> Result<Info, Refusal> {
    if self == Stage::Start && text == SIZE {
      return Ok(Info::Size);
    }
    let (hex, rest) = match text.iter().position(|byte| *byte == SP) {
      Some(split) => (
        text.get(..split).unwrap_or_default(),
        text.get(split.saturating_add(1)..),
      ),
      None => (text, None),
    };
    let id = line::object_id(format, hex, at)?;
    let size = match rest {
      None => Size::NotAsked,
      Some([]) => Size::Unknown,
      Some(digits) => Size::Bytes(line::decimal(digits).ok_or(Refusal::Number { at })?),
    };
    Ok(Info::Object { id, size })
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
    Info::Size => Ok([Part::Bytes(SIZE), end, none, none]),
    Info::Object { id, size } => {
      line::check_format(id, format, at)?;
      let space = Part::Bytes(&[SP]);
      Ok(match size {
        Size::NotAsked => [Part::Id(id), end, none, none],
        Size::Unknown => [Part::Id(id), space, end, none],
        Size::Bytes(bytes) => [Part::Id(id), space, Part::Decimal(*bytes), end],
      })
    }
  }
}

/// Walks the answer in `bytes`, handing each line to `each`: how many there were.
fn walk<'a>(
  bytes: &'a [u8],
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
/// object line with a size and no `size` line before it, or one without a size after it), an id or a size is none, a
/// packet comes where the answer has none, the bytes end before its flush or go on after it; or when its lines would
/// take more than `admission` has left, charged before the list is made.
pub fn decode_answer<'a>(
  bytes: &'a [u8],
  format: ObjectFormat,
  admission: &mut Admission,
) -> Result<Vec<Line<Info>>, Refusal> {
  let _ = (bytes, format, admission);
  Err(Refusal::Unfinished { at: 0 })
}

/// Writes an `object-info` answer of `lines`, each as it ended, its ids in `format`, then its flush. Refused, nothing
/// written, when a line has no place where it stands or an id is in another format.
pub fn encode_answer(
  lines: &[Line<Info>],
  format: ObjectFormat,
  out: &mut impl Out,
) -> Result<(), Refusal> {
  let _ = (lines, format, out);
  Err(Refusal::Line { at: 0 })
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

/// Answers a request's `arguments` as git 2.50.1 does, each object's size from `sizes`, its ids in `format`: nothing
/// but the flush when no object is named; otherwise `size` where it was asked for, then a line for each object in the
/// order named, every line without an LF. Refused, nothing written, when an id is in another format.
pub fn answer(
  arguments: &[Line<Argument>],
  format: ObjectFormat,
  sizes: &impl ObjectSizes,
  out: &mut impl Out,
) -> Result<(), Refusal> {
  let _ = (arguments, format, sizes, out);
  Err(Refusal::Line { at: 0 })
}

/// Writes the answer to `arguments` to `out`, checking each line before it writes it.
fn write_answer_to(
  arguments: &[Line<Argument>],
  format: ObjectFormat,
  sizes: &impl ObjectSizes,
  out: &mut impl Out,
) -> Result<(), Refusal> {
  let asked = arguments.iter().any(|line| line.value == Argument::Size);
  let mut ids = arguments.iter().filter_map(|line| match line.value {
    Argument::Oid(id) => Some(id),
    Argument::Size => None,
  });
  let Some(first) = ids.next() else {
    line::put_special(Packet::Flush, out);
    return Ok(());
  };
  let mut at = 0u64;
  if asked {
    at = line::put_line(None, &[Part::Bytes(SIZE)], at, out)?;
  }
  for id in core::iter::once(first).chain(ids) {
    let size = match (asked, sizes.size(&id)) {
      (false, _) => Size::NotAsked,
      (true, None) => Size::Unknown,
      (true, Some(bytes)) => Size::Bytes(bytes),
    };
    let info = Info::Object { id, size };
    let parts = parts(&info, Ending::NoLf, format, at)?;
    at = at.saturating_add(line::put_line(None, &parts, at, out)?);
  }
  line::put_special(Packet::Flush, out);
  Ok(())
}

#[cfg(test)]
#[path = "tests/object_info.rs"]
mod tests;
