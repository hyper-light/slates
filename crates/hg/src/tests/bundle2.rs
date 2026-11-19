//! Tests of `bundle2` (A-514): the golden streams stock Mercurial 7.2 wrote, decoded as 7.2 lists them and encoded
//! back to their bytes; hostile streams refused by kind; nesting held to the admission, with no recursion.

use super::{Bundle2, EncodeRefusal, Event, Param, Part, Refusal, StreamParam};
use crate::{Admission, fixtures};
use proptest::prelude::*;

fn room() -> Admission {
  Admission::new(u64::MAX)
}

/// A part's line as 7.2's `debugbundle` writes it: the type lower-cased, the parameters sorted by key, and
/// "mandatory" as Python prints a bool.
fn describe(part: &Part<'_>) -> String {
  let mut params: Vec<String> = part
    .params
    .iter()
    .map(|p| {
      format!(
        "{}: {}",
        String::from_utf8_lossy(p.key),
        String::from_utf8_lossy(p.value)
      )
    })
    .collect();
  params.sort();
  let mandatory = if part.is_mandatory() { "True" } else { "False" };
  format!(
    "{} -- {{{}}} (mandatory: {mandatory})",
    String::from_utf8_lossy(part.kind).to_lowercase(),
    params.join(", ")
  )
}

fn encode(stream: &Bundle2<'_>) -> Vec<u8> {
  let mut out = Vec::new();
  stream.encode(&mut out).unwrap();
  out
}

/// A stream of one part with no parameters and no payload, around a raw part header.
fn with_header(header: &[u8]) -> Vec<u8> {
  let mut out = b"HG20".to_vec();
  out.extend_from_slice(&0_i32.to_be_bytes());
  out.extend_from_slice(&i32::try_from(header.len()).unwrap().to_be_bytes());
  out.extend_from_slice(header);
  out.extend_from_slice(&0_i32.to_be_bytes());
  out.extend_from_slice(&0_i32.to_be_bytes());
  out
}

/// A stream holding only `params`, encoded.
fn with_params(params: Vec<StreamParam>) -> Vec<u8> {
  encode(&Bundle2 {
    params,
    parts: Vec::new(),
    events: Vec::new(),
  })
}

fn param(name: &[u8], value: Option<&[u8]>) -> StreamParam {
  StreamParam {
    name: name.to_vec(),
    value: value.map(<[u8]>::to_vec),
  }
}

fn part(
  kind: &'static [u8],
  params: Vec<Param<'static>>,
  mandatory: usize,
  depth: u64,
) -> Part<'static> {
  Part {
    kind,
    id: 0,
    params,
    mandatory_params: mandatory,
    depth,
  }
}

/// `depth` parts, each interrupting the one before it, closed innermost first.
fn nested(depth: usize) -> Vec<u8> {
  let parts = (0..depth)
    .map(|level| part(b"error:abort", Vec::new(), 0, u64::try_from(level).unwrap()))
    .collect();
  let mut events = Vec::new();
  for level in 0..depth {
    if level > 0 {
      events.push(Event::Interrupt);
    }
    events.push(Event::Open(level));
  }
  events.extend(std::iter::repeat_n(Event::Close, depth));
  encode(&Bundle2 {
    params: Vec::new(),
    parts,
    events,
  })
}

const GOLDEN: [(&[u8], &str); 3] = [
  (fixtures::CG02, fixtures::CG02_LISTING),
  (fixtures::CG03, fixtures::CG03_LISTING),
  (fixtures::CG04, fixtures::CG04_LISTING),
];

