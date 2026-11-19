use proptest::prelude::*;

use super::{BodyReader, BodyRefused, Step};
use crate::head::Framing;
use crate::reply::Status;
use crate::{Admission, MAX_LINE, PastAdmission, to_u64};

/// Room enough for every trailer section these tests send.
const ROOM: u64 = 1 << 20;

/// What a body read in `pieces` gave: its data and the bytes it took in all, or its refusal.
fn read_in(framing: Framing, pieces: &[&[u8]], admission: &mut Admission) -> Result<(Vec<u8>, usize), BodyRefused> {
  let mut reader = BodyReader::new(framing);
  let (mut data, mut taken) = (Vec::new(), 0usize);
  for piece in pieces {
    let mut at = 0;
    loop {
      match reader.step(&piece[at..], admission)? {
        Step::Data { data: run, consumed } => {
          assert!(!run.is_empty(), "an empty run of data");
          data.extend_from_slice(run);
          at += consumed;
        }
        Step::More { consumed } => {
          assert_eq!(at + consumed, piece.len(), "More left input untaken");
          taken += piece.len();
          break;
        }
        Step::End { consumed } => return Ok((data, taken + at + consumed)),
      }
    }
  }
  panic!("the body did not end: {} bytes of data so far", data.len());
}

/// What `input` read whole gives.
fn read_whole(framing: Framing, input: &[u8]) -> Result<(Vec<u8>, usize), BodyRefused> {
  read_in(framing, &[input], &mut Admission::new(ROOM))
}

/// What `input` read whole is refused with (a body that ends is a failure here).
fn refused(input: &[u8]) -> BodyRefused {
  let mut reader = BodyReader::new(Framing::Chunked);
  let mut admission = Admission::new(ROOM);
  let mut at = 0;
  loop {
    match reader.step(&input[at..], &mut admission) {
      Err(refusal) => return refusal,
      Ok(Step::Data { consumed, .. }) => at += consumed,
      Ok(other) => panic!("{:?} read {other:?}", String::from_utf8_lossy(input)),
    }
  }
}

/// `data` chunked in runs of `sizes` (cycled), with no extensions and no trailers: the reference encoder.
fn chunked(data: &[u8], sizes: &[usize]) -> Vec<u8> {
  let mut out = Vec::new();
  let mut rest = data;
  let mut size = sizes.iter().cycle();
  while !rest.is_empty() {
    let take = (*size.next().unwrap()).clamp(1, rest.len());
    out.extend_from_slice(format!("{take:x}\r\n").as_bytes());
    out.extend_from_slice(&rest[..take]);
    out.extend_from_slice(b"\r\n");
    rest = &rest[take..];
  }
  out.extend_from_slice(b"0\r\n\r\n");
  out
}

/// The chunked body of RFC 9112's era's own illustration, with a next request after it.
const WIKI: &[u8] = b"4\r\nWiki\r\n6\r\npedia \r\nE\r\nin \r\n\r\nchunks.\r\n0\r\n\r\nGET / HTTP/1.1\r\n";

/// A-595: do read a `Content-Length` body whole and at every cut, a next request after it. Expect its bytes, and the
/// end exactly after them.
#[test]
fn a_length_body_is_its_bytes_and_ends_there() {
  let input = b"helloGET";
  assert_eq!(read_whole(Framing::Length(5), input), Ok((b"hello".to_vec(), 5)));
  for cut in 0..=input.len() {
    let pieces: [&[u8]; 2] = [&input[..cut], &input[cut..]];
    assert_eq!(
      read_in(Framing::Length(5), &pieces, &mut Admission::new(ROOM)),
      Ok((b"hello".to_vec(), 5)),
      "cut at {cut}"
    );
  }
}

/// A-595: do read no body and a body of length zero. Expect the end at once, taking nothing.
#[test]
fn no_body_and_an_empty_body_end_at_once() {
  for framing in [Framing::None, Framing::Length(0)] {
    let mut reader = BodyReader::new(framing);
    let mut admission = Admission::new(ROOM);
    assert_eq!(reader.step(b"GET", &mut admission), Ok(Step::End { consumed: 0 }));
    assert_eq!(reader.step(b"", &mut admission), Ok(Step::End { consumed: 0 }));
  }
}

