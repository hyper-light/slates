//! The daemon's liveness beat tells a starved shard from a hung one (§4.14 `daemon.alive`, A-228). The anchor kills a
//! daemon whose beat lapses, so the beat must keep coming while every shard is alive — a starved one included — and stop
//! once a shard is wedged (no step and no CPU for a window) or spinning (a window of CPU without a step). Each test
//! drives an in-process two-shard daemon, puts work on a shard through an observation (a closure run as one poll on
//! that shard's thread), and watches the anchor's heartbeat word in the segment, as the anchor does.
//!
//! Before A-228 the beat was a task on the control shard: a starved poll there (a thread kept off its core, running a
//! little at a time) stopped it for as long as the poll lasted, and the anchor killed live daemons on CI's busy Windows
//! runners; and a wedged or spinning shard other than the control shard never stopped it at all.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::panic_in_result_fn
)]

mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use slates_rt::shard::ShardId;
use slates_server::daemon::{HEARTBEAT_NS, LIVENESS_BUDGET_NS, OBSERVE_BUDGET_NS};
use slates_server::state::ShardState;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: the liveness window, the anchor's budget.
const WINDOW: Duration = Duration::from_nanos(LIVENESS_BUDGET_NS);
/// Shape: one beat interval.
const BEAT: Duration = Duration::from_nanos(HEARTBEAT_NS);
/// Shape: how often the test reads the beat word: a tenth of a beat, so a gap is measured to that.
const SAMPLE: Duration = Duration::from_nanos(HEARTBEAT_NS / 10);
/// Shape: a starved poll's CPU burst and its time off the CPU: a fifth of the time on it, as a thread kept off its core
/// by spinners runs a little at a time.
const BURST: Duration = Duration::from_millis(10);
const OFF: Duration = Duration::from_millis(40);

fn daemon(name: &str) -> (Daemon, ShardId, ShardId) {
  let profile = common::machine_profile();
  let instance = format!("liveness-{name}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(2));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-liveness-{name}-{}", std::process::id()),
    },
  )
  .unwrap();
  let control = daemon.shards()[0];
  let other = daemon.shards()[1];
  (daemon, control, other)
}

/// The anchor's heartbeat word now.
fn beat(daemon: &Daemon) -> u64 {
  daemon.segment().supervision().unwrap().heartbeat_ns()
}

/// Waits until the daemon serves and has beaten twice, so a test starts from a booted, beating daemon; the returned
/// connection is held for the test, so no client's reap runs inside it. The beat starts before the boot's client-id
/// recovery asks every shard (`control_loop`), and a connection is accepted only after it, so a beat alone let a test's
/// block land inside the boot, which then refused the boot (`liveness-repro.log` on adacachy, 2026-11-07).
fn beating(daemon: &Daemon) -> slates_ipc::Connected {
  let started = Instant::now();
  let serving = loop {
    match slates_ipc::connect(daemon.instance()) {
      Ok(connected) => break connected,
      Err(slates_ipc::IpcError::DaemonUnavailable { .. }) => {
        assert!(started.elapsed() < WINDOW * 10, "the daemon began serving");
        wait(SAMPLE);
      }
      Err(refused) => panic!("the daemon refused a client: {refused}"),
    }
  };
  let first = beat(daemon);
  while beat(daemon) == first || beat(daemon) == 0 {
    assert!(started.elapsed() < WINDOW * 10, "the daemon began beating");
    wait(SAMPLE);
  }
  serving
}

/// Waits `span` on the test thread (a channel nobody sends on, as the lint wall wants).
fn wait(span: Duration) {
  let (_hold, rx) = std::sync::mpsc::channel::<()>();
  let _ = rx.recv_timeout(span);
}

