//! Protocol v0 and v1, served (A-640 part 3), held to stock git 2.50.1's own exchanges (A-604's `fixtures/v0`,
//! `make-v0-fixtures.sh`): answered from the repository git answered from, rebuilt in A-614's store from the pack git
//! sent its clone.

use std::collections::BTreeSet;

use slates_vcs::ContentStore;

use super::steps::Counting;
use super::{WholeObjects, pkt, serving};
use crate::Admission;
use crate::http::Version;
use crate::object_name::{ObjectFormat, ObjectId};
use crate::pktline::{self, Packet};
use crate::served::{self, id};
use crate::store::{NamedRef, Repository};
use crate::unpack::{self, NoIndex};
use crate::upload::{Answer, Asked, Refusal, Written, serve};

/// Includes one of A-604's v0 fixtures.
macro_rules! fixture {
  ($name:literal) => {
    include_bytes!(concat!("../../../fixtures/v0/", $name))
  };
}

/// Format: `main`'s head, the third commit.
const TIP: &str = "28aeb6b4c504f8dfee81575a2c87fb1bcb209cf3";
/// Format: the second commit, which the tag names.
const TWO: &str = "a9f7e713aff24dd1e76d85a910bf3436140d78bf";
/// Format: the first commit.
const ONE: &str = "5b302b426e46aabec52855efdddd8d4f7a49ce93";
/// Format: `side`'s head.
const SIDE: &str = "ee60cb3cf0fbe1ac6e64b6d9108a8206740eabaf";
/// Format: the annotated tag `v1`.
const TAG: &str = "6fc536f46ce84b0fddae9ff695a720db3e51fbce";

/// Format: the refs, each naming its green, in name order.
const REFS: [NamedRef<'static>; 3] = [
  NamedRef {
    name: b"refs/heads/main",
    green: 1,
  },
  NamedRef {
    name: b"refs/heads/side",
    green: 2,
  },
  NamedRef {
    name: b"refs/tags/v1",
    green: 3,
  },
];

/// Format: the requests git's tests wrote and upload-pack answered statelessly, each named, its request and git's
/// answer.
const STATELESS: [(&str, &[u8], &[u8]); 7] = [
  (
    "multi-ack",
    fixture!("multi-ack.request.bin"),
    fixture!("multi-ack.response.bin"),
  ),
  (
    "plain",
    fixture!("plain.request.bin"),
    fixture!("plain.response.bin"),
  ),
  (
    "detailed-done",
    fixture!("detailed-done.request.bin"),
    fixture!("detailed-done.response.bin"),
  ),
  (
    "ready",
    fixture!("ready.request.bin"),
    fixture!("ready.response.bin"),
  ),
  (
    "unshallow",
    fixture!("unshallow.request.bin"),
    fixture!("unshallow.response.bin"),
  ),
  (
    "unknown-want",
    fixture!("unknown-want.request.bin"),
    fixture!("unknown-want.response.bin"),
  ),
  (
    "sparse-v0",
    fixture!("sparse-v0.request.bin"),
    fixture!("sparse-v0.response.bin"),
  ),
];

/// Format: whole sessions of stock `git fetch-pack` with stock `git upload-pack`, each one round, so served statelessly
/// here: each named, the client's side and the server's (its advertisement first).
const SESSIONS: [(&str, &[u8], &[u8]); 9] = [
  (
    "clone-v0",
    fixture!("clone-v0.request.bin"),
    fixture!("clone-v0.response.bin"),
  ),
  (
    "clone-v1",
    fixture!("clone-v1.request.bin"),
    fixture!("clone-v1.response.bin"),
  ),
  (
    "fetch-v0",
    fixture!("fetch-v0.request.bin"),
    fixture!("fetch-v0.response.bin"),
  ),
  (
    "fetch-v1",
    fixture!("fetch-v1.request.bin"),
    fixture!("fetch-v1.response.bin"),
  ),
  (
    "depth-v0",
    fixture!("depth-v0.request.bin"),
    fixture!("depth-v0.response.bin"),
  ),
  (
    "deepen-v0",
    fixture!("deepen-v0.request.bin"),
    fixture!("deepen-v0.response.bin"),
  ),
  (
    "since-v0",
    fixture!("since-v0.request.bin"),
    fixture!("since-v0.response.bin"),
  ),
  (
    "exclude-v0",
    fixture!("exclude-v0.request.bin"),
    fixture!("exclude-v0.response.bin"),
  ),
  (
    "filter-v0",
    fixture!("filter-v0.request.bin"),
    fixture!("filter-v0.response.bin"),
  ),
];