/// A-595: do read a chunked body whole, at every cut, and a byte at a time. Expect its chunks' data joined each time,
/// and the end exactly before the next request.
#[test]
fn every_cut_of_a_chunked_body_reads_its_data() {
  let body_len = WIKI.len() - b"GET / HTTP/1.1\r\n".len();
  let expected = (b"Wikipedia in \r\n\r\nchunks.".to_vec(), body_len);
  assert_eq!(read_whole(Framing::Chunked, WIKI), Ok(expected.clone()));
  for cut in 0..=WIKI.len() {
    let pieces: [&[u8]; 2] = [&WIKI[..cut], &WIKI[cut..]];
    assert_eq!(
      read_in(Framing::Chunked, &pieces, &mut Admission::new(ROOM)),
      Ok(expected.clone()),
      "cut at {cut}"
    );
  }
  let bytes: Vec<&[u8]> = WIKI.chunks(1).collect();
  assert_eq!(read_in(Framing::Chunked, &bytes, &mut Admission::new(ROOM)), Ok(expected));
}

/// A-595: do read chunks with extensions (tokens, quoted strings holding `;` and an escaped quote, whitespace before the
/// `;`) and a trailer section. Expect the extensions skipped, the trailers dropped and charged, the data alone.
#[test]
fn chunk_extensions_are_skipped_and_trailers_dropped_and_charged() {
  let trailers = b"Expires: never\r\nX-Checksum: \tabc\r\n";
  let mut input = b"5;name=value;q=\"a;\\\"b\"\r\nhello\r\n000006 ;x\r\n world\r\n0;last\r\n".to_vec();
  input.extend_from_slice(trailers);
  input.extend_from_slice(b"\r\n");
  let mut admission = Admission::new(ROOM);
  assert_eq!(
    read_in(Framing::Chunked, &[&input], &mut admission),
    Ok((b"hello world".to_vec(), input.len()))
  );
  assert_eq!(admission.left(), ROOM - to_u64(trailers.len() + 2));
}

/// A-595: do send every malformed chunk-size line, a chunk without its CRLF and a size line past [`MAX_LINE`]. Expect
/// each refused by its kind, and leading zeros read.
#[test]
fn hostile_chunk_lines_are_refused_by_kind() {
  use BodyRefused::*;
  let long_extension = format!("1;{}\r\n", "a".repeat(MAX_LINE));
  let cases: [(&[u8], BodyRefused); 15] = [
    (b"z\r\n", ChunkSize),
    (b"\r\n", ChunkSize),
    (b";ext\r\n", ChunkSize),
    (b"10000000000000000\r\n", ChunkSize),
    (b"0x5\r\nhello\r\n", ChunkSize),
    (b"-5\r\n", ChunkSize),
    (b"5 \r\nhello\r\n", ChunkExtension),
    (b"5 x\r\n", ChunkExtension),
    (b"5;a\nb\r\n", LineBreak),
    (b"5;a\x00\r\n", ChunkExtension),
    (b"5\rx", LineBreak),
    (b"5\nhello\r\n", LineBreak),
    (b"5\r\nhelloX\r\n", ChunkEnd),
    (b"5\r\nhello\rX", ChunkEnd),
    (long_extension.as_bytes(), LineTooLong),
  ];
  for (input, refusal) in cases {
    assert_eq!(refused(input), refusal, "{:?}", String::from_utf8_lossy(input));
  }
  assert_eq!(
    read_whole(Framing::Chunked, b"0000000005\r\nhello\r\n00\r\n\r\n"),
    Ok((b"hello".to_vec(), 25))
  );
  let at_bound = format!("1;{}\r\nx\r\n0\r\n\r\n", "a".repeat(MAX_LINE - 2));
  assert_eq!(
    read_whole(Framing::Chunked, at_bound.as_bytes()),
    Ok((b"x".to_vec(), at_bound.len()))
  );
}

