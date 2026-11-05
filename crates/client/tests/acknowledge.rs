//! A synchronous call is never held behind the client's periodic acknowledgement (§4.9's completion records, A-216,
//! R9): the acknowledgement releases the client's records on every shard, and its reply waited for all of them, so the
//! call that sent it first paid every shard's turn, a parked shard's wake included. Since A-216 the reply is sent once
//! the client's own shard has released, and the other shards' releases run as tracked tasks. Driven through the Rust
//! client against an in-process two-shard daemon whose other shard is held busy.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::panic_in_result_fn
)]

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use slates_client::{Client, ClientError, CreateSpec, Deadlines, NamePolicy, SizeClass, VolumeId};
use slates_machine::{MachineError, MachineProfile, ProfileOptions};
use slates_server::daemon::{LIVENESS_BUDGET_NS, OBSERVE_BUDGET_NS};
use slates_server::state::ShardState;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: each probe's wall budget for the test's machine profile (milliseconds), as `client.rs` measures it.
const PROBE_MS: u64 = 5;
/// Shape: how long the other shard is held: one liveness budget, so a call that waits for it waits out a second, and a
/// call that does not returns in microseconds.
const HOLD: Duration = Duration::from_nanos(LIVENESS_BUDGET_NS);
/// Raised by the held shard when its hold begins, so the test's calls start inside it.
static HELD: AtomicBool = AtomicBool::new(false);

/// Shape: the volumes made while looking for one the client's own shard owns; with two shards, a run that finds none
/// in this many is a fault of the placement, not luck.
const PLACEMENT_TRIES: usize = 64;

fn profile() -> MachineProfile {
  static PROFILE: OnceLock<Result<MachineProfile, MachineError>> = OnceLock::new();
  let measured = PROFILE
    .get_or_init(|| {
      MachineProfile::measure_patiently(&ProfileOptions {
        budget_per_probe: Duration::from_millis(PROBE_MS),
        codecs: false,
        core_matrix: false,
      })
    })
    .clone()
    .expect("the machine profile measures");
  static RESERVED: OnceLock<bool> = OnceLock::new();
  let _ = RESERVED.get_or_init(|| {
    let config = DaemonConfig::derive(&measured, "ack-reserve", Some(2));
    slates_server::seal_keys::reserve(config.seal_key_slots)
  });
  measured
}

fn deadlines() -> Deadlines {
  Deadlines::derive(LIVENESS_BUDGET_NS, slates_db::replay::RECOVERY_BUDGET_NS).get()
}

fn two_shard_daemon(tag: &str) -> (Daemon, Client) {
  let profile = profile();
  let instance = format!("ack-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(2));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-ack-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture creates its local consensus group");
  let started = Instant::now();
  let client = loop {
    match Client::connect(&instance, deadlines()) {
      Ok(client) => break client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < HOLD.saturating_mul(10) =>
      {
        std::thread::yield_now();
      }
      Err(e) => panic!("{e}"),
    }
  };
  (daemon, client)
}

/// The index of the daemon's shard that serves `client_id`.
fn serving_shard(daemon: &Daemon, client_id: u32) -> usize {
  daemon
    .shards()
    .iter()
    .position(|shard| {
      daemon
        .observation(
          Some(*shard),
          OBSERVE_BUDGET_NS,
          move |state: &mut ShardState| {
            state
              .clients
              .iter()
              .any(|(_, slot)| slot.client_id == client_id)
          },
        )
        .unwrap()
        .wait()
        .unwrap()
    })
    .expect("a shard serves the client")
}

/// A volume the client's own shard owns, so a status of it is served there alone.
fn local_volume(daemon: &Daemon, client: &mut Client, mine: usize) -> VolumeId {
  for attempt in 0..PLACEMENT_TRIES {
    let id = client
      .create(&CreateSpec {
        name: format!("local-{attempt}"),
        size: SizeClass::Bounded { limit: 1 << 20 },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      })
      .unwrap();
    if usize::from(slates_server::verbs::owner_of(id)) == mine {
      return id;
    }
  }
  panic!(
    "no volume of {PLACEMENT_TRIES} landed on shard {mine} of {}",
    daemon.shards().len()
  );
}

/// A-216, §4.9, R9. Do: on a two-shard daemon, hold the shard that does not serve the client for a second, and
/// meanwhile make status calls on a volume the client's own shard owns until one of them sends the client's periodic
/// acknowledgement. Expect: that call returns in under half the hold, as every other does; the acknowledgement goes out
/// while the other shard is still held; and the client never has a second reply in flight (none dropped unawaited,
/// nothing outstanding after a call). Before A-216 the call that carried the acknowledgement waited for the held
/// shard's part of it and failed `Stalled` after the reply deadline (1 s).
#[test]
fn a_call_is_never_held_behind_the_periodic_acknowledgement() {
  let (daemon, mut client) = two_shard_daemon("held");
  let mine = serving_shard(&daemon, client.client_id());
  let id = local_volume(&daemon, &mut client, mine);
  let other = daemon
    .shards()
    .iter()
    .enumerate()
    .find(|(index, _)| *index != mine)
    .map(|(_, shard)| *shard)
    .unwrap();
  let hold = daemon
    .observation(Some(other), OBSERVE_BUDGET_NS, move |_: &mut ShardState| {
      HELD.store(true, Ordering::Release);
      // The held shard's thread does nothing else for the hold: the scatter's part on it waits.
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(HOLD);
    })
    .unwrap();
  let holder = std::thread::spawn(move || hold.wait());
  let waiting = Instant::now();
  while !HELD.load(Ordering::Acquire) {
    assert!(
      waiting.elapsed() < HOLD.saturating_mul(10),
      "the hold began"
    );
    std::thread::yield_now();
  }
  let held_at = Instant::now();
  let before = client.acknowledged();
  let mut acknowledging_call = None;
  while held_at.elapsed() < HOLD / 2 {
    let started = Instant::now();
    client.status(id).unwrap();
    let took = started.elapsed();
    assert!(
      client.outstanding().is_empty() && client.unawaited_dropped() == 0,
      "one reply in flight at a time: outstanding {:?}, dropped {}",
      client.outstanding(),
      client.unawaited_dropped()
    );
    if client.acknowledged() != before {
      acknowledging_call = Some(took);
      break;
    }
  }
  let sent_while_held = held_at.elapsed() < HOLD;
  holder.join().unwrap().unwrap();
  let took = acknowledging_call.expect("the client acknowledged within half the hold");
  assert!(
    sent_while_held,
    "the acknowledgement went out while the other shard was held"
  );
  assert!(
    took < HOLD / 2,
    "the call that sent the acknowledgement took {took:?}, against a {HOLD:?} hold of the other shard"
  );
  daemon.stop();
}
