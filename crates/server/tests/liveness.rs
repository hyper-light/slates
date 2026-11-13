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

use slates_rt::shard::ShardId;
use slates_rt::{RunState, SlotHolder};
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
      name: common::fixture_object("slates-seg", &format!("liveness-{name}")),
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
  let (daemon, control, _) = daemon("starved-poll");
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
  let _serving = beating(&daemon);
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

/// Shape: spinners at nice 0 on the starved shard's CPU: 63, the most measured, which held a SCHED_IDLE thread off its
/// CPU for 5.7 s (CachyOS 7.3) and 7.9 s (Fedora 6.19) of 8 s; 4 and 16 left it 0.4-5.1 s.
#[cfg(target_os = "linux")]
const SPINNERS_AT_NICE_ZERO: u64 = 63;

/// Format: the kernel's scheduling weight of each nice value from 0 to 19 (`sched_prio_to_weight`, kernel/sched/core.c);
/// nice 0 weighs 1024 and SCHED_IDLE 3.
#[cfg(target_os = "linux")]
const NICE_WEIGHT: [u64; 20] = [
  1024, 820, 655, 526, 423, 335, 272, 215, 172, 137, 110, 87, 70, 56, 45, 36, 29, 23, 18, 15,
];

/// Shape: a spinner's stack. It runs one loop, and the weight-derived count can reach thousands.
#[cfg(target_os = "linux")]
const SPINNER_STACK: usize = 64 * 1024;

/// The nice every spinner takes: 0, or the nearest to it this process may reach. A process started nice inherits it in
/// every thread (adacachy2's ananicy-cpp renices `cargo` to 19, and a test it runs inherits that), and the spinners'
/// weight against the shard's SCHED_IDLE is the test's precondition: at nice 19 the same 63 spinners left the shard's
/// clock flat 81-142 ms at most, 3 of 3 failing to starve it, where at nice 0 it stood still 2.5-3.0 s (adacachy2,
/// 2026-11-08). An unprivileged thread may lower its nice to `20 - RLIMIT_NICE`.
#[cfg(target_os = "linux")]
fn spinner_nice() -> i32 {
  // SAFETY: plain values; `who` 0 names this process.
  let now = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
  if now <= 0 {
    return now;
  }
  let mut limit = libc::rlimit {
    rlim_cur: 0,
    rlim_max: 0,
  };
  // SAFETY: `limit` is this frame's own, written by the call.
  let read = unsafe { libc::getrlimit(libc::RLIMIT_NICE, &raw mut limit) };
  let floor = if read == 0 {
    20 - i32::try_from(limit.rlim_cur.min(40)).unwrap()
  } else {
    now
  };
  now.min(floor.max(0))
}