/// A-595: do send trailer sections that break the field grammar, a trailer line past [`MAX_LINE`], and a section past
/// the room. Expect each refused by its kind, the room's refusal naming what it was asked.
#[test]
fn trailers_are_held_to_the_grammar_and_the_room() {
  use BodyRefused::*;
  let long = format!("0\r\nX: {}\r\n\r\n", "a".repeat(MAX_LINE));
  let cases: [(&[u8], BodyRefused); 7] = [
    (b"0\r\n folded: x\r\n\r\n", Trailer),
    (b"0\r\nNoColon\r\n\r\n", Trailer),
    (b"0\r\nBad Name: x\r\n\r\n", Trailer),
    (b"0\r\nX: a\x00b\r\n\r\n", Trailer),
    (b"0\r\nX: a\nb\r\n\r\n", LineBreak),
    (b"0\r\nX: a\rb\r\n\r\n", LineBreak),
    (long.as_bytes(), LineTooLong),
  ];
  for (input, refusal) in cases {
    assert_eq!(refused(input), refusal, "{:?}", String::from_utf8_lossy(input));
  }
  let mut reader = BodyReader::new(Framing::Chunked);
  let mut admission = Admission::new(5);
  assert_eq!(
    reader.step(b"0\r\nX: abcdef\r\n\r\n", &mut admission),
    Err(TrailersTooLarge(PastAdmission { asked: 1, left: 0 }))
  );
}

/// A-595: do stream a large `Content-Length` body in pieces. Expect every run of data a slice of its own piece, nothing
/// charged: the reader holds none of the body.
#[test]
fn a_body_is_streamed_never_held() {
  let body: Vec<u8> = (0..(1 << 20)).map(|at: u32| at.to_le_bytes()[0]).collect();
  let mut reader = BodyReader::new(Framing::Length(to_u64(body.len())));
  let mut admission = Admission::new(ROOM);
  let mut seen = 0usize;
  for piece in body.chunks(65536) {
    match reader.step(piece, &mut admission) {
      Ok(Step::Data { data, consumed }) => {
        assert_eq!(data.as_ptr(), piece.as_ptr());
        assert_eq!((data.len(), consumed), (piece.len(), piece.len()));
        seen += data.len();
      }
      other => panic!("{other:?}"),
    }
  }
  assert_eq!(seen, body.len());
  assert_eq!(reader.step(b"", &mut admission), Ok(Step::End { consumed: 0 }));
  assert_eq!(admission.left(), ROOM);
}

/// A-595: do ask each refusal's status. Expect 431 for trailers past the room and 400 for every other.
#[test]
fn each_refusal_has_its_status() {
  assert_eq!(
    BodyRefused::TrailersTooLarge(PastAdmission { asked: 1, left: 0 }).status(),
    Status::FIELDS_TOO_LARGE
  );
  for refusal in [
    BodyRefused::ChunkSize,
    BodyRefused::ChunkExtension,
    BodyRefused::ChunkEnd,
    BodyRefused::LineBreak,
    BodyRefused::LineTooLong,
    BodyRefused::Trailer,
  ] {
    assert_eq!(refusal.status(), Status::BAD_REQUEST, "{refusal:?}");
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-595: do chunk any data in any sizes (the reference encoder), then read it in any cuts. Expect the data back and
  /// the end exactly at the encoding's last byte, the next request's bytes untaken.
  #[test]
  fn any_chunking_read_in_any_cuts_gives_its_data_back(
    data in proptest::collection::vec(any::<u8>(), 0..2048),
    sizes in proptest::collection::vec(1usize..300, 1..8),
    cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..12),
  ) {
    let mut input = chunked(&data, &sizes);
    let body_len = input.len();
    input.extend_from_slice(b"POST /next HTTP/1.1\r\n");
    let mut points: Vec<usize> = cuts.iter().map(|cut| cut.index(input.len() + 1)).collect();
    points.sort_unstable();
    let mut pieces: Vec<&[u8]> = Vec::new();
    let mut from = 0;
    for point in points {
      pieces.push(&input[from..point]);
      from = point;
    }
    pieces.push(&input[from..]);
    prop_assert_eq!(
      read_in(Framing::Chunked, &pieces, &mut Admission::new(ROOM)),
      Ok((data, body_len))
    );
  }
}
