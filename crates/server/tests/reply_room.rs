//! A reply carried home to the client's shard is never dropped (§4.3 "admission limit": "backpressure, never a drop";
//! CLAUDE.md §2 items 8 and 9): a forwarded verb's answer, and a cross-shard call's result, wait for room on a full
//! channel for as long as the shard they go to lives. Until 2026-11-09 they waited one coordinator period
//! (`xshard::REPLY_ROOM_NS`) and were dropped, counted `xshard.reply_dropped`, and the client waited out its whole reply
//! deadline: about 4,300 a run of loadtest-scale's 3,000-client rung on main 80be552a.
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
use slates_server::daemon::{HEARTBEAT_NS, LIVENESS_BUDGET_NS, OBSERVE_BUDGET_NS};
use slates_server::state::ShardState;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;

/// Shape: two shards, so a client's shard and a volume's owner differ.
const TEST_SHARDS: u16 = 2;
/// Shape: how long the owner is held so the forward waits there, short of the client's own hold.
const OWNER_HOLD: Duration = Duration::from_nanos(HEARTBEAT_NS);
/// Shape: how long the client's shard is held with its control channel full: past the owner's hold and a coordinator
/// period more, so the owner's answer meets the full channel for longer than it used to wait.
const CLIENT_HOLD: Duration = Duration::from_nanos(HEARTBEAT_NS * 5);
/// Shape: how long the answer may take once the client's shard runs again: the liveness budget, far past its drain of
/// the flood and far short of the client's reply deadline.
const ANSWER_WAIT: Duration = Duration::from_nanos(LIVENESS_BUDGET_NS);
/// Shape: how long the test waits for a hold to begin.
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

/// Holds the shard at `partition` for `hold`: its thread runs nothing else meanwhile. Returns once the hold has begun,
/// with the thread that waits for it to end.
fn hold(
  daemon: &Daemon,
  partition: usize,
  held: &'static AtomicBool,
  hold: Duration,
) -> std::thread::JoinHandle<()> {
  let shard = daemon.shards()[partition];
  let observation = daemon
    .observation(Some(shard), OBSERVE_BUDGET_NS, move |_: &mut ShardState| {
      held.store(true, Ordering::Release);
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(hold);
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

/// §4.3, CLAUDE.md §2 items 8 and 9. Do: on a two-shard daemon, create a volume the shard not serving the client owns;
/// hold the owner, send a `Status` of the volume (forwarded to the owner, where it waits), then hold the client's shard
/// with its control channel filled for longer than the owner's hold and a coordinator period more, so the owner's answer
/// meets the full channel; release both. Expect: the answer within the liveness budget of the client's shard running
/// again. Before, it was dropped after one coordinator period and the client waited out its reply deadline.
#[test]
fn a_forwarded_answer_waits_for_room_on_its_clients_shard_rather_than_being_dropped() {
  static OWNER_HELD: AtomicBool = AtomicBool::new(false);
  static CLIENT_HELD: AtomicBool = AtomicBool::new(false);
  let profile = common::machine_profile();
  let instance = format!("srv-reply-room-{}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS)),
    SegmentSource::Create {
      name: format!("slates-seg-reply-room-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = common::client::connect(&instance);
  let mine = serving(&daemon, client.client_id());
  let owner = 1 - mine;
  let volume = client
    .create(&CreateSpec {
      name: name_on(owner, "answered"),
      size: SizeClass::Bounded { limit: 1 << 20 },
      names: NamePolicy::Exact,
      require_locked: false,
      base: None,
    })
    .unwrap();
  let owner_holding = hold(&daemon, owner, &OWNER_HELD, OWNER_HOLD);
  let word = client
    .begin(&RequestBody::Status { volume })
    .unwrap()
    .word();
  let client_holding = hold(&daemon, mine, &CLIENT_HELD, CLIENT_HOLD);
  let flooded = daemon.flood_control_channel_of(mine).unwrap();
  owner_holding.join().unwrap();
  client_holding.join().unwrap();
  let released = Instant::now();
  let mut answer = None;
  while released.elapsed() < ANSWER_WAIT && answer.is_none() {
    answer = client.poll_reply_word(word).unwrap();
    std::thread::yield_now();
  }
  let waited = released.elapsed();
  let dropped = daemon
    .refusals_on_every_shard()
    .unwrap()
    .get("xshard.reply_dropped")
    .copied()
    .unwrap_or(0);
  drop(client);
  daemon.stop();
  assert!(flooded > 0, "the client's control channel was filled");
  assert_eq!(dropped, 0, "no reply was dropped");
  assert!(
    matches!(answer, Some(ReplyBody::Status { .. })),
    "the answer came within {ANSWER_WAIT:?} of the client's shard running again (waited {waited:?}): {answer:?}"
  );
}
