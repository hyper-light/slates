//! A computed verdict's wire form (A-658 part 3a, `computed_wire`): its layout held to vectors assembled field by field
//! from the module's documented format, and every cut, every flipped bit and every malformed field refused, typed,
//! before anything is reserved. That every verdict a copy computes reads back to the owner's own verdict is the oracle's
//! (`compute_tests`, which takes every held verdict through the wire form).

use super::position::{Placed, Unit};
use super::{Computed, ConflictWindow, ContentSource, Effect, Green, Increment, Reached};
use crate::map::Mapped;
use crate::ops_doc::{DocDecodeError, Op, OpKind, OpsDoc};
use crate::range::Range;
use crate::verdict::MergeConflictClass;

/// Shape: the identity of the increment the hand-made verdicts judged.
const ID: [u8; 32] = [7; 32];
/// Shape: the hand-made verdicts' base.
const BASE: u64 = 3;
/// Shape: the head the hand-made verdicts were computed against.
const AT: u64 = 5;
/// Shape: the fold floor of the copy that computed the hand-made verdicts.
const FLOOR: u64 = 2;

/// An increment of no ops, the one the hand-made verdicts name: nothing it declares abbreviates their effects.
fn bare() -> Increment {
  Increment {
    id: ID,
    base: BASE,
    doc: OpsDoc::new(),
    post_state: Vec::new(),
    evidence: Vec::new(),
  }
}

/// A verdict the copy reached as `reached`, at the hand-made position.
fn computed(reached: Reached) -> Computed {
  Computed {
    id: ID,
    base: BASE,
    at: AT,
    floor: FLOOR,
    reached,
  }
}

/// The form's fixed head: magic, version, identity, base, position and floor, then the tag of what it reached.
fn head(tag: u8) -> Vec<u8> {
  let mut bytes = b"GCMP".to_vec();
  bytes.extend_from_slice(&1u32.to_le_bytes());
  bytes.extend_from_slice(&ID);
  for word in [BASE, AT, FLOOR] {
    bytes.extend_from_slice(&word.to_le_bytes());
  }
  bytes.push(tag);
  bytes
}

/// `body` with its digest after it, as the form ends.
fn sealed(mut body: Vec<u8>) -> Vec<u8> {
  let digest = *blake3::hash(&body).as_bytes();
  body.extend_from_slice(&digest);
  body
}

/// A string as the form writes one: its length, then its bytes.
fn text(bytes: &mut Vec<u8>, text: &str) {
  bytes.extend_from_slice(&u32::try_from(text.len()).unwrap().to_le_bytes());
  bytes.extend_from_slice(text.as_bytes());
}

/// Two words, little-endian, as a range or two fields are written.
fn words(bytes: &mut Vec<u8>, first: u64, second: u64) {
  bytes.extend_from_slice(&first.to_le_bytes());
  bytes.extend_from_slice(&second.to_le_bytes());
}

/// The splice the hand-made verdict's first unit holds: one insert, at the file's byte 1, of two bytes from the
/// increment's post-state at 3.
fn splice() -> Op {
  Op {
    kind: OpKind::Insert,
    flags: 0,
    path: 0,
    at: 1,
    len: 2,
    src: 3,
  }
}

/// A verdict of two units: the first a splice left unmade, a directory made and its mode; the second a window and where
/// its op stood.
fn two_units() -> Computed {
  computed(Reached::of(vec![
    Unit {
      effects: vec![
        Effect::SetContent("f".to_owned(), None, vec![splice()], ContentSource::Mapped),
        Effect::MakeDir("d".to_owned()),
        Effect::Mode("d".to_owned(), 0o755),
      ],
      ..Unit::default()
    },
    Unit {
      windows: vec![ConflictWindow {
        path: "g".to_owned(),
        range: Range::new(8, 9),
        class: MergeConflictClass::DeleteModify,
      }],
      placed: vec![Placed::from_parts(
        Mapped::Shifted(Range::new(10, 11)),
        Mapped::Overlaps,
      )],
      ..Unit::default()
    },
  ]))
}

