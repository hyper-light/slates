//! A latency-tracking runtime's shards run in macOS's real-time band (84a, A-831; §4.3). A shard woken behind busy
//! time-share neighbours waited out their quanta: a full edit and merge's loaded p99 was 14-17 ms on CI's macos-latest,
//! its tail a forward to the owner shard and back. Under Mach's time-constraint policy a woken shard runs ahead of
//! them: 0.41-2.11 ms (BENCHMARKS "84a on macOS: Mach's time-constraint policy"). A runtime that tracks no wake estimate
//! (one nothing serves latency from) keeps the default policy.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use slates_rt::runtime::{NO_IDLE_CEILING_NS, Runtime, RuntimeConfig, WakeTracking};

/// Shape: R9's provisioning floor, the daemon's idle window and so its shards' computation.
const WINDOW_NS: u64 = 50_000;

/// Shape: how long the test waits for the shard's answer, a guard against a shard that never runs its task.
const BACKSTOP: Duration = Duration::from_secs(20);

/// Format: the answer's states: not yet, held, not held, unknown.
const PENDING: u8 = 0;
const HELD: u8 = 1;
const NOT_HELD: u8 = 2;
const UNKNOWN: u8 = 3;

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

/// Whether the one shard of a runtime built from `config` runs under the time-constraint policy, as a task on it reads.
fn shard_holds(config: &RuntimeConfig) -> u8 {
  static ANSWER: AtomicU8 = AtomicU8::new(PENDING);
  ANSWER.store(PENDING, Ordering::Release);
  let rt = Runtime::start(config).unwrap();
  let shard = rt.shard_ids()[0];
  rt.spawn_on(shard, async {
    let answer = match slates_machine::time_constraint::holds() {
      Some(true) => HELD,
      Some(false) => NOT_HELD,
      None => UNKNOWN,
    };
    ANSWER.store(answer, Ordering::Release);
  })
  .unwrap();
  let started = Instant::now();
  while ANSWER.load(Ordering::Acquire) == PENDING && started.elapsed() < BACKSTOP {
    std::thread::yield_now();
  }
  rt.shutdown().unwrap();
  ANSWER.load(Ordering::Acquire)
}

/// A-831. Do: start a one-shard runtime that tracks a wake estimate (as the daemon's do), and one that tracks none; on
/// each shard, ask whether the thread runs under the time-constraint policy. Expect, on macOS: the tracking runtime's
/// shard held, the other not. Until A-831 neither was (every shard time-share). Elsewhere the policy is macOS's and the
/// test skips loudly.
#[test]
fn a_tracking_runtimes_shard_runs_in_the_real_time_band() {
  if !cfg!(target_os = "macos") {
    eprintln!("skipping: Mach's time-constraint policy is macOS's");
    return;
  }
  assert_eq!(
    shard_holds(&config(true)),
    HELD,
    "a latency-tracking runtime's shard runs under the time-constraint policy"
  );
  assert_eq!(
    shard_holds(&config(false)),
    NOT_HELD,
    "a runtime that tracks no wake estimate keeps the default policy"
  );
}