/// AC-HG.1 (golden), A-514: do decode the three bundle2 streams stock Mercurial 7.2 wrote. Expect no stream
/// parameter, no interruption, and every part as 7.2's own `debugbundle` lists it: its type, its parameters and
/// whether it is mandatory, in order.
#[test]
fn the_golden_streams_hold_the_parts_mercurial_lists() {
  for (bytes, listing) in GOLDEN {
    let stream = Bundle2::decode(bytes, &mut room()).unwrap();
    assert!(stream.params.is_empty());
    let described: Vec<String> = stream.top_level().map(|(_, part)| describe(part)).collect();
    assert_eq!(described, fixtures::part_lines(listing));
    assert_eq!(stream.top_level().count(), stream.parts.len());
    assert!(!stream.events.contains(&Event::Interrupt));
  }
}

/// A-514 (golden): do decode each golden stream and encode it again. Expect its own bytes back, byte for byte, and
/// `encoded_len` their length.
#[test]
fn the_golden_streams_encode_back_to_their_bytes() {
  for (bytes, _) in GOLDEN {
    let stream = Bundle2::decode(bytes, &mut room()).unwrap();
    assert_eq!(encode(&stream), bytes);
    assert_eq!(
      stream.encoded_len().unwrap(),
      u64::try_from(bytes.len()).unwrap()
    );
  }
}

/// A-514: do read a golden stream's changegroup payload whole, against an admission of exactly its length and of one
/// byte less. Expect the bytes that follow the part's header, and the smaller admission refused with nothing taken.
#[test]
fn a_payload_is_read_whole_within_its_admission() {
  let stream = Bundle2::decode(fixtures::CG02, &mut room()).unwrap();
  let (index, changegroup) = stream
    .top_level()
    .find(|(_, part)| part.is(b"changegroup"))
    .unwrap();
  assert_eq!(changegroup.param(b"version"), Some(&b"02"[..]));
  assert_eq!(changegroup.param(b"nbchanges"), Some(&b"7"[..]));
  assert_eq!(changegroup.mandatory().len(), 1);
  let chunks: Vec<&[u8]> = stream.payload(index).collect();
  let len: usize = chunks.iter().map(|chunk| chunk.len()).sum();
  let mut short = Admission::new(u64::try_from(len).unwrap() - 1);
  assert!(stream.payload_bytes(index, &mut short).is_err());
  assert_eq!(short.left(), u64::try_from(len).unwrap() - 1);
  let mut exact = Admission::new(u64::try_from(len).unwrap());
  assert_eq!(
    stream.payload_bytes(index, &mut exact).unwrap(),
    chunks.concat()
  );
  assert_eq!(exact.left(), 0);
}

/// AC-HG.9 (hostile input), A-514: do give a bundle1, other magics and a stream cut inside its magic. Expect
/// `ForeignMagic` naming the bytes found, and `Truncated` for the cut.
#[test]
fn a_foreign_magic_is_refused() {
  assert_eq!(
    Bundle2::decode(fixtures::CG01, &mut room()),
    Err(Refusal::ForeignMagic { found: *b"HG10" })
  );
  assert_eq!(
    Bundle2::decode(b"HG21\0\0\0\0\0\0\0\0", &mut room()),
    Err(Refusal::ForeignMagic { found: *b"HG21" })
  );
  assert!(matches!(
    Bundle2::decode(b"HG2", &mut room()),
    Err(Refusal::Truncated(_))
  ));
}