/// Watches the beat for `span` while `busy` is raised by the work under test: the largest gap between two successive
/// beats seen, and when (since the watch began) the beat last advanced.
fn watch_beat(daemon: &Daemon, span: Duration) -> (Duration, Duration) {
  let started = Instant::now();
  let mut last = beat(daemon);
  let mut last_seen = Duration::ZERO;
  let mut largest = Duration::ZERO;
  while started.elapsed() < span {
    wait(SAMPLE);
    let now = beat(daemon);
    if now != last {
      if last != 0 {
        largest = largest.max(Duration::from_nanos(now.saturating_sub(last)));
      }
      last = now;
      last_seen = started.elapsed();
    }
  }
  // A beat still missing at the watch's end is a gap too.
  largest = largest.max(started.elapsed().saturating_sub(last_seen));
  (largest, last_seen)
}

/// Puts `work` on `shard` as one poll, run on its own thread; the handle joins the observation's answer.
fn on_shard(
  daemon: &Daemon,
  shard: ShardId,
  work: impl FnOnce() + Clone + Send + 'static,
) -> std::thread::JoinHandle<()> {
  let observation = daemon
    .observation(Some(shard), OBSERVE_BUDGET_NS, move |_: &mut ShardState| {
      work()
    })
    .unwrap();
  std::thread::spawn(move || {
    let _ = observation.wait();
  })
}

/// Runs on the CPU for `burst`, without yielding.
fn spin_for(burst: Duration) {
  let started = Instant::now();
  while started.elapsed() < burst {
    std::hint::spin_loop();
  }
}

/// A-228, §4.14. Do: on the control shard, run one poll for a window and a half that is on the CPU a fifth of the time
/// (10 ms bursts, 40 ms off it), as a starved thread runs. Expect: the beat never gaps a window, since the shard keeps
/// making CPU progress and runs far less than a window of CPU. Before A-228 the beat, a task on that shard, waited out
/// the whole poll.
#[test]
fn a_starved_poll_on_the_control_shard_does_not_stop_the_beat() {
  let (daemon, control, _) = daemon("starved");
  let _serving = beating(&daemon);
  let span = WINDOW + WINDOW / 2;
  let worker = on_shard(&daemon, control, move || {
    let started = Instant::now();
    while started.elapsed() < span {
      spin_for(BURST);
      #[allow(clippy::disallowed_methods)]
      // the thread kept off its core, which is what the test models
      std::thread::sleep(OFF);
    }
  });
  let (largest, _) = watch_beat(&daemon, span);
  worker.join().unwrap();
  println!("largest beat gap through a starved poll: {largest:?}");
  assert!(
    largest < WINDOW,
    "a starved control shard stopped the beat for {largest:?}, against the {WINDOW:?} window"
  );
  daemon.stop();
}

