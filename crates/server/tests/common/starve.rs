//! One shard's thread starved of its CPU while it stays runnable (A-228's starved-is-not-stuck; A-232): the thread
//! lowered as far as an unprivileged thread can go, and spinners ahead of it. On Linux the thread takes SCHED_IDLE pinned
//! to its CPU, beside spinners pinned there at nice 0 (or the nearest this process may reach, counted by weight); on
//! Windows IDLE priority beside TIME_CRITICAL spinners on its CPU; on macOS the background QoS class beside two
//! user-interactive spinners a CPU, which the scheduler honours only loosely. The same rule as `liveness.rs`'s own
//! copy, which predates this module.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use slates_rt::shard::ShardId;
use slates_server::Daemon;
use slates_server::daemon::OBSERVE_BUDGET_NS;
use slates_server::state::ShardState;

/// Shape: how long the shard's lowering and the spinners' start may take before the test says so: ten observe budgets,
/// far past what a busy machine adds to starting a thread.
const SETTLE: Duration = Duration::from_nanos(10 * OBSERVE_BUDGET_NS);
/// Shape: how often the settling is looked at: a hundredth of [`SETTLE`].
const LOOK: Duration = Duration::from_nanos(OBSERVE_BUDGET_NS / 10);

/// Shape: spinners at nice 0 on the starved shard's CPU: 63, the most `liveness.rs` measured, which held a SCHED_IDLE
/// thread off its CPU for 5.7 s (CachyOS 7.3) and 7.9 s (Fedora 6.19) of 8 s; 4 and 16 left it 0.4-5.1 s.
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

/// How far a shard is starved.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Depth {
  /// Held off its CPU for seconds at a time: SCHED_IDLE (Linux) or IDLE priority (Windows) beside the most spinners
  /// measured, so nothing on the shard runs for long stretches (A-228's starved extreme).
  Idle,
  /// Slow but running: nice 19 beside two spinners at nice 0 on its CPU (Linux), as a whole daemon at nice 19 beside two
  /// spinners a CPU runs (clients' false-death runs, A-232): about 15/2,063 of its CPU, in short slices. On Windows,
  /// whose priorities are strict, LOWEST priority beside two TIME_CRITICAL spinners runs on the scheduler's boosts of a
  /// starved thread alone; on macOS it is the same as [`Depth::Idle`].
  Nice,
}

/// Shape: the spinners ahead of a [`Depth::Nice`] shard on its CPU, at nice 0: two, as the busy-machine tests run.
#[cfg(any(target_os = "linux", windows))]
const NICE_SPINNERS: u64 = 2;

/// A shard held starved, with its spinners; [`Starved::stop`] stops the spinners. The shard's thread keeps its lowered
/// priority for its life (an unprivileged thread cannot raise it again), so the daemon is stopped after.
pub(crate) struct Starved {
  running: &'static AtomicBool,
  spinners: Vec<JoinHandle<()>>,
  /// The spinners started and their nice, for the record.
  pub(crate) shown: String,
}

impl Starved {
  /// Stops the spinners and waits for them.
  pub(crate) fn stop(self) {
    self.running.store(false, Ordering::Relaxed);
    for spinner in self.spinners {
      let _ = spinner.join();
    }
  }
}

/// Starves `shard` of `daemon` to `depth`: lowers its thread (from a poll on it) and starts the spinners ahead of it.
// The test thread waits off-shard for the lowering and the spinners to take hold; it holds nothing a shard needs.
#[allow(clippy::disallowed_methods)]
pub(crate) fn starve_shard(daemon: &Daemon, shard: ShardId, depth: Depth) -> Starved {
  let on_cpu = &*Box::leak(Box::new(AtomicUsize::new(usize::MAX)));
  let lowered = daemon
    .observation(Some(shard), OBSERVE_BUDGET_NS, move |_: &mut ShardState| {
      on_cpu.store(starve_this_thread(depth), Ordering::Release);
    })
    .unwrap();
  let _ = lowered.wait();
  let waited = Instant::now();
  while on_cpu.load(Ordering::Acquire) == usize::MAX {
    assert!(waited.elapsed() < SETTLE, "the lowering reached its shard");
    std::thread::sleep(LOOK);
  }
  let cpu = on_cpu.load(Ordering::Acquire);
  let running = &*Box::leak(Box::new(AtomicBool::new(true)));
  let settled = &*Box::leak(Box::new(AtomicUsize::new(0)));
  let spinners: Vec<_> = (0..spinners_wanted(depth))
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
      waited.elapsed() < SETTLE,
      "{} of {} spinners took their CPU and priority",
      settled.load(Ordering::Acquire),
      spinners.len()
    );
    std::thread::sleep(LOOK);
  }
  let shown = format!(
    "{} spinners at nice {}",
    spinners.len(),
    spinner_nice_shown()
  );
  Starved {
    running,
    spinners,
    shown,
  }
}

