//! `ls-refs` (A-582, part 2; gitprotocol-v2 "ls-refs" [B]; `ls-refs.c` `ls_refs` and `send_ref`, `connect.c`
//! `get_remote_refs` and `process_ref_v2` [C]): its arguments, and its answer, one line a ref.
//!
//! The arguments are `peel`, `symrefs`, `unborn` and `ref-prefix <prefix>`, each a whole line, in any order, any of
//! them again; anything else is refused, as `ls-refs.c` dies on it ("unexpected line"). A prefix is any bytes of a
//! text line, the empty one too, which git reads as matching every ref. `ls-refs.c` stops honouring prefixes past
//! 65536 of them (`TOO_MANY_PREFIXES`); that is a server's choice, since the document lets a server show refs no prefix
//! matches, and the request's admission bounds how many a decode holds.
//!
//! The answer:
//!
//! ```text
//! output = *ref flush-pkt
//! obj-id-or-unborn = (obj-id | "unborn")
//! ref = PKT-LINE(obj-id-or-unborn SP refname *(SP ref-attribute) LF)
//! ref-attribute = (symref | peeled)
//! symref = "symref-target:" symref-target
//! peeled = "peeled:" obj-id
//! ```
//!
//! git's server writes a symref's target before a peeled id; its client reads them in either order ("don't have a
//! particular order"), so a line is read in either and written back in its own. Each kind comes at most once: git's
//! server writes one of each, and its client would make a second `peeled` a second `^{}` ref. An attribute of another
//! kind is refused.

use super::line::{self, Line, Packets, Part, SP};
use super::{Refusal, refname};
use crate::Admission;
use crate::object_name::{ObjectFormat, ObjectId};
use crate::out::{Measure, Out};
use crate::pktline::Packet;

/// Format: the argument asking for peeled tags [gitprotocol-v2 "ls-refs"].
const PEEL: &[u8] = b"peel";

/// Format: the argument asking for symbolic refs' targets [gitprotocol-v2 "ls-refs"].
const SYMREFS: &[u8] = b"symrefs";

/// Format: the argument asking for `HEAD` when it names an unborn branch [gitprotocol-v2 "ls-refs"].
const UNBORN: &[u8] = b"unborn";

/// Format: the argument naming a prefix, and the space after its word [gitprotocol-v2: `ref-prefix <prefix>`].
const REF_PREFIX: &[u8] = b"ref-prefix ";

/// Format: the word an answer line holds in place of an id for an unborn ref [gitprotocol-v2: `obj-id-or-unborn`].
const UNBORN_TARGET: &[u8] = b"unborn";

/// Format: what a symref's target follows [gitprotocol-v2: `symref = "symref-target:" symref-target`].
const SYMREF_TARGET: &[u8] = b"symref-target:";

/// Format: what a peeled tag's id follows [gitprotocol-v2: `peeled = "peeled:" obj-id`].
const PEELED: &[u8] = b"peeled:";

/// Format: the kinds of attribute an answer line may carry, each at most once: a symref's target and a tag's peeled
/// id.
const ATTRIBUTE_KINDS: usize = 2;

/// Derived: the parts one answer line is written in: its target, a space, its name, three for each attribute (a
/// space, the attribute's word, its value) and its ending.
const LINE_PARTS: usize = 4 + 3 * ATTRIBUTE_KINDS;

/// One `ls-refs` argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Argument<'a> {
  /// `peel`: show peeled tags.
  Peel,
  /// `symrefs`: show a symbolic ref's target.
  Symrefs,
  /// `unborn`: show `HEAD` even when it names an unborn branch.
  Unborn,
  /// `ref-prefix <prefix>`: show the refs beginning with one of the prefixes named.
  RefPrefix(&'a [u8]),
}

impl<'a> Argument<'a> {
  /// The argument a line's text writes. Refused when it writes none.
  pub(crate) fn parse(text: &'a [u8], at: u64) -> Result<Argument<'a>, Refusal> {
    if let Some(prefix) = text.strip_prefix(REF_PREFIX) {
      return Ok(Argument::RefPrefix(prefix));
    }
    [Argument::Peel, Argument::Symrefs, Argument::Unborn]
      .into_iter()
      .find(|flag| flag.parts().first() == Some(&text))
      .ok_or(Refusal::UnknownArgument { at })
  }

  /// The line's text in parts: the word, then the prefix where there is one.
  pub(crate) fn parts(&self) -> [&'a [u8]; 2] {
    match self {
      Argument::Peel => [PEEL, b""],
      Argument::Symrefs => [SYMREFS, b""],
      Argument::Unborn => [UNBORN, b""],
      Argument::RefPrefix(prefix) => [REF_PREFIX, prefix],
    }
  }

  /// Refused when a prefix holds a NUL or an LF.
  pub(crate) fn check(&self, at: u64) -> Result<(), Refusal> {
    match self {
      Argument::RefPrefix(prefix) => line::check_text(prefix, at),
      _ => Ok(()),
    }
  }
}

/// Whether `arguments` holds `flag`.
pub fn holds(arguments: &[Line<Argument<'_>>], flag: Argument<'_>) -> bool {
  arguments.iter().any(|line| line.value == flag)
}

/// The prefixes `arguments` name, in order.
pub fn prefixes<'s, 'a>(
  arguments: &'s [Line<Argument<'a>>],
) -> impl Iterator<Item = &'a [u8]> + use<'s, 'a> {
  arguments.iter().filter_map(|line| match line.value {
    Argument::RefPrefix(prefix) => Some(prefix),
    _ => None,
  })
}

