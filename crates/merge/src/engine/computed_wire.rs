//! A computed verdict's wire form (A-658 part 3a; A-645 "Merges computed anywhere, committed by position"; §4.16 "Apply
//! on holders"): what a green's holder sends the green's owner, so the owner takes a verdict the holder computed on its
//! replica by position ([`Green::judge_computed`]) and judges again only what changed since, never the whole work.
//!
//! **What crosses.** The identity and base of the increment it judged, the head it was computed against (the position the
//! owner checks), the computing copy's fold floor, and what it reached: the version the copy had accepted the increment
//! as, or the verdict unit by unit in the order the work's units are visited (`position`: the owner pairs them with its
//! own by that order), each unit its effects, its conflict windows and where its content ops stood. An effect crosses as
//! a chain entry records it ([`crate::chain_entry::Applied`]: content by reference to the increment's post-state, never a
//! file's bytes) through the chain entry's own codec, so one encoder and one decoder serve both; the owner holds the
//! increment, so nothing it holds crosses again.
//!
//! **Read back by the owner** ([`Green::computed_from`]) against that increment: each effect rebuilt as a replica's
//! replay rebuilds it ([`Green::apply_recorded`]: the head's bytes and the increment's post-state), except a splice,
//! which stays unmade as the live verdict leaves it, until a commit makes the file over the head it was judged at, so a
//! conflict builds no file (A-658 part 1, merged with "a verdict makes no file until it accepts"). A unit whose paths
//! changed since its position is judged again and what it read back dropped; a unit nothing it reads changed since reads
//! the same bytes at both heads, so its effects are the computing copy's.
//!
//! **The encoding** follows the chain entry's: magic and version, little-endian, every count checked against the bytes
//! that remain before anything is allocated, a BLAKE3 digest of everything before it last, any malformation refused
//! typed ([`DocDecodeError`]; a damaged message reads as one cut short, the chain entry's rule). The digest finds a
//! damaged message; the holder is trusted as its replica is: a member of the green's fleet whose head identity the
//! record plane compares at every version (§4.16 "Apply on holders").

use super::copy::EntryCopies;
use super::position::{Placed, Unit};
use super::{Computed, ConflictWindow, ContentSource, Effect, Green, Increment, Reached};
use crate::chain_entry::{Applied, Declared, put_applied, take_applied};
use crate::map::Mapped;
use crate::ops_doc::{DocDecodeError, Reader};
use crate::origin::{put_bytes, put_count, take_str};
use crate::range::Range;
use crate::verdict::MergeConflictClass;

/// Format: the wire form's magic, "GCMP" little-endian, so bytes of another kind are refused at the first word.
const MAGIC: u32 = u32::from_le_bytes(*b"GCMP");
/// Format: the wire form's version; a decoder refuses any other.
const VERSION: u32 = 1;
/// Format: an increment's identity and the trailing digest, each a BLAKE3 hash.
const HASH_BYTES: usize = 32;
/// Format: the tag of a verdict the computing copy had answered before, as the version it accepted the increment as.
const BEFORE: u8 = 0;
/// Format: the tag of a verdict reached unit by unit.
const VERDICT: u8 = 1;
/// Format: the tag of a range that mapped to a shifted range at the head.
const SHIFTED: u8 = 0;
/// Format: the tag of a range that met an intervening change.
const OVERLAPS: u8 = 1;
/// Format: the smallest a unit encodes to: its three counts (effects, windows, placements).
const UNIT_MIN_BYTES: usize = 3 * 4;
/// Format: the smallest an effect encodes to, as a chain entry writes one: its tag and one length prefix.
const EFFECT_MIN_BYTES: usize = 1 + 4;
/// Format: the smallest a window encodes to: its path's length prefix, its range's start and length, its class.
const WINDOW_MIN_BYTES: usize = 4 + 8 + 8 + 1;
/// Format: the smallest a placement encodes to: two tags of ranges that met a change.
const PLACED_MIN_BYTES: usize = 2;
/// Format: the conflict classes in their wire order, a class's tag its index here: appended to, never reordered. A class
/// missing here writes a tag no reader takes, refused typed where it is read, never misread as another.
const CLASSES: [MergeConflictClass; 9] = [
  MergeConflictClass::Overlap,
  MergeConflictClass::AnchoredInDelete,
  MergeConflictClass::SamePositionDiffering,
  MergeConflictClass::RenameRename,
  MergeConflictClass::CreateCreate,
  MergeConflictClass::DeleteModify,
  MergeConflictClass::ModifyDelete,
  MergeConflictClass::TypeChanged,
  MergeConflictClass::MetaMeta,
];

