use super::{Advertisement, decode, encode};
use crate::fixtures::{ADVERTISEMENT, ADVERTISEMENT_URIS};
use crate::object_name::ObjectFormat;
use crate::pktline::{MAX_DATA, Packet};
use crate::v2::tests::{room, wire};
use crate::v2::{Capability, Ending, Line, Refusal};
use crate::{Admission, PastAdmission, to_u64};

/// The keys and values of `advertisement`'s capabilities, in order.
fn listed<'a>(advertisement: &Advertisement<'a>) -> Vec<(&'a [u8], Option<&'a [u8]>)> {
  advertisement
    .capabilities
    .iter()
    .map(|line| (line.value.key, line.value.value))
    .collect()
}

/// A-582 part 2, golden vectors: do decode the two advertisements stock git 2.50.1 wrote, plain and allowing
/// `sideband-all` and `packfile-uris`, then encode them. Expect: `version 2` and each capability as git listed it, every
/// line ending with an LF, the lookups answering as git's client does, and each stream's own bytes back.
#[test]
fn stock_gits_advertisements_decode_to_their_capabilities_and_encode_back() {
  let plain = decode(ADVERTISEMENT, &mut room()).unwrap();
  assert_eq!(plain.version, Ending::Lf);
  let expected: [(&[u8], Option<&[u8]>); 5] = [
    (b"agent", Some(b"git/2.50.1-Darwin")),
    (b"ls-refs", Some(b"unborn")),
    (b"fetch", Some(b"shallow wait-for-done ref-in-want")),
    (b"server-option", None),
    (b"object-format", Some(b"sha1")),
  ];
  assert_eq!(listed(&plain), expected);
  assert!(
    plain
      .capabilities
      .iter()
      .all(|line| line.ending == Ending::Lf)
  );
  assert!(plain.supports(b"fetch", b"shallow"));
  assert!(plain.supports(b"fetch", b"ref-in-want"));
  assert!(!plain.supports(b"fetch", b"sideband-all"));
  assert!(!plain.supports(b"fetch", b"shallow wait"));
  assert!(plain.supports(b"ls-refs", b"unborn"));
  assert_eq!(plain.get(b"server-option"), Some(None));
  assert_eq!(plain.get(b"object-info"), None);
  assert_eq!(plain.object_format(), Some(ObjectFormat::Sha1));
  let mut out = Vec::new();
  encode(&plain, &mut out).unwrap();
  assert_eq!(out, ADVERTISEMENT);

  let uris = decode(ADVERTISEMENT_URIS, &mut room()).unwrap();
  assert_eq!(
    uris.get(b"fetch"),
    Some(Some(
      &b"shallow wait-for-done ref-in-want sideband-all packfile-uris"[..]
    ))
  );
  assert!(uris.supports(b"fetch", b"sideband-all") && uris.supports(b"fetch", b"packfile-uris"));
  let mut out = Vec::new();
  encode(&uris, &mut out).unwrap();
  assert_eq!(out, ADVERTISEMENT_URIS);
}

/// A-582 part 2 (§4.17.2's list of v2 capabilities): do encode the advertisement slates makes for a SHA-256 repository,
/// every capability the design names, one line without its LF, and decode it. Expect: the same advertisement back, its
/// object format SHA-256 and each feature found.
#[test]
fn slates_own_advertisement_encodes_and_decodes_back() {
  let capability =
    |key: &'static [u8], value: Option<&'static [u8]>| Line::lf(Capability { key, value });
  let mut capabilities = vec![
    capability(b"agent", Some(b"slates/0.1-Linux")),
    capability(b"ls-refs", Some(b"unborn")),
    capability(
      b"fetch",
      Some(b"shallow wait-for-done filter ref-in-want sideband-all packfile-uris"),
    ),
    capability(b"server-option", None),
    capability(b"object-format", Some(b"sha256")),
    capability(b"session-id", Some(b"20261119T000000.000000Z-H1-P2")),
  ];
  capabilities[3].ending = Ending::NoLf;
  let ours = Advertisement {
    version: Ending::Lf,
    capabilities,
  };
  let mut out = Vec::new();
  encode(&ours, &mut out).unwrap();
  let read = decode(&out, &mut room()).unwrap();
  assert_eq!(read, ours);
  assert_eq!(read.object_format(), Some(ObjectFormat::Sha256));
  assert!(read.supports(b"fetch", b"filter"));
}

