//! The tuple 7.2 makes of a marker, held to 7.2's own codec: each case's payload, and the payloads 7.2's
//! `_fm0encodeonemarker` and `_fm1encodeonemarker` write from the tuple its reader made of it (generated 2026-11-23
//! with the Mercurial 7.2 Homebrew installs, `obsolete._readmarkers` then each writer).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use super::{Tuple, python_repr};
use crate::obsmarkers::{self, Format};

/// A case: its name, its payload, and 7.2's format 0 and format 1 payloads of its tuple, in hexadecimal.
type Case = (&'static str, &'static str, &'static str, &'static str);

/// The cases (module docs): a fractional date with a negative timezone and a parent; a marker recording no parents
/// (`p0`, left in its metadata); underscores in the date's halves; a date neither half of which reads; a parent that is
/// no hexadecimal (no parent data); a key twice (the later value, in the earlier's place); a format 1 marker whose date
/// Python writes in exponent notation.
const CASES: [Case; 7] = [
  (
    "frac_tz",
    "00010000004e0001010101010101010101010101010101010101010202020202020202020202020202020202020202646174653a313730303030303030302e35202d333630300070313a3033303330333033303330333033303330333033303330333033303330333033303330333033303300757365723a616c696365",
    "00010000004e0001010101010101010101010101010101010101010202020202020202020202020202020202020202646174653a313730303030303030302e35202d333630300070313a3033303330333033303330333033303330333033303330333033303330333033303330333033303300757365723a616c696365",
    "010000005a41d954fc40200000ffc40000010101010101010101010101010101010101010101010102020202020202020202020202020202020202020303030303030303030303030303030303030303040575736572616c696365",
  ),
  (
    "p0_only",
    "00000000000c01010101010101010101010101010101010101010170303a00646174653a302030",
    "00000000000e010101010101010101010101010101010101010101646174653a302e3020300070303a",
    "010000002b000000000000000000000001000001010101010101010101010101010101010101010102007030",
  ),
  (
    "underscores",
    "00010000000f0001010101010101010101010101010101010101010202020202020202020202020202020202020202646174653a315f302e3520315f3230",
    "00010000000d0001010101010101010101010101010101010101010202020202020202020202020202020202020202646174653a31302e3520313230",
    "010000003b40250000000000000002000001030001010101010101010101010101010101010101010202020202020202020202020202020202020202",
  ),
  (
    "garbage_date",
    "0001000000100001010101010101010101010101010101010101010202020202020202020202020202020202020202646174653a736f6f6e00757365723a78",
    "0001000000110001010101010101010101010101010101010101010202020202020202020202020202020202020202646174653a302e30203000757365723a78",
    "01000000420000000000000000000000000103010101010101010101010101010101010101010101020202020202020202020202020202020202020204017573657278",
  ),
  (
    "bad_parent",
    "00010000000e0001010101010101010101010101010101010101010202020202020202020202020202020202020202646174653a3520300070313a7a7a",
    "00010000000a0001010101010101010101010101010101010101010202020202020202020202020202020202020202646174653a352e302030",
    "010000003b40140000000000000000000001030001010101010101010101010101010101010101010202020202020202020202020202020202020202",
  ),
  (
    "dup_key",
    "0001000000100001010101010101010101010101010101010101010202020202020202020202020202020202020202613a3100613a3200646174653a372030",
    "00010000000e0001010101010101010101010101010101010101010202020202020202020202020202020202020202613a3200646174653a372e302030",
    "010000003f401c000000000000000000000103010101010101010101010101010101010101010101020202020202020202020202020202020202020201016132",
  ),
  (
    "v1_exp",
    "01000000544341c37937e08000ff88000001030201010101010101010101010101010101010101010202020202020202020202020202020202020202090504036f7065726174696f6e616d656e6475736572626f62",
    "0001000000290001010101010101010101010101010101010101010202020202020202020202020202020202020202646174653a31652b3136202d37323030006f7065726174696f6e3a616d656e6400757365723a626f62",
    "01000000544341c37937e08000ff88000001030201010101010101010101010101010101010101010202020202020202020202020202020202020202090504036f7065726174696f6e616d656e6475736572626f62",
  ),
];