impl Computed {
  /// This verdict's wire form (module docs), written against `inc`, the increment it judged, whose declared ops its
  /// effects name: what a holder sends the green's owner, which reads it back with [`Green::computed_from`].
  pub fn encode(&self, inc: &Increment) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&self.id);
    for word in [self.base, self.at, self.floor] {
      out.extend_from_slice(&word.to_le_bytes());
    }
    match &self.reached {
      Reached::Before(version) => {
        out.push(BEFORE);
        out.extend_from_slice(&version.to_le_bytes());
      }
      Reached::Verdict { units, .. } => {
        out.push(VERDICT);
        let declared = Declared::of(inc);
        put_count(&mut out, units.len());
        for unit in units {
          put_unit(&mut out, &declared, unit);
        }
      }
    }
    let digest = *blake3::hash(&out).as_bytes();
    out.extend_from_slice(&digest);
    out
  }
}

impl Green {
  /// A verdict another copy of this green computed, read back from its wire form ([`Computed::encode`]) against `inc`,
  /// the increment it judged, for [`Green::judge_computed`]: each effect rebuilt over this green's head and `inc`'s
  /// post-state (module docs). Refused typed, nothing kept: a damaged message, another magic or version, a count past
  /// the bytes that remain, a tag no form has, bytes after the last field.
  pub fn computed_from(&self, inc: &Increment, bytes: &[u8]) -> Result<Computed, DocDecodeError> {
    let mut reader = checked(bytes)?;
    let id = <[u8; HASH_BYTES]>::try_from(reader.bytes(HASH_BYTES)?)
      .map_err(|_| DocDecodeError::Truncated)?;
    let base = reader.u64()?;
    let at = reader.u64()?;
    let floor = reader.u64()?;
    let reached = match reader.u8()? {
      BEFORE => Reached::Before(reader.u64()?),
      VERDICT => Reached::of(self.units_from(&mut reader, inc)?),
      _ => return Err(DocDecodeError::BadKind),
    };
    if !reader.is_empty() {
      return Err(DocDecodeError::TrailingBytes);
    }
    Ok(Computed {
      id,
      base,
      at,
      floor,
      reached,
    })
  }

  /// A verdict's units read back, each effect rebuilt (module docs): every unit read first, since a copied file's
  /// effect reads the copies the whole verdict makes ([`EntryCopies`]), as a replay reads its entry's.
  fn units_from(
    &self,
    reader: &mut Reader<'_>,
    inc: &Increment,
  ) -> Result<Vec<Unit>, DocDecodeError> {
    let declared = Declared::of(inc);
    let count = count_of(reader, UNIT_MIN_BYTES)?;
    let mut read = Vec::with_capacity(count);
    for _ in 0..count {
      read.push(take_unit(reader, &declared)?);
    }
    let every: Vec<Applied> = read
      .iter()
      .flat_map(|unit| unit.applied.iter().cloned())
      .collect();
    let copies = EntryCopies::of(&every);
    Ok(
      read
        .into_iter()
        .map(|unit| Unit {
          effects: unit
            .applied
            .iter()
            .map(|applied| self.verdict_effect(applied, (&inc.post_state, &copies)))
            .collect(),
          windows: unit.windows,
          placed: unit.placed,
        })
        .collect(),
    )
  }

