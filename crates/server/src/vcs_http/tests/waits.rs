use slates_http_edge::connection::Phase;
use slates_rt::tcp::RoundTrip;

use super::{PhaseWaits, Tracker, first_rto_ns};
use crate::daemon::LIVENESS_BUDGET_NS;

/// Shape: a wait as a client on a quick link takes one, 40 µs.
const QUICK_NS: u64 = 40_000;
/// Shape: the shard clock's reading the tests' connections start at.
const START_NS: u64 = 1_000_000_000;
/// Shape: how many quick waits a phase is taught before its bound is read.
const TAUGHT: u64 = 8;
/// Shape: a connection's handshake round trip as a kernel reports it at microsecond resolution: a quarter millisecond.
const ROUND_TRIP: RoundTrip = RoundTrip {
  ns: 250_000,
  resolution_ns: 1_000,
};
/// Shape: the shard clock's granularity `G` in these tests: a tenth of a millisecond, under twice the round trip.
const GRANULARITY_NS: u64 = 100_000;
/// Every phase a connection waits on its client in.
const PHASES: [Phase; 6] = [
  Phase::Handshake,
  Phase::Idle,
  Phase::Head,
  Phase::Body,
  Phase::Reply,
  Phase::Drain,
];

/// `phase`'s waits taught [`TAUGHT`] quick completed waits.
fn taught(waits: &mut PhaseWaits, phase: Phase) {
  for _ in 0..TAUGHT {
    waits.completed(phase, QUICK_NS);
  }
}

/// A proven connection's wait in `phase`, armed at `at`, marked overdue and reaped.
fn reaped(waits: &mut PhaseWaits, phase: Phase, at: u64) {
  let mut tracker = Tracker::new(ROUND_TRIP);
  tracker.prove(waits);
  let deadline = tracker.deadline_ns(phase, at, GRANULARITY_NS, waits);
  waits.reap(&tracker.overdue(phase, deadline));
}

/// A-625 part 2. Do: read every phase's learned bound with nothing measured; then teach the head phase quick waits.
/// Expect: no learned bound for any phase before its first completed wait, none for `Closed` ever; then the head phase
/// alone bounded, a little past a liveness window (the learned tail plus the window), its samples counted, every other
/// phase still unlearned.
#[test]
fn a_phase_has_no_learned_bound_before_its_first_completed_wait_and_learns_apart() {
  let mut waits = PhaseWaits::default();
  assert_eq!(PHASES.map(|phase| waits.bound_ns(phase)), [None; 6]);
  taught(&mut waits, Phase::Head);
  let head = waits.bound_ns(Phase::Head).unwrap();
  assert!(
    (LIVENESS_BUDGET_NS..LIVENESS_BUDGET_NS + LIVENESS_BUDGET_NS / 10).contains(&head),
    "quick heads bound the head phase a little past a window: {head} ns"
  );
  assert_eq!(waits.samples(Phase::Head), TAUGHT);
  let others = PHASES.map(|phase| {
    (phase != Phase::Head)
      .then(|| waits.bound_ns(phase))
      .flatten()
  });
  assert_eq!(others, [None; 6]);
  assert_eq!(waits.bound_ns(Phase::Closed), None);
}

/// A-625 part 2 (main's ruling: a reap teaches nothing and backs off; RFC 6298 §5.5, Karn's rule). Do: with the body
/// phase taught quick waits, reap two proven connections overdue in it, one after the other; then complete a wait.
/// Expect: each reap doubles the body's bound and is counted, no sample taken from either, and the completed wait ends the
/// back-off.
#[test]
fn a_reap_backs_its_phase_off_and_a_completed_wait_ends_it() {
  let mut waits = PhaseWaits::default();
  taught(&mut waits, Phase::Body);
  let paced = waits.bound_ns(Phase::Body).unwrap();
  reaped(&mut waits, Phase::Body, START_NS);
  assert_eq!(waits.bound_ns(Phase::Body), Some(2 * paced));
  reaped(&mut waits, Phase::Body, START_NS);
  assert_eq!(waits.bound_ns(Phase::Body), Some(4 * paced));
  assert_eq!(
    (waits.reaps(Phase::Body), waits.samples(Phase::Body)),
    (2, TAUGHT)
  );
  waits.completed(Phase::Body, QUICK_NS);
  assert!(waits.bound_ns(Phase::Body).unwrap() < 2 * paced);
}

