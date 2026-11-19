//! `fetch` and `object-info`, served (A-640 part 2; §4.17.2 "A fetch" items 2 to 4).

use std::collections::BTreeMap;

use super::{answer_v2, command, naming, pkt, read_fetch};
use crate::Admission;
use crate::fixtures::{
  FETCH_CLONE_REQUEST, FETCH_CLONE_RESPONSE, FETCH_HAVES_REQUEST, FETCH_HAVES_RESPONSE,
  FETCH_PROGRESS_REQUEST, FETCH_PROGRESS_RESPONSE, FETCH_REF_IN_WANT_REQUEST,
  FETCH_REF_IN_WANT_RESPONSE, FETCH_SHALLOW_REQUEST, FETCH_SHALLOW_RESPONSE, FETCH_URIS_REQUEST,
};
use crate::object_name::{ObjectFormat, ObjectId};
use crate::served::{self, FIRST, SECOND, TAG, THIRD, id};
use crate::v2::fetch::{Event, Section, WantedRef};
use crate::v2::object_info::{self, ObjectSizes};
use crate::v2::request::{self, Arguments};
use crate::v2::{Ending, Line};

/// Format: stock git's exchanges a fetch is held to, each named, its request and its answer
/// (`fixtures/make-fixtures.sh`).
const EXCHANGES: [(&str, &[u8], &[u8]); 5] = [
  ("fetch-clone", FETCH_CLONE_REQUEST, FETCH_CLONE_RESPONSE),
  ("fetch-haves", FETCH_HAVES_REQUEST, FETCH_HAVES_RESPONSE),
  (
    "fetch-shallow",
    FETCH_SHALLOW_REQUEST,
    FETCH_SHALLOW_RESPONSE,
  ),
  (
    "fetch-ref-in-want",
    FETCH_REF_IN_WANT_REQUEST,
    FETCH_REF_IN_WANT_RESPONSE,
  ),
  (
    "fetch-progress",
    FETCH_PROGRESS_REQUEST,
    FETCH_PROGRESS_RESPONSE,
  ),
];

/// A-640 part 2 (§4.17.2 "A fetch"; git's `fetch-clone`, `fetch-haves`, `fetch-shallow`, `fetch-ref-in-want` and
/// `fetch-progress` exchanges): do send each request stock git's client sent. Expect: every packet of the answer but
/// the pack's data the one stock git 2.50.1 sent (git's progress on band 2 aside, which this server does not send), and
/// a pack holding exactly the objects git's pack held.
#[test]
fn every_fetch_stock_git_answered_is_answered_with_its_sections_and_its_objects() {
  let fixture = served::served();
  for (name, request, response) in EXCHANGES {
    let (bytes, answer) = answer_v2(&fixture, request);
    assert_eq!(answer.refusal(), None, "{name}");
    assert_eq!(
      read_fetch(&bytes, false, &fixture.objects),
      read_fetch(response, false, &fixture.objects),
      "{name}"
    );
  }
}

/// A-640 part 2 (§4.17.2 "A fetch" item 2, the one round on a version head): do the fetch with `first` as its have, and
/// the clone. Expect: the fetch's want answered in one round with no commit walked (the run of `main`'s versions 1 and
/// 2), and the clone found by the walk.
#[test]
fn a_client_versions_behind_is_answered_in_one_round_and_a_clone_by_the_walk() {
  let fixture = served::served();
  let (_, haves) = answer_v2(&fixture, FETCH_HAVES_REQUEST);
  let counters = haves.counters().unwrap();
  assert_eq!((counters.one_round, counters.walked), (1, 0));
  let (_, clone) = answer_v2(&fixture, FETCH_CLONE_REQUEST);
  let counters = clone.counters().unwrap();
  assert!(counters.one_round == 0 && counters.walked > 0);
}

