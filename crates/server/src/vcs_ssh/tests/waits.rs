use slates_rt::tcp::RoundTrip;

use super::{Phase, PhaseWaits, Tracker, whole};
use crate::vcs_http::waits::first_rto_ns;

/// Shape: a TCP handshake's round trip on loopback, as a kernel measures it: 50 µs, read from a microsecond clock.
const ROUND_TRIP: RoundTrip = RoundTrip {
  ns: 50_000,
  resolution_ns: 1_000,
};
/// Shape: the shard clock's granularity `G`: a 100 µs timer tick.
const TICK_NS: u64 = 100_000;
/// Shape: the edge's own work on a handshake: 3 ms of key exchange and signing.
const WORKED_NS: u64 = 3_000_000;
/// Shape: an SSH handshake's whole length: 8 ms.
const HANDSHAKE_NS: u64 = 8_000_000;
/// Shape: a connection's acceptance on the shard's clock.
const START_NS: u64 = 10_000_000_000;
/// Shape: a quick wait, 1 ms.
const QUICK_NS: u64 = 1_000_000;

/// A-651 (RFC 6298 §2.2's first RTO, the HTTP edge's rule). Do: arm a handshake wait at acceptance, add the edge's own
/// work, and arm again. Expect: the first RTO of the kernel's round trip, floored by the larger of `G` and the kernel
/// clock's resolution; then of the round trip with the work added, from the same start.
#[test]
fn a_handshake_is_bounded_by_its_own_round_trip_and_the_edges_work() {
  let mut waits = PhaseWaits::default();
  let mut tracker = Tracker::new(ROUND_TRIP);
  let at_accept = tracker.deadline_ns(Phase::Handshake, START_NS, TICK_NS, &mut waits);
  tracker.worked(WORKED_NS);
  let after_work = tracker.deadline_ns(Phase::Handshake, START_NS + WORKED_NS, TICK_NS, &mut waits);
  assert_eq!(
    (at_accept, after_work),
    (
      START_NS + first_rto_ns(ROUND_TRIP.ns, TICK_NS),
      START_NS + first_rto_ns(ROUND_TRIP.ns + WORKED_NS, TICK_NS)
    )
  );
}

/// A-651. Do: end a handshake after 8 ms, then arm authentication; then teach authentication one quick wait and arm a
/// new connection's. Expect: the first RTO of the handshake's whole length from authentication's start; then the edge's
/// learned bound, the connection's own measurement no longer used.
#[test]
fn after_its_handshake_a_connection_waits_the_first_rto_of_it_until_its_phase_has_learned() {
  let mut waits = PhaseWaits::default();
  let mut tracker = Tracker::new(ROUND_TRIP);
  tracker.observe(Phase::Handshake, START_NS, &mut waits);
  let authenticating = START_NS + HANDSHAKE_NS;
  let first = tracker.deadline_ns(Phase::Authentication, authenticating, TICK_NS, &mut waits);
  waits.completed(Phase::Authentication, QUICK_NS);
  let mut later = Tracker::new(ROUND_TRIP);
  let learned = later.deadline_ns(Phase::Authentication, START_NS, TICK_NS, &mut waits);
  assert_eq!(
    (first, learned),
    (
      authenticating + first_rto_ns(HANDSHAKE_NS, TICK_NS),
      START_NS + waits.bound_ns(Phase::Authentication).expect("learned")
    )
  );
}

