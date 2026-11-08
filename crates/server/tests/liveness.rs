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

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use slates_rt::SlotHolder;
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

/// Waits until the daemon has beaten twice, so a test starts from a beating daemon.
fn beating(daemon: &Daemon) {
  let started = Instant::now();
  let first = beat(daemon);
  while beat(daemon) == first || beat(daemon) == 0 {
    assert!(started.elapsed() < WINDOW * 10, "the daemon began beating");
    wait(SAMPLE);
  }
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
  beating(&daemon);
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

/// Blocks a shard's poll on a channel nobody sends on for `held`: no CPU and never runnable.
fn block_on_a_channel(held: Duration) {
  wait(held);
}

/// Blocks a shard's poll in a real read of a pipe, which a helper thread writes to after `held`.
fn block_in_a_read(held: Duration) {
  use std::io::{Read, Write};
  let (mut reader, mut writer) = std::io::pipe().unwrap();
  std::thread::spawn(move || {
    wait(held);
    writer.write_all(b"x").unwrap();
  });
  let mut byte = [0_u8];
  reader.read_exact(&mut byte).unwrap();
}

/// Runs `block` for three windows as one poll on the shard that is not the control shard, noting when the block began
/// on the shard, and holds the beat to stopping — a gap of a whole window — no later than a window and three beats after
/// the block began. The stop is timed from the block's own start, since under load the observation reaches the shard
/// some time after it is sent, and the beat resumes once the block ends, so only the first window-long gap counts.
fn a_blocked_shard_stops_the_beat(name: &str, block: fn(Duration)) {
  let (daemon, _, other) = daemon(name);
  beating(&daemon);
  let held = WINDOW * 3;
  let base = Instant::now();
  let began_us = &*Box::leak(Box::new(AtomicU64::new(0)));
  let worker = on_shard(&daemon, other, move || {
    began_us.store(
      u64::try_from(base.elapsed().as_micros()).unwrap().max(1),
      Ordering::Release,
    );
    block(held);
  });
  // Each beat's arrival, by the test's clock since `base`, until a whole window passes without one.
  let mut last = beat(&daemon);
  let mut last_seen = base.elapsed();
  let mut stopped_at = None;
  while base.elapsed() < held {
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
  println!(
    "wedged shard ({name}): the block began {began:?} in, and the beat stopped {after:?} after it"
  );
  assert!(
    after <= WINDOW + BEAT * 3,
    "the beat stopped {after:?} after the shard wedged, past a window and three beats"
  );
  daemon.stop();
}

/// A-228, §4.14. Do: on the shard that is not the control shard, block one poll with no CPU for three windows (a
/// deadlock, or a host call that does not return). Expect: the beat stops within a window and three beats of the block.
/// Before A-228 the control shard went on beating, so the anchor never saw the wedged shard.
#[test]
fn a_wedged_shard_stops_the_beat() {
  a_blocked_shard_stops_the_beat("wedged", block_on_a_channel);
}

/// A-228, §4.14, the stuck extreme of starved-is-not-stuck. Do: block one poll in a real syscall, a read of a pipe nobody
/// writes, for three windows. Expect: the beat stops within a window and three beats of the block: the scheduler never
/// sees the thread runnable, so a shard stuck in the kernel is still found.
#[test]
fn a_shard_blocked_in_a_syscall_stops_the_beat() {
  a_blocked_shard_stops_the_beat("syscall", block_in_a_read);
}

/// Shape: spinners on the starved shard's CPU. Linux: 63, the most measured, which held a SCHED_IDLE thread off its CPU
/// for 5.7 s (CachyOS 7.3) and 7.9 s (Fedora 6.19) of 8 s; 4 and 16 left it 0.4-5.1 s.
#[cfg(target_os = "linux")]
fn spinners_wanted() -> usize {
  63
}

/// Shape: four TIME_CRITICAL spinners on its CPU held an IDLE-priority thread off it for all of 8.9 s (Windows x86_64).
#[cfg(windows)]
fn spinners_wanted() -> usize {
  4
}

/// Shape: two spinners a CPU, as the busy-machine test runs. macOS has no way to pin them beside the shard.
#[cfg(target_os = "macos")]
fn spinners_wanted() -> usize {
  std::thread::available_parallelism().map_or(1, usize::from) * 2
}

/// Lowers the calling thread (a shard's) as far as an unprivileged thread can and pins it to the CPU it is on; that CPU.
/// The policy stays for the thread's life: an unprivileged thread cannot leave SCHED_IDLE, and the test's daemon stops.
#[cfg(target_os = "linux")]
fn starve_this_thread() -> usize {
  // SAFETY: takes no argument and returns the calling thread's CPU.
  let cpu = usize::try_from(unsafe { libc::sched_getcpu() }).unwrap();
  pin_to(cpu);
  let param = libc::sched_param { sched_priority: 0 };
  // SAFETY: pid 0 names the calling thread; `param` is this frame's own, and is read for the call only.
  let status = unsafe { libc::sched_setscheduler(0, libc::SCHED_IDLE, &raw const param) };
  assert_eq!(status, 0, "SCHED_IDLE needs no privilege");
  cpu
}

/// Keeps the calling thread (a spinner) on `cpu`, at normal priority, ahead of a SCHED_IDLE thread there.
#[cfg(target_os = "linux")]
fn outrank_on(cpu: usize) {
  pin_to(cpu);
}

#[cfg(target_os = "linux")]
fn pin_to(cpu: usize) {
  let mut set = rustix::thread::CpuSet::new();
  set.set(cpu);
  rustix::thread::sched_setaffinity(None, &set).unwrap();
}

/// Lowers the calling thread (a shard's) to IDLE priority with its boosts off and pins it to the CPU it is on; that CPU.
#[cfg(windows)]
fn starve_this_thread() -> usize {
  use windows_sys::Win32::System::Threading::{
    GetCurrentProcessorNumber, GetCurrentThread, SetThreadPriority, SetThreadPriorityBoost,
    THREAD_PRIORITY_IDLE,
  };
  // SAFETY: takes no argument and returns the calling thread's processor.
  let cpu = unsafe { GetCurrentProcessorNumber() } as usize;
  pin_to(cpu);
  // SAFETY: the pseudo-handle names the calling thread, alive for the calls.
  unsafe {
    assert_ne!(
      SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_IDLE),
      0
    );
    assert_ne!(SetThreadPriorityBoost(GetCurrentThread(), 1), 0);
  }
  cpu
}

/// Puts the calling thread (a spinner) on `cpu` at TIME_CRITICAL priority.
#[cfg(windows)]
fn outrank_on(cpu: usize) {
  use windows_sys::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
  };
  pin_to(cpu);
  // SAFETY: the pseudo-handle names the calling thread, alive for the call.
  assert_ne!(
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) },
    0
  );
}

