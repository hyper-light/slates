//! Refusals, each answered in git's words (A-640 part 2; the module's **Refusals**), and hostile requests.

use super::{
  MAIN_REF, WholeObjects, WholePack, answer_of, answer_v2, command, naming, pkt, serving,
};
use crate::fixtures::{FETCH_CLONE_REQUEST, FETCH_HAVES_REQUEST};
use crate::http::Version;
use crate::negotiation;
use crate::object_name::ObjectFormat;
use crate::served::{self, FIRST, REFS, THIRD, id};
use crate::sideband::{self, Band};
use crate::store::NamedRef;
use crate::upload::{
  ABORT, Asked, PackPlan, PackRefused, PackSource, Refusal, Served, Written, serve,
};
use crate::v2::Line;
use crate::v2::fetch::{Event, Section, decode_response};
use crate::{Admission, PastAdmission};

/// Format: an id no object of the fixture has.
const UNKNOWN: &str = "1111111111111111111111111111111111111111";

/// The band-3 packet carrying `words`.
fn on_band_three(words: &[u8]) -> Vec<u8> {
  let mut bytes = Vec::new();
  sideband::write(Band::Error, words, &mut bytes);
  bytes
}

/// A-640 part 2 (`parse_want`, `packet_writer_error`): do want an object the repository lacks, plainly and under
/// `sideband-all`. Expect: `ERR upload-pack: not our ref <id>`, and the same words on band 3, git's.
#[test]
fn a_want_the_store_lacks_is_not_our_ref_plainly_or_on_band_three() {
  let fixture = served::served();
  let request = command("fetch", &[&naming("want", UNKNOWN), b"done"]);
  let (bytes, answer) = answer_v2(&fixture, &request);
  assert_eq!(
    answer.refusal(),
    Some(&Refusal::Negotiation(negotiation::Refusal::NotOurs {
      id: id(UNKNOWN)
    }))
  );
  let words = format!("upload-pack: not our ref {UNKNOWN}");
  assert_eq!(bytes, pkt(&[b"ERR ", words.as_bytes()].concat()));
  let multiplexed = command(
    "fetch",
    &[b"sideband-all", &naming("want", UNKNOWN), b"done"],
  );
  let (bytes, _) = answer_v2(&fixture, &multiplexed);
  assert_eq!(bytes, on_band_three(words.as_bytes()));
}

/// A-640 part 2 (`parse_want_ref`; the document's rule on a ref wanted twice): do want a ref that does not exist, then
/// one ref twice. Expect: `ERR unknown ref refs/heads/nope`, git's words; then the second refused as sent twice.
#[test]
fn a_ref_wanted_by_name_must_exist_and_be_wanted_once() {
  let fixture = served::served();
  let unknown = command("fetch", &[b"want-ref refs/heads/nope", b"done"]);
  let (bytes, answer) = answer_v2(&fixture, &unknown);
  assert_eq!(
    answer.refusal(),
    Some(&Refusal::UnknownRef {
      name: b"refs/heads/nope".to_vec()
    })
  );
  assert_eq!(bytes, pkt(b"ERR unknown ref refs/heads/nope"));
  let twice = command(
    "fetch",
    &[
      b"want-ref refs/heads/side",
      b"want-ref refs/heads/main",
      b"want-ref refs/heads/side",
      b"done",
    ],
  );
  let (_, answer) = answer_v2(&fixture, &twice);
  assert_eq!(
    answer.refusal(),
    Some(&Refusal::WantRefTwice {
      name: b"refs/heads/side".to_vec()
    })
  );
}

/// A-640 part 2 (`process_deepen_not` through `expand_ref`): do cut at a name no ref has, then at `v1` where both a tag
/// and a branch are named so. Expect: git's "deepen-not is not a ref", then git's "ambiguous deepen-not".
#[test]
fn deepen_not_must_name_exactly_one_ref() {
  let mut fixture = served::served();
  let nowhere = command(
    "fetch",
    &[&naming("want", THIRD), b"deepen-not nowhere", b"done"],
  );
  let (bytes, answer) = answer_v2(&fixture, &nowhere);
  assert_eq!(
    answer.refusal(),
    Some(&Refusal::DeepenNotRef {
      rev: b"nowhere".to_vec()
    })
  );
  assert_eq!(
    bytes,
    pkt(b"ERR git upload-pack: deepen-not is not a ref: deepen-not nowhere")
  );
  served::commit(
    &mut fixture.repository,
    &mut fixture.store,
    &[],
    &[(4, FIRST)],
  );
  let refs = [
    REFS[0],
    REFS[1],
    NamedRef {
      name: b"refs/heads/v1",
      green: 4,
    },
    REFS[2],
  ];
  let ambiguous = command(
    "fetch",
    &[&naming("want", THIRD), b"deepen-not v1", b"done"],
  );
  let (_, answer) = answer_of(
    &fixture,
    &refs,
    MAIN_REF,
    Version::V2,
    Asked::Request(&ambiguous),
  );
  assert_eq!(
    answer.refusal(),
    Some(&Refusal::DeepenNotAmbiguous {
      rev: b"v1".to_vec()
    })
  );
}

