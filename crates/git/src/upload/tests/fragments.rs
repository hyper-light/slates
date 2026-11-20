//! The service's pack from A-639's stream (A-640 part 3, joining A-639): stock git's fetches answered with the packs
//! the store streams from its own objects. And from the parts the store keeps (A-653): a stock clone of small.git
//! before any version is encoded and once every one is, and every fetch stock git answered from A-582's repository
//! with every version encoded and `main`'s base pack made, each pack holding each object once; a pack through the
//! source copying the kept parts; and a plan past its admission refused.

use std::collections::BTreeSet;

use super::clone::{FETCH, branches, refs, small};
use super::{MAIN_REF, read_fetch, serving};
use crate::Admission;
use crate::fixtures::{
  FETCH_CLONE_REQUEST, FETCH_CLONE_RESPONSE, FETCH_HAVES_REQUEST, FETCH_HAVES_RESPONSE,
  FETCH_PROGRESS_REQUEST, FETCH_PROGRESS_RESPONSE, FETCH_REF_IN_WANT_REQUEST,
  FETCH_REF_IN_WANT_RESPONSE, FETCH_SHALLOW_REQUEST, FETCH_SHALLOW_RESPONSE,
};
use crate::http::Version;
use crate::object_name::{ObjectFormat, ObjectId};
use crate::served::{self, MAIN, REFS, SIDE, V1};
use crate::store::learn::{Clock, Exchange};
use crate::unpack::{self, NoIndex, Object};
use crate::upload::{Asked, Fragments, PackPlan, PackRefused, PackSource, Served, Written, serve};
use crate::v2::Line;
use crate::v2::fetch::{Event, Section, decode_response};

/// A source that asks the source it wraps for at most `most` bytes a call, so what one turn wrote is handed out over
/// many calls.
struct Narrow<S> {
  /// The source it wraps.
  inner: S,
  /// The most bytes it asks for a call.
  most: usize,
}

impl<S: PackSource> PackSource for Narrow<S> {
  type Pack = S::Pack;

  fn begin(
    &mut self,
    served: &Served<'_>,
    plan: &PackPlan<'_>,
    admission: &mut Admission,
  ) -> Result<S::Pack, PackRefused> {
    self.inner.begin(served, plan, admission)
  }

  fn write(
    &mut self,
    served: &Served<'_>,
    pack: &mut S::Pack,
    admission: &mut Admission,
    out: &mut Vec<u8>,
    room: usize,
  ) -> Result<bool, PackRefused> {
    self
      .inner
      .write(served, pack, admission, out, room.min(self.most))
  }
}

/// The tests' clock: a nanosecond a reading.
struct Ticks(u64);

impl Clock for Ticks {
  fn now_ns(&mut self) -> u64 {
    self.0 += 1;
    self.0
  }
}

/// Shape: the exchange a test's turns judge by, a saved byte worth a nanosecond.
/// Shape: the tests' exchange, a nanosecond a byte, and fixed rates (A-718: the encoder's time is the measured rates',
/// never a clock's), each level a nanosecond a KiB dearer than the one before.
const EXCHANGE: Exchange = Exchange {
  ns: 1,
  bytes: 1,
  rates: crate::store::learn::Rates {
    deflate_ns_per_kib: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
    search_ns_per_kib: 1,
  },
};

/// Shape: a turn long enough for many units of work, as the tests' clock counts.
const LONG_TURN: u64 = 1 << 20;

/// Format: stock git's exchanges, each named, its request and its answer (`fixtures/make-fixtures.sh`).
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

/// The answer to `request` from the fixture repository, its refs `REFS` and its head `main` ([`streamed_from`]).
fn streamed(
  fixture: &served::Served,
  request: &[u8],
  quantum_ns: u64,
  room: usize,
  most: usize,
) -> (Vec<u8>, usize) {
  let served = serving(fixture, &REFS, MAIN_REF);
  streamed_from(&served, request, quantum_ns, room, most)
}

