use slates_http_edge::connection::Phase;

use super::{PhaseWaits, Tracker};
use crate::daemon::LIVENESS_BUDGET_NS;

/// Shape: a wait as a client on a quick link takes one, 40 µs.
const QUICK_NS: u64 = 40_000;
/// Shape: the shard clock's reading the tests' connections start at.
const START_NS: u64 = 1_000_000_000;
/// Shape: how many quick waits a phase is taught before its bound is read.
const TAUGHT: u64 = 8;
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

/// A-625 part 2. Do: read every phase's bound with nothing measured; then teach the head phase quick waits. Expect: no
/// bound for any phase before its first completed wait, none for `Closed` ever; then the head phase alone bounded, a
/// little past a liveness window (the learned tail plus the window), its samples counted, every other phase still
/// unbounded.
#[test]
fn a_phase_has_no_bound_before_its_first_completed_wait_and_learns_apart() {
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

/// A-625 part 2 (RFC 6298 §5.5's back-off, Karn's rule). Do: with the body phase taught quick waits, cut two of its waits
/// in a row, then complete one. Expect: each cut doubles the body's bound and is counted, no sample taken from it, and the
/// completed wait ends the back-off.
#[test]
fn a_cut_wait_backs_its_phase_off_and_a_completed_one_ends_it() {
  let mut waits = PhaseWaits::default();
  taught(&mut waits, Phase::Body);
  let paced = waits.bound_ns(Phase::Body).unwrap();
  waits.ran_out(Phase::Body);
  assert_eq!(waits.bound_ns(Phase::Body), Some(2 * paced));
  waits.ran_out(Phase::Body);
  assert_eq!(waits.bound_ns(Phase::Body), Some(4 * paced));
  assert_eq!(
    (waits.cuts(Phase::Body), waits.samples(Phase::Body)),
    (2, TAUGHT)
  );
  waits.completed(Phase::Body, QUICK_NS);
  assert!(waits.bound_ns(Phase::Body).unwrap() < 2 * paced);
}

/// A-625 part 2 (A-523's rule: only a proven client teaches). Do: a connection runs its handshake, idles to its first
/// byte and reads a head over two waits, the head decided; one of its body waits is cut; then its credentials prove a
/// principal, and two body silences complete. Expect: nothing learned and nothing backed off before the proof; at the
/// proof each of the three phases one sample (its longest wait); after it, each silence a sample of its own.
#[test]
fn a_connections_waits_teach_only_once_its_credentials_prove_a_principal() {
  let mut waits = PhaseWaits::default();
  let mut tracker = Tracker::default();
  assert_eq!(
    tracker.deadline_ns(Phase::Handshake, START_NS, &mut waits),
    None
  );
  tracker.deadline_ns(Phase::Handshake, START_NS + QUICK_NS, &mut waits);
  tracker.deadline_ns(Phase::Idle, START_NS + 3 * QUICK_NS, &mut waits);
  tracker.observe(Phase::Head, START_NS + 4 * QUICK_NS, &mut waits);
  tracker.deadline_ns(Phase::Head, START_NS + 5 * QUICK_NS, &mut waits);
  tracker.end(START_NS + 6 * QUICK_NS, &mut waits);
  tracker.cut(Phase::Body, &mut waits);
  let learned = |waits: &PhaseWaits| PHASES.map(|phase| (waits.samples(phase), waits.cuts(phase)));
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
/// wait ends at the deadline set when the head began (a trickled head is held to the head's learned length), and each
/// body silence's at the body's bound from its own start.
#[test]
fn a_whole_phases_deadline_is_set_at_its_start_and_a_silences_at_each_wait() {
  let mut waits = PhaseWaits::default();
  for phase in PHASES {
    taught(&mut waits, phase);
  }
  let mut tracker = Tracker::default();
  tracker.prove(&mut waits);
  let head = waits.bound_ns(Phase::Head).unwrap();
  let first = tracker.deadline_ns(Phase::Head, START_NS, &mut waits);
  let later = tracker.deadline_ns(Phase::Head, START_NS + head / 2, &mut waits);
  assert_eq!(
    (first, later),
    (Some(START_NS + head), Some(START_NS + head))
  );
  let body = waits.bound_ns(Phase::Body).unwrap();
  let silence = START_NS + head;
  let silences =
    [silence, silence + QUICK_NS].map(|at| tracker.deadline_ns(Phase::Body, at, &mut waits));
  assert_eq!(
    silences,
    [Some(silence + body), Some(silence + QUICK_NS + body)]
  );
}

/// A-625 part 2 (Karn's rule). Do: a proven connection idles, and its client leaves mid-wait; then another idles and is
/// cut. Expect: the left wait teaches nothing and backs nothing off; the cut one backs the idle phase off once.
#[test]
fn a_wait_its_client_left_teaches_nothing_and_a_cut_one_backs_off() {
  let mut waits = PhaseWaits::default();
  taught(&mut waits, Phase::Idle);
  let mut left = Tracker::default();
  left.prove(&mut waits);
  left.deadline_ns(Phase::Idle, START_NS, &mut waits);
  left.abandon();
  left.observe(Phase::Closed, START_NS + QUICK_NS, &mut waits);
  assert_eq!(
    (waits.samples(Phase::Idle), waits.cuts(Phase::Idle)),
    (TAUGHT, 0)
  );
  let mut stalled = Tracker::default();
  stalled.prove(&mut waits);
  stalled.deadline_ns(Phase::Idle, START_NS, &mut waits);
  stalled.cut(Phase::Idle, &mut waits);
  assert_eq!(
    (waits.samples(Phase::Idle), waits.cuts(Phase::Idle)),
    (TAUGHT, 1)
  );
}