/// Spinners enough that their weight against the shard's matches [`SPINNERS_AT_NICE_ZERO`] at nice 0, whatever nice
/// [`spinner_nice`] reaches.
#[cfg(target_os = "linux")]
fn spinners_wanted() -> usize {
  let weight = usize::try_from(spinner_nice())
    .ok()
    .and_then(|nice| NICE_WEIGHT.get(nice))
    .copied()
    .unwrap_or(NICE_WEIGHT[0]);
  usize::try_from((SPINNERS_AT_NICE_ZERO * NICE_WEIGHT[0]).div_ceil(weight)).unwrap()
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

/// Keeps the calling thread (a spinner) on `cpu`, at [`spinner_nice`], ahead of a SCHED_IDLE thread there.
#[cfg(target_os = "linux")]
fn outrank_on(cpu: usize) {
  pin_to(cpu);
  let nice = spinner_nice();
  // SAFETY: takes no argument and returns the calling thread's id.
  let thread = unsafe { libc::syscall(libc::SYS_gettid) };
  // SAFETY: plain values; on Linux PRIO_PROCESS with a thread id sets that thread's nice alone.
  let set = unsafe {
    libc::setpriority(
      libc::PRIO_PROCESS,
      libc::id_t::try_from(thread).unwrap(),
      nice,
    )
  };
  assert_eq!(
    set, 0,
    "a spinner takes nice {nice}, which RLIMIT_NICE allows"
  );
}

/// Starts a spinner: on Linux with a small stack, since the weight-derived count can reach thousands.
fn spawn_spinner(work: impl FnOnce() + Send + 'static) -> std::thread::JoinHandle<()> {
  let builder = std::thread::Builder::new();
  #[cfg(target_os = "linux")]
  let builder = builder.stack_size(SPINNER_STACK);
  builder.spawn(work).unwrap()
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
  let raised = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
  assert_ne!(raised, 0);
}

#[cfg(windows)]
fn pin_to(cpu: usize) {
  use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};
  // SAFETY: the pseudo-handle names the calling thread, alive for the call.
  let previous = unsafe { SetThreadAffinityMask(GetCurrentThread(), 1 << cpu) };
  assert_ne!(previous, 0);
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

/// Where this platform's scheduler reader gets its answer (`slates_rt::thread_clock` says why).
fn scheduler_source() -> &'static str {
  if cfg!(target_os = "linux") {
    "/proc task stat state and schedstat run-queue wait"
  } else if cfg!(windows) {
    "NtQuerySystemInformation thread state, no wait time kept"
  } else {
    "thread_basic_info run_state, no wait time kept"
  }
}

/// What the scheduler reader said of the shard over a watch, asked once a beat as the watchdog asks.
#[derive(Default)]
struct SchedulerSeen {
  runnable: u32,
  blocked: u32,
  unread: u32,
  waited_first_ns: Option<u64>,
  waited_last_ns: Option<u64>,
}

impl SchedulerSeen {
  fn look(&mut self, holder: SlotHolder) {
    match slates_rt::registry::shard_run_readings(&[holder])
      .pop()
      .flatten()
    {
      None => self.unread += 1,
      Some(reading) => {
        match reading.state {
          RunState::Runnable => self.runnable += 1,
          RunState::Blocked => self.blocked += 1,
        }
        self.waited_first_ns = self.waited_first_ns.or(reading.waited_ns);
        self.waited_last_ns = reading.waited_ns.or(self.waited_last_ns);
      }
    }
  }
}

/// What a watch of a starved shard saw.
struct Starving {
  largest_gap: Duration,
  longest_flat: Duration,
  scheduler: SchedulerSeen,
}

