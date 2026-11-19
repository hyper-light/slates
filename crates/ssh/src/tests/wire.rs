//! SSH's data types (A-594; RFC 4251 §5, §6): golden values from RFC 4251's own examples, hostile lengths, and
//! round trips.

use proptest::prelude::*;

use crate::support::hex;
use crate::wire::{MAX_NAME, Reader, WireRefusal, put_mpint_unsigned, put_name_list, put_string, put_uint32};

/// A-594 golden (RFC 4251 §5's `mpint` examples): do write 0, 0x9a378f9b2e332a7 and 0x80 from their unsigned bytes, the
/// first two with leading zero bytes. Expect RFC 4251's encodings: `00000000`, `00000008 09 a3 78 f9 b2 e3 32 a7` and
/// `00000002 00 80`.
#[test]
fn mpints_are_written_as_rfc_4251_writes_its_examples() {
  let cases: [(&str, &str); 4] = [
    ("", "00000000"),
    ("0000", "00000000"),
    ("00 09a378f9b2e332a7", "00000008 09a378f9b2e332a7"),
    ("80", "00000002 0080"),
  ];
  for (magnitude, expected) in cases {
    let mut out = Vec::new();
    put_mpint_unsigned(&mut out, &hex(magnitude)).unwrap();
    assert_eq!(out, hex(expected), "mpint of {magnitude:?}");
  }
}

/// A-594: do read name-lists a client could send. Expect the names in order; an empty list is a list of no names
/// (RFC 4251 §5), and a 64-byte name, the longest §6 allows, reads.
#[test]
fn name_lists_read_their_names_in_order() {
  let mut payload = Vec::new();
  put_name_list(&mut payload, &["curve25519-sha256", "kex-strict-c-v00@openssh.com", "ext-info-c"]).unwrap();
  put_string(&mut payload, b"").unwrap();
  let longest = "n".repeat(MAX_NAME);
  put_string(&mut payload, longest.as_bytes()).unwrap();
  let mut reader = Reader::new(&payload);
  let names: Vec<&[u8]> = reader.name_list().unwrap().names().collect();
  assert_eq!(names, [&b"curve25519-sha256"[..], b"kex-strict-c-v00@openssh.com", b"ext-info-c"]);
  let empty = reader.name_list().unwrap();
  assert!(empty.is_empty());
  assert_eq!(empty.first(), None);
  assert_eq!(reader.name_list().unwrap().first(), Some(longest.as_bytes()));
  reader.finish().unwrap();
}

/// A-594 hostile: do read name-lists with an empty name at each place, a 65-byte name, a space, a control byte, DEL and
/// a byte past US-ASCII. Expect each refused as a bad name-list naming where the list began.
#[test]
fn name_lists_with_bad_names_are_refused() {
  let too_long = "n".repeat(MAX_NAME + 1);
  let bad: [&[u8]; 8] = [b",a", b"a,,b", b"a,", too_long.as_bytes(), b"a b", b"a\x01", b"a\x7f", "é".as_bytes()];
  for list in bad {
    let mut payload = vec![0xff];
    put_string(&mut payload, list).unwrap();
    let mut reader = Reader::new(&payload);
    reader.byte().unwrap();
    assert_eq!(reader.name_list(), Err(WireRefusal::BadNameList { at: 5 }), "list {list:?}");
  }
}

/// A-594 hostile: do read a string whose length is `u32::MAX` over a few bytes, a `uint32` cut short, and a message with
/// a byte left over. Expect each refused, typed and placed, without reading past the input.
#[test]
fn lengths_past_the_input_and_trailing_bytes_are_refused() {
  let mut payload = Vec::new();
  put_uint32(&mut payload, u32::MAX);
  payload.extend_from_slice(b"abc");
  assert_eq!(Reader::new(&payload).string(), Err(WireRefusal::Truncated { at: 0 }));
  assert_eq!(Reader::new(&[0, 0, 1]).uint32(), Err(WireRefusal::Truncated { at: 0 }));
  let mut reader = Reader::new(&[7, 8]);
  assert_eq!(reader.byte(), Ok(7));
  assert_eq!(reader.finish(), Err(WireRefusal::TrailingBytes { at: 1 }));
  let mut reader = Reader::new(&[2]);
  assert_eq!(reader.boolean(), Ok(true));
  assert_eq!(reader.byte(), Err(WireRefusal::Truncated { at: 1 }));
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-594: do write any bytes as a string and any valid names as a name-list, then read them back. Expect the same
  /// bytes and names, and nothing left over.
  #[test]
  fn strings_and_name_lists_round_trip(
    bytes in proptest::collection::vec(any::<u8>(), 0..300),
    names in proptest::collection::vec("[!-+\\--~]{1,64}", 0..8),
  ) {
    let mut payload = Vec::new();
    put_string(&mut payload, &bytes).unwrap();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    put_name_list(&mut payload, &refs).unwrap();
    let mut reader = Reader::new(&payload);
    prop_assert_eq!(reader.string().unwrap(), &bytes[..]);
    let read: Vec<&[u8]> = reader.name_list().unwrap().names().collect();
    let expected: Vec<&[u8]> = names.iter().map(|name| name.as_bytes()).collect();
    prop_assert_eq!(read, expected);
    prop_assert_eq!(reader.finish(), Ok(()));
  }

  /// A-594 hostile: do read any bytes as each type in turn. Expect a value or a typed refusal, never a panic, and a
  /// reader that never moves past the input.
  #[test]
  fn any_bytes_read_or_refuse(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
    let mut reader = Reader::new(&bytes);
    let outcomes = [reader.uint32().is_ok(), reader.string().is_ok(), reader.name_list().is_ok()];
    prop_assert!(reader.position() <= bytes.len(), "outcomes {:?}", outcomes);
  }
}
