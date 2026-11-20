use slates_mem::handle::Encoded;
use slates_rt::TaskId;

use super::{Phase, PhaseWaits, Reap, SeatView, Tracker, Waiting, choose, first_value_ns, whole};
use crate::daemon::LIVENESS_BUDGET_NS;
use crate::learned_wait::LearnedWait;

/// Shape: a handshake on loopback, 2 ms.
const HANDSHAKE_NS: u64 = 2_000_000;
/// Shape: a phase's start on the shard's clock.
const START_NS: u64 = 10_000_000_000;
/// Shape: a quick wait, 1 ms.
const QUICK_NS: u64 = 1_000_000;

/// Task `slot`'s id on shard 0.
fn task(slot: u32) -> TaskId {
  TaskId(Encoded::pack(0, slot, 1).expect("a task id"))
}

/// A taken seat of task `slot` waiting in `phase` since `since_ns`, with the first value `first_ns`.
fn seat(slot: u32, phase: Phase, since_ns: u64, first_ns: Option<u64>) -> SeatView {
  SeatView {
    taken: true,
    task: Some(task(slot)),
    proven: false,
    waiting: Some(Waiting {
      phase,
      since_ns,
      first_ns,
    }),
    reaped: false,
  }
}

/// A-651 (RFC 6298 §2.2's first sample). Do: compute the first value of a 2 ms handshake, and the bound one sample of it
/// gives the learned-wait rule. Expect: the same bound, past the liveness window by about three handshakes (R + 4·R/2).
#[test]
fn the_first_value_is_one_sample_of_the_connections_own_handshake() {
  let mut own = LearnedWait::default();
  own.completed(HANDSHAKE_NS);
  let first = first_value_ns(HANDSHAKE_NS);
  assert_eq!(first, own.bound_ns(u64::MAX));
  assert!(
    first >= LIVENESS_BUDGET_NS + 3 * HANDSHAKE_NS && first < LIVENESS_BUDGET_NS + 4 * HANDSHAKE_NS,
    "{first} ns"
  );
}

/// A-651. Do: track a connection through its handshake, then ask each phase's deadline before the edge has samples, and
/// after a proven connection taught authentication. Expect: no deadline for the handshake itself; the connection's own
/// first value for the phases after it; and the edge's learned bound once it has one.
#[test]
fn a_phase_without_samples_waits_its_connections_first_value() {
  let mut waits = PhaseWaits::default();
  let mut tracker = Tracker::default();
  tracker.enter(Phase::Handshake, START_NS, &mut waits);
  let handshake = tracker.waiting().expect("the handshake waits");
  assert_eq!(handshake.deadline_ns(&waits), None);
  tracker.enter(Phase::Authentication, START_NS + HANDSHAKE_NS, &mut waits);
  let authentication = tracker.waiting().expect("authentication waits");
  assert_eq!(
    authentication.deadline_ns(&waits),
    Some(START_NS + HANDSHAKE_NS + first_value_ns(HANDSHAKE_NS))
  );
  waits.completed(Phase::Authentication, QUICK_NS);
  assert_eq!(
    authentication.deadline_ns(&waits),
    Some(START_NS + HANDSHAKE_NS + waits.bound_ns(Phase::Authentication).expect("learned"))
  );
}