/// [`two_units`]' form, assembled field by field: the unit count; the first unit's three effects as a chain entry
/// writes them (a splice's tag 3, its path and its one op; a directory's tag 7 and path; a mode's tag 9, path and mode),
/// no window and no placement; the second's no effect, its window (path, range, class `DeleteModify`'s tag 5) and its
/// placement (a shifted range's tag 0 and range, an overlap's tag 1); the digest.
fn two_units_form() -> Vec<u8> {
  let mut bytes = head(1);
  bytes.extend_from_slice(&2u32.to_le_bytes());
  bytes.extend_from_slice(&3u32.to_le_bytes());
  bytes.push(3);
  text(&mut bytes, "f");
  bytes.extend_from_slice(&1u32.to_le_bytes());
  let op = splice();
  bytes.extend_from_slice(&[op.kind.to_wire(), op.flags]);
  words(&mut bytes, op.path, op.at);
  words(&mut bytes, op.len, op.src);
  bytes.push(7);
  text(&mut bytes, "d");
  bytes.push(9);
  text(&mut bytes, "d");
  bytes.extend_from_slice(&0o755u32.to_le_bytes());
  bytes.extend_from_slice(&[0u8; 8]);
  bytes.extend_from_slice(&0u32.to_le_bytes());
  bytes.extend_from_slice(&1u32.to_le_bytes());
  text(&mut bytes, "g");
  words(&mut bytes, 8, 9);
  bytes.push(5);
  bytes.extend_from_slice(&1u32.to_le_bytes());
  bytes.push(0);
  words(&mut bytes, 10, 11);
  bytes.push(1);
  sealed(bytes)
}

/// A-658 part 3a. Do: write a verdict the copy had answered before, as version 4, and one of two units; read each back
/// on a green of no versions. Expect: each form the bytes assembled field by field from the documented layout (golden
/// vectors); each read back writes the same bytes again, its splice still unmade and nothing of it judged.
#[test]
fn a_computed_verdicts_wire_form_is_its_documented_layout() {
  let mut before = head(0);
  before.extend_from_slice(&4u64.to_le_bytes());
  let forms = [
    (computed(Reached::Before(4)), sealed(before)),
    (two_units(), two_units_form()),
  ];
  let green = Green::new();
  for (verdict, form) in forms {
    assert_eq!(verdict.encode(&bare()), form);
    let read = green.computed_from(&bare(), &form).unwrap();
    assert_eq!(read.encode(&bare()), form);
  }
}

/// A-658 part 3a. Do: read back [`two_units`]' form cut at every length, and with each bit of each byte flipped in turn.
/// Expect: every one refused, typed, never read as a verdict.
#[test]
fn every_cut_or_flipped_wire_form_is_refused() {
  let form = two_units_form();
  let green = Green::new();
  for len in 0..form.len() {
    assert!(
      green.computed_from(&bare(), &form[..len]).is_err(),
      "cut at {len}"
    );
  }
  for at in 0..form.len() {
    for bit in 0..8 {
      let mut flipped = form.clone();
      flipped[at] ^= 1 << bit;
      assert!(
        green.computed_from(&bare(), &flipped).is_err(),
        "byte {at} bit {bit}"
      );
    }
  }
}

/// A-658 part 3a. Do: read back forms whose digest holds but whose fields do not: another magic, another version, a unit
/// count of `u32::MAX`, an effect count past the bytes left, an unknown tag of what was reached, of a class and of a
/// mapped range, and a byte after the last field. Expect: each refused by its kind, the counts before anything is
/// reserved.
#[test]
fn a_wire_form_whose_fields_are_wrong_is_refused_by_kind() {
  let body = |form: Vec<u8>| form[..form.len() - 32].to_vec();
  let good = body(two_units_form());
  let mut magic = good.clone();
  magic[0] ^= 1;
  let mut version = good.clone();
  version[4] = 2;
  let units_at = head(1).len();
  let mut units = good.clone();
  units[units_at..units_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
  let mut effects = good.clone();
  effects[units_at + 4..units_at + 8].copy_from_slice(&1_000_000u32.to_le_bytes());
  let mut reached = good.clone();
  reached[units_at - 1] = 2;
  let class_at = good.len() - 1 - 8 - 8 - 1 - 4 - 1;
  let mut class = good.clone();
  class[class_at] = 9;
  let mut mapped = good.clone();
  let last = mapped.len() - 1;
  mapped[last] = 2;
  let mut trailing = good.clone();
  trailing.push(0);
  let green = Green::new();
  for (wrong, kind) in [
    (magic, DocDecodeError::BadMagic),
    (version, DocDecodeError::BadVersion),
    (units, DocDecodeError::Truncated),
    (effects, DocDecodeError::Truncated),
    (reached, DocDecodeError::BadKind),
    (class, DocDecodeError::BadKind),
    (mapped, DocDecodeError::BadKind),
    (trailing, DocDecodeError::TrailingBytes),
  ] {
    assert_eq!(
      green.computed_from(&bare(), &sealed(wrong)).map(|_| ()),
      Err(kind)
    );
  }
}
