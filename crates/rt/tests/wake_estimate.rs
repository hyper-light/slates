//! The shard's online wake estimate and the attribution of its long polls (§4.1, §4.3; A-31;
//! docs/bugs/2026-09-25-wake-estimate-frozen-at-boot-and-preemptions-counted-as-long-steps.md).
//!
//! The boot probe's mean wake converges slowly under a heavy tail, so a shard that tracks one refines it
//! from the kicked parks it pays: a sender stamps the first kick of a park, and the shard folds the
//! kick-to-running latency of a park that slept into its estimate, which its step quantum and idle spin
//! follow. A shard whose configuration carries no measured prior keeps its fixed values. A poll past the
//! quantum by the wall clock is attributed in a window of the thread's account: the task's when it ran
//! past the quantum on the CPU or blocked in a call, the host's when it was runnable and held off the
//! CPU, unattributed where the platform cannot tell (the first long poll opens the first window).

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use slates_rt::registry;
use slates_rt::runtime::{LocalRuntime, RuntimeConfig, WakeTracking};

/// Shape: the prior the tracking shard is seeded with — a second, far above any real wake — so an
/// estimate that learned from its wakes has unmistakably moved off it.
const PRIOR_NS: u64 = 1_000_000_000;
/// Shape: a short weighting window (sixteen wakes), so a handful of kicks moves the estimate far.
const SHIFT: u32 = 4;
/// Shape: kicked parks the tracking test drives.
const KICKS: u64 = 24;
/// Shape: the fixed quantum of the long-step test — a millisecond, far above a poll's own cost.
const QUANTUM_NS: u64 = 1_000_000;
/// Shape: how long a long poll holds its thread, three quanta: past the quantum by any clock.
const HOLD: Duration = Duration::from_millis(3);
/// Shape: polls each long-step history runs in one busy period.
const LONG_POLLS: u64 = 3;
/// Shape: how long a helper thread waits for the shard to announce its park before the test fails.
const PARK_WAIT: Duration = Duration::from_secs(10);
/// Shape: how long the kicker holds its kick after the shard announced its park, so the shard is asleep
/// in its wait when the kick lands (a millisecond: hundreds of wakes).
const ASLEEP_AFTER: Duration = Duration::from_millis(1);

fn config(step_budget_ns: u64, wake_tracking: Option<WakeTracking>) -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 64,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns,
    timer_tick_ns: 100_000,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    // No idle spin: every wait below is a park.
    spin_ns: 0,
    wake_tracking,
  }
}

/// A future that is pending once — handing its waker to the test — and ready when polled again.
struct PendOnce {
  wakers: Sender<Waker>,
  polled: bool,
}

impl Future for PendOnce {
  type Output = ();
  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
    if self.polled {
      return Poll::Ready(());
    }
    self.polled = true;
    let _ = self.wakers.send(cx.waker().clone());
    Poll::Pending
  }
}

/// Steps the shard until it has nothing to do.
fn step_until_idle(ctx: &'static slates_rt::shard::ShardContext) {
  while ctx.step().did_work {}
}

/// Parks the shard and wakes it from another thread a millisecond after it announced the park: a real
/// kick of a sleeping shard, the event the online estimate learns from.
fn park_and_kick(ctx: &'static slates_rt::shard::ShardContext, wakers: &Receiver<Waker>) {
  let waker = wakers.try_recv().expect("the task handed over its waker");
  let entry = registry::entry(ctx.id).expect("a local shard is registered");
  std::thread::scope(|scope| {
    scope.spawn(move || {
      let began = Instant::now();
      while !entry.parking.parked() {
        assert!(
          began.elapsed() < PARK_WAIT,
          "the shard never announced its park"
        );
        std::thread::yield_now();
      }
      let announced = Instant::now();
      while announced.elapsed() < ASLEEP_AFTER {
        std::thread::yield_now();
      }
      waker.wake();
    });
    ctx.park(None);
  });
}

