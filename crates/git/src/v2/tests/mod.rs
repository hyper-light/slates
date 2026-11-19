//! A-582 part 2: what every message's tests share, the golden streams with the decoder each is read by, and the
//! hostile-input tests run over all of them: every cut, every bit flip, lines at and past a packet's length, and
//! arbitrary messages.

use proptest::prelude::*;

use super::{Refusal, advertisement, fetch, ls_refs, request};
use crate::fixtures;
use crate::object_name::{ObjectFormat, ObjectId};
use crate::pktline::{self, HEADER_LEN, MAX_DATA, MAX_LEN, Packet};
use crate::{Admission, to_u64};

/// The message a stream holds, and so the decoder that reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Message {
  /// A capability advertisement.
  Advertisement,
  /// A command request.
  Request,
  /// An `ls-refs` answer, in SHA-1.
  LsRefsAnswer,
  /// A `fetch` response, in SHA-1, multiplexed throughout or not.
  FetchResponse(bool),
}

/// Every stream stock git 2.50.1 wrote or read (`crate::fixtures`), named, with the message it holds.
pub(crate) const GOLDEN: [(&str, &[u8], Message); 16] = [
  (
    "advertisement",
    fixtures::ADVERTISEMENT,
    Message::Advertisement,
  ),
  (
    "advertisement-uris",
    fixtures::ADVERTISEMENT_URIS,
    Message::Advertisement,
  ),
  (
    "ls-refs request",
    fixtures::LS_REFS_REQUEST,
    Message::Request,
  ),
  (
    "ls-refs response",
    fixtures::LS_REFS_RESPONSE,
    Message::LsRefsAnswer,
  ),
  (
    "fetch-clone request",
    fixtures::FETCH_CLONE_REQUEST,
    Message::Request,
  ),
  (
    "fetch-clone response",
    fixtures::FETCH_CLONE_RESPONSE,
    Message::FetchResponse(false),
  ),
  (
    "fetch-haves request",
    fixtures::FETCH_HAVES_REQUEST,
    Message::Request,
  ),
  (
    "fetch-haves response",
    fixtures::FETCH_HAVES_RESPONSE,
    Message::FetchResponse(false),
  ),
  (
    "fetch-shallow request",
    fixtures::FETCH_SHALLOW_REQUEST,
    Message::Request,
  ),
  (
    "fetch-shallow response",
    fixtures::FETCH_SHALLOW_RESPONSE,
    Message::FetchResponse(false),
  ),
  (
    "fetch-ref-in-want request",
    fixtures::FETCH_REF_IN_WANT_REQUEST,
    Message::Request,
  ),
  (
    "fetch-ref-in-want response",
    fixtures::FETCH_REF_IN_WANT_RESPONSE,
    Message::FetchResponse(false),
  ),
  (
    "fetch-progress request",
    fixtures::FETCH_PROGRESS_REQUEST,
    Message::Request,
  ),
  (
    "fetch-progress response",
    fixtures::FETCH_PROGRESS_RESPONSE,
    Message::FetchResponse(false),
  ),
  (
    "fetch-uris request",
    fixtures::FETCH_URIS_REQUEST,
    Message::Request,
  ),
  (
    "fetch-uris response",
    fixtures::FETCH_URIS_RESPONSE,
    Message::FetchResponse(true),
  ),
];

/// The room a test's decode is given: as much as there is.
pub(crate) fn room() -> Admission {
  Admission::new(u64::MAX)
}

/// The SHA-1 id `hex` writes.
pub(crate) fn sha1(hex: &str) -> ObjectId {
  ObjectId::from_hex(ObjectFormat::Sha1, hex.as_bytes()).unwrap()
}

