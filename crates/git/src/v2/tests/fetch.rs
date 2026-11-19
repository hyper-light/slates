use super::{Event, PackfileUri, Response, Section, WantedRef, decode_response, encode_response};
use crate::fixtures;
use crate::object_name::{HexRefusal, ObjectFormat, ObjectId};
use crate::pktline::Packet;
use crate::sideband::{self, MAX_BAND_DATA};
use crate::v2::tests::{room, sha1, wire};
use crate::v2::{Ending, Line, Refusal};
use crate::{Admission, PastAdmission, to_u64};

/// The id of `main` in the golden repository.
const MAIN: &str = "d0f175d3390380c935e9b56e0cebb715995d838d";
/// The id of `side`.
const SIDE: &str = "de204ecf4cbcbf9dec27d422e67f087c136709db";

/// `events` without their pack and progress bytes: the response's shape.
fn shape<'a>(events: &[Event<'a>]) -> Vec<Event<'a>> {
  events
    .iter()
    .filter(|event| !matches!(event, Event::Pack(_) | Event::Progress(_)))
    .copied()
    .collect()
}

/// The pack `events` carry, joined.
fn pack(events: &[Event<'_>]) -> Vec<u8> {
  events
    .iter()
    .filter_map(|event| match event {
      Event::Pack(bytes) => Some(*bytes),
      _ => None,
    })
    .flatten()
    .copied()
    .collect()
}

/// `section`'s header on a line ending with an LF.
fn header(section: Section) -> Event<'static> {
  Event::Header(Line::lf(section))
}

