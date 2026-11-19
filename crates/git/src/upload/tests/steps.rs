//! An answer written a slice at a time (A-640 part 2; the module's **Bounds**).

use proptest::prelude::*;

use super::{MAIN_REF, WholeObjects, answer_v2, read_fetch, serving};
use crate::Admission;
use crate::fixtures::{FETCH_CLONE_REQUEST, FETCH_CLONE_RESPONSE, LS_REFS_REQUEST, LS_REFS_RESPONSE};
use crate::http::Version;
use crate::object_name::ObjectFormat;
use crate::served::{self, REFS};
use crate::sideband::MAX_BAND_DATA;
use crate::upload::{Asked, PackPlan, PackRefused, PackSource, Served, Written, serve};
use crate::v2::fetch::{Event, decode_response};

/// The answer to `request`, its pack from `source`, written in steps of at most `room` bytes: its bytes, and how many
/// bytes each step wrote.
fn stepped<P: PackSource>(
  fixture: &served::Served,
  request: &[u8],
  room: usize,
  source: &mut P,
) -> (Vec<u8>, Vec<usize>) {
  let served = serving(fixture, &REFS, MAIN_REF);
  let mut answer = serve(
    &served,
    Version::V2,
    Asked::Request(request),
    source,
    &mut Admission::new(u64::MAX),
  );
  let mut bytes = Vec::new();
  let mut steps = Vec::new();
  loop {
    let before = bytes.len();
    let written = answer.write(&served, source, &mut bytes, room);
    steps.push(bytes.len() - before);
    if written == Written::End {
      return (bytes, steps);
    }
  }
}

/// A source of `len` bytes counting up: no pack, since the framing never reads what it frames, but longer than a packet.
struct Counting {
  /// How many bytes it writes.
  len: usize,
}

impl PackSource for Counting {
  type Pack = usize;

  fn begin(
    &mut self,
    _: &Served<'_>,
    _: &PackPlan<'_>,
    _: &mut Admission,
  ) -> Result<usize, PackRefused> {
    Ok(0)
  }

  fn write(
    &mut self,
    _: &Served<'_>,
    at: &mut usize,
    out: &mut Vec<u8>,
    room: usize,
  ) -> Result<bool, PackRefused> {
    let take = room.min(self.len - *at);
    out.extend((*at..*at + take).map(|byte| byte as u8));
    *at += take;
    Ok(*at < self.len)
  }
}

/// Format: rooms at and around the edges a step meets: one byte, a packet's header, a band-1 packet whole and one byte
/// either side, and more than the whole answer.
const ROOMS: [usize; 9] = [1, 2, 4, 5, 65_519, 65_520, 65_521, 4096, 1 << 22];

/// The clone's answer written in one step, held first to stock git's: its sections, and its pack's objects.
fn clone_answer(fixture: &served::Served) -> Vec<u8> {
  let (whole, _) = answer_v2(fixture, FETCH_CLONE_REQUEST);
  assert_eq!(
    read_fetch(&whole, false, &fixture.objects),
    read_fetch(FETCH_CLONE_RESPONSE, false, &fixture.objects)
  );
  whole
}

/// A-640 part 2 (a step writes at most its room, and always moves): do write the clone's answer and the `ls-refs`
/// answer, each first held to stock git's, in steps of each edge room. Expect: the same bytes as one step writes,
/// every step within its room, every step but the last writing at least one byte, and one byte a step at a room of one.
#[test]
fn an_answer_written_in_steps_of_any_room_is_the_answer_written_whole() {
  let fixture = served::served();
  let (ls_refs, _) = answer_v2(&fixture, LS_REFS_REQUEST);
  assert_eq!(ls_refs, LS_REFS_RESPONSE);
  let clone = clone_answer(&fixture);
  for (request, whole) in [(FETCH_CLONE_REQUEST, &clone), (LS_REFS_REQUEST, &ls_refs)] {
    for room in ROOMS {
      let (bytes, steps) = stepped(
        &fixture,
        request,
        room,
        &mut WholeObjects::new(&fixture.objects),
      );
      assert_eq!(&bytes, whole, "room {room}");
      assert!(steps.iter().all(|step| *step <= room), "room {room}");
      let (last, before) = steps.split_last().unwrap();
      assert!(
        before.iter().all(|step| *step > 0) && *last <= room,
        "room {room}"
      );
      if room == 1 {
        assert_eq!(steps.iter().filter(|step| **step == 1).count(), whole.len());
      }
    }
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config {
    cases: 64,
    ..proptest::test_runner::Config::default()
  }))]

  /// A-640 part 2: do write the clone's answer, held first to stock git's, in steps of a generated room. Expect: the
  /// answer written whole, each step within its room.
  #[test]
  fn any_room_writes_the_same_answer(room in 1usize..200_000) {
    let fixture = served::served();
    let whole = clone_answer(&fixture);
    let (bytes, steps) = stepped(&fixture, FETCH_CLONE_REQUEST, room, &mut WholeObjects::new(&fixture.objects));
    prop_assert_eq!(bytes, whole);
    prop_assert!(steps.iter().all(|step| *step <= room));
  }
}

/// A-640 part 2 (a pack longer than a packet, framed on band 1): do serve the clone with a source of 200,000 bytes, in
/// steps of each edge room. Expect: the source's bytes on band 1 in order, every packet full (65,515 bytes) but the
/// last, and the same bytes whatever the room.
#[test]
fn a_pack_longer_than_a_packet_goes_out_in_full_packets() {
  let fixture = served::served();
  let len = 200_000;
  let (whole, _) = stepped(
    &fixture,
    FETCH_CLONE_REQUEST,
    usize::MAX,
    &mut Counting { len },
  );
  let events = decode_response(
    &whole,
    ObjectFormat::Sha1,
    false,
    &mut Admission::new(u64::MAX),
  )
  .unwrap();
  let packets: Vec<&[u8]> = events
    .iter()
    .filter_map(|event| match event {
      Event::Pack(bytes) => Some(*bytes),
      _ => None,
    })
    .collect();
  assert_eq!(
    packets.concat(),
    (0..len).map(|byte| byte as u8).collect::<Vec<u8>>()
  );
  let (last, full) = packets.split_last().unwrap();
  assert!(
    full.len() == len / MAX_BAND_DATA && full.iter().all(|packet| packet.len() == MAX_BAND_DATA)
  );
  assert_eq!(last.len(), len % MAX_BAND_DATA);
  for room in ROOMS {
    let (bytes, steps) = stepped(&fixture, FETCH_CLONE_REQUEST, room, &mut Counting { len });
    assert_eq!(bytes, whole, "room {room}");
    assert!(steps.iter().all(|step| *step <= room), "room {room}");
  }
}