/// The answer to `request` from `served`, its pack from A-639's stream turning `quantum_ns` at a time and asked for at
/// most `most` bytes a call, written in steps of at most `room` bytes: its bytes, and how many steps wrote nothing.
fn streamed_from(
  served: &Served<'_>,
  request: &[u8],
  quantum_ns: u64,
  room: usize,
  most: usize,
) -> (Vec<u8>, usize) {
  let mut clock = Ticks(0);
  let mut admission = Admission::new(u64::MAX);
  let mut answer = serve(
    served,
    Version::V2,
    Asked::Request(request),
    &mut Narrow {
      inner: Fragments {
        clock: &mut clock,
        exchange: EXCHANGE,
        quantum_ns,
      },
      most,
    },
    &mut admission,
  );
  assert_eq!(answer.refusal(), None);
  let mut bytes = Vec::new();
  let mut empty = 0;
  loop {
    let mut source = Narrow {
      inner: Fragments {
        clock: &mut clock,
        exchange: EXCHANGE,
        quantum_ns,
      },
      most,
    };
    let before = bytes.len();
    if answer.write(served, &mut source, &mut admission, &mut bytes, room) == Written::End {
      assert_eq!(answer.refusal(), None);
      return (bytes, empty);
    }
    if bytes.len() == before {
      empty += 1;
    }
  }
}

/// A-640 part 3 (A-639's pack, served): do answer each fetch stock git's client sent, the pack streamed from the
/// store's own objects. Expect: every packet but the pack's data stock git 2.50.1's, and a pack holding exactly the
/// objects git's pack held, read back through A-608's reader (a thin one with its bases from the repository).
#[test]
fn every_fetch_stock_git_answered_is_answered_from_the_stores_own_pack() {
  let fixture = served::served();
  for (name, request, response) in EXCHANGES {
    let (bytes, _) = streamed(&fixture, request, LONG_TURN, usize::MAX, usize::MAX);
    assert_eq!(
      read_fetch(&bytes, false, &fixture.objects),
      read_fetch(response, false, &fixture.objects),
      "{name}"
    );
  }
}

/// A-640 part 3 (one turn a step, and a turn that writes nothing yet): do answer the clone with turns of one tick, in
/// steps of one byte. Expect: the same bytes, packet for packet, as long turns write in steps of unbounded room, since
/// a packet is framed only once the turns have filled it or the pack has ended; and steps that wrote nothing while the
/// stream worked toward its header and the turns filled the packet.
#[test]
fn turns_of_one_tick_in_steps_of_one_byte_write_the_same_answer() {
  let fixture = served::served();
  let (whole, _) = streamed(
    &fixture,
    FETCH_CLONE_REQUEST,
    LONG_TURN,
    usize::MAX,
    usize::MAX,
  );
  let (stepped, empty) = streamed(&fixture, FETCH_CLONE_REQUEST, 1, 1, usize::MAX);
  assert_eq!(stepped, whole);
  assert!(empty > 0);
}

/// A-640 part 3 (what one turn wrote, handed out over many calls): do answer the clone with long turns, the stream
/// asked for at most one byte a call. Expect: the same bytes as a stream asked for a packet's room a call, so every
/// byte a turn wrote is handed out before the stream turns again, and none is lost once the stream has written its end.
#[test]
fn a_turn_handed_out_a_byte_a_call_writes_the_same_answer() {
  let fixture = served::served();
  let (whole, _) = streamed(
    &fixture,
    FETCH_CLONE_REQUEST,
    LONG_TURN,
    usize::MAX,
    usize::MAX,
  );
  let (narrow, _) = streamed(&fixture, FETCH_CLONE_REQUEST, LONG_TURN, usize::MAX, 1);
  assert_eq!(narrow, whole);
}

/// The objects of the pack a `fetch` response carries, in pack order, a thin pack's bases taken from `objects`; none
/// when it carries no pack.
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
  if pack.is_empty() {
    return Vec::new();
  }
  let mut reader = unpack::Reader::new(ObjectFormat::Sha1, &NoIndex);
  let mut admission = Admission::new(u64::MAX);
  assert_eq!(reader.read(&pack, &mut admission).unwrap(), pack.len());
  reader
    .finish_with(&served::Bases(objects), &mut admission)
    .unwrap()
    .objects
}