/// What a ref names: an object, or nothing yet, for a symbolic ref to an unborn branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
  /// The object the ref names.
  Id(ObjectId),
  /// `unborn`: a symbolic ref (`HEAD`) to a branch with no commit yet.
  Unborn,
}

/// An attribute of an answer line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attribute<'a> {
  /// `symref-target:<name>`: the ref a symbolic ref names.
  SymrefTarget(&'a [u8]),
  /// `peeled:<id>`: what an annotated tag names, peeled.
  Peeled(ObjectId),
}

/// An answer line's attributes, in the order they came, at most one of each kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attributes<'a> {
  /// The attributes, in order, then nothing.
  slots: [Option<Attribute<'a>>; ATTRIBUTE_KINDS],
}

impl<'a> Attributes<'a> {
  /// These attributes and `attribute` after them; `None` when one of its kind is here already.
  pub fn with(self, attribute: Attribute<'a>) -> Option<Attributes<'a>> {
    let kind = core::mem::discriminant(&attribute);
    if self
      .iter()
      .any(|held| core::mem::discriminant(&held) == kind)
    {
      return None;
    }
    let mut slots = self.slots;
    *slots.iter_mut().find(|slot| slot.is_none())? = Some(attribute);
    Some(Attributes { slots })
  }

  /// The attributes, in order.
  pub fn iter(&self) -> impl Iterator<Item = Attribute<'a>> + use<'a> {
    self.slots.into_iter().flatten()
  }

  /// The symbolic ref's target, if the line names one.
  pub fn symref_target(&self) -> Option<&'a [u8]> {
    self.iter().find_map(|attribute| match attribute {
      Attribute::SymrefTarget(name) => Some(name),
      Attribute::Peeled(_) => None,
    })
  }

  /// The peeled id, if the line names one.
  pub fn peeled(&self) -> Option<ObjectId> {
    self.iter().find_map(|attribute| match attribute {
      Attribute::Peeled(id) => Some(id),
      Attribute::SymrefTarget(_) => None,
    })
  }
}

/// One line of an `ls-refs` answer: a ref.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ref<'a> {
  /// What it names.
  pub target: Target,
  /// Its name: `HEAD`, or a name under `refs/`.
  pub name: &'a [u8],
  /// Its attributes.
  pub attributes: Attributes<'a>,
}

impl<'a> Ref<'a> {
  /// The ref an answer line's text writes, its ids in `format`.
  fn parse(text: &'a [u8], format: ObjectFormat, at: u64) -> Result<Ref<'a>, Refusal> {
    let mut words = text.split(|byte| *byte == SP);
    let target = match words.next().unwrap_or_default() {
      UNBORN_TARGET => Target::Unborn,
      hex => Target::Id(line::object_id(format, hex, at)?),
    };
    let name = words.next().ok_or(Refusal::Line { at })?;
    if !refname::is_valid(name) {
      return Err(Refusal::Refname { at });
    }
    let mut attributes = Attributes::default();
    for word in words {
      attributes = attributes
        .with(attribute(word, format, at)?)
        .ok_or(Refusal::Repeated { at })?;
    }
    Ok(Ref {
      target,
      name,
      attributes,
    })
  }

  /// Refused when a name or a target is not a ref name, or an id is not of `format`.
  fn check(&self, format: ObjectFormat, at: u64) -> Result<(), Refusal> {
    if let Target::Id(id) = &self.target {
      line::check_format(id, format, at)?;
    }
    if !refname::is_valid(self.name) {
      return Err(Refusal::Refname { at });
    }
    self
      .attributes
      .iter()
      .try_for_each(|attribute| check_attribute(attribute, format, at))
  }
}

/// The attribute `word` writes.
fn attribute<'a>(word: &'a [u8], format: ObjectFormat, at: u64) -> Result<Attribute<'a>, Refusal> {
  if let Some(name) = word.strip_prefix(SYMREF_TARGET) {
    let target = Attribute::SymrefTarget(name);
    check_attribute(target, format, at)?;
    return Ok(target);
  }
  match word.strip_prefix(PEELED) {
    Some(hex) => Ok(Attribute::Peeled(line::object_id(format, hex, at)?)),
    None => Err(Refusal::Line { at }),
  }
}

