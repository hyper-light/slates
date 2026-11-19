//! A-653: A-640's pack source over A-639's stream (`store::StorePacks`), held to stock git's own answers: a stock clone's
//! `fetch` of small.git, and every `fetch` stock git answered from A-582's repository, served through `upload::serve`
//! with the store's packs in place of whole objects; a pack written a little room at a time the pack written whole; and
//! a plan past its admission refused.

use super::clone::{FETCH, branches, refs, small};
use super::fetch::EXCHANGES;
use super::{MAIN_REF, read_fetch, serving};
use crate::Admission;
use crate::http::Version;
use crate::object_name::{ObjectFormat, ObjectId};
use crate::served::{self, MAIN, REFS, SIDE, V1};
use crate::store::learn::{Clock, Exchange};
use crate::store::{StorePacks, UploadCounters};
use crate::unpack::{self, NoIndex, Object};
use crate::upload::{Asked, PackPlan, PackRefused, PackSource, Served, Written, serve};
use crate::v2::Line;
use crate::v2::fetch::{Event, Section, decode_response};

/// The tests' clock: a nanosecond a reading.
struct Ticks(u64);

impl Clock for Ticks {
  fn now_ns(&mut self) -> u64 {
    self.0 += 1;
    self.0
  }
}

/// Shape: the tests' exchange, a byte saved worth a nanosecond.
const EXCHANGE: Exchange = Exchange { ns: 1, bytes: 1 };

/// A source of the store's packs, learning by the tests' clock.
fn packs() -> StorePacks<Ticks> {
  StorePacks::new(Ticks(0), EXCHANGE)
}

/// What `request` is answered with by `served`, its pack from the store, written in one step; whether it was refused,
/// and what the source's packs did.
fn answered(served: &Served<'_>, request: &[u8]) -> (Vec<u8>, bool, UploadCounters) {
  let mut source = packs();
  let mut answer = serve(
    served,
    Version::V2,
    Asked::Request(request),
    &mut source,
    &mut Admission::new(u64::MAX),
  );
  let mut bytes = Vec::new();
  assert_eq!(
    answer.write(served, &mut source, &mut bytes, usize::MAX),
    Written::End
  );
  (bytes, answer.refusal().is_some(), source.counters())
}

/// The objects of the pack a `fetch` response carries, in pack order, a thin pack's bases taken from `objects`.
fn pack_objects(response: &[u8], objects: &[Object]) -> Vec<Object> {
  let events = decode_response(
    response,
    ObjectFormat::Sha1,
    false,
    &mut Admission::new(u64::MAX),
  )
  .unwrap();
  let pack: Vec<u8> = events
    .iter()
    .filter_map(|event| match event {
      Event::Pack(bytes) => Some(*bytes),
      _ => None,
    })
    .flatten()
    .copied()
    .collect();
  let mut reader = unpack::Reader::new(ObjectFormat::Sha1, &NoIndex);
  let mut admission = Admission::new(u64::MAX);
  assert_eq!(reader.read(&pack, &mut admission).unwrap(), pack.len());
  reader
    .finish_with(&served::Bases(objects), &mut admission)
    .unwrap()
    .objects
}

/// Every green's every version of `fixture` encoded, as the owner encodes each after its commit.
fn encode_all(fixture: &mut served::Served, greens: &[u64]) {
  for green in greens {
    for version in 0..fixture.repository.versions(*green) {
      fixture
        .repository
        .encode_fragments(
          &mut fixture.store,
          (*green, version),
          &mut Ticks(0),
          EXCHANGE,
        )
        .unwrap();
    }
  }
}

/// A-653 (A-640's clone, from the store): do serve the `fetch` a stock `git clone` of small.git sent (32 wants,
/// `thin-pack`, `ofs-delta`, `done`), once before any version is encoded and once after every version is. Expect: the
/// packfile section alone, its pack exactly small.git's 32 objects, each once, its bytes git's; encoded, kept parts
/// sent; not encoded, every object encoded afresh.
#[test]
fn a_stock_clones_fetch_is_answered_from_the_store() {
  for encoded in [false, true] {
    let (mut fixture, _) = small();
    if encoded {
      let greens: Vec<u64> = (1..=31).collect();
      encode_all(&mut fixture, &greens);
    }
    let names = branches();
    let refs = refs(&names);
    let served = serving(&fixture, &refs, b"refs/heads/main");
    let (bytes, refused, counters) = answered(&served, FETCH);
    assert!(!refused);
    let (events, _) = read_fetch(&bytes, false, &fixture.objects);
    assert_eq!(
      events,
      [Event::Header(Line::lf(Section::Packfile)), Event::Flush]
    );
    let read = pack_objects(&bytes, &fixture.objects);
    let mut ids: Vec<ObjectId> = read.iter().map(|object| object.id).collect();
    ids.sort();
    let mut every: Vec<ObjectId> = fixture.objects.iter().map(|object| object.id).collect();
    every.sort();
    assert_eq!(ids, every);
    for object in &read {
      let kept = fixture
        .objects
        .iter()
        .find(|kept| kept.id == object.id)
        .unwrap();
      assert_eq!(kept.content, object.content);
    }
    let parts = counters.parts_copied + counters.parts_by_entry;
    assert_eq!(parts > 0, encoded, "{counters:?}");
  }
}