#[cfg(windows)]
fn pin_to(cpu: usize) {
  use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};
  // SAFETY: the pseudo-handle names the calling thread, alive for the call.
  assert_ne!(
    unsafe { SetThreadAffinityMask(GetCurrentThread(), 1 << cpu) },
    0
  );
}

/// Lowers the calling thread (a shard's) to the background QoS class. macOS has no CPU to name; the answer is 0.
#[cfg(target_os = "macos")]
fn starve_this_thread() -> usize {
  // SAFETY: changes the calling thread's own QoS class; the arguments are plain values.
  let status =
    unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0) };
  assert_eq!(status, 0);
  0
}

/// Raises the calling thread (a spinner) to the user-interactive QoS class.
#[cfg(target_os = "macos")]
fn outrank_on(_cpu: usize) {
  // SAFETY: changes the calling thread's own QoS class; the arguments are plain values.
  let status = unsafe {
    libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
  };
  assert_eq!(status, 0);
}

/// Watches the beat and the shard's own CPU clock together, for three windows at the least and on until the clock has
/// stood still for `flat_enough` or `bound` passes: the largest gap between beats, and the clock's longest standstill.
fn watch_starving(
  daemon: &Daemon,
  holder: SlotHolder,
  flat_enough: Duration,
  bound: Duration,
) -> (Duration, Duration) {
  let progress = || slates_rt::registry::shard_cpu(holder).map(|reading| reading.progress);
  let least = WINDOW * 3;
  let started = Instant::now();
  let mut last_beat = beat(daemon);
  let mut last_beat_seen = Instant::now();
  let mut largest_gap = Duration::ZERO;
  let mut last_progress = progress();
  let mut progressed = Instant::now();
  let mut longest_flat = Duration::ZERO;
  while started.elapsed() < bound && (started.elapsed() < least || longest_flat < flat_enough) {
    wait(SAMPLE);
    let now = beat(daemon);
    if now != last_beat {
      last_beat = now;
      last_beat_seen = Instant::now();
    }
    largest_gap = largest_gap.max(last_beat_seen.elapsed());
    let reading = progress();
    if reading != last_progress {
      last_progress = reading;
      progressed = Instant::now();
    }
    longest_flat = longest_flat.max(progressed.elapsed());
  }
  (largest_gap, longest_flat)
}

