//! A pack entry's header (A-582 part 3; §4.17.2 "Pieces (git)", piece 2, "a pack entry's header"): what an entry
//! holds, how many bytes its data inflates to, and for a delta which base it applies to.
//!
//! **Sources** (read 2026-11-19, git v2.50.1). The document [B], `Documentation/gitformat-pack.adoc`:
//! - an entry is "n-byte type and length (3-bit type, (n-1)*7+4-bit length)", then a REF_DELTA's "base object name" or
//!   an OFS_DELTA's "negative relative offset from the delta object's position in the pack", then the compressed data
//!   (lines 51-66), whose size "is the size before compression" for an object and "the size of the delta data that
//!   follows" for a delta (235-255);
//! - the types: "Type 5 is reserved for future expansion. Type 0 is invalid." (70-81);
//! - the size encoding, seven bits a byte, later bytes more significant (83-93);
//! - the offset encoding: "n bytes with MSB set in all but the last one", and "for n >= 2 adding 2^7 + 2^14 + ... +
//!   2^(7*(n-1)) to the result" (257-262).
//!
//! The code [C], which clients and git's own receive-pack run, followed where the document leaves room: `packfile.c`'s
//! `unpack_object_header_buffer` (1116-1139) and `get_delta_base` (1238-1277); `builtin/index-pack.c`'s
//! `unpack_raw_entry` (520-582); `pack-write.c`'s `encode_in_pack_object_header` (505-526); and
//! `builtin/pack-objects.c`'s offset writer (548-559). The four object types and their numbers are A-600's
//! ([`Kind::pack_number`]), and a REF_DELTA's base is an [`ObjectId`] in the pack's [`ObjectFormat`].
//!
//! [`read`] reads one header from the bytes a pack stream has delivered so far, `None` until its last byte arrives, so
//! the pack reader (§4.17.2 piece 3) reads each entry as it comes; [`write`] writes one as `pack-objects` does, each
//! number in the fewest bytes, and [`written_len`] says how long that is beforehand.
//!
//! **Hostile input (§4.9).** Every byte is untrusted, and every refusal is a typed variant naming where its entry
//! begins.
//! - Type 0 (invalid) and 5 (reserved) are refused as soon as the first byte arrives (`Type`), as index-pack refuses
//!   them ("unknown object type").
//! - A size is read as `unpack_object_header_buffer` reads it on a 64-bit word: it refuses a further byte once the
//!   shift passes the word less seven bits ("bad object header"), so a size has at most 60 bits (`SizeTooLong`), refused
//!   before the byte that would pass it arrives. index-pack's own loop has no such check and shifts past the word; this
//!   reader holds every entry to the stricter rule, which no pack git writes can break: a size past 60 bits is an object
//!   of an exbibyte.
//! - An offset whose next byte would lose bits is refused as git refuses it ("offset value overflow for delta base
//!   object", `OffsetTooLong`).
//! - An OFS_DELTA's base must begin before its own entry and after the pack's header: an offset of zero names the entry
//!   itself (`BaseNotBefore`), and one that reaches inside the pack's twelve-byte header or before the pack begins is
//!   `BaseBeforePack`. git refuses a base at or before byte zero ("delta base offset is out of bound"); no entry begins
//!   inside the header either, so index-pack would find no base there and end with the delta unresolved.
//!
//! **Bounds.** Nothing here allocates: a REF_DELTA's base is a fixed-size [`ObjectId`]. The size a whole header names
//! is charged to the caller's [`Admission`] before any of the entry's data is read, as A-600's object header is, so an
//! entry that claims more than its request may hold is refused at its header; a header not yet whole charges nothing.

use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::out::{Measure, Out};
use crate::{Admission, PastAdmission};

/// Format: a pack's header, the signature `PACK`, a version and the number of objects, four bytes each
/// (`gitformat-pack.adoc` 37-49). The first entry begins here, so no entry, and no delta's base, begins before it.
pub const PACK_HEADER_LEN: u64 = 12;

/// Format: the bit of each byte of a header's numbers that says another byte follows (`gitformat-pack.adoc` 83-93 and
/// 257-262).
const MORE: u8 = 0x80;

/// Format: the seven bits of a number each byte carries after the first byte of a size, and in every byte of an offset.
const GROUP: u8 = 0x7f;

/// Format: the bits one group carries.
const GROUP_BITS: u32 = 7;