/// A-640 part 2: do send a filter git does not read, and a `sparse:oid` filter, which is owed. Expect: each refused,
/// the first as an invalid spec, the second by the negotiation.
#[test]
fn a_filter_git_does_not_read_is_refused_and_sparse_oid_waits_for_part_four() {
  let fixture = served::served();
  let nonsense = command(
    "fetch",
    &[&naming("want", THIRD), b"filter nonsense", b"done"],
  );
  let (bytes, answer) = answer_v2(&fixture, &nonsense);
  assert!(matches!(answer.refusal(), Some(Refusal::Filter(_))));
  assert_eq!(bytes, pkt(b"ERR upload-pack: invalid filter-spec"));
  let spec = format!("filter sparse:oid={THIRD}");
  let sparse = command("fetch", &[&naming("want", THIRD), spec.as_bytes(), b"done"]);
  let (_, answer) = answer_v2(&fixture, &sparse);
  assert!(
    matches!(
      answer.refusal(),
      Some(Refusal::Negotiation(negotiation::Refusal::Filter(_)) | Refusal::Filter(_))
    ),
    "{:?}",
    answer.refusal()
  );
}

/// A-640 part 2 (the request's admission bounds what the answer keeps): do answer the clone within admissions too
/// small for it. Expect: each refused as past its admission, in an `ERR` line; and the same request answered whole
/// with room enough.
#[test]
fn an_answer_past_its_admission_is_refused() {
  let fixture = served::served();
  let served = serving(&fixture, &REFS, MAIN_REF);
  for room in [0u64, 64, 512, 4096] {
    let mut source = WholeObjects::new(&fixture.objects);
    let mut answer = serve(
      &served,
      Version::V2,
      Asked::Request(FETCH_CLONE_REQUEST),
      &mut source,
      &mut Admission::new(room),
    );
    assert!(
      matches!(
        answer.refusal(),
        Some(Refusal::PastAdmission(_) | Refusal::Wire(_) | Refusal::Negotiation(_))
      ),
      "room {room}: {:?}",
      answer.refusal()
    );
    let mut bytes = Vec::new();
    assert_eq!(
      answer.write(&served, &mut source, &mut bytes, usize::MAX),
      Written::End
    );
    assert!(
      bytes.get(4..).is_some_and(|data| data.starts_with(b"ERR ")),
      "room {room}"
    );
  }
  let (_, answer) = answer_v2(&fixture, FETCH_CLONE_REQUEST);
  assert_eq!(answer.refusal(), None);
}

/// A-640 part 2 (a flush alone; the served refs' order): do send a flush alone, then serve refs out of order. Expect:
/// nothing answered and the session's end; then a refusal naming the first ref out of place.
#[test]
fn a_flush_alone_ends_the_session_and_refs_must_be_in_order() {
  let fixture = served::served();
  let (bytes, answer) = answer_v2(&fixture, b"0000");
  assert!(bytes.is_empty() && answer.ends_session() && answer.refusal().is_none());
  let unordered = [REFS[1], REFS[0], REFS[2]];
  let (_, answer) = answer_of(
    &fixture,
    &unordered,
    MAIN_REF,
    Version::V2,
    Asked::Request(FETCH_CLONE_REQUEST),
  );
  assert_eq!(answer.refusal(), Some(&Refusal::Refs { at: 1 }));
}

/// A source that writes a whole-object pack until `before` of its bytes are written, then refuses with `refusal`.
struct Failing<'o> {
  /// The pack it writes until then.
  whole: WholeObjects<'o>,
  /// How many bytes it writes.
  before: usize,
  /// Its refusal.
  refusal: PackRefused,
}

impl PackSource for Failing<'_> {
  type Pack = WholePack;

  fn begin(
    &mut self,
    served: &Served<'_>,
    plan: &PackPlan<'_>,
    admission: &mut Admission,
  ) -> Result<WholePack, PackRefused> {
    self.whole.begin(served, plan, admission)
  }

  fn write(
    &mut self,
    served: &Served<'_>,
    pack: &mut WholePack,
    out: &mut Vec<u8>,
    room: usize,
  ) -> Result<bool, PackRefused> {
    if pack.1 >= self.before {
      return Err(self.refusal);
    }
    let room = room.min(self.before - pack.1);
    self.whole.write(served, pack, out, room)
  }
}

