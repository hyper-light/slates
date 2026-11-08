//! A forward refused by its owner shard's full control channel is sent again once the channel has room, whether or not
//! another request comes (§4.3 "admission limit": "backpressure, never a drop: kept and retried next round"). The serve
//! loop idled when no client ring held a request and nothing was deferred, and a kept forward was neither, so the
//! shard slept with the forward kept until some other request woke it: at 10,000 clients over one green, every shard
//! forwarding to the green's owner, requests waited 10 to 42 s and the daemon idled (5 min 22 s of CPU in 30 min on the
//! Studio, 2026-11-07).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
#![cfg(unix)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use slates_client::{CreateSpec, NamePolicy, SizeClass};
use slates_ipc::protocol::{ReplyBody, RequestBody};
use slates_server::daemon::{LIVENESS_BUDGET_NS, OBSERVE_BUDGET_NS};
use slates_server::state::ShardState;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;

/// Shape: two shards, so a client's shard and a volume's owner differ.
const TEST_SHARDS: u16 = 2;
/// Shape: how long the owner shard is held while its control channel is filled: the liveness budget, long enough for
/// the forward to meet the full channel and be kept.
const HOLD: Duration = Duration::from_nanos(LIVENESS_BUDGET_NS);
/// Shape: how long the kept forward's answer may take once the owner runs again: the liveness budget, far past the
/// owner's drain of the flood (microseconds) and the origin's retry, and far short of forever.
const ANSWER_WAIT: Duration = Duration::from_nanos(LIVENESS_BUDGET_NS);
/// Shape: how long the test waits for the hold to begin.
const HOLD_START_WAIT: Duration = Duration::from_secs(5);
/// Shape: names tried for one the other shard owns.
const NAME_TRIES: usize = 64;

/// The shard (by position) that serves the client `client_id`.
fn serving(daemon: &Daemon, client_id: u32) -> usize {
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

/// A name the shard at `partition` owns.
fn name_on(partition: usize, tag: &str) -> String {
  (0..NAME_TRIES)
    .map(|n| format!("{tag}-{n}"))
    .find(|name| {
      usize::from(slates_server::verbs::owner_of_name(
        name,
        usize::from(TEST_SHARDS),
      )) == partition
    })
    .expect("a name the partition owns")
}

/// Holds the shard at `partition` for [`HOLD`]: its thread runs nothing else meanwhile. Returns once the hold has begun,
/// with the thread that waits for it to end.
fn hold(
  daemon: &Daemon,
  partition: usize,
  held: &'static AtomicBool,
) -> std::thread::JoinHandle<()> {
  let shard = daemon.shards()[partition];
  let observation = daemon
    .observation(Some(shard), OBSERVE_BUDGET_NS, move |_: &mut ShardState| {
      held.store(true, Ordering::Release);
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(HOLD);
    })
    .unwrap();
  let waiter = std::thread::spawn(move || observation.wait().unwrap());
  let started = Instant::now();
  while !held.load(Ordering::Acquire) {
    assert!(started.elapsed() < HOLD_START_WAIT, "the hold began");
    std::thread::yield_now();
  }
  waiter
}

/// §4.3, §4.7. Do: on a two-shard daemon, create a volume the shard not serving the client owns; hold that shard and
/// fill its control channel; send one `Status` of the volume, which the client's shard forwards and the full channel
/// refuses, so the forward is kept; end the hold, and send nothing else. Expect: the answer within the liveness budget
/// of the hold's end. Before, the client's shard idled with the forward kept and nothing woke it.
#[test]
fn a_forward_kept_for_a_full_channel_is_sent_once_the_channel_has_room() {
  static HELD: AtomicBool = AtomicBool::new(false);
  let profile = common::machine_profile();
  let instance = format!("srv-forward-retry-{}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS)),
    SegmentSource::Create {
      name: format!("slates-seg-forward-retry-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = common::client::connect(&instance);
  let owner = 1 - serving(&daemon, client.client_id());
  let volume = client
    .create(&CreateSpec {
      name: name_on(owner, "kept"),
      size: SizeClass::Bounded { limit: 1 << 20 },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let holding = hold(&daemon, owner, &HELD);
  let flooded = daemon.flood_control_channel_of(owner).unwrap();
  let word = client
    .begin(&RequestBody::Status { volume })
    .unwrap()
    .word();
  holding.join().unwrap();
  let released = Instant::now();
  let mut answer = None;
  while released.elapsed() < ANSWER_WAIT && answer.is_none() {
    answer = client.poll_reply_word(word).unwrap();
    std::thread::yield_now();
  }
  let waited = released.elapsed();
  eprintln!("owner shard {owner}; flooded {flooded}; answered after {waited:?}: {answer:?}");
  drop(client);
  daemon.stop();
  assert!(flooded > 0, "the owner's control channel was filled");
  assert!(
    matches!(answer, Some(ReplyBody::Status { .. })),
    "the kept forward was answered within {ANSWER_WAIT:?} of the hold's end (waited {waited:?}): {answer:?}"
  );
}
