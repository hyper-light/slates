//! Observation delivery (§4.14 "observability"; `slates_server::observe`): what a question asked of a
//! daemon from another thread reports at each stage it can end in — a full control channel, a receptive
//! channel with a full arena, a target that stops while the question is pending and whose registry slot a
//! later daemon reuses, a budget that elapses before a late reply, an unavailable shard told apart from an
//! observed zero — and the two pure rules the fleet harness's waits rest on: the verdict of an observation,
//! and the progress charge under unequal starts with one daemon stalled. Every history drives a real
//! daemon (one shard, a laptop: no fleet) through the public observation entry points.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use common::wait::{ProgressCharge, Verdict, verdict};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_rt::RtError;
use slates_server::daemon::{LIVENESS_BUDGET_NS, OBSERVE_BUDGET_NS};
use slates_server::observe::{OBSERVE_LATE_REPLY, ObserveError, ObserveStage};
use slates_server::state::ShardState;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: the profile probe budget (milliseconds); an input to derivations, not a gate.
const PROBE_MS: u64 = 5;
/// Derived: how long a held control shard spins — two anchor liveness budgets ([`LIVENESS_BUDGET_NS`]),
/// long past the short budget below, so a question submitted behind the hold is judged while the hold
/// still stands, and short enough that a history stays a few seconds.
const HOLD_NS: u64 = 2 * LIVENESS_BUDGET_NS;
/// Derived: a budget an observation is given to fail inside a hold — three tenths of a liveness budget,
/// many paces of the observation's retry cadence (a tenth of a heartbeat), so a refused submission is
/// attempted several times before the deadline names it.
const SHORT_BUDGET_NS: u64 = LIVENESS_BUDGET_NS * 3 / 10;
/// Derived: how long a deliberately slow question spins on its shard — twice the short budget, so its
/// answer is late by construction.
const SLOW_QUESTION_NS: u64 = 2 * SHORT_BUDGET_NS;
/// Shape: how long a late reply is waited for to be counted, or a stopped daemon's pending question to
/// end — the whole observe budget, the bound past which either is a failure.
const SETTLE: Duration = Duration::from_nanos(OBSERVE_BUDGET_NS);

/// The observation histories each hold or fill a daemon's control shard, so they run one at a time —
/// the test-harness exception to R2's no-`Mutex` rule (D-8 exception 3), as the fleet tests do.
#[allow(clippy::disallowed_types)]
static OBSERVE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serialize() -> std::sync::MutexGuard<'static, ()> {
  OBSERVE_TEST_LOCK
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A one-shard laptop daemon (no fleet) named for the history.
fn laptop(name: &str) -> Daemon {
  let profile = MachineProfile::measure(ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures");
  let instance = format!("observe-{name}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(1));
  Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-observe-{name}"),
    },
  )
  .expect("the laptop daemon starts")
}

/// A question with a fixed answer, so a delivered answer is told from a stranger's.
fn answer(token: u8) -> impl FnOnce(&mut ShardState) -> u8 + Clone + Send + 'static {
  move |_state| token
}

/// Waits, bounded by [`SETTLE`], until the daemon's late-reply count reaches `expected`; the count seen.
fn late_replies_reach(daemon: &Daemon, expected: u64) -> u64 {
  let began = Instant::now();
  loop {
    let late = daemon
      .fleet_refusals()
      .map(|refusals| refusals.get(OBSERVE_LATE_REPLY).copied().unwrap_or(0))
      .unwrap_or(0);
    if late >= expected || began.elapsed() > SETTLE {
      return late;
    }
    std::thread::yield_now();
  }
}

/// A full control channel: the submission meets `ControlFull`, is retried inside the one budget, and the
/// deadline names the submission stage and that refusal; once the shard drains, the same question answers.
#[test]
fn a_full_control_channel_is_retried_then_named_by_the_deadline() {
  let _serial = serialize();
  let daemon = laptop("control-full");
  // Held, the shard drains nothing: the flood fills its control channel to the bound.
  let hold = daemon
    .starve_control_shard(HOLD_NS)
    .expect("the hold is admitted");
  let queued = daemon
    .flood_control_channel()
    .expect("the flood is submitted");
  assert!(queued >= 1, "the channel took the flood before refusing");
  let refused = daemon.observe_control(SHORT_BUDGET_NS, answer(7));
  assert!(
    matches!(
      &refused,
      Err(ObserveError::Deadline {
        stage: ObserveStage::Submission,
        attempts,
        last_refusal: Some(RtError::ControlFull { .. }),
        ..
      }) if *attempts >= 2
    ),
    "the deadline names the submission stage and the full channel, after retries: {refused:?}"
  );
  let held_ns = hold.answer().expect("the hold reports its span");
  assert!(
    held_ns >= HOLD_NS,
    "the hold held for its span: {held_ns} ns"
  );
  assert_eq!(
    daemon.observe_control(OBSERVE_BUDGET_NS, answer(7)),
    Ok(7),
    "once the shard drains, the same question is answered"
  );
  daemon.stop();
}