/// The nice every spinner takes: 0, or the nearest to it this process may reach. A process started nice inherits it in
/// every thread (adacachy2's ananicy-cpp renices `cargo` to 19, and has run the test at -4), and an unprivileged thread
/// may lower its nice to `20 - RLIMIT_NICE` and raise it freely. A process below 0 raises its spinners to 0: at -4 they
/// outweighed a [`Depth::Nice`] shard 2.4 times as much as at 0 and starved it whole (0 reads served, 2026-11-09).
#[cfg(target_os = "linux")]
fn spinner_nice() -> i32 {
  // SAFETY: plain values; `who` 0 names this process.
  let now = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
  if now <= 0 {
    return 0;
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

/// The nice the spinners run at, for the record; elsewhere the platform's own priority scheme stands in, and 0 is shown.
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

/// Spinners enough that their weight against the shard's matches [`SPINNERS_AT_NICE_ZERO`] ([`Depth::Idle`]) or
/// [`NICE_SPINNERS`] ([`Depth::Nice`]) at nice 0, whatever nice [`spinner_nice`] reaches.
#[cfg(target_os = "linux")]
fn spinners_wanted(depth: Depth) -> usize {
  let at_nice_zero = match depth {
    Depth::Idle => SPINNERS_AT_NICE_ZERO,
    Depth::Nice => NICE_SPINNERS,
  };
  let weight = usize::try_from(spinner_nice())
    .ok()
    .and_then(|nice| NICE_WEIGHT.get(nice))
    .copied()
    .unwrap_or(NICE_WEIGHT[0]);
  usize::try_from((at_nice_zero * NICE_WEIGHT[0]).div_ceil(weight)).unwrap()
}

/// Shape: four TIME_CRITICAL spinners on its CPU held an IDLE-priority thread off it for all of 8.9 s (Windows x86_64).
#[cfg(windows)]
fn spinners_wanted(depth: Depth) -> usize {
  match depth {
    Depth::Idle => 4,
    Depth::Nice => usize::try_from(NICE_SPINNERS).unwrap(),
  }
}

/// Shape: two spinners a CPU, as the busy-machine tests run. macOS has no way to pin them beside the shard.
#[cfg(target_os = "macos")]
fn spinners_wanted(_depth: Depth) -> usize {
  std::thread::available_parallelism().map_or(1, usize::from) * 2
}

/// Shape: the nice a [`Depth::Nice`] shard takes: the lowest priority a nice value names.
#[cfg(target_os = "linux")]
const LOWEST_NICE: i32 = 19;

/// Lowers the calling thread (a shard's) to SCHED_IDLE ([`Depth::Idle`]) or nice 19 ([`Depth::Nice`]) and pins it to the
/// CPU it is on; that CPU.
#[cfg(target_os = "linux")]
fn starve_this_thread(depth: Depth) -> usize {
  // SAFETY: takes no argument and returns the calling thread's CPU.
  let cpu = usize::try_from(unsafe { libc::sched_getcpu() }).unwrap();
  pin_to(cpu);
  if matches!(depth, Depth::Nice) {
    // SAFETY: takes no argument and returns the calling thread's id.
    let thread = unsafe { libc::syscall(libc::SYS_gettid) };
    // SAFETY: plain values; on Linux PRIO_PROCESS with a thread id sets that thread's nice alone, and raising it needs
    // no privilege.
    let set = unsafe {
      libc::setpriority(
        libc::PRIO_PROCESS,
        libc::id_t::try_from(thread).unwrap(),
        LOWEST_NICE,
      )
    };
    assert_eq!(set, 0, "a thread may raise its own nice");
    return cpu;
  }
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

#[cfg(target_os = "linux")]
fn pin_to(cpu: usize) {
  let mut set = rustix::thread::CpuSet::new();
  set.set(cpu);
  rustix::thread::sched_setaffinity(None, &set).unwrap();
}

/// Lowers the calling thread (a shard's) to IDLE ([`Depth::Idle`]) or LOWEST ([`Depth::Nice`]) priority with its boosts
/// off and pins it to the CPU it is on; that CPU.
#[cfg(windows)]
fn starve_this_thread(depth: Depth) -> usize {
  use windows_sys::Win32::System::Threading::{
    GetCurrentProcessorNumber, GetCurrentThread, SetThreadPriority, SetThreadPriorityBoost,
    THREAD_PRIORITY_IDLE, THREAD_PRIORITY_LOWEST,
  };
  let priority = match depth {
    Depth::Idle => THREAD_PRIORITY_IDLE,
    Depth::Nice => THREAD_PRIORITY_LOWEST,
  };
  // SAFETY: takes no argument and returns the calling thread's processor.
  let cpu = unsafe { GetCurrentProcessorNumber() } as usize;
  pin_to(cpu);
  // SAFETY: the pseudo-handle names the calling thread, alive for the calls.
  unsafe {
    assert_ne!(SetThreadPriority(GetCurrentThread(), priority), 0);
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
fn starve_this_thread(_depth: Depth) -> usize {
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

/// Starts a spinner: on Linux with a small stack, since the weight-derived count can reach thousands.
fn spawn_spinner(work: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
  let builder = std::thread::Builder::new();
  #[cfg(target_os = "linux")]
  let builder = builder.stack_size(SPINNER_STACK);
  builder.spawn(work).unwrap()
}
