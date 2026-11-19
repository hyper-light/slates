//! The service's pack from A-639's stream (A-640 part 3, joining A-639): stock git's fetches answered with the packs
//! the store streams from its own objects.

use super::{MAIN_REF, read_fetch, serving};
use crate::Admission;
use crate::fixtures::{
  FETCH_CLONE_REQUEST, FETCH_CLONE_RESPONSE, FETCH_HAVES_REQUEST, FETCH_HAVES_RESPONSE,
  FETCH_PROGRESS_REQUEST, FETCH_PROGRESS_RESPONSE, FETCH_REF_IN_WANT_REQUEST,
  FETCH_REF_IN_WANT_RESPONSE, FETCH_SHALLOW_REQUEST, FETCH_SHALLOW_RESPONSE,
};
use crate::http::Version;
use crate::served::{self, REFS};
use crate::store::learn::{Clock, Exchange};
use crate::upload::{Asked, Fragments, Written, serve};

/// The tests' clock: a nanosecond a reading.
struct Ticks(u64);

impl Clock for Ticks {
  fn now_ns(&mut self) -> u64 {
    self.0 += 1;
    self.0
  }
}

/// Shape: the exchange a test's turns judge by, a saved byte worth a nanosecond.
const EXCHANGE: Exchange = Exchange { ns: 1, bytes: 1 };

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

/// The answer to `request` from the fixture repository, its pack from A-639's stream turning `quantum_ns` at a time,
/// written in steps of at most `room` bytes: its bytes, and how many steps wrote nothing.
fn streamed(
  fixture: &served::Served,
  request: &[u8],
  quantum_ns: u64,
  room: usize,
) -> (Vec<u8>, usize) {
  let served = serving(fixture, &REFS, MAIN_REF);
  let mut clock = Ticks(0);
  let mut answer = serve(
    &served,
    Version::V2,
    Asked::Request(request),
    &mut Fragments {
      clock: &mut clock,
      exchange: EXCHANGE,
      quantum_ns,
    },
    &mut Admission::new(u64::MAX),
  );
  assert_eq!(answer.refusal(), None);
  let mut bytes = Vec::new();
  let mut empty = 0;
  loop {
    let mut source = Fragments {
      clock: &mut clock,
      exchange: EXCHANGE,
      quantum_ns,
    };
    let before = bytes.len();
    if answer.write(&served, &mut source, &mut bytes, room) == Written::End {
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
    let (bytes, _) = streamed(&fixture, request, LONG_TURN, usize::MAX);
    assert_eq!(
      read_fetch(&bytes, false, &fixture.objects),
      read_fetch(response, false, &fixture.objects),
      "{name}"
    );
  }
}

/// A-640 part 3 (one turn a step, and a turn that writes nothing yet): do answer the clone with turns of one tick, in
/// steps of one byte. Expect: the same bytes as long turns write in one step, and steps that wrote nothing while the
/// stream worked toward its header.
#[test]
fn turns_of_one_tick_in_steps_of_one_byte_write_the_same_answer() {
  let fixture = served::served();
  let (whole, _) = streamed(&fixture, FETCH_CLONE_REQUEST, LONG_TURN, usize::MAX);
  let (stepped, empty) = streamed(&fixture, FETCH_CLONE_REQUEST, 1, 1);
  assert_eq!(stepped, whole);
  assert!(empty > 0);
}
