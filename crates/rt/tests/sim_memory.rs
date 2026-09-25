//! A simulation gives back what it allocated (§4.3; banned item 8: reclamation is part of every bound):
//! its shard contexts through the registry, its clock with the runtime, its per-shard driver flags
//! through the registry slot. Before 2026-09-14 every `SimRuntime::new` leaked its contexts, its clock
//! and one flag block per shard for the process lifetime — the simulation-driven suites (transport,
//! cluster, db, vfs) build thousands of runtimes per process. One test per binary, so no other test's
//! allocations move the resident-size numbers.

// The whole file measures resident size through `ps` (Unix), so it is a Unix-only test and the crate is
// empty on Windows. Without this, `--all-targets` clippy on Windows flags the imports, `config`, `CYCLES`
// and `one_simulation` as unused there — the only test that uses them is `#[cfg(unix)]`.
#![cfg(unix)]
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use slates_rt::registry::contexts_reclaimed;
use slates_rt::runtime::RuntimeConfig;
use slates_rt::sim::SimRuntime;

/// Shape: a daemon-sized task budget per shard, so one simulation is megabytes — far above the page
/// granularity of the resident-size accounting.
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

/// Shape: how many simulations are built in turn after the warm-up — enough that a per-run leak of one
/// footprint is many times the footprint, so the bound below cannot be met by noise.
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

/// Runs one simulation to idle with a task on each shard, and drops it.
fn one_simulation() {
  let mut sim = SimRuntime::new(&config(), 7).unwrap();
  for id in sim.shard_ids() {
    sim.spawn_on(id, async {}).unwrap();
  }
  sim.run_until_idle();
}

/// Do: measure what one live two-shard simulation occupies, drop it, then build and drop `CYCLES` more in
/// turn. Expect: the resident size grows over those cycles by less than one footprint, and the
/// reclamation counter moved by every context built (non-vacuity: the footprint is positive). Before the
/// fix, 32 simulations grew the process by 32 footprints.
#[cfg(unix)]
#[test]
fn a_dropped_simulation_gives_back_its_contexts_clock_and_flags() {
  let reclaimed_before = contexts_reclaimed();
  let before = resident_kib();
  let warm = SimRuntime::new(&config(), 7).unwrap();
  let live = resident_kib();
  drop(warm);
  let footprint = live.saturating_sub(before);
  let after_warm = resident_kib();
  for _ in 0..CYCLES {
    one_simulation();
  }
  let after = resident_kib();
  let growth = after.saturating_sub(after_warm);
  let reclaimed = contexts_reclaimed() - reclaimed_before;
  eprintln!(
    "resident KiB: before {before}, one simulation live {live} (footprint {footprint}), after warm-up \
     {after_warm}, after {CYCLES} more cycles {after} (growth {growth}); contexts reclaimed {reclaimed}"
  );
  assert!(
    footprint > 0,
    "a live simulation occupies memory (non-vacuity)"
  );
  assert_eq!(
    reclaimed,
    (CYCLES + 1) * u64::from(config().shards),
    "every context built was reclaimed"
  );
  assert!(
    growth < footprint,
    "the contexts, clocks and flags of {CYCLES} dropped simulations were given back: the process grew \
     {growth} KiB against a per-simulation footprint of {footprint} KiB"
  );
}