/// A-625 part 2 (main's ruling: a silent client never holds a seat a new connection needs; RFC 6298 §2.2). Do: on a
/// cleartext connection with nothing learned, ask where a wait in each phase passes its bound, with the clock's
/// granularity under twice the round trip and then over it, and with the kernel's clock coarser than both. Expect: every
/// phase bounded by the first RTO of the connection's own round trip, `R + max(G, 2R)`: three round trips while `2R`
/// exceeds `G`, the round trip and `G` once `G` is the larger, and `G` floored by the resolution of the clock that
/// measured `R`.
#[test]
fn a_phase_without_samples_is_bounded_by_the_connections_first_rto() {
  let mut waits = PhaseWaits::default();
  let r = ROUND_TRIP.ns;
  let bounds = PHASES
    .map(|phase| Tracker::new(ROUND_TRIP).deadline_ns(phase, START_NS, GRANULARITY_NS, &mut waits));
  assert_eq!(bounds, [START_NS + 3 * r; 6]);
  let mut tracker = Tracker::new(ROUND_TRIP);
  let coarse_g = 10 * r;
  assert_eq!(
    tracker.deadline_ns(Phase::Body, START_NS, coarse_g, &mut waits),
    START_NS + r + coarse_g
  );
  let millisecond = RoundTrip {
    ns: 0,
    resolution_ns: 1_000_000,
  };
  let mut coarse_clock = Tracker::new(millisecond);
  assert_eq!(
    coarse_clock.deadline_ns(Phase::Idle, START_NS, GRANULARITY_NS, &mut waits),
    START_NS + millisecond.resolution_ns
  );
  assert_eq!(first_rto_ns(r, 0), 3 * r);
}

/// A-625 part 2 (main's ruling; RFC 6298 §5.5). Do: before the body phase has a sample, reap a proven connection overdue
/// in it, then arm another body wait. Expect: the first RTO of the connection's own round trip, doubled for the reap.
#[test]
fn a_reap_before_a_phase_has_samples_doubles_its_first_rto() {
  let mut waits = PhaseWaits::default();
  let r = ROUND_TRIP.ns;
  reaped(&mut waits, Phase::Body, START_NS);
  let mut next = Tracker::new(ROUND_TRIP);
  assert_eq!(
    next.deadline_ns(Phase::Body, START_NS, GRANULARITY_NS, &mut waits),
    START_NS + 2 * 3 * r
  );
}

/// A-625 part 2 (the TLS handshake as the connection's measurement). Do: a TLS connection's handshake begins at accept;
/// the edge spends work on it; its client's next flight is awaited; the handshake ends and idleness begins. Expect: the
/// handshake's waits bounded by the first RTO of the round trip with the edge's own work added, from the handshake's
/// start; and once it ended, idleness bounded by the first RTO of the handshake's whole duration.
#[test]
fn a_tls_handshake_bounds_its_waits_by_its_work_and_then_by_its_duration() {
  let mut waits = PhaseWaits::default();
  let mut tracker = Tracker::new(ROUND_TRIP);
  let r = ROUND_TRIP.ns;
  assert_eq!(
    tracker.deadline_ns(Phase::Handshake, START_NS, GRANULARITY_NS, &mut waits),
    START_NS + 3 * r
  );
  let work = 2 * r;
  tracker.worked(work);
  let flight = START_NS + r;
  assert_eq!(
    tracker.deadline_ns(Phase::Handshake, flight, GRANULARITY_NS, &mut waits),
    START_NS + 3 * (r + work)
  );
  let done = START_NS + 4 * r;
  assert_eq!(
    tracker.deadline_ns(Phase::Idle, done, GRANULARITY_NS, &mut waits),
    done + 3 * (done - START_NS)
  );
}

/// A-625 part 2 (A-523's rule: only a proven client teaches). Do: a connection runs its handshake, idles to its first
/// byte and reads a head over two waits, the head decided; then its credentials prove a principal, and two body silences
/// complete. Expect: nothing learned before the proof; at the proof each of the three phases one sample (its longest
/// wait); after it, each silence a sample of its own.
#[test]
fn a_connections_waits_teach_only_once_its_credentials_prove_a_principal() {
  let mut waits = PhaseWaits::default();
  let mut tracker = Tracker::new(ROUND_TRIP);
  for (phase, at) in [
    (Phase::Handshake, START_NS),
    (Phase::Handshake, START_NS + QUICK_NS),
    (Phase::Idle, START_NS + 3 * QUICK_NS),
  ] {
    tracker.deadline_ns(phase, at, GRANULARITY_NS, &mut waits);
  }
  tracker.observe(Phase::Head, START_NS + 4 * QUICK_NS, &mut waits);
  tracker.deadline_ns(
    Phase::Head,
    START_NS + 5 * QUICK_NS,
    GRANULARITY_NS,
    &mut waits,
  );
  tracker.end(START_NS + 6 * QUICK_NS, &mut waits);
  let learned = |waits: &PhaseWaits| PHASES.map(|phase| (waits.samples(phase), waits.reaps(phase)));
  assert_eq!(learned(&waits), [(0, 0); 6]);
  tracker.prove(&mut waits);
  assert_eq!(
    learned(&waits),
    [(1, 0), (1, 0), (1, 0), (0, 0), (0, 0), (0, 0)]
  );
  tracker.heard(Phase::Body, QUICK_NS, &mut waits);
  tracker.heard(Phase::Body, QUICK_NS, &mut waits);
  assert_eq!(waits.samples(Phase::Body), 2);
}

