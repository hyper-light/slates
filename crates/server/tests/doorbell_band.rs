//! The daemon's doorbell thread runs in macOS's real-time band (84a, A-868; §4.7 "Wake strategy"). On macOS a client's
//! request to a parked shard rings the bootstrap object's word, and the doorbell thread wakes and kicks the shard. Under
//! load that wake was the request's tail: on CI's macos-latest, beside two spinners a CPU, "rung -> bell thread woke"
//! measured 0.48-2.95 ms at the median of the p99 tail and 5.76-23.5 ms in the p999 tail, while every other hop took
//! microseconds (run 45091701553, its stamped rounds). The shards and a client's reply wait already run in the band
//! (A-831); the doorbell thread ran time-share and waited out its neighbours' quanta. Off macOS the band is not the
//! platform's and the test skips loudly.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// A-868. Do: start a two-shard daemon as the tests do, and ask whether its doorbell thread holds the time-constraint
/// policy. Expect, on macOS: it does. Until A-868 it did not: the thread ran time-share, the one thread on a parked
/// shard's wake path outside the band.
#[test]
fn the_doorbell_thread_runs_in_the_real_time_band() {
  if !cfg!(target_os = "macos") {
    eprintln!(
      "skipping: Mach's time-constraint policy is macOS's, and only macOS and Windows run a doorbell thread"
    );
    return;
  }
  let profile = common::machine_profile();
  let instance = format!("doorbell-band-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(2));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", "doorbell-band"),
    },
  )
  .unwrap();
  let held = daemon.doorbell_real_time();
  daemon.stop();
  assert_eq!(
    held,
    Some(true),
    "the doorbell thread holds the time-constraint policy, as the shards and a client's reply wait do"
  );
}