  /// The effect the live verdict holds for `applied` (module docs): a splice unmade, every other as a replay rebuilds it.
  fn verdict_effect(&self, applied: &Applied, sources: (&[u8], &EntryCopies<'_>)) -> Effect {
    match applied {
      Applied::Spliced(path, ops) => {
        Effect::SetContent(path.clone(), None, ops.clone(), ContentSource::Mapped)
      }
      other => self.effect_of(other, sources),
    }
  }
}

/// One unit as read, before its effects are rebuilt.
struct ReadUnit {
  /// Its effects, as a chain entry records them.
  applied: Vec<Applied>,
  /// Its conflict windows.
  windows: Vec<ConflictWindow>,
  /// Where its content ops stood.
  placed: Vec<Placed>,
}

/// Appends one unit: its effects, its windows, its placements, each counted.
fn put_unit(out: &mut Vec<u8>, declared: &Declared<'_>, unit: &Unit) {
  put_count(out, unit.effects.len());
  for effect in &unit.effects {
    put_applied(out, declared, &effect.applied());
  }
  put_count(out, unit.windows.len());
  for window in &unit.windows {
    put_bytes(out, window.path.as_bytes());
    put_range(out, window.range);
    out.push(class_tag(window.class));
  }
  put_count(out, unit.placed.len());
  for placed in &unit.placed {
    let (range, point) = placed.parts();
    put_mapped(out, range);
    put_mapped(out, point);
  }
}

/// Reads one unit [`put_unit`] wrote.
fn take_unit(reader: &mut Reader<'_>, declared: &Declared<'_>) -> Result<ReadUnit, DocDecodeError> {
  let count = count_of(reader, EFFECT_MIN_BYTES)?;
  let mut applied = Vec::with_capacity(count);
  for _ in 0..count {
    applied.push(take_applied(reader, declared)?);
  }
  let count = count_of(reader, WINDOW_MIN_BYTES)?;
  let mut windows = Vec::with_capacity(count);
  for _ in 0..count {
    windows.push(take_window(reader)?);
  }
  let count = count_of(reader, PLACED_MIN_BYTES)?;
  let mut placed = Vec::with_capacity(count);
  for _ in 0..count {
    let range = take_mapped(reader)?;
    placed.push(Placed::from_parts(range, take_mapped(reader)?));
  }
  Ok(ReadUnit {
    applied,
    windows,
    placed,
  })
}

/// Reads one window: its path, its range and its class.
fn take_window(reader: &mut Reader<'_>) -> Result<ConflictWindow, DocDecodeError> {
  let path = take_str(reader)?;
  let range = take_range(reader)?;
  let class = CLASSES
    .get(usize::from(reader.u8()?))
    .copied()
    .ok_or(DocDecodeError::BadKind)?;
  Ok(ConflictWindow { path, range, class })
}

/// A conflict class's tag ([`CLASSES`]).
fn class_tag(class: MergeConflictClass) -> u8 {
  CLASSES
    .iter()
    .position(|listed| *listed == class)
    .and_then(|at| u8::try_from(at).ok())
    .unwrap_or(u8::MAX)
}

/// Appends a range: its start and its length.
fn put_range(out: &mut Vec<u8>, range: Range) {
  out.extend_from_slice(&range.start.to_le_bytes());
  out.extend_from_slice(&range.len.to_le_bytes());
}

/// Reads a range [`put_range`] wrote.
fn take_range(reader: &mut Reader<'_>) -> Result<Range, DocDecodeError> {
  let start = reader.u64()?;
  Ok(Range::new(start, reader.u64()?))
}

/// Appends a mapped range: shifted, with its range, or overlapping a change.
fn put_mapped(out: &mut Vec<u8>, mapped: Mapped) {
  match mapped {
    Mapped::Shifted(range) => {
      out.push(SHIFTED);
      put_range(out, range);
    }
    Mapped::Overlaps => out.push(OVERLAPS),
  }
}

/// Reads a mapped range [`put_mapped`] wrote.
fn take_mapped(reader: &mut Reader<'_>) -> Result<Mapped, DocDecodeError> {
  match reader.u8()? {
    SHIFTED => Ok(Mapped::Shifted(take_range(reader)?)),
    OVERLAPS => Ok(Mapped::Overlaps),
    _ => Err(DocDecodeError::BadKind),
  }
}

/// A count read and checked against the bytes that remain, at `least` bytes an item, before anything is reserved.
fn count_of(reader: &mut Reader<'_>, least: usize) -> Result<usize, DocDecodeError> {
  let count = usize::try_from(reader.u32()?).map_err(|_| DocDecodeError::Truncated)?;
  match reader.remaining().checked_div(least) {
    Some(room) if count <= room => Ok(count),
    _ => Err(DocDecodeError::Truncated),
  }
}

/// A reader over the form's body past its magic and version, the trailing digest verified and left out.
fn checked(bytes: &[u8]) -> Result<Reader<'_>, DocDecodeError> {
  let body_len = bytes
    .len()
    .checked_sub(HASH_BYTES)
    .ok_or(DocDecodeError::Truncated)?;
  let (body, digest) = bytes
    .split_at_checked(body_len)
    .ok_or(DocDecodeError::Truncated)?;
  let mut reader = Reader::new(body);
  if reader.u32()? != MAGIC {
    return Err(DocDecodeError::BadMagic);
  }
  if reader.u32()? != VERSION {
    return Err(DocDecodeError::BadVersion);
  }
  if blake3::hash(body).as_bytes().as_slice() != digest {
    return Err(DocDecodeError::Truncated);
  }
  Ok(reader)
}