/// A-640 part 2 (`sideband-all`; git's `fetch-uris` request, `sideband-all` and `packfile-uris https`): do send it.
/// Expect: the answer multiplexed throughout, the packfile section's header on band 1, no `packfile-uris` section (the
/// store keeps no URI packs, so the pack is whole inline), and a pack of every object the clone sends.
#[test]
fn a_multiplexed_request_is_answered_on_band_one_throughout() {
  let fixture = served::served();
  let (bytes, answer) = answer_v2(&fixture, FETCH_URIS_REQUEST);
  assert_eq!(answer.refusal(), None);
  assert!(bytes.starts_with(&pkt(b"\x01packfile\n")));
  let (events, names) = read_fetch(&bytes, true, &fixture.objects);
  assert_eq!(
    events,
    [Event::Header(Line::lf(Section::Packfile)), Event::Flush]
  );
  assert_eq!(
    names,
    read_fetch(FETCH_CLONE_RESPONSE, false, &fixture.objects).1
  );
}

/// A-640 part 2 (the document's order, which git's client requires): do want `main` by name and deepen by one.
/// Expect: shallow-info (`shallow <third>`, without an LF) before wanted-refs (`<third> refs/heads/main`), then the
/// packfile.
#[test]
fn shallow_info_comes_before_wanted_refs_as_the_document_orders_them() {
  let fixture = served::served();
  let request = command(
    "fetch",
    &[b"want-ref refs/heads/main", b"deepen 1", b"done"],
  );
  let (bytes, answer) = answer_v2(&fixture, &request);
  assert_eq!(answer.refusal(), None);
  let (events, _) = read_fetch(&bytes, false, &fixture.objects);
  assert_eq!(
    events,
    [
      Event::Header(Line::lf(Section::ShallowInfo)),
      Event::Shallow(Line::bare(id(THIRD))),
      Event::Delim,
      Event::Header(Line::lf(Section::WantedRefs)),
      Event::WantedRef(Line::lf(WantedRef {
        id: id(THIRD),
        name: b"refs/heads/main"
      })),
      Event::Delim,
      Event::Header(Line::lf(Section::Packfile)),
      Event::Flush,
    ]
  );
}

/// A-640 part 2 (`parse_want_ref` reads `HEAD` through its symref): do want `HEAD` and the tag by name. Expect:
/// wanted-refs naming each with what it names, in the order wanted, and a pack holding the tag.
#[test]
fn a_ref_wanted_by_name_is_read_through_head_and_named_back() {
  let fixture = served::served();
  let request = command(
    "fetch",
    &[b"want-ref HEAD", b"want-ref refs/tags/v1", b"done"],
  );
  let (bytes, answer) = answer_v2(&fixture, &request);
  assert_eq!(answer.refusal(), None);
  let (events, names) = read_fetch(&bytes, false, &fixture.objects);
  assert_eq!(
    events.get(..3),
    Some(
      [
        Event::Header(Line::lf(Section::WantedRefs)),
        Event::WantedRef(Line::lf(WantedRef {
          id: id(THIRD),
          name: b"HEAD"
        })),
        Event::WantedRef(Line::lf(WantedRef {
          id: id(TAG),
          name: b"refs/tags/v1"
        })),
      ]
      .as_slice()
    )
  );
  assert!(names.contains(&id(TAG)) && names.contains(&id(THIRD)));
}

/// A-640 part 2 (`process_deepen_not` through `expand_ref`): do want `third` with `deepen-not side`, the branch's short
/// name. Expect: the history cut where `side` begins, `shallow <second>`, and a pack holding `third` and `second` and
/// not `first`.
#[test]
fn deepen_not_finds_a_ref_by_its_short_name() {
  let fixture = served::served();
  let request = command(
    "fetch",
    &[&naming("want", THIRD), b"deepen-not side", b"done"],
  );
  let (bytes, answer) = answer_v2(&fixture, &request);
  assert_eq!(answer.refusal(), None);
  let (events, names) = read_fetch(&bytes, false, &fixture.objects);
  assert_eq!(
    events.get(..3),
    Some(
      [
        Event::Header(Line::lf(Section::ShallowInfo)),
        Event::Shallow(Line::bare(id(SECOND))),
        Event::Delim,
      ]
      .as_slice()
    )
  );
  assert!(names.contains(&id(THIRD)) && names.contains(&id(SECOND)) && !names.contains(&id(FIRST)));
}