/// A-228, §4.14, fixtures' false kills at CPUWeight=1, the starved extreme of starved-is-not-stuck. Do: lower the shard
/// that is not the control shard as far as an unprivileged thread can, spin one poll on it without yielding, and keep it
/// off its CPU with higher-priority spinners there, until its own clock has stood still long enough that the old rule
/// withholds the beat for a whole window (two windows and three beats) and three windows have passed. Expect: the beat
/// never gaps a window: the thread is runnable throughout. Linux and Windows must see that flat window, or the test
/// fails saying it did not starve; macOS' scheduler will not hold a thread off for that long on demand (1.1 s at best,
/// 0-0.1 s at more spinners), so there it prints what it saw and holds only the beat.
#[test]
fn a_shard_starved_off_its_cpu_keeps_the_beat() {
  let (daemon, _, other) = daemon("starved");
  beating(&daemon);
  let on_cpu = &*Box::leak(Box::new(AtomicUsize::new(usize::MAX)));
  let running = &*Box::leak(Box::new(AtomicBool::new(true)));
  let worker = on_shard(&daemon, other, move || {
    on_cpu.store(starve_this_thread(), Ordering::Release);
    while running.load(Ordering::Relaxed) {
      std::hint::spin_loop();
    }
  });
  let waited = Instant::now();
  while on_cpu.load(Ordering::Acquire) == usize::MAX {
    assert!(waited.elapsed() < WINDOW * 10, "the poll reached its shard");
    wait(SAMPLE);
  }
  let cpu = on_cpu.load(Ordering::Acquire);
  let spinners: Vec<_> = (0..spinners_wanted())
    .map(|_| {
      std::thread::spawn(move || {
        outrank_on(cpu);
        while running.load(Ordering::Relaxed) {
          std::hint::spin_loop();
        }
      })
    })
    .collect();
  let holder = slates_rt::registry::holder_of(other.0).unwrap();
  let must_starve = !cfg!(target_os = "macos");
  let flat_enough = WINDOW * 2 + BEAT * 3;
  let bound = if must_starve { WINDOW * 10 } else { WINDOW * 3 };
  let started = Instant::now();
  let (largest_gap, longest_flat) = watch_starving(&daemon, holder, flat_enough, bound);
  running.store(false, Ordering::Relaxed);
  for spinner in spinners {
    spinner.join().unwrap();
  }
  worker.join().unwrap();
  println!(
    "starved shard: {} spinners, its CPU clock stood still for {longest_flat:?} at most in {:?}; largest beat gap \
     {largest_gap:?}",
    spinners_wanted(),
    started.elapsed()
  );
  if must_starve {
    assert!(
      longest_flat >= flat_enough,
      "did not starve: the shard's CPU clock stood still for {longest_flat:?} at most, under the {flat_enough:?} that \
       tests the rule"
    );
  }
  assert!(
    largest_gap < WINDOW,
    "the beat stopped for {largest_gap:?} while the shard was starved (its CPU clock stood still for {longest_flat:?}): \
     a starved shard was judged stuck"
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
  beating(&daemon);
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
  beating(&daemon);
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
  beating(&daemon);
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