/// A-653 (§4.17.2 "A fetch"; git's `fetch-clone`, `fetch-haves`, `fetch-shallow`, `fetch-ref-in-want` and
/// `fetch-progress` exchanges): do send each request stock git's client sent, its pack from the store, once with no
/// version encoded and once with every version encoded and `main`'s base pack made. Expect: every packet but the
/// pack's data the one stock git 2.50.1 sent (its progress aside), and a pack of exactly the objects git's held, a
/// thin one completed from the repository's objects.
#[test]
fn every_fetch_stock_git_answered_is_answered_from_the_store() {
  for encoded in [false, true] {
    let mut fixture = served::served();
    if encoded {
      encode_all(&mut fixture, &[MAIN, SIDE, V1]);
      fixture
        .repository
        .rebuild_base(&mut fixture.store, MAIN, &mut Ticks(0), EXCHANGE)
        .unwrap();
    }
    let served = serving(&fixture, &REFS, MAIN_REF);
    for (name, request, response) in EXCHANGES {
      let (bytes, refused, _) = answered(&served, request);
      assert!(!refused, "{name}");
      assert_eq!(
        read_fetch(&bytes, false, &fixture.objects),
        read_fetch(response, false, &fixture.objects),
        "{name} encoded {encoded}"
      );
      let read = pack_objects(&bytes, &fixture.objects);
      let once: std::collections::BTreeSet<ObjectId> =
        read.iter().map(|object| object.id).collect();
      assert_eq!(once.len(), read.len(), "{name}: an object sent twice");
    }
  }
}

/// Every object of `fixture`, by position, ascending.
fn every_position(fixture: &served::Served) -> Vec<u64> {
  let mut positions: Vec<u64> = fixture
    .objects
    .iter()
    .map(|object| {
      fixture
        .repository
        .object(&fixture.store, &object.id)
        .unwrap()
        .position
    })
    .collect();
  positions.sort_unstable();
  positions
}

/// Writes all of `pack` from `source` a step of `room` at a time: the bytes, and the most one step wrote.
fn written<P: PackSource>(
  source: &mut P,
  served: &Served<'_>,
  pack: &mut P::Pack,
  room: usize,
) -> (Vec<u8>, usize) {
  let (mut bytes, mut most) = (Vec::new(), 0);
  loop {
    let mut step = Vec::new();
    let more = source.write(served, pack, &mut step, room).unwrap();
    assert!(!(more && step.is_empty()), "a step that moved nothing");
    most = most.max(step.len());
    bytes.extend(step);
    if !more {
      return (bytes, most);
    }
  }
}

/// A-653 (A-640's seam: "at least one while any are left and at most `room`"). Do: write the pack of every object of
/// A-582's repository, its versions encoded, seven bytes a step, then in one step from a fresh source. Expect: no step
/// past its room, none empty while more is left, the same bytes either way, and a pack of exactly those objects.
#[test]
fn a_pack_written_a_little_room_at_a_time_is_the_pack_written_whole() {
  let mut fixture = served::served();
  encode_all(&mut fixture, &[MAIN, SIDE, V1]);
  let objects = every_position(&fixture);
  let plan = PackPlan {
    runs: &[],
    objects: &objects,
    filter: None,
    thin: false,
    ofs_delta: true,
  };
  let served = serving(&fixture, &REFS, MAIN_REF);
  let mut whole = packs();
  let mut pack = whole
    .begin(&served, &plan, &mut Admission::new(u64::MAX))
    .unwrap();
  let (once, _) = written(&mut whole, &served, &mut pack, usize::MAX);
  let mut stepped = packs();
  let mut pack = stepped
    .begin(&served, &plan, &mut Admission::new(u64::MAX))
    .unwrap();
  let (sliced, most) = written(&mut stepped, &served, &mut pack, 7);
  assert_eq!(sliced, once);
  assert!(most <= 7);
  let read = unpack::read_whole(
    &once,
    ObjectFormat::Sha1,
    &NoIndex,
    &mut Admission::new(u64::MAX),
  )
  .unwrap();
  assert_eq!(read.objects.len(), fixture.objects.len());
}

/// A-653 (what a pack keeps until its end is charged first). Do: begin a pack of every object with no room admitted.
/// Expect: refused past its admission, before anything is kept.
#[test]
fn a_plan_past_its_admission_is_refused() {
  let fixture = served::served();
  let objects = every_position(&fixture);
  let plan = PackPlan {
    runs: &[],
    objects: &objects,
    filter: None,
    thin: true,
    ofs_delta: true,
  };
  let served = serving(&fixture, &REFS, MAIN_REF);
  let begun = packs().begin(&served, &plan, &mut Admission::new(0));
  assert!(matches!(begun, Err(PackRefused::PastAdmission(_))));
}
