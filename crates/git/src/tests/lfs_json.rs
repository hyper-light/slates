//! A-610: the bounded JSON reader and the compact writer, held to RFC 8259's grammar, to hostile input, and to
//! serde_json as an independent reader.

use proptest::prelude::*;

use super::{Cursor, Integer, NumberRefusal, Refusal, put_i64, put_str, put_u64, validate};
use crate::Admission;
use crate::lfs::tests::{EXCHANGE, exchanges};

/// Room enough for any nesting these tests open but the deep ones.
const ROOM: u64 = 1 << 20;

/// `input` validated within [`ROOM`].
fn check(input: &[u8]) -> Result<(), Refusal> {
  validate(input, &mut Admission::new(ROOM))
}

/// A-610: do validate a document of every kind RFC 8259 allows: each literal, numbers in every form of §6, strings
/// with every escape and a surrogate pair, multi-byte UTF-8, nested containers and whitespace around everything.
/// Expect each valid, as serde_json also reads it.
#[test]
fn every_kind_of_value_is_valid() {
  let documents: [&[u8]; 14] = [
    b"true",
    b" false ",
    b"null",
    b"0",
    b"-0",
    b"-12.5e+3",
    b"1E-2",
    b"\"\"",
    b"\"\\\" \\\\ \\/ \\b \\f \\n \\r \\t \\u00e9 \\ud83d\\ude00\"",
    "\"caf\u{e9} \u{1f600} \u{7f}\"".as_bytes(),
    b"[]",
    b"{}",
    b"\t\r\n{\"a\" : [1, {\"b\": null}, [\"c\"]], \"d\": {}}\n",
    b"[[[[[[[[[[]]]]]]]]]]",
  ];
  for document in documents {
    assert_eq!(
      check(document),
      Ok(()),
      "{}",
      String::from_utf8_lossy(document)
    );
    assert!(serde_json::from_slice::<serde_json::Value>(document).is_ok());
  }
}

/// A-610: do validate inputs that break RFC 8259 each one way. Expect each refused by its kind at its byte: a cut, a
/// stray byte, a raw control character, a bad escape, an unpaired surrogate either way, bytes that are not UTF-8, a
/// number outside the grammar, and bytes after the value.
#[test]
fn every_malformed_input_is_refused_by_kind() {
  let cases: [(&[u8], Refusal); 22] = [
    (b"", Refusal::Truncated { at: 0 }),
    (b"   ", Refusal::Truncated { at: 3 }),
    (b"{", Refusal::Truncated { at: 1 }),
    (b"tru", Refusal::Truncated { at: 3 }),
    (b"trux", Refusal::Unexpected { at: 3 }),
    (b"[1,]", Refusal::Unexpected { at: 3 }),
    (b"[1 2]", Refusal::Unexpected { at: 3 }),
    (b"{1:2}", Refusal::Unexpected { at: 1 }),
    (b"{\"a\" 1}", Refusal::Unexpected { at: 5 }),
    (b"{\"a\":1,}", Refusal::Unexpected { at: 7 }),
    (b"[}", Refusal::Unexpected { at: 1 }),
    (b"01", Refusal::Number { at: 0 }),
    (b"1.", Refusal::Truncated { at: 2 }),
    (b"1.e", Refusal::Number { at: 0 }),
    (b"-", Refusal::Truncated { at: 1 }),
    (b"\"\x01\"", Refusal::Control { at: 1 }),
    (b"\"\\x\"", Refusal::Escape { at: 1 }),
    (b"\"\\u12\"", Refusal::Escape { at: 1 }),
    (b"\"\\ud800\"", Refusal::Escape { at: 1 }),
    (b"\"\\udc00\"", Refusal::Escape { at: 1 }),
    (b"\"\xff\"", Refusal::NotUtf8 { at: 1 }),
    (b"[1] x", Refusal::Trailing { at: 4 }),
  ];
  for (input, refusal) in cases {
    assert_eq!(
      check(input),
      Err(refusal),
      "{}",
      String::from_utf8_lossy(input)
    );
    assert!(serde_json::from_slice::<serde_json::Value>(input).is_err());
  }
}

/// A-610, hostile input: do validate every cut of every JSON body the stock client sent and the responder answered.
/// Expect each refused as cut short at the cut, and each whole body valid.
#[test]
fn every_cut_of_every_recorded_body_is_refused_as_truncated() {
  let mut bodies = 0;
  for exchange in exchanges() {
    for body in [&exchange.request, &exchange.response] {
      if !body.starts_with('{') {
        continue;
      }
      let bytes = body.as_bytes();
      assert_eq!(check(bytes), Ok(()));
      for cut in 0..bytes.len() {
        assert_eq!(
          check(&bytes[..cut]),
          Err(Refusal::Truncated { at: cut as u64 })
        );
      }
      bodies += 1;
    }
  }
  assert!(
    bodies > 30,
    "{bodies} bodies; the recording has {} lines",
    EXCHANGE.lines().count()
  );
}