/// The packets of `bytes` from `at` on, each as its bytes, and where they end.
fn packets_from(bytes: &[u8], mut at: usize) -> Vec<&[u8]> {
  let mut packets = Vec::new();
  while at < bytes.len() {
    let (packet, took) = pktline::read(&bytes[at..], at as u64).unwrap().unwrap();
    let len = match packet {
      Packet::Data(_) => took,
      _ => pktline::HEADER_LEN,
    };
    packets.push(&bytes[at..at + len]);
    at += took;
  }
  packets
}

/// A response's packets after its advertisement (from `skip`), the pack's data and progress aside, and the pack's
/// data.
fn split(response: &[u8], skip: usize) -> (Vec<&[u8]>, Vec<u8>) {
  split_keeping(response, skip, false)
}

/// A response's packets after its advertisement (from `skip`), the pack's data aside, and band 2's too unless
/// `progress` keeps them; and the pack's data.
fn split_keeping(response: &[u8], skip: usize, progress: bool) -> (Vec<&[u8]>, Vec<u8>) {
  let mut others = Vec::new();
  let mut pack = Vec::new();
  for packet in packets_from(response, skip) {
    match packet.get(4) {
      Some(1) => pack.extend_from_slice(&packet[5..]),
      Some(2) if !progress => {}
      _ => others.push(packet),
    }
  }
  (others, pack)
}

/// How many bytes the advertisement a session's response begins with takes.
fn advertised_len(response: &[u8]) -> usize {
  crate::v0::Advertisement::decode(response, &mut Admission::new(u64::MAX))
    .unwrap()
    .unwrap()
    .1
}

/// The repository git served the v0 fixtures from, rebuilt from the pack it sent its clone: `main` as three versions,
/// `side` and the tag.
fn repository() -> served::Served {
  let response = fixture!("clone-v0.response.bin");
  let (_, pack) = split(response, advertised_len(response));
  let objects = unpack::read_whole(
    &pack,
    ObjectFormat::Sha1,
    &NoIndex,
    &mut Admission::new(u64::MAX),
  )
  .unwrap()
  .objects;
  let mut store = ContentStore::new(1 << 26, 1 << 10).unwrap();
  let mut repository = Repository::new(ObjectFormat::Sha1);
  served::commit(
    &mut repository,
    &mut store,
    &objects,
    &[(1, ONE), (2, SIDE), (3, TAG)],
  );
  served::commit(&mut repository, &mut store, &[], &[(1, TWO)]);
  served::commit(&mut repository, &mut store, &[], &[(1, TIP)]);
  served::Served {
    repository,
    store,
    objects,
  }
}

/// What `asked` is answered with at `version`, written whole in steps of unbounded room (a step frames one packet, so a
/// pack of several takes as many steps), and the answer.
fn answered(
  fixture: &served::Served,
  version: Version,
  asked: Asked<'_>,
) -> (Vec<u8>, Answer<super::WholePack>) {
  answered_with(fixture, &REFS, version, asked)
}