/// Watches the beat and the shard's own CPU clock together, for three windows at the least and on until the clock has
/// stood still for `flat_enough` or `bound` passes: the largest gap between beats, the clock's longest standstill, and
/// what the scheduler reader said meanwhile.
fn watch_starving(
  daemon: &Daemon,
  holder: SlotHolder,
  flat_enough: Duration,
  bound: Duration,
) -> Starving {
  let progress = || slates_rt::registry::shard_cpu(holder).map(|reading| reading.progress);
  let least = WINDOW * 3;
  let started = Instant::now();
  let mut last_beat = beat(daemon);
  let mut last_beat_seen = Instant::now();
  let mut largest_gap = Duration::ZERO;
  let mut last_progress = progress();
  let mut progressed = Instant::now();
  let mut longest_flat = Duration::ZERO;
  let mut scheduler = SchedulerSeen::default();
  let mut last_asked = Instant::now();
  while started.elapsed() < bound && (started.elapsed() < least || longest_flat < flat_enough) {
    wait(SAMPLE);
    if last_asked.elapsed() >= BEAT {
      last_asked = Instant::now();
      scheduler.look(holder);
    }
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
  Starving {
    largest_gap,
    longest_flat,
    scheduler,
  }
}

/// Whether a test that starves a thread is running in this process ([`one_starving_test_at_a_time`]).
static STARVING: AtomicBool = AtomicBool::new(false);

/// This test's turn to starve a thread, given back when dropped.
struct StarvingTurn;

impl Drop for StarvingTurn {
  fn drop(&mut self) {
    STARVING.store(false, Ordering::Release);
  }
}

/// Waits until no other test of this process starves a thread, then takes the turn. A starved thread is lowered for
/// good and held off its CPU, and whatever process-wide lock it holds when it is (an allocator arena, the standard
/// error stream) blocks every thread that next needs it until the spinners stop: two such tests at once, each needing
/// a lock the other's starved thread held, hung the whole suite, 1 run in 3 in parallel on adacachy2 (2026-11-08), where
/// one at a time never did. Their spinners stop before the turn is given back.
fn one_starving_test_at_a_time() -> StarvingTurn {
  while STARVING
    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
    .is_err()
  {
    wait(SAMPLE);
  }
  StarvingTurn
}

/// A-228, §4.14, fixtures' false kills at CPUWeight=1, the starved extreme of starved-is-not-stuck. Do: lower the shard
/// that is not the control shard as far as an unprivileged thread can, spin one poll on it without yielding, and keep it
/// off its CPU with higher-priority spinners there, until its own clock has stood still long enough that the old rule
/// withholds the beat for a whole window (two windows and three beats) and three windows have passed. Expect: the
/// watchdog never withholds the beat: the thread is runnable throughout. Linux and Windows must see that flat window, or
/// the test fails saying it did not starve; on Linux the spinners take nice 0 whatever nice the test inherited, and are
/// counted by weight where it cannot ([`spinner_nice`]), so the precondition holds on any box. macOS' scheduler will not
/// hold a thread off for that long on demand (1.1 s at best, 0-0.1 s at more spinners), so there it prints what it saw.
/// The beat's gap is held under a window where the spinners are confined to the shard's CPU (Linux, Windows); on macOS
/// they starve the watchdog's thread too, so there only a withheld look counts against the rule.
#[test]
fn a_shard_starved_off_its_cpu_keeps_the_beat() {
  let _turn = one_starving_test_at_a_time();
  let (daemon, _, other) = daemon("starved");
  let _serving = beating(&daemon);
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
  let spinners = settled_spinners(cpu, running);
  let holder = slates_rt::registry::holder_of(other.0).unwrap();
  let turns_before = daemon.watchdog_turns().unwrap();
  let must_starve = !cfg!(target_os = "macos");
  let flat_enough = WINDOW * 2 + BEAT * 3;
  let bound = if must_starve { WINDOW * 10 } else { WINDOW * 3 };
  let started = Instant::now();
  let Starving {
    largest_gap,
    longest_flat,
    scheduler,
  } = watch_starving(&daemon, holder, flat_enough, bound);
  let turns_after = daemon.watchdog_turns().unwrap();
  let looks = turns_after.0 - turns_before.0;
  let withheld = turns_after.1 - turns_before.1;
  running.store(false, Ordering::Relaxed);
  for spinner in spinners {
    spinner.join().unwrap();
  }
  worker.join().unwrap();
  println!(
    "starved shard: {} spinners at nice {}, its CPU clock stood still for {longest_flat:?} at most in {:?}; largest beat \
     gap {largest_gap:?}; the watchdog looked {looks} times and withheld the beat at {withheld}",
    spinners_wanted(),
    spinner_nice_shown(),
    started.elapsed()
  );
  println!(
    "scheduler reader: {}; asked once a beat: runnable {}, blocked {}, unread {}; run-queue wait first {:?} ns, last \
     {:?} ns",
    scheduler_source(),
    scheduler.runnable,
    scheduler.blocked,
    scheduler.unread,
    scheduler.waited_first_ns,
    scheduler.waited_last_ns
  );
  if must_starve {
    assert!(
      longest_flat >= flat_enough,
      "did not starve: the shard's CPU clock stood still for {longest_flat:?} at most, under the {flat_enough:?} that \
       tests the rule"
    );
  }
  assert!(
    looks > 0 && withheld == 0,
    "the watchdog withheld the beat at {withheld} of its {looks} looks while the shard was starved (its CPU clock stood \
     still for {longest_flat:?}): a starved shard was judged stuck"
  );
  // The gap is the rule's to answer for only where the spinners are confined to the shard's CPU (Linux and Windows pin
  // them); macOS cannot pin, so its spinners starve the watchdog's own thread as well, and a gap there with no withheld
  // look is that thread's lateness (1.146 s with 36 spinners at load ~40 on the 18-CPU Mac, 2026-11-08).
  if must_starve {
    assert!(
      largest_gap < WINDOW,
      "the beat stopped for {largest_gap:?} while the shard was starved (its CPU clock stood still for \
       {longest_flat:?}; the watchdog withheld {withheld} of {looks} looks)"
    );
  }
  daemon.stop();
}

/// Starts [`spinners_wanted`] spinners outranking the shard on `cpu` until `running` falls, and returns once every one
/// has taken its CPU and its nice. Before that they spin at the nice the test inherited, and a shard given CPU in that
/// moment was flat 2.31 s at most where the settled runs were 2.9-3.0 s (adacachy2 at nice 19, 2026-11-08).
fn settled_spinners(cpu: usize, running: &'static AtomicBool) -> Vec<std::thread::JoinHandle<()>> {
  let settled = &*Box::leak(Box::new(AtomicUsize::new(0)));
  let spinners: Vec<_> = (0..spinners_wanted())
    .map(|_| {
      spawn_spinner(move || {
        outrank_on(cpu);
        settled.fetch_add(1, Ordering::AcqRel);
        while running.load(Ordering::Relaxed) {
          std::hint::spin_loop();
        }
      })
    })
    .collect();
  let waited = Instant::now();
  while settled.load(Ordering::Acquire) < spinners.len() {
    assert!(
      waited.elapsed() < WINDOW * 10,
      "{} of {} spinners took their CPU and nice",
      settled.load(Ordering::Acquire),
      spinners.len()
    );
    wait(SAMPLE);
  }
  spinners
}

/// The nice the spinners run at, for the record: Linux's [`spinner_nice`]; elsewhere the platform's own priority scheme
/// stands in for it, and 0 is printed.
fn spinner_nice_shown() -> i32 {
  #[cfg(target_os = "linux")]
  {
    spinner_nice()
  }
  #[cfg(not(target_os = "linux"))]
  {
    0
  }
}

/// Shape: the least share of a CPU the spinning shard's thread is waited for: the CPU-clock wait ends at a window of its
/// CPU or this many windows of wall, whichever comes first. At load 185 on the 18-CPU Mac a spinner got about a tenth of
/// a CPU (the supervisor's measurement, 2026-11-08), so a window of its CPU took about ten of wall, the old test's whole
/// bound; a sixtieth leaves six times that.
const LEAST_CPU_SHARE_WINDOWS: u32 = 60;

/// A-228, §4.14. Do: on the shard that is not the control shard, spin one poll on the CPU without yielding. Wait on the
/// shard's own CPU clock, the quantity the rule reads, until the spin has run a window of it; then watch the beat for a
/// window of wall from its last beat. Expect: the beat stops, as the rule says once a shard has run a window of CPU without
/// a step. How long the CPU takes is the machine's: a window of wall on a free core, ten at load 185 on the 18-CPU Mac,
/// where the old test's ten windows of wall were about one window of the shard's CPU and failed. Before A-228 nothing
/// saw a spinning shard.
#[test]
fn a_spinning_shard_stops_the_beat() {
  let (daemon, _, other) = daemon("spinning");
  let _serving = beating(&daemon);
  let backstop = WINDOW * LEAST_CPU_SHARE_WINDOWS;
  let running = &*Box::leak(Box::new(AtomicBool::new(true)));
  let spinning = &*Box::leak(Box::new(AtomicBool::new(false)));
  let worker = on_shard(&daemon, other, move || {
    spinning.store(true, Ordering::Release);
    let started = Instant::now();
    while running.load(Ordering::Relaxed) && started.elapsed() < backstop * 2 {
      std::hint::spin_loop();
    }
  });
  let holder = slates_rt::registry::holder_of(other.0).unwrap();
  let spent = || slates_rt::registry::shard_cpu(holder).map_or(0, |reading| reading.spent_ns);
  let waited = Instant::now();
  while !spinning.load(Ordering::Acquire) && waited.elapsed() < backstop {
    wait(SAMPLE);
  }
  let at_spin = spent();
  let started = Instant::now();
  let window_ns = u64::try_from(WINDOW.as_nanos()).unwrap();
  // Phase one: the spin runs a window of the shard's CPU, the rule's own measure.
  while spent().saturating_sub(at_spin) < window_ns && started.elapsed() < backstop {
    wait(SAMPLE);
  }
  let cpu_at_window = Duration::from_nanos(spent().saturating_sub(at_spin));
  let wall_to_window = started.elapsed();
  // Phase two: from there the next look judges the shard spinning and no beat follows, so the beat word stands still for a
  // window of wall. A watchdog starved itself only looks later, which stops the beat sooner, never later.
  let mut last = beat(&daemon);
  let mut last_seen = Instant::now();
  let mut beats_after = 0u32;
  let mut stopped = false;
  let watching = Instant::now();
  while watching.elapsed() < backstop {
    wait(SAMPLE);
    let now = beat(&daemon);
    if now != last {
      last = now;
      last_seen = Instant::now();
      beats_after += 1;
    } else if last_seen.elapsed() >= WINDOW {
      stopped = true;
      break;
    }
  }
  let cpu_at_end = Duration::from_nanos(spent().saturating_sub(at_spin));
  running.store(false, Ordering::Relaxed);
  worker.join().unwrap();
  println!(
    "spinning shard: a window of its CPU ({cpu_at_window:?}) took {wall_to_window:?} of wall; {beats_after} beat(s) \
     after; the beat stopped {stopped}; its CPU by the end {cpu_at_end:?}"
  );
  assert!(
    cpu_at_window >= WINDOW,
    "the spinning shard's CPU clock reached {cpu_at_window:?} in {wall_to_window:?}, under a window: it got less than a \
     {LEAST_CPU_SHARE_WINDOWS}th of a CPU, which this test does not judge"
  );
  assert!(
    stopped,
    "a shard that ran {cpu_at_end:?} of CPU without a step never stopped the beat ({beats_after} beat(s) after its \
     window of CPU, in {:?})",
    watching.elapsed()
  );
  daemon.stop();
}

/// A-228's sibling in `ShardTime` (§4.14, A-65): a wait judged in a starved shard's own time must not give up on it. Do:
/// starve the shard that is not the control shard as the test above does (one poll spinning on it, held off its CPU by
/// higher-priority spinners there); submit an observation of it with a budget of one window, which waits behind that
/// poll; keep the shard starved until its clock has stood still past the observation's deadline (two windows and three
/// beats), then let the poll end. Expect: the observation is answered. A starved shard is runnable while its clock
/// stands still, which the scheduler says; one wedged is not. Linux and Windows must see the flat window or the test
/// fails saying it did not starve; macOS cannot hold a thread off on demand, so there it says so and passes.
#[test]
fn an_observation_of_a_starved_shard_is_answered() {
  let _turn = one_starving_test_at_a_time();
  if cfg!(target_os = "macos") {
    eprintln!(
      "SKIP an_observation_of_a_starved_shard_is_answered: macOS will not hold a thread off its CPU on demand"
    );
    return;
  }
  let (daemon, _, other) = daemon("observe-starved");
  let _serving = beating(&daemon);
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
  let spinners = settled_spinners(on_cpu.load(Ordering::Acquire), running);
  let budget_ns = u64::try_from(WINDOW.as_nanos()).unwrap();
  let observation = daemon
    .observation(Some(other), budget_ns, |_: &mut ShardState| 42u32)
    .unwrap();
  let asked = Instant::now();
  let answer = std::thread::spawn(move || observation.wait());
  let holder = slates_rt::registry::holder_of(other.0).unwrap();
  let flat_enough = WINDOW * 2 + BEAT * 3;
  let Starving { longest_flat, .. } = watch_starving(&daemon, holder, flat_enough, WINDOW * 10);
  running.store(false, Ordering::Relaxed);
  for spinner in spinners {
    spinner.join().unwrap();
  }
  worker.join().unwrap();
  let answered = answer.join().unwrap();
  println!(
    "observation of a starved shard: its CPU clock stood still for {longest_flat:?} at most; answered {answered:?} \
     {:?} after it was asked, against a budget of {WINDOW:?}",
    asked.elapsed()
  );
  assert!(
    longest_flat >= flat_enough,
    "did not starve: the shard's CPU clock stood still for {longest_flat:?} at most, under the {flat_enough:?} that \
     tests the rule"
  );
  assert!(
    matches!(answered, Ok(42)),
    "a starved shard's observation was given up on: {answered:?} (its CPU clock stood still for {longest_flat:?})"
  );
  daemon.stop();
}

/// Shape: the CPU the starved holder runs before it releases the shard: enough that a holder given none for a window
/// cannot finish early, little enough that it finishes once it runs.
#[cfg(any(target_os = "linux", windows))]
const HOLDER_CPU: Duration = Duration::from_millis(50);

/// The calling thread's CPU time, read from its own clock.
#[cfg(any(target_os = "linux", windows))]
fn own_cpu() -> Duration {
  Duration::from_nanos(
    slates_rt::registry::calling_thread_cpu().map_or(0, |reading| reading.spent_ns),
  )
}

/// A-228's transitive wait (§4.14). Do: block the non-control shard's poll, marked as waiting on another thread
/// (`slates_rt::registry::waiting_on`), until that thread releases it; starve that thread (lowered and pinned beside
/// outranking spinners, as the starved shard's test does) while it owes [`HOLDER_CPU`] of its own CPU before the
/// release; watch for four windows. Expect: the watchdog never withholds the beat: the shard waits on a thread that is
/// runnable, so it is starved, not stuck. Before, the shard read blocked and its clock stood still, and the watchdog
/// withheld the beat at 30 of 40 looks, the beat gapping 3.05-3.09 s (adacachy2, 3 of 3, 2026-11-08). macOS cannot
/// starve a thread on demand, so the test is Linux and Windows.
#[cfg(any(target_os = "linux", windows))]
#[test]
fn a_shard_waiting_on_a_starved_thread_keeps_the_beat() {
  let _turn = one_starving_test_at_a_time();
  let (daemon, _, other) = daemon("waits-on-starved");
  let _serving = beating(&daemon);
  let holder: &'static std::sync::OnceLock<Option<slates_rt::registry::ThreadHandle>> =
    Box::leak(Box::default());
  let shard_thread: &'static std::sync::OnceLock<std::thread::Thread> = Box::leak(Box::default());
  let released = &*Box::leak(Box::new(AtomicBool::new(false)));
  let on_cpu = &*Box::leak(Box::new(AtomicUsize::new(usize::MAX)));
  let running = &*Box::leak(Box::new(AtomicBool::new(true)));
  let holder_thread = std::thread::spawn(move || {
    let _ = holder.set(slates_rt::registry::calling_thread());
    on_cpu.store(starve_this_thread(), Ordering::Release);
    let started = own_cpu();
    while own_cpu().saturating_sub(started) < HOLDER_CPU {
      std::hint::spin_loop();
    }
    released.store(true, Ordering::Release);
    if let Some(shard) = shard_thread.get() {
      shard.unpark();
    }
  });
  while on_cpu.load(Ordering::Acquire) == usize::MAX {
    wait(SAMPLE);
  }
  let worker = on_shard(&daemon, other, move || {
    let _ = shard_thread.set(std::thread::current());
    let _waiting = slates_rt::registry::waiting_on(holder.get().copied().flatten());
    while !released.load(Ordering::Acquire) {
      std::thread::park();
    }
  });
  let spinners = settled_spinners(on_cpu.load(Ordering::Acquire), running);
  let before = daemon.watchdog_turns().unwrap();
  let (largest, _) = watch_beat(&daemon, WINDOW * 4);
  let after = daemon.watchdog_turns().unwrap();
  let held_throughout = !released.load(Ordering::Acquire);
  running.store(false, Ordering::Relaxed);
  for spinner in spinners {
    spinner.join().unwrap();
  }
  holder_thread.join().unwrap();
  worker.join().unwrap();
  let (looks, withheld) = (after.0 - before.0, after.1 - before.1);
  println!(
    "a shard waiting on a starved thread: held throughout {held_throughout}; the watchdog looked {looks} times and \
     withheld the beat at {withheld}; largest beat gap {largest:?}"
  );
  assert!(
    held_throughout,
    "did not starve: the holder ran its {HOLDER_CPU:?} of CPU within the watch, so the shard was not held a window"
  );
  assert!(
    looks > 0 && withheld == 0,
    "a shard waiting on a starved thread was judged stuck: the beat withheld at {withheld} of {looks} looks, the \
     largest gap {largest:?}"
  );
  daemon.stop();
}

/// A-228's transitive wait, the deadlock it must still find (§4.14). Do: block the non-control shard's poll, marked as
/// waiting on another thread (`slates_rt::registry::waiting_on`), on a thread that is itself parked until the test ends;
/// watch for three windows. Expect: the beat stops: the shard and the thread it waits on are both blocked, which is a
/// deadlock however the wait is marked.
#[test]
fn a_shard_waiting_on_a_blocked_thread_stops_the_beat() {
  let (daemon, _, other) = daemon("waits-on-blocked");
  let _serving = beating(&daemon);
  let holder: &'static std::sync::OnceLock<Option<slates_rt::registry::ThreadHandle>> =
    Box::leak(Box::default());
  let released = &*Box::leak(Box::new(AtomicBool::new(false)));
  let parked: &'static std::sync::OnceLock<std::thread::Thread> = Box::leak(Box::default());
  let holder_thread = std::thread::spawn(move || {
    let _ = holder.set(slates_rt::registry::calling_thread());
    let _ = parked.set(std::thread::current());
    while !released.load(Ordering::Acquire) {
      std::thread::park();
    }
  });
  while holder.get().is_none() {
    wait(SAMPLE);
  }
  let shard_thread: &'static std::sync::OnceLock<std::thread::Thread> = Box::leak(Box::default());
  let worker = on_shard(&daemon, other, move || {
    let _ = shard_thread.set(std::thread::current());
    let _waiting = slates_rt::registry::waiting_on(holder.get().copied().flatten());
    while !released.load(Ordering::Acquire) {
      std::thread::park();
    }
  });
  let before = daemon.watchdog_turns().unwrap();
  let (largest, _) = watch_beat(&daemon, WINDOW * 3);
  let after = daemon.watchdog_turns().unwrap();
  released.store(true, Ordering::Release);
  for thread in [parked.get(), shard_thread.get()].into_iter().flatten() {
    thread.unpark();
  }
  holder_thread.join().unwrap();
  worker.join().unwrap();
  let (looks, withheld) = (after.0 - before.0, after.1 - before.1);
  println!(
    "a shard waiting on a blocked thread: the watchdog looked {looks} times and withheld the beat at {withheld}; \
     largest beat gap {largest:?}"
  );
  assert!(
    withheld > 0 && largest >= WINDOW,
    "a shard blocked on a blocked thread was not found stuck: withheld at {withheld} of {looks} looks, the largest gap \
     {largest:?}"
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
  let _serving = beating(&daemon);
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