/// A-582 part 2, golden vectors: do decode each `fetch` response stock git 2.50.1 wrote, then encode it. Expect: each
/// section in order with its lines (git's `shallow` line without its LF), a pack beginning `PACK` version 2, progress on
/// band 2 where it was left on, every packet of the `sideband-all` response on a band, and each stream's own bytes back.
#[test]
fn stock_gits_responses_decode_to_their_sections_and_encode_back() {
  let main = sha1(MAIN);
  let packfile = header(Section::Packfile);
  let cases: [(&[u8], bool, Vec<Event<'_>>); 6] = [
    (
      fixtures::FETCH_CLONE_RESPONSE,
      false,
      vec![packfile, Event::Flush],
    ),
    (
      fixtures::FETCH_HAVES_RESPONSE,
      false,
      vec![
        header(Section::Acknowledgments),
        Event::Ack(Line::lf(sha1(SIDE))),
        Event::Ready(Ending::Lf),
        Event::Delim,
        packfile,
        Event::Flush,
      ],
    ),
    (
      fixtures::FETCH_SHALLOW_RESPONSE,
      false,
      vec![
        header(Section::ShallowInfo),
        Event::Shallow(Line::bare(main)),
        Event::Delim,
        packfile,
        Event::Flush,
      ],
    ),
    (
      fixtures::FETCH_REF_IN_WANT_RESPONSE,
      false,
      vec![
        header(Section::WantedRefs),
        Event::WantedRef(Line::lf(WantedRef {
          id: sha1(SIDE),
          name: b"refs/heads/side",
        })),
        Event::Delim,
        packfile,
        Event::Flush,
      ],
    ),
    (
      fixtures::FETCH_PROGRESS_RESPONSE,
      false,
      vec![packfile, Event::Flush],
    ),
    (
      fixtures::FETCH_URIS_RESPONSE,
      true,
      vec![
        header(Section::PackfileUris),
        Event::PackfileUri(Line::lf(PackfileUri {
          hash: sha1("63f6afec3b8d0c61c53e79c8e24b6a83567e93ac"),
          uri: b"https://example.invalid/63f6afec3b8d0c61c53e79c8e24b6a83567e93ac.pack",
        })),
        Event::Delim,
        packfile,
        Event::Flush,
      ],
    ),
  ];
  for (stream, multiplexed, expected) in cases {
    let events = decode_response(stream, ObjectFormat::Sha1, multiplexed, &mut room()).unwrap();
    assert_eq!(shape(&events), expected);
    assert!(pack(&events).starts_with(b"PACK\0\0\0\x02"));
    let mut out = Vec::new();
    encode_response(&events, ObjectFormat::Sha1, multiplexed, &mut out).unwrap();
    assert_eq!(out, stream);
  }
  let progress = decode_response(
    fixtures::FETCH_PROGRESS_RESPONSE,
    ObjectFormat::Sha1,
    false,
    &mut room(),
  )
  .unwrap();
  assert!(
    progress
      .iter()
      .filter(|event| matches!(event, Event::Progress(_)))
      .count()
      >= 2
  );
}

/// A-582 part 2 (gitprotocol-v2 "fetch" `output`; git `191740edaa4a`): do decode a response with `wanted-refs` before
/// `shallow-info`, as git 2.50.1's `upload_pack_v2` wrote one; a section twice; acknowledgments after shallow-info; a
/// header where a section's line must stand; and write git 2.50.1's order event by event. Expect: `SectionOutOfOrder`
/// at the late header each time it is read; a header inside a section refused as a line of it; and the writer refusing
/// the late header with nothing written, its response still where it stood.
#[test]
fn sections_out_of_order_are_refused_reading_and_writing() {
  let wanted = format!("{SIDE} refs/heads/side\n");
  let shallow = format!("shallow {MAIN}\n");
  let git_2_50_1 = wire(&[
    Packet::Data(b"wanted-refs\n"),
    Packet::Data(wanted.as_bytes()),
    Packet::Delim,
    Packet::Data(b"shallow-info\n"),
    Packet::Data(shallow.as_bytes()),
    Packet::Delim,
    Packet::Data(b"packfile\n"),
    Packet::Flush,
  ]);
  assert_eq!(
    decode_response(&git_2_50_1, ObjectFormat::Sha1, false, &mut room()),
    Err(Refusal::SectionOutOfOrder { at: 81 })
  );
  let twice = wire(&[
    Packet::Data(b"shallow-info\n"),
    Packet::Delim,
    Packet::Data(b"shallow-info\n"),
    Packet::Flush,
  ]);
  assert_eq!(
    decode_response(&twice, ObjectFormat::Sha1, false, &mut room()),
    Err(Refusal::SectionOutOfOrder { at: 21 })
  );
  let late = wire(&[
    Packet::Data(b"shallow-info\n"),
    Packet::Delim,
    Packet::Data(b"acknowledgments\n"),
    Packet::Flush,
  ]);
  assert_eq!(
    decode_response(&late, ObjectFormat::Sha1, false, &mut room()),
    Err(Refusal::SectionOutOfOrder { at: 21 })
  );
  let undelimited = wire(&[
    Packet::Data(b"shallow-info\n"),
    Packet::Data(b"packfile\n"),
    Packet::Flush,
  ]);
  assert_eq!(
    decode_response(&undelimited, ObjectFormat::Sha1, false, &mut room()),
    Err(Refusal::Line { at: 17 })
  );

  let mut response = Response::new(ObjectFormat::Sha1, false);
  let mut out = Vec::new();
  let mut at = 0u64;
  for event in [
    header(Section::WantedRefs),
    Event::WantedRef(Line::lf(WantedRef {
      id: sha1(SIDE),
      name: b"refs/heads/side",
    })),
    Event::Delim,
  ] {
    at += response.write(&event, at, &mut out).unwrap();
  }
  let before = out.clone();
  assert_eq!(
    response.write(&header(Section::ShallowInfo), at, &mut out),
    Err(Refusal::SectionOutOfOrder { at: 81 })
  );
  assert_eq!(out, before, "nothing written");
  at += response
    .write(&header(Section::Packfile), at, &mut out)
    .unwrap();
  response.write(&Event::Flush, at, &mut out).unwrap();
  assert!(response.finished());
}

/// A-582 part 2 (gitprotocol-v2 "acknowledgments section"; `fetch-pack.c` `process_ack`): do decode acknowledgments
/// out of their grammar, and the two shapes in it that end the response there. Expect: `NAK` with `ACK` either way,
/// a second `NAK`, a line after `ready`, a flush after `ready`, a delimiter without it, and a malformed `ACK` refused;
/// `ACK`s or `NAK` without `ready` then a flush read as a whole response.
#[test]
fn an_acknowledgments_section_out_of_its_grammar_is_refused() {
  let ack = format!("ACK {MAIN}\n");
  let ack = Packet::Data(ack.as_bytes());
  let head = Packet::Data(b"acknowledgments\n");
  let nak = Packet::Data(b"NAK\n");
  let ready = Packet::Data(b"ready\n");
  let cases = [
    (
      wire(&[head, nak, ack, Packet::Flush]),
      Refusal::Acknowledgment { at: 28 },
    ),
    (
      wire(&[head, ack, nak, Packet::Flush]),
      Refusal::Acknowledgment { at: 69 },
    ),
    (
      wire(&[head, nak, nak, Packet::Flush]),
      Refusal::Acknowledgment { at: 28 },
    ),
    (
      wire(&[head, ready, ack, Packet::Flush]),
      Refusal::Acknowledgment { at: 30 },
    ),
    (
      wire(&[head, ready, Packet::Flush]),
      Refusal::Acknowledgment { at: 30 },
    ),
    (
      wire(&[head, ack, Packet::Delim]),
      Refusal::Acknowledgment { at: 69 },
    ),
    (
      wire(&[head, Packet::Data(b"ACK\n"), Packet::Flush]),
      Refusal::Acknowledgment { at: 20 },
    ),
    (
      wire(&[head, Packet::Data(b"common\n"), Packet::Flush]),
      Refusal::Acknowledgment { at: 20 },
    ),
    (
      wire(&[head, Packet::Data(b"ACK 123\n"), Packet::Flush]),
      Refusal::ObjectId {
        at: 20,
        refusal: HexRefusal::Length { len: 3 },
      },
    ),
  ];
  for (stream, refusal) in cases {
    assert_eq!(
      decode_response(&stream, ObjectFormat::Sha1, false, &mut room()),
      Err(refusal),
      "{stream:?}"
    );
  }
  let acks = wire(&[head, ack, ack, Packet::Flush]);
  assert_eq!(
    decode_response(&acks, ObjectFormat::Sha1, false, &mut room()),
    Ok(vec![
      header(Section::Acknowledgments),
      Event::Ack(Line::lf(sha1(MAIN))),
      Event::Ack(Line::lf(sha1(MAIN))),
      Event::Flush
    ])
  );
  let naked = wire(&[head, nak, Packet::Flush]);
  assert!(decode_response(&naked, ObjectFormat::Sha1, false, &mut room()).is_ok());
}

/// A-582 part 2: do decode responses that end without their pack, a response end packet, and no bytes. Expect:
/// `NoPackfile` at the flush wherever a pack was still due, `Misplaced` for the response end, `Unfinished` for nothing.
#[test]
fn a_response_without_its_pack_is_refused() {
  let shallow = format!("shallow {MAIN}\n");
  let cases = [
    (
      wire(&[
        Packet::Data(b"shallow-info\n"),
        Packet::Data(shallow.as_bytes()),
        Packet::Flush,
      ]),
      Refusal::NoPackfile { at: 70 },
    ),
    (
      wire(&[
        Packet::Data(b"shallow-info\n"),
        Packet::Delim,
        Packet::Flush,
      ]),
      Refusal::NoPackfile { at: 21 },
    ),
    (wire(&[Packet::Flush]), Refusal::NoPackfile { at: 0 }),
    (wire(&[Packet::Delim]), Refusal::Misplaced { at: 0 }),
    (
      wire(&[Packet::Data(b"packfile\n"), Packet::ResponseEnd]),
      Refusal::Misplaced { at: 13 },
    ),
    (wire(&[]), Refusal::Unfinished { at: 0 }),
  ];
  for (stream, refusal) in cases {
    assert_eq!(
      decode_response(&stream, ObjectFormat::Sha1, false, &mut room()),
      Err(refusal),
      "{stream:?}"
    );
  }
}

/// A-582 part 2 (gitprotocol-v2 `sideband-all` and "packfile section"; `sideband.c`): do decode multiplexed responses
/// with progress before the first section, between lines and after a delimiter; a fatal error and a packet after it;
/// an empty band-1 packet where a section's name must stand; bands 0 and 4 and no band in the packfile; and progress
/// outside the packfile of a response that is not multiplexed. Expect: progress read anywhere it may come and written
/// back; the error ending the response and anything after it `AfterError`; the empty line `UnknownSection`, as git's
/// own client would refuse it; each bad band `Band`; the unmultiplexed progress refused, read and written.
#[test]
fn bands_are_read_where_the_response_carries_them() {
  let banded = |text: &[u8]| {
    let mut data = vec![1u8];
    data.extend_from_slice(text);
    data
  };
  let shallow = banded(format!("shallow {MAIN}\n").as_bytes());
  let header_line = banded(b"shallow-info\n");
  let packfile = banded(b"packfile\n");
  let progressive = wire(&[
    Packet::Data(b"\x02counting\r"),
    Packet::Data(&header_line),
    Packet::Data(b"\x02"),
    Packet::Data(&shallow),
    Packet::Delim,
    Packet::Data(b"\x02still here\n"),
    Packet::Data(&packfile),
    Packet::Data(b"\x01PACK"),
    Packet::Flush,
  ]);
  let events = decode_response(&progressive, ObjectFormat::Sha1, true, &mut room()).unwrap();
  assert_eq!(
    events
      .iter()
      .filter(|event| matches!(event, Event::Progress(_)))
      .count(),
    3
  );
  let mut out = Vec::new();
  encode_response(&events, ObjectFormat::Sha1, true, &mut out).unwrap();
  assert_eq!(out, progressive);

  let fatal = wire(&[Packet::Data(b"\x03upload-pack: not our ref")]);
  let events = decode_response(&fatal, ObjectFormat::Sha1, true, &mut room()).unwrap();
  assert_eq!(events, [Event::Error(b"upload-pack: not our ref")]);
  let after = wire(&[Packet::Data(b"\x03boom"), Packet::Flush]);
  assert_eq!(
    decode_response(&after, ObjectFormat::Sha1, true, &mut room()),
    Err(Refusal::AfterError { at: 9 })
  );
  let keepalive = wire(&[
    Packet::Data(b"\x01"),
    Packet::Data(&packfile),
    Packet::Flush,
  ]);
  assert_eq!(
    decode_response(&keepalive, ObjectFormat::Sha1, true, &mut room()),
    Err(Refusal::UnknownSection { at: 0 })
  );
  for (data, refusal) in [
    (&b"\x00PACK"[..], sideband::Refusal::UnknownBand { byte: 0 }),
    (b"\x04PACK", sideband::Refusal::UnknownBand { byte: 4 }),
    (b"", sideband::Refusal::NoBand),
  ] {
    let stream = wire(&[
      Packet::Data(b"packfile\n"),
      Packet::Data(data),
      Packet::Flush,
    ]);
    assert_eq!(
      decode_response(&stream, ObjectFormat::Sha1, false, &mut room()),
      Err(Refusal::Band { at: 13, refusal })
    );
  }
  let plain = wire(&[
    Packet::Data(b"\x02counting\n"),
    Packet::Data(b"packfile\n"),
    Packet::Flush,
  ]);
  assert_eq!(
    decode_response(&plain, ObjectFormat::Sha1, false, &mut room()),
    Err(Refusal::UnknownSection { at: 0 })
  );
  let mut out = Vec::new();
  assert_eq!(
    encode_response(
      &[
        Event::Progress(b"counting"),
        header(Section::Packfile),
        Event::Flush
      ],
      ObjectFormat::Sha1,
      false,
      &mut out
    ),
    Err(Refusal::Misplaced { at: 0 })
  );
  assert!(out.is_empty());
}

/// A-582 part 2: do write responses a server must not send, one fault each: pack bytes past a band packet, a wanted
/// ref that is no ref name, an id of the other format, a URI holding a control byte, pack bytes outside the packfile,
/// and events that stop before the response ends. Expect: each refused by its kind at its place, nothing written.
#[test]
fn a_refused_write_writes_nothing() {
  let too_long = vec![0u8; MAX_BAND_DATA + 1];
  let sha256 = ObjectId::from_hex(ObjectFormat::Sha256, "d".repeat(64).as_bytes()).unwrap();
  let packfile = header(Section::Packfile);
  let cases = [
    (
      vec![packfile, Event::Pack(&too_long), Event::Flush],
      Refusal::TooLong {
        at: 13,
        len: to_u64(MAX_BAND_DATA + 2),
      },
    ),
    (
      vec![
        header(Section::WantedRefs),
        Event::WantedRef(Line::lf(WantedRef {
          id: sha1(SIDE),
          name: b"side",
        })),
      ],
      Refusal::Refname { at: 16 },
    ),
    (
      vec![
        header(Section::Acknowledgments),
        Event::Ack(Line::lf(sha256)),
      ],
      Refusal::ObjectId {
        at: 20,
        refusal: HexRefusal::Length { len: 64 },
      },
    ),
    (
      vec![
        header(Section::PackfileUris),
        Event::PackfileUri(Line::lf(PackfileUri {
          hash: sha1(MAIN),
          uri: b"https://x/\x07",
        })),
      ],
      Refusal::Line { at: 18 },
    ),
    (
      vec![header(Section::ShallowInfo), Event::Pack(b"PACK")],
      Refusal::Misplaced { at: 17 },
    ),
    (
      vec![packfile, Event::Pack(b"PACK")],
      Refusal::Unfinished { at: 22 },
    ),
    (
      vec![Event::Shallow(Line::lf(sha1(MAIN)))],
      Refusal::Line { at: 0 },
    ),
  ];
  for (events, refusal) in cases {
    let mut out = Vec::new();
    assert_eq!(
      encode_response(&events, ObjectFormat::Sha1, false, &mut out),
      Err(refusal),
      "{events:?}"
    );
    assert!(out.is_empty(), "{events:?}");
  }
}

/// A-582 part 2 (§4.2): do decode stock git's clone response with one byte less room than its events take, then with
/// exactly the room. Expect: `PastAdmission` with nothing charged, then the response and the room spent.
#[test]
fn a_decode_past_its_admission_is_refused_before_its_list_is_made() {
  let count = decode_response(
    fixtures::FETCH_CLONE_RESPONSE,
    ObjectFormat::Sha1,
    false,
    &mut room(),
  )
  .unwrap()
  .len();
  let needed = to_u64(count * core::mem::size_of::<Event<'_>>());
  let mut short = Admission::new(needed - 1);
  assert_eq!(
    decode_response(
      fixtures::FETCH_CLONE_RESPONSE,
      ObjectFormat::Sha1,
      false,
      &mut short
    ),
    Err(Refusal::PastAdmission(PastAdmission {
      asked: needed,
      left: needed - 1
    }))
  );
  assert_eq!(short.left(), needed - 1);
  let mut exact = Admission::new(needed);
  assert!(
    decode_response(
      fixtures::FETCH_CLONE_RESPONSE,
      ObjectFormat::Sha1,
      false,
      &mut exact
    )
    .is_ok()
  );
  assert_eq!(exact.left(), 0);
}