/// A-651 (A-625 part 2's rule). Do: track an unproven connection through its handshake and authentication, then prove it,
/// then run a command whose silence ends. Expect: nothing taught before the proof; one sample a phase folded in at it;
/// and its later waits each a sample.
#[test]
fn only_a_proven_connection_teaches() {
  let mut waits = PhaseWaits::default();
  let mut tracker = Tracker::new(ROUND_TRIP);
  tracker.observe(Phase::Handshake, START_NS, &mut waits);
  tracker.observe(Phase::Authentication, START_NS + HANDSHAKE_NS, &mut waits);
  let before = [Phase::Handshake, Phase::Authentication].map(|phase| waits.samples(phase));
  tracker.prove(&mut waits);
  tracker.observe(Phase::Setup, START_NS + 2 * HANDSHAKE_NS, &mut waits);
  let silence = START_NS + 3 * HANDSHAKE_NS;
  let _ = tracker.deadline_ns(Phase::Running, silence, TICK_NS, &mut waits);
  tracker.heard(silence + QUICK_NS, &mut waits);
  let after = [
    Phase::Handshake,
    Phase::Authentication,
    Phase::Setup,
    Phase::Running,
  ]
  .map(|phase| waits.samples(phase));
  assert_eq!((before, after), ([0, 0], [1, 1, 1, 1]));
  assert!(whole(Phase::Setup) && !whole(Phase::Running));
}

/// A-651 (a silence keeps its start across the session's own timer). Do: arm a running silence, arm it again later as
/// a read the session's rekey timer interrupted would, then hear it. Expect: one deadline from the first arming, and one
/// sample of the whole silence.
#[test]
fn a_silence_keeps_its_start_when_its_read_is_armed_again() {
  let mut waits = PhaseWaits::default();
  let mut tracker = Tracker::new(ROUND_TRIP);
  tracker.prove(&mut waits);
  let first = tracker.deadline_ns(Phase::Running, START_NS, TICK_NS, &mut waits);
  let again = tracker.deadline_ns(Phase::Running, START_NS + QUICK_NS, TICK_NS, &mut waits);
  tracker.heard(START_NS + 2 * QUICK_NS, &mut waits);
  assert_eq!((first, waits.samples(Phase::Running)), (again, 1));
}

/// A-651 (Karn's rule, RFC 6298 §5.5). Do: reap two proven connections whose waits were armed under the same bound, then
/// one armed after the back-off; then reap an unproven one. Expect: the two reaps back the phase off once, the third once
/// more, the unproven one not at all; every reap counted, none a sample.
#[test]
fn reaps_armed_under_one_bound_back_it_off_once() {
  let mut waits = PhaseWaits::default();
  let mut proven = [Tracker::new(ROUND_TRIP), Tracker::new(ROUND_TRIP)];
  let deadlines = proven.each_mut().map(|tracker| {
    tracker.prove(&mut waits);
    tracker.deadline_ns(Phase::Running, START_NS, TICK_NS, &mut waits)
  });
  for (tracker, deadline_ns) in proven.iter().zip(deadlines) {
    waits.reap(&tracker.overdue(Phase::Running, deadline_ns));
  }
  let once = waits.generation(Phase::Running);
  let mut later = Tracker::new(ROUND_TRIP);
  later.prove(&mut waits);
  let deadline_ns = later.deadline_ns(Phase::Running, START_NS, TICK_NS, &mut waits);
  waits.reap(&later.overdue(Phase::Running, deadline_ns));
  let mut stranger = Tracker::new(ROUND_TRIP);
  let deadline_ns = stranger.deadline_ns(Phase::Running, START_NS, TICK_NS, &mut waits);
  waits.reap(&stranger.overdue(Phase::Running, deadline_ns));
  assert_eq!(
    (
      once,
      waits.generation(Phase::Running),
      waits.reaps(Phase::Running),
      waits.samples(Phase::Running)
    ),
    (1, 2, 4, 0)
  );
}

/// A-651. Do: before a phase has learned, arm a wait after one back-off. Expect: the connection's first RTO doubled.
#[test]
fn a_backed_off_phase_doubles_the_first_rto() {
  let mut waits = PhaseWaits::default();
  let mut reaped = Tracker::new(ROUND_TRIP);
  reaped.prove(&mut waits);
  let deadline_ns = reaped.deadline_ns(Phase::Setup, START_NS, TICK_NS, &mut waits);
  waits.reap(&reaped.overdue(Phase::Setup, deadline_ns));
  let mut next = Tracker::new(ROUND_TRIP);
  let doubled = next.deadline_ns(Phase::Setup, START_NS, TICK_NS, &mut waits);
  assert_eq!(doubled, START_NS + 2 * first_rto_ns(ROUND_TRIP.ns, TICK_NS));
}