/// The ids read, each once: a repeat fails the test.
fn once(objects: &[Object]) -> BTreeSet<ObjectId> {
  let ids: BTreeSet<ObjectId> = objects.iter().map(|object| object.id).collect();
  assert_eq!(ids.len(), objects.len(), "an object sent twice");
  ids
}

/// Every version of each of `greens` in `fixture` encoded, as the owner encodes each after its commit.
fn encode_all(fixture: &mut served::Served, greens: impl IntoIterator<Item = u64>) {
  for green in greens {
    for version in 0..fixture.repository.versions(green) {
      fixture
        .repository
        .encode_fragments(
          &mut fixture.store,
          (green, version),
          &mut Ticks(0),
          EXCHANGE,
        )
        .unwrap();
    }
  }
}

/// A-582's repository with every version of its three greens encoded and `main`'s base pack made, as the owner keeps
/// it once its encodings have run: the parts a fetch's pack is read from.
fn encoded_fixture() -> served::Served {
  let mut fixture = served::served();
  encode_all(&mut fixture, [MAIN, SIDE, V1]);
  fixture
    .repository
    .rebuild_base(&mut fixture.store, MAIN, &mut Ticks(0), EXCHANGE)
    .unwrap();
  fixture
}

/// What small.git answers the `fetch` a stock `git clone` sent, through A-639's stream in long turns, every branch's
/// versions encoded first when `encoded`; and small.git's objects.
fn small_clone(encoded: bool) -> (Vec<u8>, Vec<Object>) {
  let (mut fixture, _) = small();
  let names = branches();
  if encoded {
    encode_all(&mut fixture, 1..=u64::try_from(names.len()).unwrap());
  }
  let refs = refs(&names);
  let served = serving(&fixture, &refs, MAIN_REF);
  let (bytes, _) = streamed_from(&served, FETCH, LONG_TURN, usize::MAX, usize::MAX);
  (bytes, fixture.objects)
}

/// A-653 (A-640's clone through A-640 part 3's source, from the store's parts): do serve the `fetch` a stock
/// `git clone` of small.git sent (32 wants, `thin-pack`, `ofs-delta`, `done`), before any version is encoded and once
/// every version is. Expect: the packfile section alone, its pack exactly small.git's 32 objects, each once.
#[test]
fn a_stock_clones_fetch_is_answered_from_the_stores_parts() {
  for encoded in [false, true] {
    let (bytes, objects) = small_clone(encoded);
    let (events, _) = read_fetch(&bytes, false, &objects);
    assert_eq!(
      events,
      [Event::Header(Line::lf(Section::Packfile)), Event::Flush],
      "encoded {encoded}"
    );
    let every: BTreeSet<ObjectId> = objects.iter().map(|object| object.id).collect();
    assert_eq!(
      once(&pack_objects(&bytes, &objects)),
      every,
      "encoded {encoded}"
    );
  }
}

/// A-653 (§4.17.2 "A fetch"; git's `fetch-clone`, `fetch-haves`, `fetch-shallow`, `fetch-ref-in-want` and
/// `fetch-progress` exchanges through A-640 part 3's source, from the store's parts): do send each request stock git's
/// client sent, with every version encoded and `main`'s base pack made, so the runs' fragments, the base pack and the
/// parts partly wanted are what is sent. Expect: every packet but the pack's data the one stock git 2.50.1 sent (its
/// progress aside), and a pack of exactly the objects git's held, each once, a thin one completed from the
/// repository's objects.
#[test]
fn every_fetch_stock_git_answered_is_answered_from_the_stores_parts() {
  let fixture = encoded_fixture();
  for (name, request, response) in EXCHANGES {
    let (bytes, _) = streamed(&fixture, request, LONG_TURN, usize::MAX, usize::MAX);
    assert_eq!(
      read_fetch(&bytes, false, &fixture.objects),
      read_fetch(response, false, &fixture.objects),
      "{name}"
    );
    once(&pack_objects(&bytes, &fixture.objects));
  }
}

