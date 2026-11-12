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
use slates_server::daemon::{HEARTBEAT_NS, OBSERVE_BUDGET_NS};
use slates_server::state::ShardState;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;

/// Shape: two shards, so a client's shard and a volume's owner differ.
const TEST_SHARDS: u16 = 2;
/// Shape: how long the owner is held so the forward waits there, short of the client's own hold.
const OWNER_HOLD: Duration = Duration::from_nanos(HEARTBEAT_NS);
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

/// What one attempt of [`a_forwarded_answer_waits_for_room_on_its_clients_shard_rather_than_being_dropped`] saw.
struct Attempt {
  /// Whether the owner's answer began waiting for room while the client's shard was held, and the client stayed held
  /// [`HELD_PAST_WAIT`] after: the precondition.
  held_past_wait: bool,
  /// Messages the client's control channel was filled with.
  flooded: usize,
  /// The answer, if it came within [`ANSWER_CAP`] of the release.
  answer: Option<ReplyBody>,
  /// How long after the release it came, or the cap.
  waited: Duration,
  /// The daemon's counters: results dropped, and waits for room begun, ended and held at once at most.
  dropped: u64,
  started: u64,
  ended: u64,
  peak: u64,
}

/// Shape: how long the answer may take once both shards are released: the daemon's own budget for a slow shard's
/// answer (`OBSERVE_BUDGET_NS`, ten liveness budgets). A dropped answer never comes, so no shorter bound is needed to
/// tell it apart. The answer sits behind the flood in the client's channel (41,125 messages on an 18-CPU Mac, drained
/// about four a step: 10,660 steps), so it comes when the flood is drained, 0.26 s at load 52 and up to 9.7 s at load 10
/// to 60 as each step ran long (measured 2026-11-12, the owner's wait ending 31–101 ms after the release each time).
const ANSWER_CAP: Duration = Duration::from_nanos(OBSERVE_BUDGET_NS);
/// Shape: attempts at the precondition (the owner's answer meets the client's full channel). On a loaded host the
/// client's shard can drain the flood before the owner answers, and then nothing waited: that attempt proves nothing
/// and is made again, up to this many times.
const ATTEMPTS: usize = 4;