/// A-640 part 2 (a pack that cannot be finished): do serve the clone from a source that refuses after 20 bytes, in
/// steps of 7 bytes. Expect: the 20 bytes on band 1, then git's band-3 words for a pack that could not be finished and
/// nothing after, and the source's refusal kept.
#[test]
fn a_pack_that_fails_midway_ends_with_gits_band_three_words() {
  let fixture = served::served();
  let served = serving(&fixture, &REFS, MAIN_REF);
  let past = PackRefused::PastAdmission(PastAdmission { asked: 1, left: 0 });
  let mut source = Failing {
    whole: WholeObjects::new(&fixture.objects),
    before: 20,
    refusal: past,
  };
  let mut answer = serve(
    &served,
    Version::V2,
    Asked::Request(FETCH_CLONE_REQUEST),
    &mut source,
    &mut Admission::new(u64::MAX),
  );
  let mut bytes = Vec::new();
  while answer.write(&served, &mut source, &mut bytes, 7) == Written::More {}
  assert_eq!(answer.refusal(), Some(&Refusal::Pack(past)));
  assert!(bytes.ends_with(&on_band_three(ABORT)));
  let events = decode_response(
    &bytes,
    ObjectFormat::Sha1,
    false,
    &mut Admission::new(u64::MAX),
  )
  .unwrap();
  let packed: Vec<u8> = events
    .iter()
    .filter_map(|event| match event {
      Event::Pack(data) => Some(*data),
      _ => None,
    })
    .flatten()
    .copied()
    .collect();
  assert_eq!(packed.len(), 20);
  let others: Vec<&Event<'_>> = events
    .iter()
    .filter(|event| !matches!(event, Event::Pack(_)))
    .collect();
  assert_eq!(
    others,
    [
      &Event::Header(Line::lf(Section::Packfile)),
      &Event::Error(ABORT)
    ]
  );
}

/// A source that works without writing on every other call, as A-639's stream does while it counts the pack, and
/// otherwise writes a whole-object pack.
struct Pausing<'o> {
  /// The pack it writes.
  whole: WholeObjects<'o>,
  /// Whether its next call only works.
  pause: bool,
}

impl PackSource for Pausing<'_> {
  type Pack = WholePack;

  fn begin(
    &mut self,
    served: &Served<'_>,
    plan: &PackPlan<'_>,
    admission: &mut Admission,
  ) -> Result<WholePack, PackRefused> {
    self.whole.begin(served, plan, admission)
  }

  fn write(
    &mut self,
    served: &Served<'_>,
    pack: &mut WholePack,
    out: &mut Vec<u8>,
    room: usize,
  ) -> Result<bool, PackRefused> {
    self.pause = !self.pause;
    if self.pause {
      return Ok(true);
    }
    self.whole.write(served, pack, out, room)
  }
}

/// A-640 part 3 (a source's call may work and write nothing yet): do serve the clone from a source that writes on every
/// other call, in steps of one byte, so a call that writes nothing has a step to itself (a step calls its source once,
/// and only when everything framed is written). Expect: the same answer as a source that always writes, and steps that
/// wrote nothing yet asked for more.
#[test]
fn a_source_that_works_without_writing_ends_the_step_and_is_asked_again() {
  let fixture = served::served();
  let (whole, _) = answer_v2(&fixture, FETCH_CLONE_REQUEST);
  let served = serving(&fixture, &REFS, MAIN_REF);
  let mut source = Pausing {
    whole: WholeObjects::new(&fixture.objects),
    pause: false,
  };
  let mut answer = serve(
    &served,
    Version::V2,
    Asked::Request(FETCH_CLONE_REQUEST),
    &mut source,
    &mut Admission::new(u64::MAX),
  );
  let mut bytes = Vec::new();
  let mut empty = 0;
  loop {
    let before = bytes.len();
    let written = answer.write(&served, &mut source, &mut bytes, 1);
    if written == Written::End {
      break;
    }
    if bytes.len() == before {
      empty += 1;
    }
  }
  assert_eq!(bytes, whole);
  assert!(empty > 0 && answer.refusal().is_none());
}

/// A-640 part 2 (hostile input, §4.9): do send every cut of git's fetch request and every one of its bits flipped.
/// Expect: every answer written to its end without a panic; a cut or flipped request refused in an `ERR` line or
/// answered; and both outcomes met, so neither path is vacuous.
#[test]
fn every_cut_and_flip_of_a_request_is_refused_or_answered_whole() {
  let fixture = served::served();
  let served = serving(&fixture, &REFS, MAIN_REF);
  let mut requests: Vec<Vec<u8>> = (0..FETCH_HAVES_REQUEST.len())
    .map(|cut| FETCH_HAVES_REQUEST[..cut].to_vec())
    .collect();
  for bit in 0..FETCH_HAVES_REQUEST.len() * 8 {
    let mut flipped = FETCH_HAVES_REQUEST.to_vec();
    flipped[bit / 8] ^= 1 << (bit % 8);
    requests.push(flipped);
  }
  let (mut refused, mut answered) = (0, 0);
  for request in &requests {
    let mut source = WholeObjects::new(&fixture.objects);
    let mut answer = serve(
      &served,
      Version::V2,
      Asked::Request(request),
      &mut source,
      &mut Admission::new(1 << 20),
    );
    let mut bytes = Vec::new();
    assert_eq!(
      answer.write(&served, &mut source, &mut bytes, usize::MAX),
      Written::End
    );
    match answer.refusal() {
      Some(_) => refused += 1,
      None => answered += 1,
    }
  }
  assert!(
    refused > 0 && answered > 0,
    "refused {refused}, answered {answered}"
  );
}