/// `packets` written as pkt-lines.
pub(crate) fn wire(packets: &[Packet<'_>]) -> Vec<u8> {
  let mut out = Vec::new();
  for packet in packets {
    pktline::write(*packet, &mut out).unwrap();
  }
  out
}

/// Decodes `bytes` as `message` and, when that is accepted, encodes what it read: the bytes written, or the decode's
/// refusal. An encode of what a decode accepted never fails.
pub(crate) fn decode_then_encode(message: Message, bytes: &[u8]) -> Result<Vec<u8>, Refusal> {
  let mut out = Vec::new();
  let format = ObjectFormat::Sha1;
  match message {
    Message::Advertisement => {
      let read = advertisement::decode(bytes, &mut room())?;
      advertisement::encode(&read, &mut out).expect("a decoded advertisement encodes");
    }
    Message::Request => {
      let read = request::decode(bytes, &mut room())?;
      request::encode(&read, &mut out).expect("a decoded request encodes");
    }
    Message::LsRefsAnswer => {
      let read = ls_refs::decode_answer(bytes, format, &mut room())?;
      ls_refs::encode_answer(&read, format, &mut out).expect("a decoded answer encodes");
    }
    Message::FetchResponse(multiplexed) => {
      let read = fetch::decode_response(bytes, format, multiplexed, &mut room())?;
      fetch::encode_response(&read, format, multiplexed, &mut out)
        .expect("a decoded response encodes");
    }
  }
  Ok(out)
}

/// `bytes`, a pkt-line stream, with its length digits in lower case: what an encode of it writes, since part 1 reads
/// a length's digits in either case, as git's `hexval` does, and writes them in lower case, as git does.
pub(crate) fn canonical(bytes: &[u8]) -> Vec<u8> {
  let mut out = bytes.to_vec();
  let mut at = 0usize;
  while let Ok(Some((_, took))) = pktline::read(&bytes[at..], to_u64(at)) {
    out[at..at + HEADER_LEN].make_ascii_lowercase();
    at += took;
    if at >= bytes.len() {
      break;
    }
  }
  out
}

/// Where `refusal` stopped, for those that name a place.
pub(crate) fn place(refusal: &Refusal) -> Option<u64> {
  match *refusal {
    Refusal::Packet(packet) => match packet {
      pktline::Refusal::NotHex { at }
      | pktline::Refusal::ShorterThanHeader { at }
      | pktline::Refusal::TooLong { at, .. }
      | pktline::Refusal::Truncated { at, .. } => Some(at),
      pktline::Refusal::PastAdmission(_) => None,
    },
    Refusal::PastAdmission(_) => None,
    Refusal::Band { at, .. }
    | Refusal::Unfinished { at }
    | Refusal::Trailing { at }
    | Refusal::Misplaced { at }
    | Refusal::NotText { at }
    | Refusal::TooLong { at, .. }
    | Refusal::Version { at }
    | Refusal::Capability { at }
    | Refusal::UnknownCapability { at }
    | Refusal::ObjectFormat { at }
    | Refusal::UnknownCommand { at }
    | Refusal::SecondCommand { at }
    | Refusal::NoCommand { at }
    | Refusal::UnknownArgument { at }
    | Refusal::ObjectId { at, .. }
    | Refusal::Refname { at }
    | Refusal::Number { at }
    | Refusal::EmptyValue { at }
    | Refusal::Repeated { at }
    | Refusal::DeepenConflict { at }
    | Refusal::UnknownSection { at }
    | Refusal::SectionOutOfOrder { at }
    | Refusal::Acknowledgment { at }
    | Refusal::Line { at }
    | Refusal::NoPackfile { at }
    | Refusal::AfterError { at } => Some(at),
  }
}

/// A-582 part 2, golden vectors: do decode every advertisement, request and answer stock git 2.50.1 wrote or read with
/// the decoder its message takes, then encode what was read. Expect: each stream's own bytes, byte for byte.
#[test]
fn every_stream_stock_git_wrote_decodes_and_encodes_back_to_its_bytes() {
  for (name, stream, message) in GOLDEN {
    let written =
      decode_then_encode(message, stream).unwrap_or_else(|refusal| panic!("{name}: {refusal:?}"));
    assert_eq!(written, stream, "{name}");
  }
}

/// A-582 part 2, hostile input: do decode every cut of every golden stream. Expect: a cut inside a packet refused
/// `Truncated` where that packet begins, a cut between packets refused `Unfinished` there, never another refusal and
/// never a message, since each message ends at its last packet.
#[test]
fn every_cut_of_every_stream_is_truncated_or_unfinished() {
  for (name, stream, message) in GOLDEN {
    for cut in 0..stream.len() {
      match decode_then_encode(message, &stream[..cut]) {
        Err(Refusal::Unfinished { at }) => assert_eq!(at, to_u64(cut), "{name} cut at {cut}"),
        Err(Refusal::Packet(pktline::Refusal::Truncated { at, needed, left })) => {
          assert!(at <= to_u64(cut) && left < needed, "{name} cut at {cut}");
        }
        other => panic!("{name} cut at {cut}: {other:?}"),
      }
    }
  }
}

/// A-582 part 2, hostile input: do flip every bit of every byte of every golden stream and decode each. Expect: no
/// panic; a refusal names a place inside the stream; and a stream still accepted encodes back to exactly its bytes,
/// its length digits in lower case, so a decode never loses or invents a byte. The flips reach the refusals of every
/// layer, which the tallies show.
#[test]
fn every_bit_flip_of_every_stream_is_refused_by_kind_or_read_back_exactly() {
  let mut accepted = 0u32;
  let mut kinds = std::collections::BTreeSet::new();
  for (name, stream, message) in GOLDEN {
    for index in 0..stream.len() {
      for bit in 0..8 {
        let mut flipped = stream.to_vec();
        flipped[index] ^= 1 << bit;
        match decode_then_encode(message, &flipped) {
          Ok(written) => {
            assert_eq!(written, canonical(&flipped), "{name} {index}.{bit}");
            accepted += 1;
          }
          Err(refusal) => {
            let at = place(&refusal).unwrap_or_else(|| panic!("{name} {index}.{bit}: {refusal:?}"));
            assert!(
              at <= to_u64(flipped.len()),
              "{name} {index}.{bit}: {refusal:?}"
            );
            kinds.insert(
              format!("{refusal:?}")
                .split([' ', '('])
                .next()
                .unwrap()
                .to_owned(),
            );
          }
        }
      }
    }
  }
  assert!(accepted > 0, "some flips (inside packs) are still messages");
  for kind in [
    "Packet",
    "Band",
    "Version",
    "Capability",
    "UnknownCommand",
    "UnknownArgument",
    "ObjectId",
    "Refname",
    "UnknownSection",
    "Acknowledgment",
    "Line",
    "Misplaced",
  ] {
    assert!(kinds.contains(kind), "no flip reached {kind}: {kinds:?}");
  }
}

/// A-582 part 2, hostile input: lines at and past the most a packet carries. Do: decode a request whose `ref-prefix`
/// fills a packet whole, one whose argument of the same length is no argument, and a stream whose length is past
/// 65520 in each message; and encode a prefix one byte longer than a packet carries. Expect: the full packet read and
/// written back; the long non-argument refused `UnknownArgument`; the length past the cap refused `TooLong` by part 1
/// for every message; the long encode refused `TooLong`, nothing written.
#[test]
fn lines_at_and_past_a_packets_length_are_read_whole_or_refused_by_kind() {
  let prefix_len = MAX_DATA - b"ref-prefix \n".len();
  let prefix = format!(
    "ref-prefix refs/{}\n",
    "x".repeat(prefix_len - b"refs/".len())
  );
  assert_eq!(prefix.len(), MAX_DATA);
  let full = wire(&[
    Packet::Data(b"command=ls-refs\n"),
    Packet::Delim,
    Packet::Data(prefix.as_bytes()),
    Packet::Flush,
  ]);
  assert_eq!(
    decode_then_encode(Message::Request, &full),
    Ok(full.clone())
  );
  let junk = vec![b'x'; MAX_DATA];
  let long = wire(&[
    Packet::Data(b"command=fetch\n"),
    Packet::Delim,
    Packet::Data(&junk),
    Packet::Flush,
  ]);
  assert_eq!(
    decode_then_encode(Message::Request, &long),
    Err(Refusal::UnknownArgument { at: 22 })
  );
  for (name, _, message) in GOLDEN {
    let past = format!("{:04x}", MAX_LEN + 1);
    assert_eq!(
      decode_then_encode(message, past.as_bytes()),
      Err(Refusal::Packet(pktline::Refusal::TooLong {
        at: 0,
        len: to_u64(MAX_LEN + 1)
      })),
      "{name}"
    );
  }
  let over = vec![b'y'; MAX_DATA + 1 - b"ref-prefix ".len()];
  let request = request::Request::Command(request::CommandRequest {
    head: vec![super::Line::lf(request::Head::Command)],
    delimited: true,
    arguments: request::Arguments::LsRefs(vec![super::Line::bare(ls_refs::Argument::RefPrefix(
      &over,
    ))]),
  });
  let mut out = Vec::new();
  assert_eq!(
    request::encode(&request, &mut out),
    Err(Refusal::TooLong {
      at: 24,
      len: to_u64(MAX_DATA + 1)
    })
  );
  assert!(out.is_empty(), "nothing written");
}

/// Fragments messages are made of, for arbitrary messages near the grammar: words, ids, names, spaces and ends.
const FRAGMENTS: [&[u8]; 34] = [
  b"command=",
  b"ls-refs",
  b"fetch",
  b"object-format=",
  b"sha1",
  b"sha256",
  b"agent=",
  b"version 2",
  b"want ",
  b"have ",
  b"shallow ",
  b"unshallow ",
  b"want-ref ",
  b"deepen ",
  b"deepen-since ",
  b"filter ",
  b"done",
  b"ref-prefix ",
  b"peel",
  b"symrefs",
  b"unborn",
  b"d0f175d3390380c935e9b56e0cebb715995d838d",
  b"refs/heads/main",
  b"HEAD",
  b" ",
  b"\n",
  b"1",
  b"acknowledgments",
  b"NAK",
  b"ACK ",
  b"ready",
  b"packfile",
  b"symref-target:",
  b"\x01",
];

/// A packet of an arbitrary message: a special packet, or a data packet of fragments.
fn any_packet() -> impl Strategy<Value = (u8, Vec<u8>)> {
  prop_oneof![
    (1u8..4, Just(Vec::new())),
    (
      Just(0u8),
      proptest::collection::vec(0..FRAGMENTS.len(), 1..6).prop_map(|chosen| chosen
        .iter()
        .flat_map(|index| FRAGMENTS[*index].iter().copied())
        .collect())
    ),
    (Just(0u8), proptest::collection::vec(any::<u8>(), 0..40)),
  ]
}

/// A `fetch` argument of every kind, its values drawn from `id` and `number`.
fn any_fetch_argument() -> impl Strategy<Value = (u8, [u8; 20], u32, bool)> {
  (
    0u8..17,
    any::<[u8; 20]>(),
    1u32..=i32::MAX.unsigned_abs(),
    any::<bool>(),
  )
}

/// The `fetch` argument `kind` names, with its values.
fn fetch_argument(kind: u8, id: ObjectId, number: u32) -> fetch::Argument<'static> {
  match kind {
    0 => fetch::Argument::Want(id),
    1 => fetch::Argument::WantRef(b"refs/heads/main"),
    2 => fetch::Argument::Have(id),
    3 => fetch::Argument::Done,
    4 => fetch::Argument::ThinPack,
    5 => fetch::Argument::NoProgress,
    6 => fetch::Argument::IncludeTag,
    7 => fetch::Argument::OfsDelta,
    8 => fetch::Argument::Shallow(id),
    9 => fetch::Argument::Deepen(number),
    10 => fetch::Argument::DeepenRelative,
    11 => fetch::Argument::Filter(b"blob:none"),
    12 => fetch::Argument::PackfileUris(b"http,https"),
    13 => fetch::Argument::SidebandAll,
    14 => fetch::Argument::WaitForDone,
    15 => fetch::Argument::DeepenSince(u64::from(number)),
    _ => fetch::Argument::DeepenNot(b"refs/tags/v1"),
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-582 part 2: do write arbitrary `fetch` requests, each argument of any kind with any id, depth or time and
  /// either ending, leaving out what git refuses (a second filter or list of protocols, `deepen` beside a cut by time or
  /// rev), and decode what was written. Expect: the same request back.
  #[test]
  fn any_fetch_request_written_decodes_back_to_itself(
    chosen in proptest::collection::vec(any_fetch_argument(), 0..24),
    command_first in any::<bool>(),
    lf in any::<bool>(),
  ) {
    let mut filter = false;
    let mut uris = false;
    let mut depth = false;
    let mut cut = false;
    let arguments: Vec<super::Line<fetch::Argument<'_>>> = chosen
      .iter()
      .map(|(kind, bytes, number, ending)| {
        let id = ObjectId::from_bytes(ObjectFormat::Sha1, bytes).unwrap();
        (fetch_argument(*kind, id, *number), *ending)
      })
      .filter(|(argument, _)| match argument {
        fetch::Argument::Filter(_) => !core::mem::replace(&mut filter, true),
        fetch::Argument::PackfileUris(_) => !core::mem::replace(&mut uris, true),
        fetch::Argument::Deepen(_) if cut => false,
        fetch::Argument::Deepen(_) => {
          depth = true;
          true
        }
        fetch::Argument::DeepenSince(_) | fetch::Argument::DeepenNot(_) if depth => false,
        fetch::Argument::DeepenSince(_) | fetch::Argument::DeepenNot(_) => {
          cut = true;
          true
        }
        _ => true,
      })
      .map(|(value, ending)| super::Line { value, ending: if ending { super::Ending::Lf } else { super::Ending::NoLf } })
      .collect();
    let command = super::Line { value: request::Head::Command, ending: if lf { super::Ending::Lf } else { super::Ending::NoLf } };
    let format = super::Line::bare(request::Head::Capability(super::Capability { key: b"object-format", value: Some(b"sha1") }));
    let head = if command_first { vec![command, format] } else { vec![format, command] };
    let written = request::Request::Command(request::CommandRequest {
      head,
      delimited: true,
      arguments: request::Arguments::Fetch(arguments),
    });
    let mut wire = Vec::new();
    request::encode(&written, &mut wire).unwrap();
    prop_assert_eq!(request::decode(&wire, &mut room()), Ok(written));
  }

  /// A-582 part 2, hostile input: do decode arbitrary streams of packets made of the grammar's words and of any bytes
  /// as every message. Expect: no panic; a refusal names a place inside the stream; a stream accepted encodes back to
  /// exactly its bytes.
  #[test]
  fn any_stream_is_refused_by_kind_or_read_back_exactly(chosen in proptest::collection::vec(any_packet(), 0..12)) {
    let packets: Vec<Packet<'_>> = chosen
      .iter()
      .map(|(kind, data)| match kind {
        0 => Packet::Data(data),
        1 => Packet::Flush,
        2 => Packet::Delim,
        _ => Packet::ResponseEnd,
      })
      .collect();
    let stream = wire(&packets);
    for message in [
      Message::Advertisement,
      Message::Request,
      Message::LsRefsAnswer,
      Message::FetchResponse(false),
      Message::FetchResponse(true),
    ] {
      match decode_then_encode(message, &stream) {
        Ok(written) => prop_assert_eq!(&written, &stream),
        Err(refusal) => {
          let at = place(&refusal);
          prop_assert!(at.is_some_and(|at| at <= to_u64(stream.len())), "{refusal:?}");
        }
      }
    }
  }
}