/// `depth` arrays, one inside the next.
fn nested(depth: usize) -> Vec<u8> {
  let mut input = vec![b'['; depth];
  input.extend(vec![b']'; depth]);
  input
}

/// A-610, hostile input: do validate 1,000 levels of nesting with room for 960 levels' bookkeeping (eight bytes for
/// each 64), then with room for 1,024, then a million levels with the same room. Expect the first refused at depth 961
/// having asked for one more word, the second valid with all its room taken, and the third refused at the same depth,
/// no recursion deep enough to end the thread.
#[test]
fn nesting_deeper_than_the_admission_is_refused_by_kind() {
  let mut short = Admission::new(15 * 8);
  assert_eq!(
    validate(&nested(1_000), &mut short),
    Err(Refusal::Nesting {
      depth: 961,
      past: crate::PastAdmission { asked: 8, left: 0 }
    })
  );
  let mut enough = Admission::new(16 * 8);
  assert_eq!(validate(&nested(1_000), &mut enough), Ok(()));
  assert_eq!(enough.left(), 0);
  let mut again = Admission::new(15 * 8);
  assert!(matches!(
    validate(&nested(1_000_000), &mut again),
    Err(Refusal::Nesting { depth: 961, .. })
  ));
}

/// The integers of the numbers of the array `input`.
fn integers(input: &[u8]) -> Vec<Result<Integer, NumberRefusal>> {
  check(input).unwrap();
  let mut cursor = Cursor::new(input);
  cursor.begin_array().unwrap();
  let mut out = Vec::new();
  while cursor.next_element().unwrap() {
    out.push(cursor.number().unwrap().integer());
  }
  out
}

/// A-610, hostile input: do read as integers zero, negative zero, small numbers both ways, the largest 64-bit
/// magnitude, one past it, a fraction, an exponent and a number of a thousand digits. Expect each integer exact, `-0`
/// zero, and the rest refused by kind: past 64 bits, or not an integer.
#[test]
fn numbers_are_integers_within_64_bits_or_refused_by_kind() {
  let thousand = "9".repeat(1_000);
  let input = format!(
    "[0, -0, 123, -5, 18446744073709551615, -18446744073709551615, 18446744073709551616, 1.5, 1e3, {thousand}]"
  );
  assert_eq!(
    integers(input.as_bytes()),
    vec![
      Ok(Integer::NonNegative(0)),
      Ok(Integer::NonNegative(0)),
      Ok(Integer::NonNegative(123)),
      Ok(Integer::Negative(5)),
      Ok(Integer::NonNegative(u64::MAX)),
      Ok(Integer::Negative(u64::MAX)),
      Err(NumberRefusal::TooLarge),
      Err(NumberRefusal::NotInteger),
      Err(NumberRefusal::NotInteger),
      Err(NumberRefusal::TooLarge),
    ]
  );
}

/// A-610: do read strings with escapes, a surrogate pair and multi-byte UTF-8, and take one owned within its exact
/// length and one byte short of it. Expect each string's text with its escapes resolved, `as_str` only for the one
/// without escapes, and the second refused with the room untouched.
#[test]
fn strings_resolve_their_escapes_and_are_owned_within_their_length() {
  let input = "[\"plain\", \"a\\u00e9\\ud83d\\ude00\\n\\\"\", \"caf\u{e9}\"]";
  check(input.as_bytes()).unwrap();
  let mut cursor = Cursor::new(input.as_bytes());
  cursor.begin_array().unwrap();
  let mut strings = Vec::new();
  while cursor.next_element().unwrap() {
    strings.push(cursor.string().unwrap());
  }
  let texts: Vec<String> = strings.iter().map(|text| text.chars().collect()).collect();
  assert_eq!(texts, ["plain", "a\u{e9}\u{1f600}\n\"", "caf\u{e9}"]);
  assert_eq!(strings[0].as_str(), Some("plain"));
  assert_eq!(strings[1].as_str(), None);
  assert!(strings[1].is("a\u{e9}\u{1f600}\n\""));
  let len = strings[1].decoded_len() as u64;
  assert_eq!(len, 9);
  let mut exact = Admission::new(len);
  assert_eq!(strings[1].owned(&mut exact).unwrap(), texts[1]);
  assert_eq!(exact.left(), 0);
  let mut short = Admission::new(len - 1);
  assert!(strings[1].owned(&mut short).is_err());
  assert_eq!(short.left(), len - 1);
}