/// Format: the size's first bits, the first byte's low four ("(n-1)*7+4-bit length", `gitformat-pack.adoc` 55).
const FIRST_SIZE: u8 = 0x0f;

/// Format: how many bits the first byte gives the size, below the type's three.
const FIRST_SIZE_BITS: u32 = 4;

/// Format: the type's three bits, once shifted down past the size's four (`gitformat-pack.adoc` 55).
const TYPE_BITS: u8 = 0x07;

/// Format: the largest shift git reads a size's group at on a 64-bit word: `unpack_object_header_buffer` refuses a
/// further group when "(bitsizeof(long) - 7) < shift" (`packfile.c` 1124-1126). The same bound stops an offset: git
/// refuses one whose top seven bits are set before it shifts in another group (`MSB(base_offset, 7)`, `packfile.c`
/// 1259).
const SHIFT_MAX: u32 = u64::BITS.saturating_sub(GROUP_BITS);

/// Derived: the bits a size git reads can have, 60: the first byte's four, and seven for each shift git reads a group
/// at, from four up to [`SHIFT_MAX`].
const SIZE_BITS: u32 = FIRST_SIZE_BITS.saturating_add(
  GROUP_BITS.saturating_mul(
    SHIFT_MAX
      .saturating_sub(FIRST_SIZE_BITS)
      .saturating_div(GROUP_BITS)
      .saturating_add(1),
  ),
);

/// Format: `OBJ_OFS_DELTA`'s number (`gitformat-pack.adoc` 78).
const OBJ_OFS_DELTA: u8 = 6;

/// Format: `OBJ_REF_DELTA`'s number (`gitformat-pack.adoc` 79).
const OBJ_REF_DELTA: u8 = 7;

/// Format: git's four object types, whose numbers in a pack A-600's [`Kind::pack_number`] holds (`object.h`).
const WHOLE_KINDS: [Kind; 4] = [Kind::Commit, Kind::Tree, Kind::Blob, Kind::Tag];

/// Format: the room `pack-objects` writes an offset into, `MAX_PACK_OBJECT_HEADER` (`pack.h` 126): ten bytes, enough
/// for any 64-bit offset, since nine bytes of seven bits carry only 63.
const OFFSET_BYTES_MAX: usize = 10;

/// What a pack entry holds: an object whole, or a delta against a base object (`gitformat-pack.adoc` 95-111: "These
/// representations are assigned new types ofs-delta and ref-delta, which is only valid in a pack file").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Representation {
  /// The object whole, of one of git's four types.
  Whole(Kind),
  /// A delta against the entry that begins `base` bytes into the same pack.
  OfsDelta {
    /// Where the base's entry begins in the pack.
    base: u64,
  },
  /// A delta against the object named `base`: in the same pack, or, in a thin pack, held by the receiver.
  RefDelta {
    /// The base's name, in the pack's object format.
    base: ObjectId,
  },
}

/// A pack entry's header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
  /// What the entry holds.
  pub representation: Representation,
  /// The bytes the entry's compressed data inflates to: the object's for a whole object, the delta's for a delta.
  pub size: u64,
}

/// Why bytes are not a pack entry's header, or a header cannot be written. Each but an admission's refusal names where
/// its entry begins in the pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// Type 0, which is invalid, or 5, reserved for future expansion (`gitformat-pack.adoc` 70-81).
  Type {
    /// Where the entry begins.
    at: u64,
    /// The type's number.
    code: u8,
  },
  /// A size past the 60 bits git reads (`packfile.c` 1124-1126, "bad object header").
  SizeTooLong {
    /// Where the entry begins.
    at: u64,
  },
  /// An OFS_DELTA's offset past 64 bits (`packfile.c` 1259, `builtin/index-pack.c` 561-562, "offset value overflow
  /// for delta base object").
  OffsetTooLong {
    /// Where the entry begins.
    at: u64,
  },
  /// An OFS_DELTA's offset of zero, which names the entry itself rather than one before it; or, to the writer, a base
  /// at or after its entry.
  BaseNotBefore {
    /// Where the entry begins.
    at: u64,
  },
  /// An OFS_DELTA's offset that puts its base before the pack's first entry: inside the pack's header, or before the
  /// pack begins (`packfile.c` 1265-1266, "delta base offset is out of bound").
  BaseBeforePack {
    /// Where the entry begins.
    at: u64,
    /// The offset back from it.
    offset: u64,
  },
  /// The size the header names is more than the caller's admission had left: refused before any of its data is read.
  PastAdmission(PastAdmission),
}

