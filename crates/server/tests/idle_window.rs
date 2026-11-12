//! A shard's idle window must not grow with the load beside it (A-223 (i); §4.3, §4.7). The window is how long a shard
//! keeps polling after its last work before it parks. Priced on the shard's online wake estimate, it fed on its own
//! spinning: beside more runnable threads than CPUs a woken shard waits to run, its estimate counts that wait, the
//! window grows with it, the shard spins longer, and the next wake waits longer still (a paced client kept its shards
//! spinning at hundreds of milliseconds of window; BENCHMARKS, the idle window's rule, 2026-11-09). On Linux and macOS
//! the daemon's window is R9's provisioning floor, which no estimate moves.
//!
//! Not on Windows: there a shard parked beside two spinners a CPU waits 50-100 ms to run again, so the window keeps
//! the estimate's multiple and grows with load by design until Windows' own window is measured (the supervisor's
//! decision, 2026-11-11; GAPS, A-223).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
#![cfg(not(windows))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use slates_client::{CreateSpec, NamePolicy, SizeClass};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;

/// Shape: two shards, as the paced form of `provision_bench` runs (a create may be forwarded to the other shard).
const TEST_SHARDS: u16 = 2;
/// Shape: the estimate's weighting window for the test, sixteen wakes, so a few hundred paced requests move it far off
/// the boot prior. The daemon derives a far longer one from the probe's spread; the window rule under test does not
/// read it.
const SHIFT: u32 = 4;
/// Shape: the client's pause between requests, a millisecond: a client thinking between requests, the paced form of
/// `provision_bench` (`provision.paced_1_1000us`).
const PACE: Duration = Duration::from_millis(1);
/// Shape: paced requests the test sends at least: four weighting windows, so the estimate has forgotten its prior.
const LEAST_REQUESTS: usize = 4 << SHIFT;
/// Shape: paced requests the test sends at most, waiting for the spinners' queueing to show in an estimate.
const MOST_REQUESTS: usize = 64 << SHIFT;
/// Shape: how far past the boot prior some shard's estimate must rise before the run proves anything: an estimate
/// that stayed near it saw no queueing (the non-vacuity bound). Four times is far past the probe's own spread, ±20–30 %
/// on a virtual machine (§4.1), and within what a loaded 3-CPU macOS runner reaches: 5.9 times in the run where ten
/// was not reached (diag-liveness-37), against 23 to 200 times on Linux and macOS before the rule.
const QUEUED_FACTOR: u64 = 4;
/// Shape: spinning threads per logical CPU, as the window rule's A/B loads a machine (A-223).
const SPINNERS_PER_CPU: usize = 2;
/// Shape: the bounded volume each request creates and destroys.
const VOLUME_BYTES: u64 = 1 << 20;

/// A-223 (i), §4.3, §4.7. Do: start a two-shard daemon as derived for this machine (its estimate's weighting window
/// shortened so the test is quick); beside two spinning threads a logical CPU, send one create and its destroy each
/// millisecond from one client until some shard's wake estimate has risen to four times the boot prior. Expect: every
/// shard's idle window is still no longer than the window the daemon derived at boot, while the estimate it would
/// have been priced on climbed. Before, the window was the estimate times 100 and grew with it.
#[test]
fn a_paced_clients_shards_keep_their_idle_window_while_load_queues_their_wakes() {
  let profile = common::machine_profile();
  let instance = format!("srv-idle-window-{}", std::process::id());
  let mut config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  // The run's context: the window the daemon derived, as its boot log prints it.
  for line in config.derivations.iter().filter(|line| {
    ["provision_floor_ns", "loaded_wake_ratio", "idle_window_ns"]
      .iter()
      .any(|name| line.starts_with(name))
  }) {
    println!("slates daemon: {line}");
  }
  let boot_window_ns = config.runtime.spin_ns;
  let tracking = config
    .runtime
    .wake_tracking
    .as_mut()
    .expect("a daemon's shards track their wake");
  tracking.shift = SHIFT;
  let prior_ns = tracking.prior_ns;
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", "idle-window"),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = common::client::connect(&instance);
  client.spin_for(None);
  let queued = |daemon: &Daemon| {
    daemon
      .shard_pulses()
      .iter()
      .any(|pulse| pulse.wake_cost_ns >= QUEUED_FACTOR.saturating_mul(prior_ns))
  };
  let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
  let stop = AtomicBool::new(false);
  let sent = std::thread::scope(|scope| {
    for _ in 0..cpus * SPINNERS_PER_CPU {
      scope.spawn(|| {
        while !stop.load(Ordering::Relaxed) {
          std::hint::spin_loop();
        }
      });
    }
    let mut sent = 0;
    while sent < LEAST_REQUESTS || (sent < MOST_REQUESTS && !queued(&daemon)) {
      std::thread::park_timeout(PACE);
      let volume = client
        .create(&CreateSpec {
          name: format!("paced-{sent}"),
          size: SizeClass::Bounded {
            limit: VOLUME_BYTES,
          },
          names: NamePolicy::Exact,
          require_locked: false,
          base: None,
        })
        .unwrap();
      client.destroy(volume).unwrap();
      sent += 1;
    }
    stop.store(true, Ordering::Relaxed);
    sent
  });
  let pulses = daemon.shard_pulses();
  drop(client);
  daemon.stop();
  for pulse in &pulses {
    assert!(
      pulse.idle_window_ns <= boot_window_ns,
      "shard {}'s idle window grew with the load: {} ns against {boot_window_ns} ns at boot, its estimate {} ns \
       against a boot prior of {prior_ns} ns, after {sent} paced requests: {pulses:?}",
      pulse.shard,
      pulse.idle_window_ns,
      pulse.wake_cost_ns
    );
  }
  assert!(
    pulses
      .iter()
      .any(|pulse| pulse.wake_cost_ns >= QUEUED_FACTOR.saturating_mul(prior_ns)),
    "the spinners queued a woken shard, so its estimate rose to {QUEUED_FACTOR} times the boot prior of {prior_ns} \
     ns within {sent} paced requests: {pulses:?}"
  );
}