/// AC-HG.9, A-514: do put `len = u32::MAX` (−1 as 7.2 reads it) and `i32::MAX` in every length of a stream. Expect
/// the stream parameters' and a part header's −1 refused `NegativeLength`; a payload chunk's −1 read as the interrupt
/// it is, so the payload's own bytes then fail as a part header; −2 refused; and `i32::MAX` refused `Truncated`.
#[test]
fn every_length_at_u32_max_or_i32_max_is_refused() {
  let minus_one = u32::MAX.to_be_bytes();
  let mut params = b"HG20".to_vec();
  params.extend_from_slice(&minus_one);
  assert_eq!(
    Bundle2::decode(&params, &mut room()),
    Err(Refusal::NegativeLength {
      what: "stream parameters length",
      value: -1
    })
  );
  let mut header = b"HG20\0\0\0\0".to_vec();
  header.extend_from_slice(&minus_one);
  assert_eq!(
    Bundle2::decode(&header, &mut room()),
    Err(Refusal::NegativeLength {
      what: "part header length",
      value: -1
    })
  );
  let header_len = usize::try_from(i32::from_be_bytes(
    fixtures::CG02[8..12].try_into().unwrap(),
  ))
  .unwrap();
  let chunk_at = 12 + header_len;
  for (value, expect_negative) in [(-1_i32, false), (-2_i32, true)] {
    let mut bytes = fixtures::CG02.to_vec();
    bytes[chunk_at..chunk_at + 4].copy_from_slice(&value.to_be_bytes());
    let result = Bundle2::decode(&bytes, &mut room());
    assert!(result.is_err());
    assert_eq!(
      matches!(result, Err(Refusal::NegativeLength { value: -2, .. })),
      expect_negative
    );
  }
  let mut huge = b"HG20\0\0\0\0".to_vec();
  huge.extend_from_slice(&i32::MAX.to_be_bytes());
  assert!(matches!(
    Bundle2::decode(&huge, &mut room()),
    Err(Refusal::Truncated(_))
  ));
}

/// AC-HG.9, A-514: do cut each golden stream at every length short of its own. Expect every cut refused, none read as
/// a whole stream, and none to panic.
#[test]
fn a_stream_cut_anywhere_is_refused() {
  for (bytes, _) in GOLDEN {
    for cut in 0..bytes.len() {
      assert!(
        Bundle2::decode(&bytes[..cut], &mut room()).is_err(),
        "cut at {cut}"
      );
    }
  }
}

/// AC-HG.9, A-514: do flip every bit of a golden stream, one at a time. Expect a typed refusal or a stream that
/// encodes and decodes back to itself; never a panic.
#[test]
fn a_flipped_bit_anywhere_is_refused_or_read_consistently() {
  let mut refused = 0;
  for at in 0..fixtures::CG02.len() {
    for bit in 0..8 {
      let mut bytes = fixtures::CG02.to_vec();
      bytes[at] ^= 1 << bit;
      match Bundle2::decode(&bytes, &mut room()) {
        Ok(stream) => assert_eq!(
          Bundle2::decode(&encode(&stream), &mut room()).unwrap(),
          stream
        ),
        Err(_) => refused += 1,
      }
    }
  }
  assert!(refused > 0);
}

/// AC-HG.9, A-514: do decode ten thousand parts each interrupting the one before, then the same against an admission
/// one byte short of what it was charged. Expect the deepest part at depth 9,999 with no recursion to overflow, the
/// stream encoding back to its bytes, and the short admission refused before anything is built.
#[test]
fn parts_nested_past_their_admission_are_refused_and_depth_needs_no_stack() {
  let bytes = nested(10_000);
  let mut admission = room();
  let stream = Bundle2::decode(&bytes, &mut admission).unwrap();
  assert_eq!(stream.parts.len(), 10_000);
  assert_eq!(stream.parts.last().unwrap().depth, 9_999);
  assert_eq!(encode(&stream), bytes);
  let charged = u64::MAX - admission.left();
  let mut short = Admission::new(charged - 1);
  assert!(matches!(
    Bundle2::decode(&bytes, &mut short),
    Err(Refusal::PastAdmission(_))
  ));
  assert!(Bundle2::decode(&bytes, &mut Admission::new(charged)).is_ok());
}