/// §4.3, A-31: a tracking shard kicked while asleep learns the kick's wake — each such park is one sample
/// (the non-vacuity count), and every kicked park is either a sample or counted unslept (a kick that found
/// it awake) — and its estimate, seeded with a one-second prior, moves down to the wakes it measured; its
/// step quantum is the estimate, and the registry pulse mirrors it for another thread. A shard with no
/// measured prior learns nothing and keeps its fixed quantum.
#[test]
#[cfg_attr(miri, ignore)] // the OS driver opens a kqueue or an eventfd, which Miri does not model
fn a_tracking_shard_learns_its_wake_from_the_parks_its_kicks_ended() {
  let tracking = WakeTracking {
    prior_ns: PRIOR_NS,
    shift: SHIFT,
    idle_ratio: 1,
  };
  let rt = LocalRuntime::new(&config(PRIOR_NS, Some(tracking))).unwrap();
  let ctx = rt.context();
  let (tx, rx) = channel::<Waker>();
  rt.spawn(async move {
    for _ in 0..KICKS {
      PendOnce {
        wakers: tx.clone(),
        polled: false,
      }
      .await;
    }
  })
  .unwrap();
  for _ in 0..KICKS {
    step_until_idle(ctx);
    park_and_kick(ctx, &rx);
  }
  step_until_idle(ctx);
  let counters = ctx.counters();
  assert_eq!(
    counters.wake_samples + counters.wake_unslept,
    KICKS,
    "every kicked park was measured or counted unslept: {counters:?}"
  );
  assert!(
    counters.wake_samples >= KICKS / 2,
    "kicks that landed on a sleeping shard fed the estimate: {counters:?}"
  );
  assert!(
    counters.wake_cost_ns < PRIOR_NS / 2,
    "the estimate moved off its one-second prior toward the measured wakes: {counters:?}"
  );
  assert_eq!(ctx.quantum_ns(), counters.wake_cost_ns);
  let pulse = &registry::entry(ctx.id).unwrap().pulse;
  assert_eq!(pulse.wake_cost_ns(), counters.wake_cost_ns);

  let fixed = LocalRuntime::new(&config(QUANTUM_NS, None)).unwrap();
  let fixed_ctx = fixed.context();
  let (tx, rx) = channel::<Waker>();
  fixed
    .spawn(async move {
      PendOnce {
        wakers: tx,
        polled: false,
      }
      .await;
    })
    .unwrap();
  step_until_idle(fixed_ctx);
  park_and_kick(fixed_ctx, &rx);
  step_until_idle(fixed_ctx);
  assert_eq!(fixed_ctx.counters().wake_samples, 0);
  assert_eq!(fixed_ctx.quantum_ns(), QUANTUM_NS);
}

/// How a held poll spends its hold.
#[derive(Clone, Copy, Debug)]
enum Hold {
  /// Busy on the CPU until the thread has run [`HOLD`] of CPU time: a task's own long step.
  OnCpu,
  /// Asleep in the kernel for [`HOLD`]: a task blocked in a call.
  InCall,
  /// Yielding the CPU to a runnable competitor until [`HOLD`] has passed: a runnable thread the host
  /// keeps off its CPU, as a preemption does.
  #[cfg(target_os = "linux")]
  Runnable,
}

/// The calling thread's CPU time where the platform keeps a fine one, else the wall clock since `since`
/// (a platform with no per-thread clock attributes nothing, so the hold's exact nature does not matter).
fn thread_time(since: Instant) -> Duration {
  #[cfg(any(target_os = "linux", target_os = "macos"))]
  {
    let _ = since;
    let reading = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
    Duration::new(
      u64::try_from(reading.tv_sec).unwrap(),
      u32::try_from(reading.tv_nsec).unwrap(),
    )
  }
  #[cfg(not(any(target_os = "linux", target_os = "macos")))]
  {
    since.elapsed()
  }
}

/// A future whose every poll holds the thread for [`HOLD`] as `hold` says, then wakes itself, so its
/// polls run back to back — one per step — in one busy period with no wait between them.
struct HoldEachPoll {
  polls_left: u64,
  hold: Hold,
}

impl Future for HoldEachPoll {
  type Output = ();
  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
    if self.polls_left == 0 {
      return Poll::Ready(());
    }
    self.polls_left -= 1;
    let began = Instant::now();
    match self.hold {
      Hold::OnCpu => {
        let cpu_began = thread_time(began);
        while thread_time(began).saturating_sub(cpu_began) < HOLD {
          std::hint::spin_loop();
        }
      }
      // A test may sleep (the lint's stated exception): a sleep is a call the thread blocks in.
      #[allow(clippy::disallowed_methods)]
      Hold::InCall => std::thread::sleep(HOLD),
      #[cfg(target_os = "linux")]
      Hold::Runnable => {
        while began.elapsed() < HOLD {
          std::thread::yield_now();
        }
      }
    }
    cx.waker().wake_by_ref();
    Poll::Pending
  }
}

/// Runs one busy period of [`LONG_POLLS`] held polls and returns the shard's counters.
fn one_busy_period(hold: Hold) -> slates_rt::shard::Counters {
  let rt = LocalRuntime::new(&config(QUANTUM_NS, None)).unwrap();
  rt.spawn(HoldEachPoll {
    polls_left: LONG_POLLS,
    hold,
  })
  .unwrap();
  let ctx = rt.context();
  step_until_idle(ctx);
  let counters = ctx.counters();
  let hold_ns = u64::try_from(HOLD.as_nanos()).unwrap();
  assert!(
    counters.longest_step_ns >= hold_ns,
    "the wall clock saw each held poll: {counters:?}"
  );
  counters
}