/// Refused when a target is not a ref name or a peeled id is not of `format`.
fn check_attribute(attribute: Attribute<'_>, format: ObjectFormat, at: u64) -> Result<(), Refusal> {
  match attribute {
    Attribute::SymrefTarget(name) if refname::is_valid(name) => Ok(()),
    Attribute::SymrefTarget(_) => Err(Refusal::Refname { at }),
    Attribute::Peeled(id) => line::check_format(&id, format, at),
  }
}

/// Decodes an `ls-refs` answer, whole, its ids in `format`: refused when a line is not a ref, a packet other than the
/// closing flush comes, the bytes end before it or go on after it; or when the list of refs would take more than
/// `admission` has left, charged before the list is made.
pub fn decode_answer<'a>(
  bytes: &'a [u8],
  format: ObjectFormat,
  admission: &mut Admission,
) -> Result<Vec<Line<Ref<'a>>>, Refusal> {
  let count = walk(bytes, format, |_| {})?;
  admission
    .charge_items::<Line<Ref<'a>>>(count)
    .map_err(Refusal::PastAdmission)?;
  let mut refs = Vec::with_capacity(count);
  walk(bytes, format, |line| refs.push(line))?;
  Ok(refs)
}

/// Walks the answer in `bytes`, handing each ref to `each`: how many there were.
fn walk<'a>(
  bytes: &'a [u8],
  format: ObjectFormat,
  mut each: impl FnMut(Line<Ref<'a>>),
) -> Result<usize, Refusal> {
  let mut packets = Packets::new(bytes);
  let mut count = 0usize;
  while let Some((text, ending, at)) = packets.next_line()? {
    each(Line {
      value: Ref::parse(text, format, at)?,
      ending,
    });
    count = count.saturating_add(1);
  }
  packets.finish()?;
  Ok(count)
}

/// Writes an `ls-refs` answer: each ref's line as it ended, then the flush. Refused, nothing written, when a ref is
/// not one the protocol carries in `format` or its line is longer than a packet carries.
pub fn encode_answer(
  refs: &[Line<Ref<'_>>],
  format: ObjectFormat,
  out: &mut impl Out,
) -> Result<(), Refusal> {
  write_answer(refs, format, &mut Measure::default())?;
  write_answer(refs, format, out)
}

/// Writes the answer to `out`, checking each line before it writes it.
fn write_answer(
  refs: &[Line<Ref<'_>>],
  format: ObjectFormat,
  out: &mut impl Out,
) -> Result<(), Refusal> {
  let mut at = 0u64;
  for line in refs {
    at = at.saturating_add(write_ref(line, format, at, out)?);
  }
  line::put_special(Packet::Flush, out);
  Ok(())
}

/// Writes one ref's line, `at` bytes into its answer, so a server can stream an answer a ref at a time: the bytes it
/// wrote. Refused, nothing written, as [`encode_answer`] refuses.
pub fn write_ref(
  line: &Line<Ref<'_>>,
  format: ObjectFormat,
  at: u64,
  out: &mut impl Out,
) -> Result<u64, Refusal> {
  line.value.check(format, at)?;
  let target = match &line.value.target {
    Target::Id(id) => Part::Id(id),
    Target::Unborn => Part::Bytes(UNBORN_TARGET),
  };
  let mut parts = [Part::Bytes(b""); LINE_PARTS];
  let head = [target, Part::Bytes(&[SP]), Part::Bytes(line.value.name)];
  let attributes = line.value.attributes;
  let peeled = attributes.peeled();
  let tail = attributes.iter().flat_map(|attribute| match attribute {
    Attribute::SymrefTarget(name) => [
      Part::Bytes(&[SP]),
      Part::Bytes(SYMREF_TARGET),
      Part::Bytes(name),
    ],
    Attribute::Peeled(_) => [
      Part::Bytes(&[SP]),
      Part::Bytes(PEELED),
      peeled.as_ref().map_or(Part::Bytes(b""), Part::Id),
    ],
  });
  let ending = Part::Bytes(line.ending.bytes());
  for (slot, part) in parts
    .iter_mut()
    .zip(head.into_iter().chain(tail).chain([ending]))
  {
    *slot = part;
  }
  line::put_line(None, &parts, at, out)
}

#[cfg(test)]
#[path = "tests/ls_refs.rs"]
mod tests;
