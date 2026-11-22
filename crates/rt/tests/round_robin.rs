//! A latency-tracking runtime's shards take Linux's round-robin real-time class where the account's limits grant it, and
//! then park at once when idle (84a's Linux half; §4.3; `slates_machine::round_robin`). On CI's Linux runners a woken
//! shard behind busy time-share neighbours ran a millisecond or more after its kick: 920 µs at the median, 2,990 µs at
//! p99 on ubuntu-24.04-arm, and on ubuntu-latest a spinning shard took requests 2.8-3.9 ms after they landed (run
//! 45017505026, 4 CPUs, two spinners a CPU). A thread in the class runs ahead of every time-share thread when runnable.
//! It must never spin at that priority, or it would take its CPU from every time-share task. Where the account grants
//! no real-time priority (`RLIMIT_RTPRIO`), or limits a real-time thread's run (`RLIMIT_RTTIME`), the shard stays
//! time-share, logged: grant or skip, never a privilege (R10).

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slates_rt::runtime::{NO_IDLE_CEILING_NS, Runtime, RuntimeConfig, WakeTracking};

/// Shape: R9's provisioning floor, the daemon's idle window.
const WINDOW_NS: u64 = 50_000;

/// Shape: how long the test waits for the shard's answer, a guard against a shard that never runs its task.
const BACKSTOP: Duration = Duration::from_secs(20);

/// Format: the answer's word: none yet, then 1 + (holds the class: 2, not: 1, unknown: 0) * 2^32 + the idle window.
const PENDING: u64 = 0;

fn config(tracking: bool) -> RuntimeConfig {
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
    wake_tracking: tracking.then_some(WakeTracking {
      prior_ns: 5_000,
      shift: 4,
      idle_ratio: 0,
      idle_floor_ns: WINDOW_NS,
      idle_ceiling_ns: NO_IDLE_CEILING_NS,
    }),
  }
}

/// What the one shard of a runtime built from `config` reports, as a task on it reads: whether its thread holds the
/// round-robin class (`None` unknown), and its idle window, nanoseconds.
fn shard_reports(config: &RuntimeConfig) -> (Option<bool>, u64) {
  static ANSWER: AtomicU64 = AtomicU64::new(PENDING);
  ANSWER.store(PENDING, Ordering::Release);
  let rt = Runtime::start(config).unwrap();
  let shard = *rt.shard_ids().first().unwrap();
  rt.spawn_on(shard, async {
    let holds = match slates_machine::round_robin::holds() {
      Some(true) => 2,
      Some(false) => 1,
      None => 0,
    };
    let window = slates_rt::futures::idle_window_ns()
      .unwrap_or(u64::MAX)
      .min(u64::from(u32::MAX));
    ANSWER.store(1 + (holds << 32) + window, Ordering::Release);
  })
  .unwrap();
  let started = Instant::now();
  while ANSWER.load(Ordering::Acquire) == PENDING && started.elapsed() < BACKSTOP {
    std::thread::yield_now();
  }
  rt.shutdown().unwrap();
  let answer = ANSWER.load(Ordering::Acquire).saturating_sub(1);
  let holds = match answer >> 32 {
    2 => Some(true),
    1 => Some(false),
    _ => None,
  };
  (holds, answer & u64::from(u32::MAX))
}

/// Whether this account's limits grant the class: a real-time priority under `RLIMIT_RTPRIO`, and no `RLIMIT_RTTIME`.
#[cfg(target_os = "linux")]
fn granted() -> bool {
  use rustix::process::{Resource, getrlimit};
  getrlimit(Resource::Rttime).current.is_none()
    && getrlimit(Resource::Rtprio)
      .current
      .is_none_or(|limit| limit >= 1)
}

/// Off Linux: never.
#[cfg(not(target_os = "linux"))]
fn granted() -> bool {
  false
}

/// 84a, Linux. Do: start a one-shard runtime that tracks a wake estimate (as the daemon's do), and one that tracks none;
/// on each shard, ask whether its thread holds the round-robin real-time class, and its idle window. Expect, where the
/// account's limits grant the class: the tracking runtime's shard holds it and parks at once (an idle window of none),
/// and the other does not hold it and keeps its window. Where they grant none, the tracking shard stays time-share and
/// keeps its window. Until 84a's Linux half no shard held it. Off Linux the class is Linux's and the test skips loudly.
#[test]
fn a_tracking_runtimes_shard_takes_the_round_robin_class_where_granted_and_parks_at_once() {
  if !cfg!(target_os = "linux") {
    eprintln!("skipping: the round-robin real-time class is Linux's");
    return;
  }
  let (tracking_holds, tracking_window) = shard_reports(&config(true));
  let (other_holds, other_window) = shard_reports(&config(false));
  if granted() {
    assert_eq!(
      (tracking_holds, tracking_window),
      (Some(true), 0),
      "a latency-tracking runtime's shard holds the class and never spins idle"
    );
  } else {
    eprintln!("this account grants no real-time class: the shard must stay time-share");
    assert_eq!(
      tracking_holds,
      Some(false),
      "a shard the limits deny stays time-share"
    );
    assert!(
      tracking_window > 0,
      "a time-share shard keeps its idle window"
    );
  }
  assert_eq!(
    other_holds,
    Some(false),
    "a runtime that tracks no wake estimate keeps the default class"
  );
  assert!(other_window > 0, "and its idle window");
}