/// A-625 part 2 (the module doc's two kinds of wait). Do: with every phase taught, a proven connection starts a head and
/// waits for its bytes again half its bound later; then two body silences start a wait apart. Expect: the head's every
/// wait passes its bound at the head's learned bound from the head's start (a trickled head is held to the head's learned
/// length), and each body silence's at the body's bound from its own start.
#[test]
fn a_whole_phases_deadline_runs_from_its_start_and_a_silences_from_each_wait() {
  let mut waits = PhaseWaits::default();
  for phase in PHASES {
    taught(&mut waits, phase);
  }
  let mut tracker = Tracker::new(ROUND_TRIP);
  tracker.prove(&mut waits);
  let head = waits.bound_ns(Phase::Head).unwrap();
  let first = tracker.deadline_ns(Phase::Head, START_NS, GRANULARITY_NS, &mut waits);
  let later = tracker.deadline_ns(Phase::Head, START_NS + head / 2, GRANULARITY_NS, &mut waits);
  assert_eq!((first, later), (START_NS + head, START_NS + head));
  let body = waits.bound_ns(Phase::Body).unwrap();
  let silence = START_NS + head;
  let silences = [silence, silence + QUICK_NS]
    .map(|at| tracker.deadline_ns(Phase::Body, at, GRANULARITY_NS, &mut waits));
  assert_eq!(silences, [silence + body, silence + QUICK_NS + body]);
}

/// A-625 part 2 (Karn's rule; A-523's rule). Do: a proven connection idles and its client leaves mid-wait; then a
/// connection that proved no one is reaped overdue in idleness. Expect: the left wait teaches nothing and backs nothing
/// off; the stranger's reap is counted and backs nothing off either.
#[test]
fn a_wait_its_client_left_teaches_nothing_and_a_strangers_reap_backs_nothing_off() {
  let mut waits = PhaseWaits::default();
  taught(&mut waits, Phase::Idle);
  let paced = waits.bound_ns(Phase::Idle);
  let mut left = Tracker::new(ROUND_TRIP);
  left.prove(&mut waits);
  left.deadline_ns(Phase::Idle, START_NS, GRANULARITY_NS, &mut waits);
  left.abandon();
  left.observe(Phase::Closed, START_NS + QUICK_NS, &mut waits);
  assert_eq!(
    (waits.samples(Phase::Idle), waits.reaps(Phase::Idle)),
    (TAUGHT, 0)
  );
  let mut stranger = Tracker::new(ROUND_TRIP);
  let deadline = stranger.deadline_ns(Phase::Idle, START_NS, GRANULARITY_NS, &mut waits);
  waits.reap(&stranger.overdue(Phase::Idle, deadline));
  assert_eq!(
    (
      waits.samples(Phase::Idle),
      waits.reaps(Phase::Idle),
      waits.bound_ns(Phase::Idle)
    ),
    (TAUGHT, 1, paced)
  );
}

/// A-625 part 2 (RFC 6298 §5.5: an expiry backs off the timer that expired). Do: with the body phase taught, arm three
/// proven connections' body waits under one bound and reap all three; then reap a fourth armed under the bound that left.
/// Expect: the three reaped together back the body off once (twice its pace), each reap counted; the fourth, armed under
/// the doubled bound, doubles it again; so no burst of reaps arms a deadline many doublings away.
#[test]
fn connections_reaped_together_under_one_bound_back_it_off_once() {
  let mut waits = PhaseWaits::default();
  taught(&mut waits, Phase::Body);
  let paced = waits.bound_ns(Phase::Body).unwrap();
  let marks: Vec<_> = (0..3)
    .map(|_| {
      let mut tracker = Tracker::new(ROUND_TRIP);
      tracker.prove(&mut waits);
      let deadline = tracker.deadline_ns(Phase::Body, START_NS, GRANULARITY_NS, &mut waits);
      tracker.overdue(Phase::Body, deadline)
    })
    .collect();
  for mark in &marks {
    waits.reap(mark);
  }
  assert_eq!(
    (waits.bound_ns(Phase::Body), waits.reaps(Phase::Body)),
    (Some(2 * paced), 3)
  );
  reaped(&mut waits, Phase::Body, START_NS + 2 * paced);
  assert_eq!(waits.bound_ns(Phase::Body), Some(4 * paced));
}