/// A-610: do skip a value of each kind from the middle of an array. Expect the cursor at the next element each time.
#[test]
fn the_cursor_skips_any_value() {
  let input = b"[{\"a\": [1, {\"b\": \"]\"}]}, [[], {}], \"}\", 12.5e1, true, null, 7]";
  check(input).unwrap();
  let mut cursor = Cursor::new(input);
  cursor.begin_array().unwrap();
  for _ in 0..6 {
    assert!(cursor.next_element().unwrap());
    cursor.skip_value().unwrap();
  }
  assert!(cursor.next_element().unwrap());
  assert_eq!(
    cursor.number().unwrap().integer(),
    Ok(Integer::NonNegative(7))
  );
  assert!(!cursor.next_element().unwrap());
}

/// A-610: do write numbers at both ends of 64 bits. Expect their decimal text.
#[test]
fn numbers_are_written_in_decimal() {
  let mut out = Vec::new();
  for value in [0, 7, u64::MAX] {
    put_u64(&mut out, value);
    out.push(b' ');
  }
  for value in [i64::MIN, -1, i64::MAX] {
    put_i64(&mut out, value);
    out.push(b' ');
  }
  assert_eq!(
    String::from_utf8(out).unwrap(),
    "0 7 18446744073709551615 -9223372036854775808 -1 9223372036854775807 "
  );
}

/// A JSON value's text from a strategy of scalars and containers, numbers kept within f64's range so serde_json reads
/// every one.
fn json_text() -> impl Strategy<Value = String> {
  let scalar = prop_oneof![
    Just("true".to_string()),
    Just("null".to_string()),
    (-1_000_000i64..1_000_000).prop_map(|value| value.to_string()),
    (-1_000i32..1_000, 0u8..100).prop_map(|(whole, fraction)| format!("{whole}.{fraction}")),
    "[a-z\u{e9}\u{1f600} ]{0,6}".prop_map(|text| format!("{text:?}")),
  ];
  scalar.prop_recursive(4, 24, 4, |inner| {
    prop_oneof![
      proptest::collection::vec(inner.clone(), 0..4)
        .prop_map(|items| format!("[{}]", items.join(","))),
      proptest::collection::vec(("[a-z]{0,3}", inner), 0..4).prop_map(|pairs| {
        let fields: Vec<String> = pairs
          .into_iter()
          .map(|(key, value)| format!("{key:?}: {value}"))
          .collect();
        format!("{{{}}}", fields.join(", "))
      }),
    ]
  })
}

/// A mutation of a text: a byte replaced, inserted or removed.
fn mutated() -> impl Strategy<Value = Vec<u8>> {
  let bytes = b"{}[],:\"\\ 0-.eEtrufalsn\x01\xe9\xff".to_vec();
  (
    json_text(),
    proptest::collection::vec(
      (
        any::<proptest::sample::Index>(),
        0usize..3,
        proptest::sample::select(bytes),
      ),
      0..3,
    ),
  )
    .prop_map(|(text, edits)| {
      let mut input = text.into_bytes();
      for (place, kind, byte) in edits {
        let at = place.index(input.len() + 1);
        match kind {
          0 if at < input.len() => input[at] = byte,
          1 => input.insert(at, byte),
          _ if at < input.len() => {
            input.remove(at);
          }
          _ => {}
        }
      }
      input
    })
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-610, differential: do validate JSON texts and mutations of them (a byte replaced, inserted or removed). Expect
  /// the validator to accept exactly what serde_json reads, setting aside a number serde_json refuses as out of
  /// f64's range, which RFC 8259's grammar allows.
  #[test]
  fn the_validator_accepts_what_serde_json_reads(input in mutated()) {
    let theirs = serde_json::from_slice::<serde_json::Value>(&input);
    if let Err(error) = &theirs {
      prop_assume!(!error.to_string().contains("number out of range"));
    }
    prop_assert_eq!(check(&input).is_ok(), theirs.is_ok(), "{}", String::from_utf8_lossy(&input));
  }

  /// A-610: do write any text as a JSON string. Expect it valid, read back to the same text by this module and by
  /// serde_json.
  #[test]
  fn any_text_is_written_as_a_string_that_reads_back(text in any::<String>()) {
    let mut out = Vec::new();
    put_str(&mut out, &text);
    prop_assert_eq!(check(&out), Ok(()));
    let mut cursor = Cursor::new(&out);
    let read: String = cursor.string().unwrap().chars().collect();
    prop_assert_eq!(&read, &text);
    prop_assert_eq!(serde_json::from_slice::<String>(&out).unwrap(), text);
  }
}
