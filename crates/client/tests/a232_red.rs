//! A-232's red (diagnostics only, never on main): a call to a live, starved daemon fails `Stalled`.
//!
//! An in-process daemon and a client in one process, the process starved by its caller (CPUWeight=1 beside
//! default-weight spinners on Linux CI, or nice 19 beside spinners elsewhere). The client makes sync calls with the
//! product's own deadlines for a bounded wall time and names every `Stalled` by the daemon's own watchdog: the looks
//! and withheld beats before and after the call (A-228). A `Stalled` while the watchdog withheld no beat is a call
//! failed against a daemon its own watchdog judged alive: the defect A-232 removes.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(unix)]

use std::time::{Duration, Instant};

use slates_client::{Client, ClientError, CreateSpec, Deadlines};
use slates_ipc::protocol::{NamePolicy, SizeClass};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: the quick profile's probe budget (milliseconds), as async_core measures.
const PROBE_MS: u64 = 5;
/// Shape: two shards, so a client's shard and the control shard differ.
const TEST_SHARDS: u16 = 2;
/// Shape: how long the rendezvous is retried while a starved daemon starts: far past any start seen starved.
const START_WAIT: Duration = Duration::from_secs(600);
/// Shape: the wall time the calls run for once the daemon answers: long enough for a starved shard to miss a reply
/// deadline many times, short enough to keep ten runs inside a lane.
const CALLING: Duration = Duration::from_secs(45);

fn deadlines() -> Deadlines {
  Deadlines::derive(
    slates_server::daemon::LIVENESS_BUDGET_NS,
    slates_db::replay::RECOVERY_BUDGET_NS,
  )
  .get()
}

fn connect(instance: &str) -> Client {
  let started = Instant::now();
  loop {
    match Client::connect(instance, deadlines()) {
      Ok(client) => return client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < START_WAIT =>
      {
        std::thread::yield_now();
      }
      Err(e) => panic!("connect: {e}"),
    }
  }
}

/// Do: start a daemon, make one volume, call `list` and its `status` in turn for [`CALLING`], each call bracketed by the watchdog's turns.
/// Expect (the red): one or more calls fail `Stalled` with no beat withheld across them. The test prints every such
/// call and its counts, and fails when it saw one, so the lane's exit code says red.
#[test]
fn a_call_to_a_live_starved_daemon_is_failed_stalled() {
  let profile = MachineProfile::measure_patiently(&ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
  .expect("the machine profile measures");
  let instance = format!("a232-red-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-a232-red-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon.bootstrap(true).expect("the local consensus group");
  let mut client = connect(&instance);
  // One volume, made before the calls: a create each turn grew `list`'s reply past the client's bulk area within a
  // minute ("reply too large for the bulk area", 1,381 of 2,942 calls on this Mac, 2026-11-13).
  let volume = client
    .create(&CreateSpec {
      name: "red".to_owned(),
      size: SizeClass::Bounded { limit: 1 << 20 },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .expect("the probe's volume");
  let began = Instant::now();
  let (mut calls, mut answered, mut stalled_live, mut stalled_judged, mut other) = (0u64, 0u64, 0u64, 0u64, 0u64);
  while began.elapsed() < CALLING {
    let before = daemon.watchdog_turns();
    let call_began = Instant::now();
    let result = if calls % 2 == 0 {
      client.list().map(|_| ())
    } else {
      client.status(volume).map(|_| ())
    };
    let took = call_began.elapsed();
    let after = daemon.watchdog_turns();
    calls += 1;
    match result {
      Ok(()) => answered += 1,
      Err(ClientError::Stalled { after_ns }) => {
        let withheld = |turns: Option<(u64, u64)>| turns.map(|(_, withheld)| withheld);
        let alive = before.is_some() && withheld(before) == withheld(after);
        if alive {
          stalled_live += 1;
        } else {
          stalled_judged += 1;
        }
        println!(
          "A232-STALLED call {calls} after {:.3} s (deadline {after_ns} ns) at {:.1} s; watchdog (looks, withheld) \
           before {before:?} after {after:?}; daemon gone {}; {}",
          took.as_secs_f64(),
          began.elapsed().as_secs_f64(),
          client.daemon_gone(),
          if alive { "LIVE: no beat withheld" } else { "a beat was withheld" }
        );
      }
      Err(error) => {
        other += 1;
        println!("A232-OTHER call {calls}: {error}");
      }
    }
  }
  println!(
    "A232-SUMMARY calls {calls} answered {answered} stalled_live {stalled_live} stalled_judged {stalled_judged} \
     other {other} watchdog {:?}",
    daemon.watchdog_turns()
  );
  assert_eq!(stalled_live, 0, "calls to a daemon its watchdog judged alive failed Stalled (A-232's red)");
}