/// A-640 part 2 (`process_haves_and_send_acks`): do send a have the store does not hold, then a held one under
/// `wait-for-done`, then a request with no want. Expect: `NAK` and the flush, no pack; then `ACK first` without
/// `ready`; then nothing at all, as git answers each.
#[test]
fn acknowledgments_alone_end_a_round_that_is_not_ready() {
  let fixture = served::served();
  let unknown = "2222222222222222222222222222222222222222";
  let request = command("fetch", &[&naming("want", THIRD), &naming("have", unknown)]);
  let (bytes, _) = answer_v2(&fixture, &request);
  assert_eq!(
    read_fetch(&bytes, false, &fixture.objects).0,
    [
      Event::Header(Line::lf(Section::Acknowledgments)),
      Event::Nak(Ending::Lf),
      Event::Flush
    ]
  );
  let waiting = command(
    "fetch",
    &[
      &naming("want", THIRD),
      &naming("have", FIRST),
      b"wait-for-done",
    ],
  );
  let (bytes, _) = answer_v2(&fixture, &waiting);
  assert_eq!(
    read_fetch(&bytes, false, &fixture.objects).0,
    [
      Event::Header(Line::lf(Section::Acknowledgments)),
      Event::Ack(Line::lf(id(FIRST))),
      Event::Flush
    ]
  );
  let nothing = command("fetch", &[&naming("have", FIRST)]);
  let (bytes, answer) = answer_v2(&fixture, &nothing);
  assert!(bytes.is_empty() && answer.refusal().is_none());
}

/// The sizes stock git's `cat-file -s` printed for the fixture's objects (`fixtures/objects.txt`).
struct GitSizes(BTreeMap<ObjectId, u64>);

impl ObjectSizes for GitSizes {
  fn size(&self, id: &ObjectId) -> Option<u64> {
    self.0.get(id).copied()
  }
}

/// A-640 part 2 (§4.17.2 "A fetch" item 4, A-624's answer over the store's rows): do ask the size of a blob, a tree, a
/// commit, the tag and an object the repository lacks. Expect: the answer A-624 writes from the sizes stock git's
/// `cat-file -s` printed, the lacking object's size unknown.
#[test]
fn object_info_answers_the_sizes_stock_git_printed() {
  let fixture = served::served();
  let printed = GitSizes(
    include_str!("../../../fixtures/objects.txt")
      .lines()
      .map(|line| {
        let words: Vec<&str> = line.split(' ').collect();
        (id(words[0]), words[2].parse().unwrap())
      })
      .collect(),
  );
  let asked = [
    "4cdb2265d30204be5463b38174b2e8e717982405",
    "0351da7889aaa5309979d9a4e742f54ac1750e44",
    THIRD,
    TAG,
    "1111111111111111111111111111111111111111",
  ];
  let mut arguments = vec![b"size".to_vec()];
  arguments.extend(asked.iter().map(|hex| naming("oid", hex)));
  let arguments: Vec<&[u8]> = arguments.iter().map(Vec::as_slice).collect();
  let request_bytes = command("object-info", &arguments);
  let (bytes, answer) = answer_v2(&fixture, &request_bytes);
  assert_eq!(answer.refusal(), None);
  let request::Request::Command(decoded) =
    request::decode(&request_bytes, &mut Admission::new(u64::MAX)).unwrap()
  else {
    panic!("not a command");
  };
  let Arguments::ObjectInfo(lines) = decoded.arguments else {
    panic!("not object-info");
  };
  let mut expected = Vec::new();
  object_info::answer(&lines, ObjectFormat::Sha1, &printed, &mut expected).unwrap();
  assert_eq!(bytes, expected);
}