/// A-228, §4.14. Do: on the shard that is not the control shard, block one poll with no CPU for three windows (a
/// deadlock, or a host call that does not return), noting when the block began on the shard. Expect: the beat stops — a
/// gap of a whole window — no later than a window and three beats after the block began. Before A-228 the control shard
/// went on beating, so the anchor never saw the wedged shard. The stop is timed from the block's own start, since under
/// load the observation reaches the shard some time after it is sent, and the beat resumes once the block ends, so only
/// the first window-long gap counts.
#[test]
fn a_wedged_shard_stops_the_beat() {
  let (daemon, _, other) = daemon("wedged");
  let _serving = beating(&daemon);
  let held = WINDOW * 3;
  let base = Instant::now();
  let began_us = &*Box::leak(Box::new(AtomicU64::new(0)));
  let worker = on_shard(&daemon, other, move || {
    began_us.store(
      u64::try_from(base.elapsed().as_micros()).unwrap().max(1),
      Ordering::Release,
    );
    let (_hold, rx) = std::sync::mpsc::channel::<()>();
    let _ = rx.recv_timeout(held);
  });
  // Each beat's arrival, by the test's clock since `base`, until a whole window passes without one. The watch lasts
  // until the block ends, timed from the block's own start: under load the observation reached the shard 1.05 s and
  // 1.39 s after `base`, and a watch of `held` from `base` ended before the window-long gap could be seen, though the
  // beat had stopped 0.97 s and 1.00 s after the block began (2 of 10 runs at load 126–147 on adacachy, 2026-11-07).
  // A block that never began ends the watch once its observation has answered.
  let mut last = beat(&daemon);
  let mut last_seen = base.elapsed();
  let mut stopped_at = None;
  loop {
    let answered = worker.is_finished();
    let began = began_us.load(Ordering::Acquire);
    let block_over = began > 0 && base.elapsed() >= Duration::from_micros(began) + held;
    if block_over || (began == 0 && answered) {
      break;
    }
    wait(SAMPLE);
    let now = beat(&daemon);
    if now != last {
      last = now;
      last_seen = base.elapsed();
    } else if began_us.load(Ordering::Acquire) > 0
      && base.elapsed().saturating_sub(last_seen) >= WINDOW
    {
      stopped_at = Some(last_seen);
      break;
    }
  }
  worker.join().unwrap();
  let began = Duration::from_micros(began_us.load(Ordering::Acquire));
  let stopped_at = stopped_at.unwrap_or_else(|| {
    panic!(
      "a wedged shard never stopped the beat for a whole window: the block began {began:?} in, the beat last advanced \
       {last_seen:?} in, the watch ended {:?} in",
      base.elapsed()
    )
  });
  let after = stopped_at.saturating_sub(began);
  println!("wedged shard: the block began {began:?} in, and the beat stopped {after:?} after it");
  assert!(
    after <= WINDOW + BEAT * 3,
    "the beat stopped {after:?} after the shard wedged, past a window and three beats"
  );
  daemon.stop();
}

/// A-228, §4.14. Do: on the shard that is not the control shard, spin one poll on the CPU without yielding, until the
/// beat stops or ten windows pass. Expect: the beat stops (the shard has run a window of CPU without a step). How long
/// that takes is the shard's own CPU's pace: a window on a free core, longer where the spinner itself is starved. Before
/// A-228 nothing saw it.
#[test]
fn a_spinning_shard_stops_the_beat() {
  let (daemon, _, other) = daemon("spinning");
  let _serving = beating(&daemon);
  let bound = WINDOW * 10;
  let running = &*Box::leak(Box::new(AtomicBool::new(true)));
  let worker = on_shard(&daemon, other, move || {
    let started = Instant::now();
    while running.load(Ordering::Relaxed) && started.elapsed() < bound {
      std::hint::spin_loop();
    }
  });
  let started = Instant::now();
  let mut last = beat(&daemon);
  let mut last_seen = Instant::now();
  let mut stopped_after = None;
  while started.elapsed() < bound {
    wait(SAMPLE);
    let now = beat(&daemon);
    if now != last {
      last = now;
      last_seen = Instant::now();
    } else if last_seen.elapsed() >= WINDOW {
      stopped_after = Some(last_seen.duration_since(started));
      break;
    }
  }
  running.store(false, Ordering::Relaxed);
  worker.join().unwrap();
  println!("spinning shard: the beat stopped {stopped_after:?} after the spin began");
  assert!(
    stopped_after.is_some(),
    "a spinning shard never stopped the beat in {bound:?}"
  );
  daemon.stop();
}

/// A-228, §4.14. Do: on the control shard, block one poll for half a window (a slow host call that finishes). Expect:
/// the beat never gaps a window: a block shorter than the window is not a hang.
#[test]
fn a_slow_blocking_call_shorter_than_a_window_does_not_stop_the_beat() {
  let (daemon, control, _) = daemon("slow");
  let _serving = beating(&daemon);
  let worker = on_shard(&daemon, control, move || {
    let (_hold, rx) = std::sync::mpsc::channel::<()>();
    let _ = rx.recv_timeout(WINDOW / 2);
  });
  let (largest, _) = watch_beat(&daemon, WINDOW + WINDOW / 2);
  worker.join().unwrap();
  println!("largest beat gap through a half-window block: {largest:?}");
  assert!(
    largest < WINDOW,
    "a half-window block stopped the beat for {largest:?}"
  );
  daemon.stop();
}