/// A-514: do interrupt a part's payload with a part, and again with no part. Expect each part's payload to be its own
/// chunks alone, the interrupting part one level deeper, and the stream to encode back to its bytes.
#[test]
fn an_interrupted_part_resumes_and_keeps_its_own_payload() {
  let stream = Bundle2 {
    params: Vec::new(),
    parts: vec![
      part(b"CHANGEGROUP", Vec::new(), 0, 0),
      part(
        b"error:abort",
        vec![Param {
          key: b"message",
          value: b"boom",
        }],
        1,
        1,
      ),
    ],
    events: vec![
      Event::Open(0),
      Event::Chunk(b"ab"),
      Event::Interrupt,
      Event::Open(1),
      Event::Chunk(b"xy"),
      Event::Close,
      Event::Chunk(b"cd"),
      Event::EmptyInterrupt,
      Event::Chunk(b"ef"),
      Event::Close,
    ],
  };
  let bytes = encode(&stream);
  let decoded = Bundle2::decode(&bytes, &mut room()).unwrap();
  assert_eq!(decoded, stream);
  assert_eq!(
    decoded.payload(0).collect::<Vec<_>>(),
    vec![&b"ab"[..], b"cd", b"ef"]
  );
  assert_eq!(decoded.payload(1).collect::<Vec<_>>(), vec![&b"xy"[..]]);
  assert_eq!(decoded.payload(7).count(), 0);
  assert_eq!(decoded.top_level().count(), 1);
  assert!(decoded.parts[0].is_mandatory() && !decoded.parts[1].is_mandatory());
}

/// AC-HG.9, A-514: do give the stream parameters 7.2's `_processparam` and `processcompression` judge. Expect `UN`
/// read; the compressed engines refused `CompressedStream` in either case of the name (7.2 matches it without case);
/// a `Compression` without a known value, an unknown mandatory name, a name that is not a letter first and an empty
/// name each refused by kind; an unknown advisory name kept; and a value quoted and unquoted back to itself.
#[test]
fn stream_parameters_are_judged_as_mercurial_judges_them() {
  let decode =
    |params| Bundle2::decode(&with_params(params), &mut room()).map(|stream| stream.params);
  assert!(decode(vec![param(b"Compression", Some(b"UN"))]).is_ok());
  for (name, engine) in [
    (&b"Compression"[..], &b"GZ"[..]),
    (&b"compression"[..], &b"ZS"[..]),
    (&b"COMPRESSION"[..], &b"BZ"[..]),
  ] {
    assert_eq!(
      decode(vec![param(name, Some(engine))]),
      Err(Refusal::CompressedStream {
        engine: engine.to_vec()
      })
    );
  }
  assert_eq!(
    decode(vec![param(b"Compression", None)]),
    Err(Refusal::UnknownCompression { engine: None })
  );
  assert_eq!(
    decode(vec![param(b"Compression", Some(b"XX"))]),
    Err(Refusal::UnknownCompression {
      engine: Some(b"XX".to_vec())
    })
  );
  assert_eq!(
    decode(vec![param(b"Wanted", Some(b"1"))]),
    Err(Refusal::UnknownMandatoryParameter {
      name: b"Wanted".to_vec()
    })
  );
  assert_eq!(
    decode(vec![param(b"1st", None)]),
    Err(Refusal::ParameterNameNotLetter {
      name: b"1st".to_vec()
    })
  );
  let kept = vec![param(b"note", Some(b"a b/c%d")), param(b"flag", None)];
  assert_eq!(decode(kept.clone()), Ok(kept));
  let mut empty = b"HG20".to_vec();
  empty.extend_from_slice(&3_i32.to_be_bytes());
  empty.extend_from_slice(b"a  ");
  empty.extend_from_slice(&0_i32.to_be_bytes());
  assert_eq!(
    Bundle2::decode(&empty, &mut room()),
    Err(Refusal::EmptyParameterName)
  );
}