/// Reads the header of the entry the start of `bytes` holds, which begins `at` bytes into its pack, whose objects are
/// named in `format`, and charges the size it names to `admission`: the header and the bytes it took. `None` while
/// `bytes` holds less than the whole header, so a reader takes more and asks again; nothing is charged then. A type, a
/// size or an offset is refused as soon as the bytes so far break its rule (this module's **Hostile input**); an
/// OFS_DELTA's base is judged where it lands once its offset is whole.
pub fn read(
  bytes: &[u8],
  at: u64,
  format: ObjectFormat,
  admission: &mut Admission,
) -> Result<Option<(Header, usize)>, Refusal> {
  let Some((code, size, took)) = type_and_size(bytes, at)? else {
    return Ok(None);
  };
  let rest = bytes.get(took..).unwrap_or_default();
  let Some((representation, more)) = representation(code, rest, at, format)? else {
    return Ok(None);
  };
  admission.charge(size).map_err(Refusal::PastAdmission)?;
  Ok(Some((
    Header {
      representation,
      size,
    },
    took.saturating_add(more),
  )))
}

/// The object type a pack number names, if it names one of git's four.
fn whole_kind(code: u8) -> Option<Kind> {
  WHOLE_KINDS
    .into_iter()
    .find(|kind| kind.pack_number() == code)
}

/// The type number, the size and the bytes they took, `None` until the size's last byte arrives. An invalid or reserved
/// type is refused from the first byte, whatever follows.
fn type_and_size(bytes: &[u8], at: u64) -> Result<Option<(u8, u64, usize)>, Refusal> {
  let Some(&first) = bytes.first() else {
    return Ok(None);
  };
  let code = first.checked_shr(FIRST_SIZE_BITS).unwrap_or(0) & TYPE_BITS;
  if whole_kind(code).is_none() && code != OBJ_OFS_DELTA && code != OBJ_REF_DELTA {
    return Err(Refusal::Type { at, code });
  }
  let mut size = u64::from(first & FIRST_SIZE);
  let mut shift = FIRST_SIZE_BITS;
  let mut took = 1usize;
  let mut byte = first;
  while byte & MORE != 0 {
    if shift > SHIFT_MAX {
      return Err(Refusal::SizeTooLong { at });
    }
    let Some(&next) = bytes.get(took) else {
      return Ok(None);
    };
    byte = next;
    took = took.saturating_add(1);
    // A group is read only at shifts 4, 11, ... 53 (the next, 60, is past `SHIFT_MAX`), so its seven bits land at bit
    // 59 at most and none is lost.
    size |= u64::from(byte & GROUP).checked_shl(shift).unwrap_or(0);
    shift = shift.saturating_add(GROUP_BITS);
  }
  Ok(Some((code, size, took)))
}

/// What an entry of type `code` holds, from the bytes after its size, and the bytes its base took: `None` until a
/// delta's base is whole.
fn representation(
  code: u8,
  rest: &[u8],
  at: u64,
  format: ObjectFormat,
) -> Result<Option<(Representation, usize)>, Refusal> {
  if let Some(kind) = whole_kind(code) {
    return Ok(Some((Representation::Whole(kind), 0)));
  }
  match code {
    OBJ_OFS_DELTA => Ok(
      base_offset(rest, at)?.map(|(base, took)| (Representation::OfsDelta { base }, took)),
    ),
    OBJ_REF_DELTA => Ok(
      rest
        .get(..format.raw_len())
        .and_then(|name| ObjectId::from_bytes(format, name))
        .map(|base| (Representation::RefDelta { base }, format.raw_len())),
    ),
    code => Err(Refusal::Type { at, code }),
  }
}

/// An OFS_DELTA's base, read from the start of `bytes` for the entry that begins `at` into its pack: where the base's
/// entry begins and the bytes the offset took, `None` until its last byte arrives. Each byte after the first adds one
/// before its shift, as the offset encoding does (`gitformat-pack.adoc` 257-262; `packfile.c` 1255-1262).
fn base_offset(bytes: &[u8], at: u64) -> Result<Option<(u64, usize)>, Refusal> {
  let Some(&first) = bytes.first() else {
    return Ok(None);
  };
  let mut offset = u64::from(first & GROUP);
  let mut took = 1usize;
  let mut byte = first;
  while byte & MORE != 0 {
    offset = offset
      .checked_add(1)
      .ok_or(Refusal::OffsetTooLong { at })?;
    if offset.checked_shr(SHIFT_MAX).unwrap_or(0) != 0 {
      return Err(Refusal::OffsetTooLong { at });
    }
    let Some(&next) = bytes.get(took) else {
      return Ok(None);
    };
    byte = next;
    took = took.saturating_add(1);
    offset = offset.checked_shl(GROUP_BITS).unwrap_or(0) | u64::from(byte & GROUP);
  }
  base_before(at, offset).map(|base| Some((base, took)))
}