/// A-651 (A-625 part 2's rule). Do: track an unproven connection through its handshake and authentication, then prove it.
/// Expect: nothing taught before the proof; one sample a phase folded in at it; and its later waits each a sample.
#[test]
fn only_a_proven_connection_teaches() {
  let mut waits = PhaseWaits::default();
  let mut tracker = Tracker::default();
  tracker.enter(Phase::Handshake, START_NS, &mut waits);
  tracker.enter(Phase::Authentication, START_NS + HANDSHAKE_NS, &mut waits);
  let before = (
    waits.samples(Phase::Handshake),
    waits.samples(Phase::Authentication),
  );
  tracker.prove(&mut waits);
  tracker.enter(Phase::Setup, START_NS + 2 * HANDSHAKE_NS, &mut waits);
  tracker.enter(Phase::Running, START_NS + 3 * HANDSHAKE_NS, &mut waits);
  tracker.silence_began(START_NS + 3 * HANDSHAKE_NS);
  tracker.silence_ended(START_NS + 4 * HANDSHAKE_NS, &mut waits);
  let after = [
    Phase::Handshake,
    Phase::Authentication,
    Phase::Setup,
    Phase::Running,
  ]
  .map(|phase| waits.samples(phase));
  assert_eq!((before, after), ((0, 0), [1, 1, 1, 1]));
  assert!(whole(Phase::Setup) && !whole(Phase::Running));
}

/// A-651 (the reaper). Do: choose among three taken seats with known first values at an instant when two are overdue,
/// one by more; then when none is; then with no bound at all. Expect: the most overdue; else the earliest deadline; else
/// never. A seat already reaped, free, or without a task is never chosen.
#[test]
fn the_reaper_takes_the_most_overdue_and_otherwise_waits_for_the_first_deadline() {
  let waits = PhaseWaits::default();
  let first = first_value_ns(HANDSHAKE_NS);
  let seats = [
    seat(1, Phase::Authentication, START_NS, Some(first)),
    seat(2, Phase::Running, START_NS - QUICK_NS, Some(first)),
    seat(3, Phase::Setup, START_NS + first, Some(first)),
  ];
  let now = START_NS + first + QUICK_NS;
  assert_eq!(choose(&seats, &waits, now), Reap::Seat(1));
  assert_eq!(
    choose(&seats, &waits, START_NS),
    Reap::Until(START_NS - QUICK_NS + first)
  );
  let unbound = [seat(1, Phase::Handshake, START_NS, None)];
  assert_eq!(choose(&unbound, &waits, now), Reap::Never);
  let mut skipped = seats;
  skipped[1].reaped = true;
  skipped[0].task = None;
  skipped[2].taken = false;
  assert_eq!(choose(&skipped, &waits, now), Reap::Never);
}

/// A-651 (Karn's rule, the back-off). Do: complete a quick wait in a phase, then let the reaper take one twice. Expect: the
/// bound doubles each time and a completed wait ends the back-off; the reaps are counted as cuts.
#[test]
fn a_reap_backs_its_phase_off_and_a_completed_wait_ends_it() {
  let mut waits = PhaseWaits::default();
  waits.completed(Phase::Running, QUICK_NS);
  let paced = waits.bound_ns(Phase::Running).expect("learned");
  waits.ran_out(Phase::Running);
  let once = waits.bound_ns(Phase::Running);
  waits.ran_out(Phase::Running);
  let twice = waits.bound_ns(Phase::Running);
  waits.completed(Phase::Running, QUICK_NS);
  assert_eq!(
    (once, twice, waits.cuts(Phase::Running)),
    (Some(2 * paced), Some(4 * paced), 2)
  );
  assert!(
    waits
      .bound_ns(Phase::Running)
      .is_some_and(|bound| bound < 2 * paced)
  );
}

/// A-651. Do: complete a wait past its connection's first value. Expect: it counts as late, and it is still a sample once
/// proven: a slow client the edge could afford teaches its pace.
#[test]
fn an_overdue_wait_that_completes_is_late_and_teaches() {
  let mut waits = PhaseWaits::default();
  let mut tracker = Tracker::default();
  tracker.enter(Phase::Handshake, START_NS, &mut waits);
  tracker.enter(Phase::Authentication, START_NS + HANDSHAKE_NS, &mut waits);
  tracker.prove(&mut waits);
  let slow_end = START_NS + HANDSHAKE_NS + 2 * first_value_ns(HANDSHAKE_NS);
  tracker.enter(Phase::Setup, slow_end, &mut waits);
  assert_eq!(
    (tracker.late(), waits.samples(Phase::Authentication)),
    (1, 1)
  );
}