/// AC-HG.9, A-514: do give part headers 7.2 would misread: a type with a space, an empty type, a key twice, a byte
/// left after the parameters, parameter sizes past the header. Expect each refused by kind.
#[test]
fn part_headers_mercurial_would_misread_are_refused() {
  let id = [0, 0, 0, 0];
  let header = |kind: &[u8], counts: [u8; 2], sizes: &[u8], values: &[u8]| {
    let mut out = vec![u8::try_from(kind.len()).unwrap()];
    out.extend_from_slice(kind);
    out.extend_from_slice(&id);
    out.extend_from_slice(&counts);
    out.extend_from_slice(sizes);
    out.extend_from_slice(values);
    out
  };
  let decode =
    |bytes: Vec<u8>| Bundle2::decode(&bytes, &mut room()).map(|stream| stream.parts.len());
  assert_eq!(decode(with_header(&header(b"x", [0, 0], b"", b""))), Ok(1));
  assert_eq!(
    decode(with_header(&header(b"a b", [0, 0], b"", b""))),
    Err(Refusal::PartType {
      kind: b"a b".to_vec()
    })
  );
  assert_eq!(
    decode(with_header(&header(b"", [0, 0], b"", b""))),
    Err(Refusal::PartType { kind: Vec::new() })
  );
  assert_eq!(
    decode(with_header(&header(b"x", [0, 2], &[1, 1, 1, 2], b"k1k22"))),
    Err(Refusal::DuplicateParameter {
      kind: b"x".to_vec(),
      key: b"k".to_vec()
    })
  );
  let mut leftover = header(b"x", [0, 0], b"", b"");
  leftover.push(9);
  assert_eq!(
    decode(with_header(&leftover)),
    Err(Refusal::HeaderLeftover {
      kind: b"x".to_vec(),
      bytes: 1
    })
  );
  assert!(matches!(
    decode(with_header(&header(b"x", [1, 0], &[5, 5], b"key"))),
    Err(Refusal::Truncated(_))
  ));
}

/// AC-HG.9, A-514: do append a byte after a golden stream's end. Expect `TrailingBytes`, not a stream read short.
#[test]
fn bytes_after_the_end_are_refused() {
  let mut bytes = fixtures::CG02.to_vec();
  bytes.push(0);
  assert_eq!(
    Bundle2::decode(&bytes, &mut room()),
    Err(Refusal::TrailingBytes { bytes: 1 })
  );
}

/// The encoding of a stream of `parts` and `events`, or its refusal.
fn try_encode(parts: Vec<Part<'static>>, events: Vec<Event<'static>>) -> Result<(), EncodeRefusal> {
  Bundle2 {
    params: Vec::new(),
    parts,
    events,
  }
  .encode(&mut Vec::new())
}

/// One part with no parameters, for the malformed streams below.
fn one_part() -> Vec<Part<'static>> {
  vec![part(b"x", Vec::new(), 0, 0)]
}

/// A-514: do encode an empty chunk, and a chunk outside any part. Expect `EmptyChunk` at the chunk (its length, 0,
/// would read as the part's end) and `Malformed` at the stray chunk.
#[test]
fn empty_and_stray_chunks_are_not_encoded() {
  assert_eq!(
    try_encode(
      one_part(),
      vec![Event::Open(0), Event::Chunk(b""), Event::Close]
    ),
    Err(EncodeRefusal::EmptyChunk { at: 1 })
  );
  assert!(matches!(
    try_encode(one_part(), vec![Event::Chunk(b"a")]),
    Err(EncodeRefusal::Malformed { at: 0, .. })
  ));
}

/// A-514: do encode a part opened inside another without an interruption, a part left open, and an event naming no
/// part. Expect each refused `Malformed` at the event that broke it.
#[test]
fn parts_opened_out_of_place_left_open_or_missing_are_not_encoded() {
  assert!(matches!(
    try_encode(one_part(), vec![Event::Open(0), Event::Open(0)]),
    Err(EncodeRefusal::Malformed { at: 1, .. })
  ));
  assert!(matches!(
    try_encode(one_part(), vec![Event::Open(0)]),
    Err(EncodeRefusal::Malformed { at: 1, .. })
  ));
  assert!(matches!(
    try_encode(one_part(), vec![Event::Open(1)]),
    Err(EncodeRefusal::Malformed { at: 0, .. })
  ));
}