/// The bytes the hexadecimal `hex` names.
fn bytes(hex: &str) -> Vec<u8> {
  (0..hex.len() / 2)
    .map(|at| u8::from_str_radix(&hex[2 * at..2 * at + 2], 16).unwrap())
    .collect()
}

/// Goal 93 (changeset evolution: a marker in either format exchanged as 7.2 exchanges it). Do: read each case's
/// payload, make its tuple, and write the tuple in format 0 and in format 1; then read each written payload's tuple
/// back. Expect each written payload to be 7.2's, byte for byte, and each read-back tuple 7.2's one marker with the
/// case's own, except where format 1's minutes drop a timezone's seconds (none here).
#[test]
fn a_markers_tuple_is_written_in_either_format_as_7_2_writes_it() {
  for (name, payload, as_v0, as_v1) in CASES {
    let payload = bytes(payload);
    let decoded = obsmarkers::decode(&payload).unwrap();
    let marker = decoded.iter().next().unwrap();
    let tuple = Tuple::of(&marker).unwrap();
    for (format, expected) in [(Format::V0, as_v0), (Format::V1, as_v1)] {
      let mut written = Vec::new();
      tuple.encode(format, &mut written).unwrap();
      assert_eq!(written, bytes(expected), "{name} in {format:?}");
      let again = obsmarkers::decode(&written).unwrap();
      let back = Tuple::of(&again.iter().next().unwrap()).unwrap();
      assert!(
        tuple.same(&back),
        "{name}: {tuple:?} read back from {format:?} as {back:?}"
      );
    }
  }
}

/// Goal 93 (module docs, "Writing"; CPython's `float_repr`). Do: write Python's `repr` of zeros, whole and fractional
/// dates, the edges of fixed notation, a tie at 17 digits, the extremes and the special values. Expect CPython's words
/// (each read from `python3 -c 'print(repr(x))'`).
#[test]
fn a_float_is_written_as_python_writes_its_repr() {
  for (value, expected) in [
    (0.0, "0.0"),
    (-0.0, "-0.0"),
    (1.0, "1.0"),
    (1_700_000_000.0, "1700000000.0"),
    (1_700_000_000.123_456, "1700000000.123456"),
    (9_999_999_999_999_998.0, "9999999999999998.0"),
    (1e16, "1e+16"),
    (1.5e16, "1.5e+16"),
    (1e-4, "0.0001"),
    (1e-5, "1e-05"),
    (1.5e-5, "1.5e-05"),
    (1_059_438_285_926_254.25, "1059438285926254.2"),
    (5e-324, "5e-324"),
    (f64::MAX, "1.7976931348623157e+308"),
    (0.1 + 0.2, "0.30000000000000004"),
    (f64::INFINITY, "inf"),
    (f64::NEG_INFINITY, "-inf"),
    (f64::NAN, "nan"),
  ] {
    assert_eq!(python_repr(value), expected, "{value:e}");
  }
}

/// Goal 93 (module docs, "Comparing"). Do: compare a format 1 marker's tuple with the tuple of the same marker 7.2
/// writes in format 0, and with one whose date differs. Expect the first the one marker, the second not.
#[test]
fn a_marker_and_its_other_format_are_one_marker() {
  let (_, payload, as_v0, _) = CASES[6];
  let payload = bytes(payload);
  let other = bytes(as_v0);
  let first = obsmarkers::decode(&payload).unwrap();
  let second = obsmarkers::decode(&other).unwrap();
  let first = Tuple::of(&first.iter().next().unwrap()).unwrap();
  let second = Tuple::of(&second.iter().next().unwrap()).unwrap();
  assert!(first.same(&second));
  let mut later = first.clone();
  later.date.0 = 2e16_f64.to_bits();
  assert!(!first.same(&later));
}
