//! A shut-down runtime gives back its per-shard context heap (§4.3; banned item 8: reclamation is part
//! of every bound). A context is the task arena, the run queue and the timer wheel, each sized to the
//! shard's task budget — a daemon's is about 4,300 slots — so a process that starts runtimes repeatedly
//! (the fleet test suite: ~35 tests × 2–5 daemons × 1–5 shards) grew by that much per shard per start
//! for its whole life. The resource the leak consumes is resident memory, so that is what is measured:
//! the process's own resident size through `ps` (a read of the kernel's accounting, never a write).
//! One test per binary, so no other test's allocations move the numbers.

// The whole file measures resident size through `ps` (Unix), so it is a Unix-only test and the crate is
// empty on Windows. Without this, `--all-targets` clippy on Windows flags the imports, `config` and
// `CYCLES` as unused there — the only test that uses them is `#[cfg(unix)]`.
#![cfg(unix)]
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use slates_rt::registry::contexts_reclaimed;
use slates_rt::runtime::{Runtime, RuntimeConfig};

/// Shape: a daemon-sized task budget, so one context is megabytes — far above the page granularity of
/// the resident-size accounting — while the runtime itself stays a test's runtime (two shards, unpinned).
fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 2,
    tasks_per_shard: 4096,
    timers_per_shard: 4096,
    ring_entries: 64,
    step_budget_ns: 1_000_000,
    timer_tick_ns: 100_000,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  }
}

/// Shape: how many runtimes are started in turn after the warm-up — enough that a per-start leak of one
/// runtime's footprint is many times the footprint, so the bound below cannot be met by noise.
const CYCLES: u64 = 32;

/// The process's resident size in KiB, as the kernel accounts it (`ps`, Unix).
#[cfg(unix)]
fn resident_kib() -> u64 {
  let output = std::process::Command::new("ps")
    .args(["-o", "rss=", "-p", &std::process::id().to_string()])
    .output()
    .expect("ps runs");
  String::from_utf8_lossy(&output.stdout)
    .trim()
    .parse()
    .expect("ps prints the resident size in KiB")
}

/// Do: measure what one live two-shard runtime occupies (its footprint: resident size with it up, less
/// resident size before it), shut it down, then start and shut down `CYCLES` more in turn. Expect: the
/// resident size grows over those cycles by less than one footprint — the contexts were given back and
/// their memory reused, where a leak grows by one footprint per cycle — and the reclamation counter moved
/// by every context started (non-vacuity: the footprint is positive, and every context was reclaimed).
/// Before the fix (2026-09-14) every start leaked its contexts: 32 cycles grew the process by ~32
/// footprints.
#[cfg(unix)]
#[test]
fn a_shut_down_runtimes_context_heap_is_given_back() {
  let reclaimed_before = contexts_reclaimed();
  let before = resident_kib();
  let warm = Runtime::start(&config()).unwrap();
  let live = resident_kib();
  warm.shutdown();
  let footprint = live.saturating_sub(before);
  let after_warm = resident_kib();
  for _ in 0..CYCLES {
    let _ = Runtime::start(&config()).unwrap().shutdown();
  }
  let after = resident_kib();
  let growth = after.saturating_sub(after_warm);
  let reclaimed = contexts_reclaimed() - reclaimed_before;
  eprintln!(
    "resident KiB: before {before}, one runtime live {live} (footprint {footprint}), after warm-up \
     {after_warm}, after {CYCLES} more cycles {after} (growth {growth}); contexts reclaimed {reclaimed}"
  );
  assert!(
    footprint > 0,
    "a live runtime occupies memory (non-vacuity)"
  );
  assert_eq!(
    reclaimed,
    (CYCLES + 1) * u64::from(config().shards),
    "every context started was reclaimed"
  );
  assert!(
    growth < footprint,
    "the contexts of {CYCLES} shut-down runtimes were given back: the process grew {growth} KiB, \
     against a per-runtime footprint of {footprint} KiB (a leak grows by one footprint per cycle)"
  );
}