/// A-514: do encode a part whose type holds a space. Expect `PartType` naming it, as 7.2's `validateparttype` refuses
/// one.
#[test]
fn a_part_type_with_a_space_is_not_encoded() {
  assert_eq!(
    try_encode(
      vec![part(b"a b", Vec::new(), 0, 0)],
      vec![Event::Open(0), Event::Close]
    ),
    Err(EncodeRefusal::PartType {
      kind: b"a b".to_vec()
    })
  );
}

/// A generated part's fields: its type, its parameters (key and value), how many are mandatory, its payload chunks,
/// and whether an interruption follows them.
type GeneratedPart = (String, Vec<(String, Vec<u8>)>, usize, Vec<Vec<u8>>, bool);

/// A generated part: a valid type, distinct keys, a mandatory count, payload chunks, and maybe an interruption.
fn arbitrary_part() -> impl Strategy<Value = GeneratedPart> {
  (
    "[a-zA-Z0-9_:-]{1,12}",
    proptest::collection::btree_map(
      "[a-z]{1,6}",
      proptest::collection::vec(any::<u8>(), 0..6),
      0..4,
    )
    .prop_map(|map| map.into_iter().collect::<Vec<_>>()),
    0..4_usize,
    proptest::collection::vec(proptest::collection::vec(any::<u8>(), 1..12), 0..4),
    any::<bool>(),
  )
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// AC-HG.9, A-514 (hostile input): do decode arbitrary bytes, and arbitrary bytes after the magic. Expect a stream
  /// or a typed refusal, never a panic; only input that begins `HG20` decodes, and none shorter than a stream's
  /// smallest (its magic, an empty parameter block and the end: 12 bytes).
  #[test]
  fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
    let bare = Bundle2::decode(&bytes, &mut Admission::new(1 << 16)).is_ok();
    prop_assert!(!bare || bytes.starts_with(b"HG20"));
    let mut prefixed = b"HG20".to_vec();
    prefixed.extend_from_slice(&bytes);
    let after_magic = Bundle2::decode(&prefixed, &mut Admission::new(1 << 16)).is_ok();
    prop_assert!(!after_magic || prefixed.len() >= 12);
  }

  /// A-514 (round trip): do encode generated streams of parts with parameters, chunks and interruptions, then
  /// decode them. Expect the same stream back, and its encoding again byte for byte.
  #[test]
  fn generated_streams_round_trip(parts in proptest::collection::vec(arbitrary_part(), 0..5)) {
    let mut built = Vec::new();
    let mut events = Vec::new();
    for (kind, params, mandatory, chunks, interrupted) in &parts {
      let params: Vec<Param<'_>> = params.iter().map(|(key, value)| Param { key: key.as_bytes(), value: value.as_slice() }).collect();
      let mandatory = (*mandatory).min(params.len());
      events.push(Event::Open(built.len()));
      built.push(Part { kind: kind.as_bytes(), id: u32::try_from(built.len()).unwrap(), params, mandatory_params: mandatory, depth: 0 });
      for chunk in chunks {
        events.push(Event::Chunk(chunk));
      }
      if *interrupted {
        events.push(Event::Interrupt);
        events.push(Event::Open(built.len()));
        built.push(Part { kind: b"error:abort", id: 0, params: Vec::new(), mandatory_params: 0, depth: 1 });
        events.push(Event::Close);
        events.push(Event::Chunk(b"after"));
      }
      events.push(Event::Close);
    }
    let stream = Bundle2 { params: Vec::new(), parts: built, events };
    let bytes = encode(&stream);
    let decoded = Bundle2::decode(&bytes, &mut room()).unwrap();
    prop_assert_eq!(&decoded, &stream);
    prop_assert_eq!(encode(&decoded), bytes);
  }
}