/// The base an offset back from the entry at `at` names: refused when it names the entry itself or begins before the
/// pack's first entry.
fn base_before(at: u64, offset: u64) -> Result<u64, Refusal> {
  if offset == 0 {
    return Err(Refusal::BaseNotBefore { at });
  }
  at.checked_sub(offset)
    .filter(|base| *base >= PACK_HEADER_LEN)
    .ok_or(Refusal::BaseBeforePack { at, offset })
}

/// Writes `header`, for the entry that begins `at` bytes into its pack, as `pack-objects` writes one: the type and the
/// size in the fewest bytes, then a REF_DELTA's base name, in its own format, which must be the pack's, or an
/// OFS_DELTA's offset back to its base in the fewest bytes. Refused, nothing written, for a size past what git reads
/// and for a base not before the entry and after the pack's header.
pub fn write(header: Header, at: u64, out: &mut impl Out) -> Result<(), Refusal> {
  if header.size.checked_shr(SIZE_BITS).unwrap_or(0) != 0 {
    return Err(Refusal::SizeTooLong { at });
  }
  let code = match header.representation {
    Representation::Whole(kind) => kind.pack_number(),
    Representation::OfsDelta { base } => {
      base_before(at, at.checked_sub(base).unwrap_or(0))?;
      OBJ_OFS_DELTA
    }
    Representation::RefDelta { .. } => OBJ_REF_DELTA,
  };
  write_type_and_size(code, header.size, out);
  match header.representation {
    Representation::OfsDelta { base } => write_offset(at.saturating_sub(base), out),
    Representation::RefDelta { base } => out.put(base.as_bytes()),
    Representation::Whole(_) => {}
  }
  Ok(())
}

/// Writes a type and a size as `encode_in_pack_object_header` does: the type above the size's low four bits in the
/// first byte, then seven bits a byte.
fn write_type_and_size(code: u8, size: u64, out: &mut impl Out) {
  let low = u8::try_from(size & u64::from(FIRST_SIZE)).unwrap_or(0);
  let mut rest = size.checked_shr(FIRST_SIZE_BITS).unwrap_or(0);
  let typed = code.checked_shl(FIRST_SIZE_BITS).unwrap_or(0) | low;
  out.put(&[if rest == 0 { typed } else { typed | MORE }]);
  while rest != 0 {
    let group = u8::try_from(rest & u64::from(GROUP)).unwrap_or(0);
    rest = rest.checked_shr(GROUP_BITS).unwrap_or(0);
    out.put(&[if rest == 0 { group } else { group | MORE }]);
  }
}

/// Writes an offset as `pack-objects` does: its last group first in place, then each earlier byte one less than its
/// value, so the reader's added one gives it back ("while (ofs >>= 7) dheader[--pos] = 128 | (--ofs & 127)").
fn write_offset(offset: u64, out: &mut impl Out) {
  let mut bytes = [0u8; OFFSET_BYTES_MAX];
  let mut first = OFFSET_BYTES_MAX.saturating_sub(1);
  let mut rest = offset;
  if let Some(last) = bytes.get_mut(first) {
    *last = u8::try_from(rest & u64::from(GROUP)).unwrap_or(0);
  }
  rest = rest.checked_shr(GROUP_BITS).unwrap_or(0);
  while rest != 0 {
    rest = rest.saturating_sub(1);
    first = first.saturating_sub(1);
    if let Some(byte) = bytes.get_mut(first) {
      *byte = MORE | u8::try_from(rest & u64::from(GROUP)).unwrap_or(0);
    }
    rest = rest.checked_shr(GROUP_BITS).unwrap_or(0);
  }
  out.put(bytes.get(first..).unwrap_or_default());
}

/// The bytes `header` takes when written for the entry at `at`, or why it cannot be written.
pub fn written_len(header: Header, at: u64) -> Result<u64, Refusal> {
  let mut measure = Measure::default();
  write(header, at, &mut measure)?;
  Ok(measure.0)
}

#[cfg(test)]
#[path = "tests/entry.rs"]
mod tests;
