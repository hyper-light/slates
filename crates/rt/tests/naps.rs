//! A shard naps while it expects work and parks once it no longer does (84a's Linux arm G; `slates_machine::nap`).
//!
//! After its idle spin a tracking shard naps, in waits of its idle window, until the expectation of its measured idle
//! gaps from its last activity has passed; then it parks as before. Each nap is a timer of the shard's own, whose
//! interrupt on its own CPU lets a kick that landed meanwhile run at once rather than at that CPU's tick. Off Linux no
//! shard naps, and the test says so.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use slates_rt::registry;
use slates_rt::runtime::{Runtime, RuntimeConfig, WakeTracking};
use slates_rt::shard::{Counters, ShardId};

/// Shape: the shard's idle window, a millisecond: the length of each nap.
const WINDOW_NS: u64 = 1_000_000;
/// Shape: the gap between the test's activities, five windows, so every gap outlasts the window and teaches the
/// idle-gap estimate; also the estimate's prior, so the mean is the gap from the first.
const GAP_NS: u64 = 5 * WINDOW_NS;
/// Shape: the activities the test makes.
const ACTIVITIES: u32 = 8;
/// Shape: the estimate's weighting, sixteen samples.
const SHIFT: u32 = 4;
/// Shape: the naps one expectation must show at least: the expectation of a 5 ms mean is 23 ms, about 22 naps past
/// the window's spin, and half of them is far above any nap a stray timer could cause.
const LEAST_NAPS: u64 = 10;
/// Shape: a stretch well past the expectation (23 ms), after which the shard must park, not nap.
const QUIET: Duration = Duration::from_millis(200);
/// Shape: how long a shard gets to answer a counters question.
const ANSWER: Duration = Duration::from_secs(5);

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 64,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: 100_000,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: WINDOW_NS,
    wake_tracking: Some(WakeTracking {
      prior_ns: GAP_NS,
      shift: SHIFT,
      idle_ratio: 0,
      idle_floor_ns: WINDOW_NS,
      idle_ceiling_ns: WINDOW_NS,
    }),
  }
}

/// The shard's counters, asked of the shard by a task (which is no activity).
fn counters_of(rt: &Runtime, shard: ShardId) -> Counters {
  let (tx, rx) = channel();
  rt.spawn_on(shard, async move {
    let _ = tx.send(registry::with_current(|ctx| ctx.counters()).unwrap_or_default());
  })
  .unwrap();
  rx.recv_timeout(ANSWER).unwrap()
}

/// Waits `span` on the test thread.
fn wait(span: Duration) {
  let (_hold, rx) = channel::<()>();
  let started = Instant::now();
  while started.elapsed() < span {
    let _ = rx.recv_timeout(span.saturating_sub(started.elapsed()));
  }
}

/// 84a arm G by use. Do: note an activity on a tracking shard every [`GAP_NS`], eight times; then wait a quiet
/// stretch; then another. Expect (Linux): the shard napped after its activities (at least [`LEAST_NAPS`], the
/// non-vacuity count), each nap's wall time counted; and no nap in the second quiet stretch, so a shard that no
/// longer expects work parks as before. Off Linux: no nap at all.
#[test]
fn a_shard_naps_while_it_expects_work_and_parks_once_it_does_not() {
  let rt = Runtime::start(&config()).unwrap();
  let shard = rt.shard_ids()[0];
  let start = counters_of(&rt, shard);
  for _ in 0..ACTIVITIES {
    rt.spawn_on(shard, async {
      registry::with_current(|ctx| ctx.note_activity());
    })
    .unwrap();
    wait(Duration::from_nanos(GAP_NS));
  }
  wait(QUIET);
  let expecting = counters_of(&rt, shard);
  wait(QUIET);
  let later = counters_of(&rt, shard);
  rt.shutdown().unwrap();
  if !cfg!(target_os = "linux") {
    assert_eq!(
      later.naps, start.naps,
      "a shard napped off Linux: {later:?}"
    );
    eprintln!("skipping the nap's own checks: naps are Linux's");
    return;
  }
  assert!(
    expecting.naps >= start.naps + LEAST_NAPS,
    "the shard parked where it expected work: {} naps: {expecting:?}",
    expecting.naps - start.naps
  );
  assert!(
    expecting.nap_wall_ns > start.nap_wall_ns,
    "the naps' wall time went uncounted: {expecting:?}"
  );
  assert_eq!(
    later.naps, expecting.naps,
    "the shard napped after its expectation had passed: {later:?}"
  );
}
