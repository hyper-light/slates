//! A forward refused by its owner shard's full control channel is sent again once the channel has room, whether or not
//! another request comes (§4.3 "admission limit": "backpressure, never a drop: kept and retried next round"), told by the
//! owner's drain that makes the room (`slates_rt::room`). The serve loop idled when no client ring held a request and
//! nothing was deferred, and a kept forward was neither, so the shard slept with the forward kept until some other
//! request woke it: at 10,000 clients over one green, every shard forwarding to the green's owner, requests waited 10 to
//! 42 s and the daemon idled (5 min 22 s of CPU in 30 min on the Studio, 2026-11-07). From then until 2026-11-17 the loop
//! polled for the room once per idle window; now nothing but the owner's word moves a kept forward while its shard
//! idles, so this test fails if that word is lost.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
#![cfg(unix)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slates_client::{CreateSpec, NamePolicy, SizeClass};
use slates_ipc::protocol::{ReplyBody, RequestBody};
use slates_server::daemon::{LIVENESS_BUDGET_NS, OBSERVE_BUDGET_NS};
use slates_server::state::ShardState;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;

/// Shape: two shards, so a client's shard and a volume's owner differ.
const TEST_SHARDS: u16 = 2;
/// Shape: how long the owner shard is held while its control channel is filled: half the liveness budget, and short of
/// the window in which the watchdog judges an unmoving shard wedged: this daemon has no anchor, and since A-232 that
/// verdict ends its process, this test binary with it. A-232's own held-daemon tests hold half a window for the same
/// reason. It must outlast the flood (5 to 20 ms for 41,125 spawns on an 18-CPU Mac under 36 spinning threads,
/// 2026-11-17), the forward meeting the full channel, and the client's shard going idle with the forward kept and asking
/// the owner for room: an idle window after its last work, R9's 50 µs floor on Linux and macOS since A-223's window
/// rule (0.3 to 21 ms in the same runs, when the window was a multiple of the wake estimate).
const HOLD: Duration = Duration::from_nanos(LIVENESS_BUDGET_NS / 2);
/// Shape: how long the kept forward's answer may take after the owner's last progress through the flood queued ahead of
/// it: the liveness budget, far past the one task (the forward) and the one reply home that follow the flood's last
/// task, and far short of forever. Never from the hold's end alone: the owner first runs the whole flood, in the order
/// its channel took it, and that is the machine's speed, not the forward's wait. 41,125 tasks took 64 ms to 4.33 s under
/// 36 spinners (2026-11-17, 30 runs), and a budget from the hold's end failed 8 of 30 runs at load 35.
const ANSWER_WAIT: Duration = Duration::from_nanos(LIVENESS_BUDGET_NS);
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
  // The hold begins when the shard runs the observation, which ends otherwise by its own answer or its typed refusal past
  // the daemon's observe budget (A-571's rule; until 2026-11-20 five seconds, `HOLD_START_WAIT`).
  while !held.load(Ordering::Acquire) {
    if waiter.is_finished() {
      // The flag is set before the observation ends, so a hold that began and ended since the first read shows here.
      assert!(
        held.load(Ordering::Acquire),
        "the hold's observation ended before the hold began"
      );
      break;
    }
    std::thread::yield_now();
  }
  waiter
}

/// The room wake's counts on the shard at `partition` (`slates_rt::room`): its tasks' asks for room in a full channel,
/// the tasks a drain's word woke, and its idle window in nanoseconds, the window its serve loop polls for after its last
/// work (`slates_rt::shard::Counters::idle_window_ns`).
fn room_counts(daemon: &Daemon, partition: usize) -> (u64, u64, u64) {
  daemon
    .observation(
      Some(daemon.shards()[partition]),
      OBSERVE_BUDGET_NS,
      |_: &mut ShardState| {
        slates_rt::registry::with_current(|context| {
          let counters = context.counters();
          (
            counters.room_waits,
            counters.room_wakes,
            counters.idle_window_ns,
          )
        })
        .unwrap_or_default()
      },
    )
    .unwrap()
    .wait()
    .unwrap()
}

/// §4.3, §4.7. Do: on a two-shard daemon, create a volume the shard not serving the client owns; hold that shard and
/// fill its control channel with tasks that count their runs; send one `Status` of the volume, which the client's shard
/// forwards and the full channel refuses, so the forward is kept and the client's shard, going idle, asks the owner for
/// room; end the hold, and send nothing else. Expect: the answer within the liveness budget of the owner's last
/// progress through the flood queued ahead of the forward, and the client's shard woken by the owner's drain (its room
/// counts, the non-vacuity check). Before 2026-11-07 the client's shard idled with the forward kept and nothing woke it;
/// with the owner's word lost, nothing does.
#[test]
fn a_forward_kept_for_a_full_channel_is_sent_once_the_channel_has_room() {
  static HELD: AtomicBool = AtomicBool::new(false);
  static FLOOD_RAN: AtomicU64 = AtomicU64::new(0);
  let profile = common::machine_profile();
  let instance = format!("srv-forward-retry-{}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS)),
    SegmentSource::Create {
      name: common::fixture_object("slates-seg", "forward-retry"),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = common::client::connect(&instance);
  let origin = serving(&daemon, client.client_id());
  let owner = 1 - origin;
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
  let flooded = daemon
    .flood_control_channel_counting(owner, &FLOOD_RAN)
    .unwrap();
  let word = client
    .begin(&RequestBody::Status { volume })
    .unwrap()
    .word();
  holding.join().unwrap();
  let released = Instant::now();
  // Each step of the owner through the flood starts the answer's wait again, so the wait is the forward's own: the
  // forward is queued behind the flood and runs after its last task.
  let (mut ran, mut progressed) = (0u64, released);
  let mut answer = None;
  while answer.is_none() && progressed.elapsed() < ANSWER_WAIT {
    answer = client.poll_reply_word(word).unwrap();
    let now_ran = FLOOD_RAN.load(Ordering::Relaxed);
    if now_ran != ran {
      (ran, progressed) = (now_ran, Instant::now());
    }
    std::thread::yield_now();
  }
  let (waited, since_progress) = (released.elapsed(), progressed.elapsed());
  let (asks, wakes, idle_window_ns) = room_counts(&daemon, origin);
  drop(client);
  daemon.stop();
  assert!(flooded > 0, "the owner's control channel was filled");
  assert!(
    matches!(answer, Some(ReplyBody::Status { .. })),
    "the kept forward was answered within {ANSWER_WAIT:?} of the owner's last progress through the {flooded} tasks \
     queued ahead of it ({ran} ran; {waited:?} after the hold's end, {since_progress:?} after the last progress): \
     {answer:?}"
  );
  assert!(
    asks >= 1 && wakes >= 1,
    "the client's shard asked the owner for room and the owner's drain woke it: {asks} asks, {wakes} wakes (an idle \
     window of {idle_window_ns} ns against a hold of {HOLD:?})"
  );
}