/// A receptive control channel and a full arena: the submission lands, the receipt reports the admission
/// refused, the refusal is retried inside the budget, and the deadline names the admission stage and the
/// full arena; once the fillers are released, the same question is admitted and answered.
#[test]
fn a_full_arena_refuses_admission_on_the_receipt_then_admits_after_release() {
  let _serial = serialize();
  let daemon = laptop("arena-full");
  let mut fill = daemon.fill_task_arena().expect("the fill is submitted");
  assert!(
    fill.reached_the_bound(),
    "the arena's bound was met ({} fillers admitted)",
    fill.admitted()
  );
  let refused = daemon.observe_control(SHORT_BUDGET_NS, answer(3));
  assert!(
    matches!(
      &refused,
      Err(ObserveError::Deadline {
        stage: ObserveStage::Admission,
        attempts,
        last_refusal: Some(RtError::TooManyTasks { .. }),
        ..
      }) if *attempts >= 2
    ),
    "the deadline names the admission stage and the full arena, after retries: {refused:?}"
  );
  fill.release();
  assert_eq!(
    daemon.observe_control(OBSERVE_BUDGET_NS, answer(3)),
    Ok(3),
    "once the fillers end, the same question is admitted and answered"
  );
  daemon.stop();
}

/// A daemon stopped while a question is pending on it answers the question terminated (or gone) well
/// inside its budget, never leaves it waiting; a question begun on it and run only after a later daemon
/// took its registry slot is refused as gone, and the later daemon never runs it.
#[test]
fn a_stopped_target_terminates_its_pending_question_and_its_reused_slot_never_answers_a_stranger() {
  let _serial = serialize();
  let daemon = laptop("stop-pending");
  let hold = daemon
    .starve_control_shard(HOLD_NS)
    .expect("the hold is admitted");
  // Pending on another thread: it submits behind the hold and waits on its receipt.
  let pending = daemon
    .observation_on_control(OBSERVE_BUDGET_NS, answer(11))
    .expect("the observation begins");
  let (tx, rx) = channel();
  let began = Instant::now();
  std::thread::spawn(move || {
    let _ = tx.send(pending.wait());
  });
  // Begun before the stop, run only after a stranger holds the slot: refused as gone.
  let stale = daemon
    .observation_on_control(OBSERVE_BUDGET_NS, |state: &mut ShardState| {
      *state.refusals.entry("observe.stranger").or_insert(0) += 1;
    })
    .expect("the stale observation begins");
  drop(hold);
  daemon.stop();
  let ended = rx
    .recv_timeout(SETTLE)
    .expect("the pending question ended inside its budget");
  assert!(
    matches!(
      ended,
      Err(ObserveError::Terminated { .. }) | Err(ObserveError::ShardGone { .. })
    ),
    "the stop ended the pending question typed, after {:?}: {ended:?}",
    began.elapsed()
  );
  let stranger = laptop("stranger");
  assert!(
    matches!(
      stale.wait(),
      Err(ObserveError::ShardGone {
        stage: ObserveStage::Submission,
        ..
      })
    ),
    "a question pinned to the stopped daemon's registration is refused once its slot is free or reused"
  );
  assert_eq!(
    stranger
      .fleet_refusals()
      .map(|refusals| refusals.contains_key("observe.stranger")),
    Ok(false),
    "the stranger never ran the stopped daemon's question"
  );
  assert_eq!(
    stranger.observe_control(OBSERVE_BUDGET_NS, answer(5)),
    Ok(5)
  );
  stranger.stop();
}