/// What `asked` is answered with at `version`, served with `refs`, as [`answered`] writes it.
fn answered_with(
  fixture: &served::Served,
  refs: &[NamedRef<'_>],
  version: Version,
  asked: Asked<'_>,
) -> (Vec<u8>, Answer<super::WholePack>) {
  let served = serving(fixture, refs, b"refs/heads/main");
  let mut source = WholeObjects::new(&fixture.objects);
  let mut admission = Admission::new(u64::MAX);
  let mut answer = serve(&served, version, asked, &mut source, &mut admission);
  let mut bytes = Vec::new();
  while answer.write(&served, &mut source, &mut admission, &mut bytes, usize::MAX) == Written::More
  {
  }
  (bytes, answer)
}

/// The names of the objects in `pack`, a thin pack's bases from `fixture`.
fn names(pack: &[u8], fixture: &served::Served) -> BTreeSet<ObjectId> {
  if pack.is_empty() {
    return BTreeSet::new();
  }
  served::pack_names(pack, &fixture.objects)
}

/// A-640 part 3 (`send_ref`; git's `advertisement-v0.bin` and `advertisement-v1.bin`): do ask for the advertisement
/// at v0 and at v1. Expect: git's refs, each with its id and the tag peeled, `HEAD` first, `version 1` first under v1;
/// every capability git advertised among ours, in git's order, `symref` and `object-format` git's values, and the
/// agent ours.
#[test]
fn the_advertisement_lists_gits_refs_and_its_capabilities_in_its_order() {
  let fixture = repository();
  for (version, gits) in [
    (Version::V0, fixture!("advertisement-v0.bin").as_slice()),
    (Version::V1, fixture!("advertisement-v1.bin").as_slice()),
  ] {
    let (bytes, answer) = answered(&fixture, version, Asked::Advertise);
    assert_eq!(answer.refusal(), None);
    let ours = crate::v0::Advertisement::decode(&bytes, &mut Admission::new(u64::MAX))
      .unwrap()
      .unwrap()
      .0;
    let theirs = crate::v0::Advertisement::decode(gits, &mut Admission::new(u64::MAX))
      .unwrap()
      .unwrap()
      .0;
    assert_eq!((ours.version, &ours.refs), (theirs.version, &theirs.refs));
    let our_names: Vec<&[u8]> = ours.capabilities.iter().map(|each| each.name).collect();
    let their_names: Vec<&[u8]> = theirs.capabilities.iter().map(|each| each.name).collect();
    let shared: Vec<&[u8]> = our_names
      .iter()
      .copied()
      .filter(|name| their_names.contains(name))
      .collect();
    assert_eq!(shared, their_names);
    for name in [b"symref".as_slice(), b"object-format"] {
      assert_eq!(ours.capabilities.get(name), theirs.capabilities.get(name));
    }
    let agent = ours.capabilities.get(b"agent").unwrap().value.unwrap();
    assert!(agent.starts_with(b"slates/"));
  }
}

/// A-640 part 3 (`get_common_commits`; git's stateless answers under `multi_ack`, `multi_ack_detailed` ending `done`
/// and reaching `ready`, neither, a depth that unshallows, and a want never advertised): do send each request git's
/// tests wrote. Expect: every packet but the pack's data git's, byte for byte, and a pack holding git's objects.
#[test]
fn every_stateless_exchange_is_answered_with_gits_lines_and_objects() {
  let fixture = repository();
  for (name, request, response) in STATELESS {
    let (bytes, _) = answered(&fixture, Version::V0, Asked::Request(request));
    let (ours, our_pack) = split(&bytes, 0);
    let (theirs, their_pack) = split(response, 0);
    assert_eq!(ours, theirs, "{name}");
    assert_eq!(
      names(&our_pack, &fixture),
      names(&their_pack, &fixture),
      "{name}"
    );
  }
}

/// A-640 part 3 (git's whole sessions at v0 and v1: clones, a fetch with a have, depth, a deepening, `deepen-since`,
/// `deepen-not`, `filter blob:none`): do serve each client's side as one request. Expect: every packet git's server
/// sent after its advertisement, the pack's data aside, and a pack holding git's objects.
#[test]
fn every_one_round_session_is_answered_as_gits_server_answered() {
  let fixture = repository();
  for (name, request, response) in SESSIONS {
    let version = if name.ends_with("-v1") {
      Version::V1
    } else {
      Version::V0
    };
    let (bytes, answer) = answered(&fixture, version, Asked::Request(request));
    assert_eq!(answer.refusal(), None, "{name}");
    let (ours, our_pack) = split(&bytes, 0);
    let (theirs, their_pack) = split(response, advertised_len(response));
    assert_eq!(ours, theirs, "{name}");
    assert_eq!(
      names(&our_pack, &fixture),
      names(&their_pack, &fixture),
      "{name}"
    );
  }
}

/// A clone of `main` ending `done`, with the capabilities `chosen` on its want.
fn clone_request(chosen: &str) -> Vec<u8> {
  [
    pkt(format!("want {TIP} {chosen}\n").as_bytes()),
    b"0000".to_vec(),
    pkt(b"done\n"),
  ]
  .concat()
}

/// A-640 part 3 (side-band's 1000-byte packets, and no side-band): do clone `main` asking for `side-band`, then for
/// neither, then frame a pack of 3,000 bytes on `side-band`. Expect: `NAK`, then the pack on band 1 in packets of 1000
/// bytes at most and a flush; then `NAK` and the pack's bytes as they are, nothing after them; each pack the clone's
/// objects; and the long pack in packets of 995 bytes of data, the last holding the rest.
#[test]
fn side_band_packets_hold_a_thousand_bytes_and_no_side_band_sends_the_pack_bare() {
  let fixture = repository();
  let (whole, _) = answered(
    &fixture,
    Version::V0,
    Asked::Request(&clone_request("side-band-64k ofs-delta")),
  );
  let expected = names(&split(&whole, 0).1, &fixture);
  let (bytes, _) = answered(
    &fixture,
    Version::V0,
    Asked::Request(&clone_request("side-band ofs-delta")),
  );
  assert!(
    packets_from(&bytes, 0)
      .iter()
      .all(|packet| packet.len() <= 1000)
  );
  let (others, pack) = split(&bytes, 0);
  assert_eq!(others, [pkt(b"NAK\n").as_slice(), b"0000"]);
  assert_eq!(names(&pack, &fixture), expected);
  let (bare, _) = answered(
    &fixture,
    Version::V0,
    Asked::Request(&clone_request("ofs-delta")),
  );
  let nak = pkt(b"NAK\n");
  assert!(bare.starts_with(&nak));
  assert_eq!(names(&bare[nak.len()..], &fixture), expected);
  let served = serving(&fixture, &REFS, b"refs/heads/main");
  let mut source = Counting { len: 3000 };
  let request = clone_request("side-band ofs-delta");
  let mut admission = Admission::new(u64::MAX);
  let mut answer = serve(
    &served,
    Version::V0,
    Asked::Request(&request),
    &mut source,
    &mut admission,
  );
  let mut long = Vec::new();
  while answer.write(&served, &mut source, &mut admission, &mut long, usize::MAX) == Written::More {
  }
  let data: Vec<usize> = packets_from(&long, 0)
    .iter()
    .filter(|packet| packet.get(4) == Some(&1))
    .map(|packet| packet.len() - 5)
    .collect();
  assert_eq!(data, [995, 995, 995, 15]);
  assert!(long.ends_with(b"0000"));
}

/// A-640 part 3 (`no-done`: the pack follows `ready` at a flush): do send a have that makes the server ready under
/// `multi_ack_detailed` and `no-done`, then a flush. Expect: `ACK common`, `ACK ready`, `NAK`, `ACK`, and the pack of
/// what `main` brought after the have.
#[test]
fn no_done_sends_the_pack_after_ready_without_done() {
  let fixture = repository();
  let request = [
    pkt(format!("want {TIP} multi_ack_detailed no-done side-band-64k ofs-delta\n").as_bytes()),
    b"0000".to_vec(),
    pkt(format!("have {TWO}\n").as_bytes()),
    b"0000".to_vec(),
  ]
  .concat();
  let (bytes, answer) = answered(&fixture, Version::V0, Asked::Request(&request));
  assert_eq!(answer.refusal(), None);
  let (others, pack) = split(&bytes, 0);
  let expected = [
    pkt(format!("ACK {TWO} common\n").as_bytes()),
    pkt(format!("ACK {TWO} ready\n").as_bytes()),
    pkt(b"NAK\n"),
    pkt(format!("ACK {TWO}\n").as_bytes()),
    b"0000".to_vec(),
  ];
  assert_eq!(
    others,
    expected.iter().map(Vec::as_slice).collect::<Vec<_>>()
  );
  let names = names(&pack, &fixture);
  assert!(names.contains(&id(TIP)) && !names.contains(&id(TWO)) && !names.contains(&id(ONE)));
}

/// A-640 part 3 (`get_common_commits` says `ready` at a flush only when no have was lacked): do send, under
/// `multi_ack_detailed`, a have the repository lacks and then one that alone would make it ready, then a flush. Expect:
/// `ACK common` for the held one and `NAK`, with no `ready` and no pack.
#[test]
fn a_lacked_have_keeps_a_flush_from_ready() {
  let fixture = repository();
  let lacked = "86e52f206da93ad5319625df790d3a5ed9f57f60";
  let request = [
    pkt(format!("want {TIP} multi_ack_detailed side-band-64k\n").as_bytes()),
    b"0000".to_vec(),
    pkt(format!("have {lacked}\n").as_bytes()),
    pkt(format!("have {TWO}\n").as_bytes()),
    b"0000".to_vec(),
  ]
  .concat();
  let (bytes, answer) = answered(&fixture, Version::V0, Asked::Request(&request));
  assert_eq!(answer.refusal(), None);
  let expected = [pkt(format!("ACK {TWO} common\n").as_bytes()), pkt(b"NAK\n")].concat();
  assert_eq!(bytes, expected);
}

/// A-640 part 3 (the request held to the advertisement and its grammar): do send a capability never advertised, a
/// SHA-256 object format, a have before the wants' flush, bytes after `done`, and a request with no flush. Expect: each
/// refused, in an `ERR` line.
#[test]
fn a_request_past_the_advertisement_or_its_grammar_is_refused() {
  /// A request's bytes, and whether a refusal is the one it must meet.
  type Case = (Vec<u8>, fn(&Refusal) -> bool);
  let fixture = repository();
  let have_first = [
    pkt(format!("want {TIP} side-band-64k\n").as_bytes()),
    pkt(format!("have {TWO}\n").as_bytes()),
    b"0000".to_vec(),
  ]
  .concat();
  let after_done = [clone_request("side-band-64k"), pkt(b"done\n")].concat();
  let unfinished = pkt(format!("want {TIP} side-band-64k\n").as_bytes());
  let cases: [Case; 5] = [
    (clone_request("side-band-64k frobnicate"), |refusal| {
      *refusal == Refusal::Capability
    }),
    (
      clone_request("side-band-64k object-format=sha256"),
      |refusal| matches!(refusal, Refusal::ObjectFormat { .. }),
    ),
    (have_first, |refusal| {
      matches!(refusal, Refusal::Request { .. })
    }),
    (after_done, |refusal| {
      matches!(refusal, Refusal::Request { .. })
    }),
    (unfinished, |refusal| {
      matches!(refusal, Refusal::Request { .. })
    }),
  ];
  for (request, expected) in cases {
    let (bytes, answer) = answered(&fixture, Version::V0, Asked::Request(&request));
    let refusal = answer.refusal().unwrap();
    assert!(expected(refusal), "{refusal:?}");
    assert!(bytes[4..].starts_with(b"ERR "));
  }
}

/// A-640 part 3 (hostile input, §4.9): do send every cut of git's `detailed-done` request and every one of its bits
/// flipped. Expect: every answer written to its end without a panic, and both a refusal and an answer met.
#[test]
fn every_cut_and_flip_of_a_v0_request_is_refused_or_answered_whole() {
  let fixture = repository();
  let request = fixture!("detailed-done.request.bin");
  let mut requests: Vec<Vec<u8>> = (0..request.len())
    .map(|cut| request[..cut].to_vec())
    .collect();
  for bit in 0..request.len() * 8 {
    let mut flipped = request.to_vec();
    flipped[bit / 8] ^= 1 << (bit % 8);
    requests.push(flipped);
  }
  let (mut refused, mut answered_whole) = (0, 0);
  for each in &requests {
    let (_, answer) = answered(&fixture, Version::V0, Asked::Request(each));
    match answer.refusal() {
      Some(_) => refused += 1,
      None => answered_whole += 1,
    }
  }
  assert!(
    refused > 0 && answered_whole > 0,
    "{refused} {answered_whole}"
  );
}

/// A-640 part 4 (`create_pack_file` under protocol v0: `pack-objects`' error stream relayed on side-band-64k, and with
/// no band written to upload-pack's own; git's `sparse-missing-v0` and `sparse-bare-v0`): do send a clone's request
/// under `sparse:oid=main:missing`, a path that is not there, choosing side-band-64k and choosing no band. Expect: each
/// answer byte for byte git's (`NAK`, then git's words on band 2 and its band-3 words, or `NAK` alone), refused as a
/// name that names nothing.
#[test]
fn a_sparse_blob_that_names_nothing_is_answered_as_git_answers_it_on_a_band_and_on_none() {
  let fixture = repository();
  let deaths: [(&str, &[u8], &[u8]); 2] = [
    (
      "sparse-missing-v0",
      fixture!("sparse-missing-v0.request.bin"),
      fixture!("sparse-missing-v0.response.bin"),
    ),
    (
      "sparse-bare-v0",
      fixture!("sparse-bare-v0.request.bin"),
      fixture!("sparse-bare-v0.response.bin"),
    ),
  ];
  let refusal = Refusal::SparseBlob {
    name: b"main:missing".to_vec(),
  };
  for (name, request, response) in deaths {
    let (bytes, answer) = answered(&fixture, Version::V0, Asked::Request(request));
    assert_eq!(
      (bytes.as_slice(), answer.refusal()),
      (response, Some(&refusal)),
      "{name}"
    );
  }
}

/// A-640 part 4 (git's `pack-objects` reads a sparse filter's blob only once the pack is due): do send, under
/// `sparse:oid=main:missing`, a round that ends with a flush, its one have one the server lacks, and the same round
/// with no filter. Expect: the same answer, `NAK`, never refused, though the filter's blob names nothing.
#[test]
fn a_round_with_no_pack_due_never_reads_its_sparse_blob() {
  let fixture = repository();
  let round = |filter: &[u8]| {
    let mut request =
      pkt(format!("want {TIP} multi_ack_detailed side-band-64k filter\n").as_bytes());
    request.extend(filter);
    request.extend(b"0000");
    request.extend(pkt(b"have 3333333333333333333333333333333333333333\n"));
    request.extend(b"0000");
    answered(&fixture, Version::V0, Asked::Request(&request))
  };
  let (filtered, answer) = round(&pkt(b"filter sparse:oid=main:missing\n"));
  let (plain, _) = round(&[]);
  assert_eq!(
    (filtered.as_slice(), answer.refusal()),
    (plain.as_slice(), None)
  );
  assert_eq!(filtered, pkt(b"NAK\n"));
}

/// A-640 part 4, fourth slice (`get_oid_basic`'s warning, relayed by `create_pack_file` on side-band-64k; git's
/// `sparse-warn-v0`): do send a clone's request under `sparse:oid=main:a`, choosing side-band-64k, with a tag `main`
/// served beside the branch, both naming the tip. Expect: git's packets after the advertisement, its warning on band 2
/// after the `NAK` and before the pack among them, and a pack of git's objects; not refused.
#[test]
fn a_ref_name_two_patterns_find_is_warned_of_on_the_band_before_the_pack() {
  let mut fixture = repository();
  served::commit(
    &mut fixture.repository,
    &mut fixture.store,
    &[],
    &[(4, TIP)],
  );
  let tag = NamedRef {
    name: b"refs/tags/main",
    green: 4,
  };
  let refs = [REFS[0], REFS[1], tag, REFS[2]];
  let request = fixture!("sparse-warn-v0.request.bin");
  let response = fixture!("sparse-warn-v0.response.bin");
  let (bytes, answer) = answered_with(&fixture, &refs, Version::V0, Asked::Request(request));
  let (ours, our_pack) = split_keeping(&bytes, 0, true);
  let (theirs, their_pack) = split_keeping(response, 0, true);
  assert_eq!(
    (ours, names(&our_pack, &fixture), answer.refusal()),
    (theirs, names(&their_pack, &fixture), None)
  );
}