/// One attempt, on its own daemon (`n` names it): the owner held, a `Status` forwarded to it, the client's shard held
/// with its channel flooded past the owner's hold, both released; the answer waited for up to [`ANSWER_CAP`].
fn attempt(n: usize) -> Attempt {
  static OWNER_HELD: AtomicBool = AtomicBool::new(false);
  static CLIENT_HELD: AtomicBool = AtomicBool::new(false);
  static CLIENT_RELEASE: AtomicBool = AtomicBool::new(false);
  OWNER_HELD.store(false, Ordering::Release);
  CLIENT_HELD.store(false, Ordering::Release);
  CLIENT_RELEASE.store(false, Ordering::Release);
  let profile = common::machine_profile();
  let instance = format!("srv-reply-room-{}-{n}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS)),
    SegmentSource::Create {
      name: format!("slates-seg-reply-room-{}-{n}", std::process::id()),
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
  let client_holding = hold_until(&daemon, mine, &CLIENT_HELD, &CLIENT_RELEASE);
  let flooded = daemon.flood_control_channel_of(mine).unwrap();
  owner_holding.join().unwrap();
  // The owner answers into the full channel: the client stays held until that wait has begun and two periods more, so
  // the answer waits past the period after which it used to be dropped.
  let watching = Instant::now();
  let mut held_past_wait = false;
  while watching.elapsed() < HOLD_START_WAIT {
    if waits_begun_on(&daemon, owner) > 0 {
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(HELD_PAST_WAIT);
      held_past_wait = true;
      break;
    }
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(HOLD_POLL);
  }
  CLIENT_RELEASE.store(true, Ordering::Release);
  client_holding.join().unwrap();
  let released = Instant::now();
  let mut answer = None;
  while released.elapsed() < ANSWER_CAP && answer.is_none() {
    answer = client.poll_reply_word(word).unwrap();
    std::thread::yield_now();
  }
  let waited = released.elapsed();
  let counted = daemon.refusals_on_every_shard().unwrap();
  let count = |name: &str| counted.get(name).copied().unwrap_or(0);
  let attempt = Attempt {
    held_past_wait,
    flooded,
    answer,
    waited,
    dropped: count("xshard.reply_dropped"),
    started: count("xshard.room_waits_started"),
    ended: count("xshard.room_waits_ended"),
    peak: count("xshard.room_waits_peak"),
  };
  drop(client);
  daemon.stop();
  attempt
}

/// Shape: how often a held shard looks whether its hold is over: a hundredth of a coordinator period.
const HOLD_POLL: Duration = Duration::from_nanos(HEARTBEAT_NS / 100);
/// Shape: how long the client's shard stays held once the owner's answer has begun waiting for room: two coordinator
/// periods, past the one period after which the answer used to be dropped, with a period's margin.
const HELD_PAST_WAIT: Duration = Duration::from_nanos(HEARTBEAT_NS * 2);

/// Holds the shard at `partition` until `release` is set (or [`OBSERVE_BUDGET_NS`] passes, the daemon's own bound on an
/// observation): its thread runs nothing else meanwhile. Returns once the hold has begun, with the thread that waits for
/// it to end.
fn hold_until(
  daemon: &Daemon,
  partition: usize,
  held: &'static AtomicBool,
  release: &'static AtomicBool,
) -> std::thread::JoinHandle<()> {
  let shard = daemon.shards()[partition];
  let observation = daemon
    .observation(Some(shard), OBSERVE_BUDGET_NS, move |_: &mut ShardState| {
      held.store(true, Ordering::Release);
      let began = Instant::now();
      while !release.load(Ordering::Acquire) && began.elapsed() < ANSWER_CAP {
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(HOLD_POLL);
      }
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

/// The waits for room the shard at `partition` has begun, asked of that shard alone.
fn waits_begun_on(daemon: &Daemon, partition: usize) -> u64 {
  daemon
    .observation(
      Some(daemon.shards()[partition]),
      OBSERVE_BUDGET_NS,
      |s: &mut ShardState| {
        s.refusals
          .get("xshard.room_waits_started")
          .copied()
          .unwrap_or(0)
      },
    )
    .unwrap()
    .wait()
    .unwrap()
}

/// §4.3, CLAUDE.md §2 items 8 and 9. Do: on a two-shard daemon, create a volume the shard not serving the client owns;
/// hold the owner, send a `Status` of the volume (forwarded to the owner, where it waits), hold the client's shard with
/// its control channel filled, release the owner, and keep the client's shard held until the owner's answer has begun
/// waiting for room there and [`HELD_PAST_WAIT`] (two coordinator periods) more; release it. An attempt in which the
/// answer did not begin waiting is made again, up to [`ATTEMPTS`]. Expect, of the attempt that held: no reply dropped,
/// the answer delivered (within [`ANSWER_CAP`], the daemon's budget for a slow shard), and the wait counted: begun, held
/// at once at least one, every one ended (the waiting is bounded by the requests in flight and counted, CLAUDE.md §2
/// item 8; `xshard::send_back`). Before, the answer was dropped after one coordinator period and never came: with
/// `send_back`'s wait bounded to a period again, this fails. Until 2026-11-12 the holds were fixed spans and the answer
/// had to come within the liveness budget (1 s) of the release, which a loaded host missed with the product behaving
/// (3 of 4 runs at load 40 to 80 on an 18-CPU Mac), and which let the answer find room within a period, so the old drop
/// passed it too.
#[test]
fn a_forwarded_answer_waits_for_room_on_its_clients_shard_rather_than_being_dropped() {
  let mut missed = 0;
  let held = loop {
    let attempt = attempt(missed);
    if attempt.held_past_wait {
      break attempt;
    }
    missed += 1;
    assert!(
      missed < ATTEMPTS,
      "the precondition never held: in {missed} attempts the owner's answer never began waiting on the full channel"
    );
  };
  println!(
    "the answer waited for room ({} waits, {} at most at once) and came {:?} after the release, {missed} attempt(s) missing the precondition first",
    held.started, held.peak, held.waited
  );
  assert!(held.flooded > 0, "the client's control channel was filled");
  assert_eq!(held.dropped, 0, "no reply was dropped");
  assert_eq!(
    held.ended, held.started,
    "every wait for room ended: none is left held"
  );
  assert!(
    held.peak >= 1,
    "the most held at once is counted: {}",
    held.peak
  );
  assert!(
    matches!(held.answer, Some(ReplyBody::Status { .. })),
    "the answer came within {ANSWER_CAP:?} of the release (waited {:?}): {:?}",
    held.waited,
    held.answer
  );
}
