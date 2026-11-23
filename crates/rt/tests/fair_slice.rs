//! A latency-tracking runtime's shards ask EEVDF for a slice of their idle window, R9's floor (84a's Linux arm E;
//! `slates_machine::fair_slice`): a shard woken behind busy fair neighbours may then preempt them. On CI's Linux runners
//! a kicked shard ran 920 µs after its kick at the median (run 45020845000). Off Linux the test skips loudly.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slates_rt::runtime::{NO_IDLE_CEILING_NS, Runtime, RuntimeConfig, WakeTracking};

/// Shape: R9's provisioning floor, the daemon's idle window.
const WINDOW_NS: u64 = 50_000;

/// Shape: how long the test waits for the shard's answer.
const BACKSTOP: Duration = Duration::from_secs(20);

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

/// The slice the one shard of a runtime built from `config` holds, as a task on it reads (0: unreadable).
fn shard_slice(config: &RuntimeConfig) -> u64 {
  static ANSWER: AtomicU64 = AtomicU64::new(u64::MAX);
  ANSWER.store(u64::MAX, Ordering::Release);
  let rt = Runtime::start(config).unwrap();
  let shard = *rt.shard_ids().first().unwrap();
  rt.spawn_on(shard, async {
    ANSWER.store(
      slates_machine::fair_slice::current().unwrap_or(0),
      Ordering::Release,
    );
  })
  .unwrap();
  let started = Instant::now();
  while ANSWER.load(Ordering::Acquire) == u64::MAX && started.elapsed() < BACKSTOP {
    std::thread::yield_now();
  }
  rt.shutdown().unwrap();
  ANSWER.load(Ordering::Acquire)
}

/// 84a arm E, Linux. Do: start a one-shard runtime that tracks a wake estimate (as the daemon's do), and one that tracks
/// none; on each shard read the slice its thread holds. Expect: the tracking shard's slice shorter than the other's
/// default, and no shorter than its window. Until arm E both held the default.
#[test]
fn a_tracking_runtimes_shard_holds_a_slice_of_its_idle_window() {
  if !cfg!(target_os = "linux") {
    eprintln!("skipping: EEVDF's requested slice is Linux's");
    return;
  }
  let tracking = shard_slice(&config(true));
  let other = shard_slice(&config(false));
  assert!(
    tracking >= WINDOW_NS && tracking < other,
    "the tracking shard holds {tracking} ns against the other's {other} ns"
  );
}