/// Every object of `fixture`, by position, ascending.
fn every_position(fixture: &served::Served) -> crate::positions::Positions {
  let positions: Vec<u64> = fixture
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
  crate::positions::Positions::of(positions)
}

/// A-653 (the store's parts through A-640 part 3's source, `Streamed::counters`). Do: begin through the source the pack
/// of every object of A-582's repository, every version encoded and `main`'s base pack made, and write it. Expect: the
/// base pack's parts copied whole; `v1`'s part, its tag wanted beside the objects the base pack sent, read entry by
/// entry; nothing encoded afresh; and a pack of exactly every object, each once.
#[test]
fn a_pack_through_the_source_is_read_from_the_kept_parts() {
  let fixture = encoded_fixture();
  let objects = every_position(&fixture);
  let plan = PackPlan {
    runs: &[],
    objects: &objects,
    filter: None,
    thin: false,
    ofs_delta: true,
  };
  let served = serving(&fixture, &REFS, MAIN_REF);
  let mut clock = Ticks(0);
  let mut source = Fragments {
    clock: &mut clock,
    exchange: EXCHANGE,
    quantum_ns: LONG_TURN,
  };
  let mut admission = Admission::new(u64::MAX);
  let mut pack = source.begin(&served, &plan, &mut admission).unwrap();
  let mut bytes = Vec::new();
  while source
    .write(&served, &mut pack, &mut admission, &mut bytes, usize::MAX)
    .unwrap()
  {}
  let counters = pack.counters();
  let loose = counters.loose_whole + counters.loose_deltas;
  assert!(
    counters.parts_copied > 0 && counters.parts_by_entry > 0 && loose == 0,
    "{counters:?}"
  );
  let read = unpack::read_whole(
    &bytes,
    ObjectFormat::Sha1,
    &NoIndex,
    &mut Admission::new(u64::MAX),
  )
  .unwrap();
  let every: BTreeSet<ObjectId> = fixture.objects.iter().map(|object| object.id).collect();
  assert_eq!(once(&read.objects), every);
}

/// A-653 (what a pack keeps until its end is charged first, A-640 part 3's plan copied). Do: begin the pack of every
/// object with no room admitted. Expect: refused past its admission.
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
  let mut clock = Ticks(0);
  let mut source = Fragments {
    clock: &mut clock,
    exchange: EXCHANGE,
    quantum_ns: LONG_TURN,
  };
  let begun = source.begin(&served, &plan, &mut Admission::new(0));
  assert!(matches!(begun, Err(PackRefused::PastAdmission(_))));
}

/// A-745 (§4.17.1 "A fetch's answer", rule 1: nothing holds a list of every object a fetch sends). Do: begin through the
/// source the pack of every object of A-582's repository, and the pack of none, each with room to spare, and read what
/// each charged. Expect: what every object costs past none is one run's bytes for each run its positions make, a few
/// words however many objects the runs hold, where the plan's copy had charged a handle an object.
#[test]
fn a745_a_plans_objects_are_charged_as_runs_never_an_object_at_a_time() {
  let fixture = served::served();
  let every = every_position(&fixture);
  let none = crate::positions::Positions::new();
  let charged = |objects: &crate::positions::Positions| {
    let plan = PackPlan {
      runs: &[],
      objects,
      filter: None,
      thin: true,
      ofs_delta: true,
    };
    let served = serving(&fixture, &REFS, MAIN_REF);
    let mut clock = Ticks(0);
    let mut source = Fragments {
      clock: &mut clock,
      exchange: EXCHANGE,
      quantum_ns: LONG_TURN,
    };
    let mut admission = Admission::new(u64::MAX);
    assert!(source.begin(&served, &plan, &mut admission).is_ok());
    u64::MAX - admission.left()
  };
  let run_bytes = crate::to_u64(core::mem::size_of::<(u64, u64)>());
  let past_none = charged(&every) - charged(&none);
  assert_eq!(
    past_none,
    run_bytes * crate::to_u64(every.runs()),
    "{} objects in {} runs",
    every.len(),
    every.runs()
  );
  assert!(
    every.len() > crate::to_u64(every.runs()) * 2,
    "the repository's objects make fewer runs than objects"
  );
}