/// A-228, §4.14, the busy machine (Ada's rule: the normal case). Do: run two spinning threads a logical CPU beside the
/// daemon for three windows. Expect: the beat never gaps a window.
#[test]
fn a_daemon_beside_two_spinners_a_core_keeps_beating() {
  let (daemon, _, _) = daemon("busy");
  let _serving = beating(&daemon);
  let cpus = std::thread::available_parallelism().map_or(1, usize::from);
  let stop = &*Box::leak(Box::new(AtomicBool::new(false)));
  let spinners: Vec<_> = (0..cpus * 2)
    .map(|_| {
      std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
          spin_for(BURST);
        }
      })
    })
    .collect();
  let (largest, _) = watch_beat(&daemon, WINDOW * 3);
  stop.store(true, Ordering::Relaxed);
  for spinner in spinners {
    spinner.join().unwrap();
  }
  println!("largest beat gap beside {} spinners: {largest:?}", cpus * 2);
  assert!(
    largest < WINDOW,
    "the daemon's beat gapped {largest:?} beside two spinners a core"
  );
  daemon.stop();
}

/// Format: the environment variable under which [`a_stopping_daemon_role`] runs its role; unset, it returns at once.
const STOP_ROLE_ENV: &str = "SLATES_LIVENESS_STOP_ROLE";
/// Format: the tag the role prints once its daemon has stopped, so the parent knows the role ran to its end.
const STOPPED_TAG: &str = "LIVENESS-ROLE-STOPPED";
/// Format: what the watchdog says when the control loop ends on its own, after which the anchor restarts the daemon.
const CONTROL_ENDED: &str = "the control loop ended";

/// A-228, §4.14. Do: in a child process (this binary re-invoked as [`a_stopping_daemon_role`]), start a two-shard
/// daemon, wait for its beat, put a poll of three beats on its second shard, and stop the daemon while that poll runs,
/// so the shutdown spans beats after the control loop has ended. Expect: the child prints its stopped tag and its
/// standard error never says the control loop ended, a line that tells the anchor's reader the daemon is to be
/// restarted. Before, the watchdog, stopped last, saw the control loop end mid-shutdown and said so: under two
/// spinners a core, 8 of the 18 daemons one `nfs_hostile` run stopped said it, each just before its test passed (an
/// 18-core Mac, 2026-11-07).
#[test]
fn a_stopped_daemon_does_not_say_its_control_loop_ended() {
  let output = std::process::Command::new(std::env::current_exe().unwrap())
    .args([
      "--ignored",
      "--exact",
      "a_stopping_daemon_role",
      "--nocapture",
    ])
    .env(STOP_ROLE_ENV, "1")
    .output()
    .unwrap();
  let said = format!(
    "{}{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(
    output.status.success() && said.contains(STOPPED_TAG),
    "the role stopped its daemon: {said}"
  );
  assert!(
    !said.contains(CONTROL_ENDED),
    "a daemon's own stop said its control loop ended: {said}"
  );
}

/// The role [`a_stopped_daemon_does_not_say_its_control_loop_ended`] runs in a child process.
#[test]
#[ignore = "a role: run by a_stopped_daemon_does_not_say_its_control_loop_ended in a child process"]
fn a_stopping_daemon_role() {
  if std::env::var_os(STOP_ROLE_ENV).is_none() {
    return;
  }
  let (daemon, _control, other) = daemon("stopping");
  beating(&daemon);
  let held: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
  let poll = on_shard(&daemon, other, move || {
    held.store(true, Ordering::Release);
    wait(BEAT * 3);
  });
  while !held.load(Ordering::Acquire) {
    wait(SAMPLE);
  }
  daemon.stop();
  poll.join().unwrap();
  eprintln!("{STOPPED_TAG}");
}