/// A shard's long polls as (the task's, of those blocked, the host's, unattributed).
fn attributed(counters: &slates_rt::shard::Counters) -> (u64, u64, u64, u64) {
  (
    counters.long_steps,
    counters.blocked_steps,
    counters.preempted_steps,
    counters.unattributed_steps,
  )
}

/// Whether this platform keeps a fine per-thread CPU clock the shard attributes by.
const HAS_THREAD_CLOCK: bool = cfg!(any(target_os = "linux", target_os = "macos"));

/// §4.3, A-31: a poll that ran past the quantum on the CPU is its task's. The first long poll of a busy
/// period has no window and goes unattributed (it arms the shard); every later one is judged in a window
/// its step opened. With no per-thread clock (Windows) none is attributed.
#[test]
#[cfg_attr(miri, ignore)] // the OS driver opens a kqueue or an eventfd, which Miri does not model
fn a_poll_busy_on_the_cpu_past_the_quantum_is_its_tasks() {
  let counters = one_busy_period(Hold::OnCpu);
  let expected = if HAS_THREAD_CLOCK {
    (LONG_POLLS - 1, 0, 0, 1)
  } else {
    (0, 0, 0, LONG_POLLS)
  };
  assert_eq!(attributed(&counters), expected, "{counters:?}");
}

/// §4.3, A-31: a poll that blocked in a call past the quantum is its task's — the shard stalled on it just
/// the same — where the platform counts a thread's voluntary switches (Linux). macOS cannot tell a block
/// from a preemption, so there it is unattributed, as it is everywhere with no per-thread clock.
#[test]
#[cfg_attr(miri, ignore)] // the OS driver opens a kqueue or an eventfd, which Miri does not model
fn a_poll_blocked_in_a_call_past_the_quantum_is_its_tasks_where_blocks_are_counted() {
  let counters = one_busy_period(Hold::InCall);
  let expected = if cfg!(target_os = "linux") {
    (LONG_POLLS - 1, LONG_POLLS - 1, 0, 1)
  } else {
    (0, 0, 0, LONG_POLLS)
  };
  assert_eq!(attributed(&counters), expected, "{counters:?}");
}

/// Restores the calling thread's CPU affinity on drop, so a failed assertion leaves the test thread as
/// it found it.
#[cfg(target_os = "linux")]
struct RestoreAffinity(rustix::thread::CpuSet);

#[cfg(target_os = "linux")]
impl Drop for RestoreAffinity {
  fn drop(&mut self) {
    let _ = rustix::thread::sched_setaffinity(None, &self.0);
  }
}

/// §4.3, A-31: a runnable poll the host keeps off its CPU past the quantum is not its task's. The shard
/// thread and a spinning competitor are pinned to one CPU, and each poll yields to it until the hold has
/// passed: Linux counts those switches involuntary, as it does a preemption, and the poll's own CPU stays
/// within the quantum. Linux only: macOS will not pin, and cannot tell this from a block anyway.
#[cfg(target_os = "linux")]
#[test]
#[cfg_attr(miri, ignore)] // the OS driver opens a kqueue or an eventfd, which Miri does not model
fn a_poll_the_host_kept_off_its_cpu_is_not_its_tasks() {
  use std::sync::atomic::{AtomicBool, Ordering};
  let saved = rustix::thread::sched_getaffinity(None).unwrap();
  let cpu = rustix::thread::sched_getcpu();
  let mut one = rustix::thread::CpuSet::new();
  one.set(cpu);
  if let Err(refusal) = rustix::thread::sched_setaffinity(None, &one) {
    eprintln!(
      "SKIP a_poll_the_host_kept_off_its_cpu_is_not_its_tasks: pinning refused ({refusal})"
    );
    return;
  }
  let _restore = RestoreAffinity(saved);
  let running = AtomicBool::new(false);
  let stop = AtomicBool::new(false);
  let counters = std::thread::scope(|scope| {
    scope.spawn(|| {
      rustix::thread::sched_setaffinity(None, &one).unwrap();
      running.store(true, Ordering::Release);
      while !stop.load(Ordering::Acquire) {
        std::hint::spin_loop();
      }
    });
    while !running.load(Ordering::Acquire) {
      std::thread::yield_now();
    }
    let counters = one_busy_period(Hold::Runnable);
    stop.store(true, Ordering::Release);
    counters
  });
  assert_eq!(
    attributed(&counters),
    (0, 0, LONG_POLLS - 1, 1),
    "{counters:?}"
  );
}
