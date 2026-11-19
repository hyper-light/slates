use proptest::prelude::*;

use super::{DataTooLong, HEADER_LEN, MAX_DATA, MAX_LEN, Packet, Refusal, decode, read, write};
use crate::fixtures::STREAMS;
use crate::out::Measure;
use crate::{Admission, PastAdmission, to_u64};

/// The room a test's decode of `bytes` is given: one packet for each byte, which no stream can exceed, since each
/// packet takes four bytes at least.
fn room(bytes: &[u8]) -> Admission {
  Admission::new(to_u64(
    bytes
      .len()
      .saturating_mul(core::mem::size_of::<Packet<'_>>()),
  ))
}

/// `packets` written one after another.
fn encode(packets: &[Packet<'_>]) -> Vec<u8> {
  let mut out = Vec::new();
  for packet in packets {
    write(*packet, &mut out).unwrap();
  }
  out
}

/// A-582, golden vectors from git's own document (gitprotocol-common "pkt-line Format": `"0006a\n"` is `"a\n"`,
/// `"0005a"` is `"a"`, `"000bfoobar\n"` is `"foobar\n"`, `"0004"` is `""`). Do: decode each and encode its data back.
/// Expect: each packet the document names, and its own bytes back.
#[test]
fn the_documents_own_examples_decode_and_encode_as_written() {
  let examples: [(&[u8], &[u8]); 4] = [
    (b"0006a\n", b"a\n"),
    (b"0005a", b"a"),
    (b"000bfoobar\n", b"foobar\n"),
    (b"0004", b""),
  ];
  for (wire, data) in examples {
    assert_eq!(
      decode(wire, &mut room(wire)),
      Ok(vec![Packet::Data(data)]),
      "{wire:?}"
    );
    assert_eq!(encode(&[Packet::Data(data)]), wire);
  }
  assert_eq!(
    encode(&[Packet::Flush, Packet::Delim, Packet::ResponseEnd]),
    b"000000010002"
  );
}

/// A-582, golden vectors from stock git 2.50.1. Do: decode every stream it wrote or read, then encode the packets back.
/// Expect: each stream's own bytes, the advertisement opening `version 2` and every message ending in a flush, the
/// requests' and the multi-section answers' delimiters where protocol v2 puts them.
#[test]
fn every_stream_stock_git_wrote_decodes_and_encodes_back_to_its_bytes() {
  for (name, stream) in STREAMS {
    let packets =
      decode(stream, &mut room(stream)).unwrap_or_else(|refusal| panic!("{name}: {refusal:?}"));
    assert_eq!(encode(&packets), stream, "{name}");
    assert_eq!(
      packets.last(),
      Some(&Packet::Flush),
      "{name} ends its message"
    );
    let delimiters = packets
      .iter()
      .filter(|packet| **packet == Packet::Delim)
      .count();
    // A request's command and its arguments, and an answer's sections before its packfile, are delimited; an
    // advertisement, an `ls-refs` answer and an answer that is the packfile section alone hold no delimiter.
    let expected = match name {
      "advertisement"
      | "advertisement-uris"
      | "ls-refs response"
      | "fetch-clone response"
      | "fetch-progress response" => 0,
      _ => 1,
    };
    assert_eq!(delimiters, expected, "{name}'s delimiters");
  }
  let advertisement = decode(STREAMS[0].1, &mut room(STREAMS[0].1)).unwrap();
  assert_eq!(advertisement.first(), Some(&Packet::Data(b"version 2\n")));
}

/// A-582 hostile input: do decode every cut of every stream stock git wrote, as a whole stream and as a reader would
/// read the bytes so far. Expect: a cut on a packet's boundary decodes to that many of the stream's packets, every other
/// cut is `Truncated` and never another refusal; the reader says `None` (not yet whole) exactly where the decode says
/// `Truncated`, and never misreads a packet early.
#[test]
fn every_cut_of_every_stream_is_whole_or_truncated_and_a_reader_waits_for_the_rest() {
  for (name, stream) in STREAMS {
    let packets = decode(stream, &mut room(stream)).unwrap();
    let mut boundaries = vec![0usize];
    let mut at = 0usize;
    while let Some((_, took)) = read(&stream[at..], to_u64(at)).unwrap() {
      at += took;
      boundaries.push(at);
      if at == stream.len() {
        break;
      }
    }
    assert_eq!(boundaries.len(), packets.len() + 1, "{name}");
    for cut in 0..=stream.len() {
      let prefix = &stream[..cut];
      match decode(prefix, &mut room(prefix)) {
        Ok(whole) => {
          let index = boundaries.iter().position(|boundary| *boundary == cut);
          assert_eq!(
            index.map(|count| &packets[..count]),
            Some(&whole[..]),
            "{name} cut at {cut}"
          );
        }
        Err(Refusal::Truncated { at, left, needed }) => {
          assert!(
            !boundaries.contains(&cut),
            "{name} cut at {cut} is a boundary"
          );
          let start = usize::try_from(at).unwrap();
          assert!(
            boundaries.contains(&start),
            "{name} truncated at {at}, not a packet's start"
          );
          assert!(
            left < needed,
            "{name} cut at {cut}: {left} left of {needed}"
          );
          assert_eq!(read(&prefix[start..], at), Ok(None), "{name} cut at {cut}");
        }
        Err(other) => panic!("{name} cut at {cut}: {other:?}"),
      }
    }
  }
}

/// A-582 hostile input: do flip every bit of every byte of every stream stock git wrote and decode each. Expect: no
/// panic; every refusal one of the typed kinds, at a place inside the stream; and every flip in a length's digits read
/// as git reads it: a digit flipped to a non-digit refused `NotHex` there, and one flipped to another digit read as
/// that length or refused by it, never ignored.
#[test]
fn every_bit_flip_of_every_stream_is_refused_by_kind_or_read_as_its_length_says() {
  let mut refused = [0u32; 4];
  for (name, stream) in STREAMS {
    for index in 0..stream.len() {
      for bit in 0..8 {
        let mut flipped = stream.to_vec();
        flipped[index] ^= 1 << bit;
        match decode(&flipped, &mut room(&flipped)) {
          Ok(packets) => assert_eq!(
            encode(&packets).len(),
            flipped.len(),
            "{name} {index}.{bit}"
          ),
          Err(Refusal::NotHex { at }) => {
            assert!(usize::try_from(at).unwrap() < flipped.len());
            refused[0] += 1;
          }
          Err(Refusal::ShorterThanHeader { .. }) => refused[1] += 1,
          Err(Refusal::TooLong { len, .. }) => {
            assert!(len > to_u64(MAX_LEN));
            refused[2] += 1;
          }
          Err(Refusal::Truncated { left, needed, .. }) => {
            assert!(left < needed);
            refused[3] += 1;
          }
          Err(Refusal::PastAdmission(past)) => {
            panic!("{name} {index}.{bit}: {past:?} past a room no stream exceeds")
          }
        }
      }
    }
  }
  assert!(
    refused[0] > 0 && refused[3] > 0,
    "the flips reached the refusals: {refused:?}"
  );
}

/// A-582 hostile input: lengths at and past the caps, and digits that are none. Do: decode `fff0` with its 65516 bytes,
/// `fff1`, `ffff`, `0003`, a non-digit in each place, upper-case digits, and a header cut short before a non-digit; and
/// write data one byte past the most a packet carries. Expect: the longest packet read whole; `TooLong` naming each
/// length past 65520; `ShorterThanHeader` for `0003`; `NotHex` at the digit's own place, even before the four digits have
/// all arrived; upper case read as git's `hexval` reads it; and the long write refused with nothing written.
#[test]
fn lengths_at_and_past_the_caps_and_digits_that_are_none_are_refused_by_kind() {
  let mut longest = b"fff0".to_vec();
  longest.extend(std::iter::repeat_n(b'x', MAX_DATA));
  assert_eq!(
    read(&longest, 0),
    Ok(Some((Packet::Data(&longest[HEADER_LEN..]), MAX_LEN)))
  );
  for (header, len) in [(b"fff1", 65521u64), (b"ffff", 65535)] {
    assert_eq!(read(header, 7), Err(Refusal::TooLong { at: 7, len }));
  }
  assert_eq!(read(b"0003", 9), Err(Refusal::ShorterThanHeader { at: 9 }));
  for place in 0..HEADER_LEN {
    let mut header = *b"0010";
    header[place] = b'g';
    assert_eq!(
      read(&header, 100),
      Err(Refusal::NotHex {
        at: 100 + to_u64(place)
      })
    );
  }
  assert_eq!(read(b"0z", 0), Err(Refusal::NotHex { at: 1 }));
  assert_eq!(read(b"00", 0), Ok(None));
  assert_eq!(
    read(b"000A123456", 0),
    Ok(Some((Packet::Data(b"123456"), 10)))
  );
  let too_long = vec![b'y'; MAX_DATA + 1];
  let mut out = Vec::new();
  assert_eq!(
    write(Packet::Data(&too_long), &mut out),
    Err(DataTooLong {
      len: to_u64(MAX_DATA + 1)
    })
  );
  assert!(out.is_empty(), "nothing written");
}

/// A-582 (§4.2): do decode a stream stock git wrote with one packet less room than its list takes. Expect:
/// `PastAdmission`, the admission charged nothing; and with exactly the room, the stream decoded and the room spent.
#[test]
fn a_decode_past_its_admission_is_refused_before_its_list_is_made() {
  let (_, stream) = STREAMS[3];
  let count = decode(stream, &mut room(stream)).unwrap().len();
  let needed = to_u64(count * core::mem::size_of::<Packet<'_>>());
  let mut short = Admission::new(needed - 1);
  assert_eq!(
    decode(stream, &mut short),
    Err(Refusal::PastAdmission(PastAdmission {
      asked: needed,
      left: needed - 1
    }))
  );
  assert_eq!(short.left(), needed - 1);
  let mut exact = Admission::new(needed);
  assert_eq!(
    decode(stream, &mut exact).map(|packets| packets.len()),
    Ok(count)
  );
  assert_eq!(exact.left(), 0);
}

/// A-582: do read every stream stock git wrote one byte at a time, as a server reads a request while it arrives,
/// asking after each byte. Expect: the same packets a whole decode finds, each the moment its last byte arrives.
#[test]
fn a_reader_fed_a_byte_at_a_time_finds_each_packet_as_its_last_byte_arrives() {
  for (name, stream) in STREAMS {
    let whole = decode(stream, &mut room(stream)).unwrap();
    let mut found = Vec::new();
    let mut start = 0usize;
    for end in 1..=stream.len() {
      if let Some((packet, took)) = read(&stream[start..end], to_u64(start)).unwrap() {
        assert_eq!(start + took, end, "{name}: found before its last byte");
        found.push(packet);
        start = end;
      }
    }
    assert_eq!(found, whole, "{name}");
  }
}

/// A-582: do measure what writing every stream's packets takes, then write them. Expect: the measure equal to the bytes
/// written, so a caller can charge an encode's exact size before it makes it.
#[test]
fn a_measured_encode_takes_exactly_what_it_writes() {
  for (name, stream) in STREAMS {
    let packets = decode(stream, &mut room(stream)).unwrap();
    let mut measure = Measure::default();
    for packet in &packets {
      write(*packet, &mut measure).unwrap();
    }
    assert_eq!(measure.0, to_u64(stream.len()), "{name}");
  }
}

/// A packet for the round trip: data of any length up to the cap (most short, some at it), or a special packet.
fn any_packet() -> impl Strategy<Value = (u8, Vec<u8>)> {
  prop_oneof![
    (Just(0u8), proptest::collection::vec(any::<u8>(), 0..300)),
    (
      Just(0u8),
      proptest::collection::vec(any::<u8>(), MAX_DATA - 2..=MAX_DATA)
    ),
    (1u8..4, Just(Vec::new())),
  ]
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-582: do write arbitrary packets, data up to the cap and every special packet, and decode what was written.
  /// Expect: the same packets back.
  #[test]
  fn any_packets_written_decode_back_to_themselves(chosen in proptest::collection::vec(any_packet(), 0..12)) {
    let packets: Vec<Packet<'_>> = chosen
      .iter()
      .map(|(kind, data)| match kind {
        0 => Packet::Data(data),
        1 => Packet::Flush,
        2 => Packet::Delim,
        _ => Packet::ResponseEnd,
      })
      .collect();
    let wire = encode(&packets);
    prop_assert_eq!(decode(&wire, &mut room(&wire)), Ok(packets));
  }
}