/// A-582 part 2 (gitprotocol-v2 "Capability Advertisement"): do decode advertisements out of the grammar, one fault
/// each. Expect: each refused by its kind, at the packet at fault; a key no client knows read, since clients ignore
/// those; and an `object-format` git does not know read, its format `None`.
#[test]
fn an_advertisement_out_of_its_grammar_is_refused_by_kind() {
  let version = Packet::Data(b"version 2\n");
  let agent = Packet::Data(b"agent=git/2.50.1\n");
  let cases: [(Vec<u8>, Refusal); 13] = [
    (
      wire(&[Packet::Data(b"version 1\n"), Packet::Flush]),
      Refusal::Version { at: 0 },
    ),
    (wire(&[Packet::Flush]), Refusal::Version { at: 0 }),
    (wire(&[version, agent]), Refusal::Unfinished { at: 35 }),
    (
      wire(&[version, Packet::Delim, Packet::Flush]),
      Refusal::Misplaced { at: 14 },
    ),
    (
      wire(&[version, Packet::ResponseEnd]),
      Refusal::Misplaced { at: 14 },
    ),
    (
      wire(&[version, Packet::Flush, Packet::Flush]),
      Refusal::Trailing { at: 18 },
    ),
    (
      wire(&[version, Packet::Data(b""), Packet::Flush]),
      Refusal::Capability { at: 14 },
    ),
    (
      wire(&[version, Packet::Data(b"\n"), Packet::Flush]),
      Refusal::Capability { at: 14 },
    ),
    (
      wire(&[version, Packet::Data(b"=x\n"), Packet::Flush]),
      Refusal::Capability { at: 14 },
    ),
    (
      wire(&[version, Packet::Data(b"ke y=v\n"), Packet::Flush]),
      Refusal::Capability { at: 14 },
    ),
    (
      wire(&[version, Packet::Data(b"agent=a\0b\n"), Packet::Flush]),
      Refusal::NotText { at: 14 },
    ),
    (
      wire(&[version, Packet::Data(b"agent=a\nb\n"), Packet::Flush]),
      Refusal::NotText { at: 14 },
    ),
    (wire(&[]), Refusal::Unfinished { at: 0 }),
  ];
  for (stream, refusal) in cases {
    assert_eq!(decode(&stream, &mut room()), Err(refusal), "{stream:?}");
  }
  let unknown = wire(&[version, Packet::Data(b"frobnicate=yes\n"), Packet::Flush]);
  assert_eq!(
    decode(&unknown, &mut room()).unwrap().get(b"frobnicate"),
    Some(Some(&b"yes"[..]))
  );
  let md5 = wire(&[version, Packet::Data(b"object-format=md5\n"), Packet::Flush]);
  assert_eq!(decode(&md5, &mut room()).unwrap().object_format(), None);
}

/// A-582 part 2: do encode an advertisement holding a key out of the grammar, and one whose line is a byte longer than
/// a packet carries. Expect: `Capability` and `TooLong` at the line's place, and nothing written either time.
#[test]
fn a_refused_encode_writes_nothing() {
  let bad = Advertisement {
    version: Ending::Lf,
    capabilities: vec![Line::lf(Capability {
      key: b"no spaces",
      value: None,
    })],
  };
  let mut out = Vec::new();
  assert_eq!(encode(&bad, &mut out), Err(Refusal::Capability { at: 14 }));
  assert!(out.is_empty());
  let value = vec![b'v'; MAX_DATA - b"agent=".len()];
  let long = Advertisement {
    version: Ending::Lf,
    capabilities: vec![Line::lf(Capability {
      key: b"agent",
      value: Some(&value),
    })],
  };
  assert_eq!(
    encode(&long, &mut out),
    Err(Refusal::TooLong {
      at: 14,
      len: to_u64(MAX_DATA + 1)
    })
  );
  assert!(out.is_empty());
}

/// A-582 part 2 (§4.2): do decode stock git's advertisement with one capability less room than its list takes, then
/// with exactly the room. Expect: `PastAdmission` with nothing charged, then the advertisement and the room spent.
#[test]
fn a_decode_past_its_admission_is_refused_before_its_list_is_made() {
  let needed = to_u64(5 * core::mem::size_of::<Line<Capability<'_>>>());
  let mut short = Admission::new(needed - 1);
  assert_eq!(
    decode(ADVERTISEMENT, &mut short),
    Err(Refusal::PastAdmission(PastAdmission {
      asked: needed,
      left: needed - 1
    }))
  );
  assert_eq!(short.left(), needed - 1);
  let mut exact = Admission::new(needed);
  assert!(decode(ADVERTISEMENT, &mut exact).is_ok());
  assert_eq!(exact.left(), 0);
}