/// A budget that elapses first is named by the stage it elapsed at — admission, for a question queued
/// behind a hold; execution, for a question admitted but still running — and the reply that arrives after
/// it is discarded and counted, never delivered to a later question.
#[test]
fn a_budget_that_elapses_first_is_named_and_the_late_reply_is_discarded_and_counted() {
  let _serial = serialize();
  let daemon = laptop("late-reply");
  let hold = daemon
    .starve_control_shard(HOLD_NS)
    .expect("the hold is admitted");
  let queued = daemon.observe_control(SHORT_BUDGET_NS, answer(42));
  assert!(
    matches!(
      queued,
      Err(ObserveError::Deadline {
        stage: ObserveStage::Admission,
        last_refusal: None,
        ..
      })
    ),
    "a question queued behind the hold ends at its deadline in the admission stage: {queued:?}"
  );
  hold.answer().expect("the hold reports its span");
  // The shard drained the queued question late: its reply met a dropped channel and was counted.
  assert_eq!(
    daemon.observe_control(OBSERVE_BUDGET_NS, answer(43)),
    Ok(43),
    "a later question gets its own answer, never the late one"
  );
  assert_eq!(late_replies_reach(&daemon, 1), 1);
  // Admitted at once on the idle shard, the slow question is still running at the deadline.
  let slow = daemon
    .observation_on_control(SHORT_BUDGET_NS, |_state: &mut ShardState| {
      let end = slates_rt::futures::now_ns().saturating_add(SLOW_QUESTION_NS);
      while slates_rt::futures::now_ns() < end {
        std::hint::spin_loop();
      }
      9u8
    })
    .expect("the slow observation begins")
    .admit()
    .expect("the slow question is admitted at once");
  let late = slow.answer();
  assert!(
    matches!(
      late,
      Err(ObserveError::Deadline {
        stage: ObserveStage::Execution,
        ..
      })
    ),
    "a question still running at its deadline ends in the execution stage: {late:?}"
  );
  assert_eq!(late_replies_reach(&daemon, 2), 2);
  daemon.stop();
}

/// An unavailable shard is told apart from an observed zero, and a poll's verdict sorts the three ways an
/// ask can end: observed (the zero), unavailable (a starved shard, the wait continues), terminal (a daemon
/// that stopped, the wait ends).
#[test]
fn an_unavailable_shard_is_told_apart_from_an_observed_zero() {
  let _serial = serialize();
  let daemon = laptop("zero");
  let repairs = |state: &mut ShardState| state.repairs;
  let observed = daemon.observe_control(OBSERVE_BUDGET_NS, repairs);
  assert_eq!(
    observed,
    Ok(0),
    "a laptop has repaired nothing: an observed zero"
  );
  assert_eq!(verdict(observed.map(|count| count > 0)), Verdict::Observed);
  let hold = daemon
    .starve_control_shard(HOLD_NS)
    .expect("the hold is admitted");
  let unavailable = daemon.observe_control(SHORT_BUDGET_NS, repairs);
  assert!(
    matches!(unavailable, Err(ObserveError::Deadline { .. })),
    "the held shard is unavailable, not zero: {unavailable:?}"
  );
  assert!(matches!(
    verdict(unavailable.map(|count| count > 0)),
    Verdict::Unavailable(_)
  ));
  hold.answer().expect("the hold reports its span");
  let after_stop = daemon
    .observation_on_control(OBSERVE_BUDGET_NS, repairs)
    .expect("the observation begins");
  daemon.stop();
  let gone = after_stop.wait();
  assert!(matches!(
    verdict(gone.map(|count| count > 0)),
    Verdict::Terminal(ObserveError::ShardGone { .. })
  ));
}

/// The wait's progress charge under unequal starts with one daemon stalled: each daemon's advance is
/// counted from its own start and the wait is charged the least, so a daemon that began far ahead never
/// pays for one that stopped moving.
#[test]
fn a_wait_is_charged_the_least_advance_from_each_daemons_own_start() {
  let charge = ProgressCharge::begin([100, 5_000]);
  // The first daemon ran 4,000 periods; the second stalled at 5,000: the wait is charged nothing. The
  // rule this replaced charged the least absolute count's advance — 4,100 − 100 — and would have spent
  // the whole period budget here on a daemon that never moved.
  assert_eq!(charge.each([4_100, 5_000]), vec![4_000, 0]);
  assert_eq!(charge.advanced([4_100, 5_000]), 0);
  assert_eq!(charge.advanced([4_100, 5_001]), 1);
  // A coordinator behind its start (restarted) counts as no advance, never a wrap.
  assert_eq!(charge.advanced([50, 6_000]), 0);
  assert_eq!(ProgressCharge::begin([]).advanced([]), 0);
}
