//! The daemon's liveness watchdog (§4.14 `daemon.alive`, A-228): an owned OS thread, outside the runtime and pinned to
//! no core, that writes the anchor's heartbeat word and decides, each beat, whether every shard is alive.
//!
//! Before A-228 the beat was a task on the control shard (`heartbeat_loop`), so the beat missed whenever that shard's
//! pinned thread was kept off its core, and the anchor killed a daemon that was alive and serving once a fixed second
//! passed without one. Under two spinners a logical CPU on CI's 4-CPU Windows runners the control shard ran 2–4 steps in
//! about a second, its longest poll 503–576 ms, and the anchor killed sep1 and comb1 that way (GAPS, "The anchor kills
//! live daemons on a busy machine"; diagnostics run 42161838327). A starved shard is not a hung one, and this thread
//! tells them apart by the rule observations already use (A-65, `crate::observe`'s `ShardTime`): a shard is judged in its
//! own CPU time.
//!
//! Each beat, for every shard, from its registry pulse (`steps`), its parking flag and its thread's CPU clock
//! (`slates_rt::registry::shard_cpu`, which counts cycles on Windows):
//! - **alive** while it is parked in its driver, or has completed a step since the last look;
//! - otherwise, since its last step, **stuck (wedged)** once a whole window has passed in which it moved in no way: no
//!   CPU progress, never seen runnable, and no wait for a CPU credited to it. A deadlock, and a host call that never
//!   returns, do none of these;
//! - **stuck (spinning)** once it has run a whole window of CPU without completing a step: a task that does not yield.
//!   A cooperative task yields within the shard's step budget, microseconds, so a window of CPU is no poll's;
//! - **starved** once a whole window has passed with no CPU progress while the scheduler had it waiting for a CPU. It is
//!   not stuck (A-228: on adacachy at CPUWeight=1 a shard held off its CPU was runnable throughout while its clock stood
//!   still, and the anchor killed a live daemon); it beats, and is logged once an episode;
//! - **starved for memory** likewise while it was held off the CPU in a wait memory shows as, as the host reported a
//!   memory stall (A-336, [`held_for_memory`]): a thread waiting on a page-in, a reclaim or a refault is neither running
//!   nor runnable, as a deadlocked one is, and a lone daemon at a few hundred megabytes free ended itself on that verdict
//!   (exit 4, fleet test binaries at 0.2 to 1 GB free, 2026-11-17). Such a look counts as the shard's movement, as a
//!   runnable one does, so a stuck shard is still judged wedged a window after the stall clears;
//! - **alive** otherwise: busy inside one poll for less than a window, or given a little CPU at a time.
//!
//! What the scheduler says comes from `slates_rt::registry::shard_run_readings`, asked only about shards with no step
//! and no CPU rise since the last look, once a beat (`slates_rt`'s `thread_clock` names each source). By platform:
//! - **Linux**: the thread's state (`R` is runnable; asleep, in the kernel or stopped is not) and its run-queue wait. The
//!   state decides: the kernel credits a wait only once the thread reaches a CPU or leaves the queue, so a thread held
//!   off a CPU for 3 s showed no growth in 20 of 20 CI runs. A rise says it waited between two looks, seen in neither.
//! - **macOS and Windows**: the state alone. Neither keeps a per-thread time waited for a CPU, so the state is sampled at
//!   each look. That loses nothing the rule needs: a thread leaves the run queue for a block only by running first, which
//!   its CPU clock shows.
//! - **No answer** (Miri, or a source that cannot be read): the CPU clock stands alone, as it did before.
//!
//! A stopped thread never reads as runnable, so a frozen process is still killed.
//!
//! What the host says of memory comes from `slates_machine::memory_stall`, read once a look that suspects a shard, and
//! the thread's own kind of wait from the same scheduler reading (A-336, one rule on every platform, each giving what
//! it can):
//! - **Linux**: PSI's memory `some` total rising since the last look (`/proc/pressure/memory`), and the thread held in
//!   the kernel past signals (`D`), never asleep in a futex, a poll or a sleep (`S`), which no stall excuses. `some`, not
//!   `full`: a thread delayed on memory on a CPU that runs anything else never shows in `full`, and a busy machine runs
//!   something on every CPU (`kernel/sched/psi.c`). The thread's `wchan` names its wait in the log line.
//! - **macOS**: `kern.memorystatus_vm_pressure_level` above normal. XNU reports a user page fault's wait as it does a
//!   lock's, so any blocked wait counts while the level is raised.
//! - **Windows**: the low-memory resource notification signaled; any blocked wait counts (its wait reason is not read).
//! - **No reading** (PSI absent, a refused read, Miri): nothing excuses a still shard, as before.
//!
//! The window is the existing liveness budget ([`crate::daemon::LIVENESS_BUDGET_NS`]), a fixed number recorded as R3
//! debt in GAPS. A-228 adds no number to it. A poll's CPU is held to the build the daemon runs in (A-318,
//! [`spin_window_ns`]): the spinning window is the budget scaled by how much slower this build runs compiled code than
//! the machine runs the same work in machine code, measured once a process ([`slates_machine::build_cost`]), never below
//! the budget. The window of CPU is no poll's only where code runs at the speed the budget was set for: under
//! ThreadSanitizer, whose runtime is called at every memory access and every function's entry and exit, a poll of 1,054
//! ms and one of 1,060 ms of CPU were judged spinning and ended in-process daemons' whole test binaries on CI (runs
//! 44238377555 and diag-b13-2, 2026-11-18). The wedged and starved judgments are on the wall and stay on the budget: a
//! slower build stretches no wait. While every shard is alive and the control loop lives, the thread beats; once
//! a shard is stuck it logs what it saw and withholds the beat, and the anchor, which judges the beat's age by its own
//! measured lateness, kills the daemon. Every look is marked in the segment too, beating or not (`SUP_LOOKED`,
//! A-232 step 5), so the anchor tells a withheld beat (the looks go on) from a watchdog that was itself kept off its CPU
//! (they stop). When the control loop ends (a refused boot, A-100), the thread withholds the
//! beat too, so the anchor restarts the daemon as before. A stop tells the thread first ([`Watchdog::stopping`]), since
//! the shutdown ends the control loop before it stops the thread: a control loop that ends then is the stop's, and is
//! not announced as one to restart (under two spinners a core, 8 of 18 daemons stopped in one `nfs_hostile` run said
//! so before, an 18-core Mac, 2026-11-07). Under Miri, which has no thread clock, the wall window stands
//! alone, as it does for observations.
//!
//! **A daemon nobody supervises ends itself (A-232).** Withholding the beat ends a daemon only when an anchor reads the
//! beat. A daemon run alone (`slates daemon` with no anchor, [`crate::SegmentSource::Create`]) is read by nobody, so
//! before A-232 a stuck one sat with its beat withheld for good, and its clients waited on a daemon that would never
//! answer. The watchdog is told at its start which it is ([`Supervision`], from the segment's source), and each turn it
//! decides one of three things ([`Turn`], [`decide`]): beat; withhold the beat, for the anchor to judge; or end the
//! daemon's process. On the same verdict, the same window and the same starved-is-not-stuck rule as an anchored
//! daemon, a lone one ends its own process where an anchored one withholds the beat, so its clients see the exit as they
//! see a kill: the socket's close on Linux, the exit watch on macOS and Windows. A starved shard ends nothing, and
//! neither does anything found while the daemon stops: the stop is already its end. The end is a deliberate exit with
//! [`FAILED_EXIT_CODE`] after the verdict is logged, never a panic: a stuck shard's thread cannot be joined, so an orderly
//! stop would wait on it for good.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::thread::JoinHandle;

use slates_anchor::AnchorSegment;
use slates_machine::memory_stall::StallReading;
use slates_rt::{RunReading, RunState, SlotHolder};
use slates_vfs::clock::{Clock, HostClock};

use crate::daemon::{HEARTBEAT_NS, LIVENESS_BUDGET_NS};

/// The daemon's watchdog thread.
pub struct Watchdog {
  /// The stop signal: the thread sees this sender go away at its next turn.
  stop: Option<Sender<()>>,
  handle: Option<JoinHandle<()>>,
  /// What the thread did, for whoever asks ([`Watchdog::turns`]).
  turns: &'static Turns,
  /// The CPU a poll may run before the shard is judged spinning ([`spin_window_ns`]).
  spin_window_ns: u64,
  /// Where the thread reads the host's memory stall (A-336).
  memory: &'static MemoryStallSource,
}

/// Where the watchdog reads the host's memory stall (A-336): the host's own reading
/// ([`slates_machine::memory_stall::read`]), unless a test has put a level in its place ([`MemoryStallSource::inject`],
/// `Daemon::inject_memory_stall`), as `Daemon::inject_pressure_hold` drives admission's hold: so a test proves the rule
/// with no host short of memory, driving the very reading the rule takes. Written by a test, read by the watchdog's
/// thread; a flag, so acquire and release. One per daemon, leaked for the process's life like [`Turns`]: a word.
#[derive(Debug, Default)]
#[repr(align(128))]
pub struct MemoryStallSource {
  /// [`HOST_READING`], [`INJECTED_CLEAR`] or [`INJECTED_PRESSED`].
  injected: std::sync::atomic::AtomicU8,
}

/// Format: the source's word when the host's own reading is taken.
const HOST_READING: u8 = 0;
/// Format: the source's word when a test has put a clear level in the host's reading's place.
const INJECTED_CLEAR: u8 = 1;
/// Format: the source's word when a test has put a pressed level in the host's reading's place.
const INJECTED_PRESSED: u8 = 2;

impl MemoryStallSource {
  /// Puts a level in the host's reading's place, pressed or clear; `None` gives the host's own reading back.
  pub fn inject(&self, pressed: Option<bool>) {
    let word = match pressed {
      None => HOST_READING,
      Some(false) => INJECTED_CLEAR,
      Some(true) => INJECTED_PRESSED,
    };
    self
      .injected
      .store(word, std::sync::atomic::Ordering::Release);
  }

  /// The host's memory stall now, or the level a test put in its place.
  fn read(&self) -> Option<StallReading> {
    match self.injected.load(std::sync::atomic::Ordering::Acquire) {
      INJECTED_CLEAR => Some(StallReading::Level { pressed: false }),
      INJECTED_PRESSED => Some(StallReading::Level { pressed: true }),
      _ => slates_machine::memory_stall::read(),
    }
  }

  /// Where the reading comes from, in the banner's words.
  fn describe(&self) -> &'static str {
    match self.injected.load(std::sync::atomic::Ordering::Acquire) {
      INJECTED_CLEAR | INJECTED_PRESSED => "a level a test put in the host's reading's place",
      _ => slates_machine::memory_stall::source(),
    }
  }
}

/// What the watchdog thread did, counted as it goes: how many times it looked at the shards, and at how many of those it
/// withheld the beat because a shard was stuck. A gap in the beat with looks and no withheld one is the thread's own
/// lateness (it was itself kept off a CPU), not the rule's verdict; only a withheld look is the rule's (A-228). Written by
/// the thread alone, read by anyone; statistics, so relaxed. One per daemon, leaked for the process's life: a few words.
#[derive(Debug, Default)]
#[repr(align(128))]
pub struct Turns {
  looks: std::sync::atomic::AtomicU64,
  withheld: std::sync::atomic::AtomicU64,
}

impl Turns {
  /// The looks so far, and of them those that withheld the beat for a stuck shard.
  pub fn read(&self) -> (u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (self.looks.load(Relaxed), self.withheld.load(Relaxed))
  }

  fn looked(&self, withheld: bool) {
    use std::sync::atomic::Ordering::Relaxed;
    self.looks.fetch_add(1, Relaxed);
    if withheld {
      self.withheld.fetch_add(1, Relaxed);
    }
  }
}

impl std::fmt::Debug for Watchdog {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Watchdog")
      .field("running", &self.handle.is_some())
      .finish()
  }
}

impl Watchdog {
  /// Starts the thread over `segment` (its own mapping of the anchor's segment), judging the shards `holders` name.
  /// `control` is the control loop's life: while its sender lives the loop lives. `supervision` says whether an anchor
  /// reads the beat, and so whether a stuck verdict withholds it or ends the process (A-232).
  pub fn start(
    segment: AnchorSegment,
    holders: Vec<SlotHolder>,
    control: Receiver<()>,
    supervision: Supervision,
  ) -> std::io::Result<Watchdog> {
    let (stop, stop_signal) = channel();
    let cost = slates_machine::build_cost::build_cost();
    let spin_window_ns = spin_window_ns(cost);
    let memory: &'static MemoryStallSource = Box::leak(Box::default());
    slates_machine::log_line!(
      "slates daemon: the watchdog beats every {} ms and finds a shard stuck after a {} ms window without a step, with \
       no CPU progress and no wait for a CPU, or after {} ms of CPU without a step (the window is the liveness budget, \
       and a poll's CPU is held to this build: {}); a shard waiting for a CPU is starved, not stuck, and the beat goes \
       on, and so is one waiting on memory while the host reports a memory stall ({}: {:?} now); {}",
      HEARTBEAT_NS / NS_PER_MS,
      LIVENESS_BUDGET_NS / NS_PER_MS,
      spin_window_ns / NS_PER_MS,
      describe_cost(cost),
      memory.describe(),
      memory.read(),
      match supervision {
        Supervision::Anchored => "an anchor reads the beat, so a stuck shard withholds it",
        Supervision::Alone =>
          "no anchor reads the beat, so a stuck shard ends the daemon's process",
      }
    );
    let turns: &'static Turns = Box::leak(Box::default());
    let handle = std::thread::Builder::new()
      .name("slates-watchdog".to_owned())
      .spawn(move || {
        run(
          &segment,
          &holders,
          (&control, &stop_signal),
          supervision,
          (turns, spin_window_ns, memory),
        )
      })?;
    Ok(Watchdog {
      stop: Some(stop),
      handle: Some(handle),
      turns,
      spin_window_ns,
      memory,
    })
  }

  /// Where the thread reads the host's memory stall, for a test to put a level in its place (A-336).
  pub fn memory_stall(&self) -> &'static MemoryStallSource {
    self.memory
  }

  /// What the thread has done so far ([`Turns`]).
  pub fn turns(&self) -> &'static Turns {
    self.turns
  }

  /// The CPU a poll may run without a step before its shard is judged spinning: the liveness budget held to this build
  /// ([`spin_window_ns`], A-318).
  pub fn spin_window_ns(&self) -> u64 {
    self.spin_window_ns
  }

  /// Tells the thread the daemon is stopping, before the shutdown ends the control loop, so that end is not announced
  /// as one the anchor restarts. The thread beats on as before until [`Watchdog::stop`].
  pub fn stopping(&self) {
    if let Some(stop) = self.stop.as_ref() {
      let _ = stop.send(());
    }
  }

  /// Stops the thread and joins it.
  pub fn stop(&mut self) {
    drop(self.stop.take());
    if let Some(handle) = self.handle.take()
      && handle.join().is_err()
    {
      slates_machine::log_line!("slates-server: the watchdog thread panicked");
    }
  }
}

impl Drop for Watchdog {
  fn drop(&mut self) {
    self.stop();
  }
}

/// The CPU a poll may run without a step before its shard is judged spinning (A-318): the liveness budget held to the
/// build the daemon runs in, scaled by how much slower it runs compiled code than the machine runs the same work in
/// machine code ([`slates_machine::build_cost`]), never below the budget. A cooperative task yields within the shard's
/// step budget, so a window of CPU is no poll's in the build the budget was set for; a build that runs every poll
/// slower (ThreadSanitizer's, an unoptimized one) runs a window of CPU in a poll that is no spin. Where the cost was not
/// measured (Miri, which has no thread clock and judges by the wall; a refused measure, logged) the budget stands.
pub fn spin_window_ns(
  cost: Result<slates_machine::build_cost::BuildCost, slates_machine::build_cost::BuildCostRefused>,
) -> u64 {
  cost.map_or(LIVENESS_BUDGET_NS, |cost| cost.scale(LIVENESS_BUDGET_NS))
}

/// The build's measured cost in the banner's words.
fn describe_cost(
  cost: Result<slates_machine::build_cost::BuildCost, slates_machine::build_cost::BuildCostRefused>,
) -> String {
  /// Format: tenths in one.
  const TENTHS: u64 = 10;
  match cost {
    Ok(cost) => {
      let tenths = cost.tenths();
      format!(
        "this build runs compiled code {}.{} times as slowly as the machine runs it in machine code, {} ns against {} \
         ns for a batch of {} chains",
        tenths / TENTHS,
        tenths % TENTHS,
        cost.compiled_ns,
        cost.machine_ns,
        cost.batch
      )
    }
    Err(refused) => {
      format!("this build's cost was not measured ({refused:?}), so the budget stands")
    }
  }
}

/// Format: nanoseconds per millisecond, for the watchdog's log lines.
const NS_PER_MS: u64 = 1_000_000;
/// Format: nanoseconds per microsecond, for the lateness the end's log line names.
const NS_PER_US: u64 = 1_000;

/// Format: the exit code of a daemon its own watchdog ended (A-232), and `slates`'s code for an anchor or a daemon that
/// failed (`EXIT_FAILED` in `crates/cli/src/main.rs`, which takes it from here, so the two cannot drift).
pub const FAILED_EXIT_CODE: u8 = 4;

/// Whether an anchor reads this daemon's beat (A-232), told to the watchdog at its start from the segment's source
/// ([`crate::SegmentSource::supervision`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Supervision {
  /// An anchor in another process holds the segment, reads the beat and kills a daemon whose beat lapses (`FromEnv`,
  /// the `slates anchor` process, or a test that plays it across a spawn). A stuck shard withholds the beat, and the
  /// anchor ends the daemon.
  Anchored,
  /// No other process can end the daemon: `Create` (`slates daemon` run alone, or a test's daemon with no anchor) and
  /// `Handoff` (an anchor in this process, which cannot kill the daemon without ending itself). A stuck shard ends the
  /// daemon's own process, on the verdict that withholds an anchored daemon's beat.
  Alone,
}

/// Why a turn did not beat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
  /// A shard was found stuck (wedged or spinning): the first such shard and its verdict.
  Stuck {
    /// The shard.
    shard: u16,
    /// What the watchdog found of it.
    verdict: Verdict,
  },
  /// The control loop ended (a refused boot, A-100) while the daemon was not stopping.
  ControlEnded,
}

/// What one turn of the watchdog does (A-232).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
  /// Every shard is alive (a starved one included) and the control loop lives: write the beat.
  Beat,
  /// Leave the beat unwritten, for the anchor to judge, or because the daemon is stopping.
  Withhold(Cause),
  /// End the daemon's process: no anchor reads the beat, so nobody else would.
  End(Cause),
}

/// One turn's decision, pure so the table is tested on every host: what the shards' verdicts and the control loop's life
/// mean for a daemon of `supervision`. `stuck` is the first shard found stuck this turn, if any; `control_ended` whether
/// the control loop has ended; `stopping` whether the daemon has begun its own stop ([`Watchdog::stopping`]), which
/// already ends it, so nothing found then ends the process.
pub fn decide(
  supervision: Supervision,
  stuck: Option<(u16, Verdict)>,
  control_ended: bool,
  stopping: bool,
) -> Turn {
  let cause = match (stuck, control_ended) {
    (Some((shard, verdict)), _) => Cause::Stuck { shard, verdict },
    (None, true) => Cause::ControlEnded,
    (None, false) => return Turn::Beat,
  };
  match (supervision, stopping) {
    (Supervision::Alone, false) => Turn::End(cause),
    (Supervision::Anchored, _) | (Supervision::Alone, true) => Turn::Withhold(cause),
  }
}

/// What the watchdog found of one shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
  /// Parked, stepping, given a little CPU at a time, or inside one poll for less than a window.
  Alive,
  /// No step and no CPU progress for `for_ns` (at least a window), but the scheduler had it waiting for a CPU, or it was
  /// held off the CPU waiting on memory while the host reported a memory stall (A-336). It beats.
  Starved {
    /// How long the shard has run no CPU without a step.
    for_ns: u64,
    /// What it waited for at the last look that excused it.
    of: Want,
  },
  /// No step, no CPU progress and no wait for a CPU for `for_ns` (at least a window).
  Wedged {
    /// How long the shard has not moved at all.
    for_ns: u64,
  },
  /// No step while it ran `cpu_ns` of CPU (at least the spin window, `window_ns`).
  Spinning {
    /// The CPU the shard ran since its last step.
    cpu_ns: u64,
    /// The spin window it was judged against: the liveness budget held to this build ([`spin_window_ns`], A-318).
    window_ns: u64,
  },
}

impl Verdict {
  /// Whether a shard with this verdict lets the watchdog beat: a starved shard is not a stuck one.
  fn beats(self) -> bool {
    matches!(self, Verdict::Alive | Verdict::Starved { .. })
  }
}

/// What a starved shard waited for, at the last look that told it from a wedged one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Want {
  /// A CPU: the scheduler had its thread runnable, or credited it a wait for one (A-228).
  #[default]
  Cpu,
  /// Memory: its thread was off the CPU in a wait memory shows as, while the host reported a memory stall (A-336,
  /// [`held_for_memory`]).
  Memory,
}

/// Whether a still shard's look finds it held waiting on memory (A-336): the host reported a memory stall since the
/// look before (`before`, at this look `now`; [`slates_machine::memory_stall::stalled_between`]), and its thread is off
/// the CPU in a wait memory can show as. Where the platform tells such a wait (Linux), that is one the kernel holds the
/// thread in past signals (`D`: a page-in, a reclaim's throttle, a refault), never a futex, a poll or a sleep (`S`),
/// however the host stalls; where it does not (macOS reports a user page fault's wait as it does a lock's, and Windows'
/// wait reason is not read), any blocked wait. With no reading of the thread, nothing excuses it, as before. Pure, so
/// the rule is tested on every host.
pub fn held_for_memory(
  before: Option<StallReading>,
  now: Option<StallReading>,
  run: Option<RunReading>,
) -> bool {
  run.is_some_and(|run| run.state == RunState::Blocked && run.uninterruptible != Some(false))
    && slates_machine::memory_stall::stalled_between(before, now)
}

/// One shard's history since its last step, as the watchdog keeps it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShardWatch {
  /// The step count at the last look.
  steps: u64,
  /// When the shard was last seen stepping or parked (the watchdog's clock).
  stepped_ns: u64,
  /// Its thread's CPU time then, nanoseconds (`None` without a thread clock).
  cpu_at_step_ns: Option<u64>,
  /// Its thread's progress count at the last look it rose (`None` without a thread clock).
  progress: Option<u64>,
  /// When its progress last rose.
  progressed_ns: u64,
  /// When it last moved in any way: its progress rose, it was runnable, or a wait for a CPU was credited to it.
  moved_ns: u64,
  /// Its thread's time waited for a CPU at the last look, nanoseconds, where the scheduler says.
  waited_ns: Option<u64>,
  /// When the watchdog last looked at it.
  looked_ns: u64,
  /// The look before the one that last saw it move: its last movement came after this look, so a verdict's lateness is
  /// measured from here ([`ShardWatch::still_after_ns`]).
  still_after_ns: u64,
  /// The host's memory stall at the last look, where it was read ([`Look::memory`]).
  memory: Option<StallReading>,
  /// What it waited for at the last look that excused its stillness: a CPU or memory.
  starved_of: Want,
}

/// One look at a shard: its step count, whether it is parked, its thread clock reading (CPU nanoseconds, progress),
/// what the scheduler says of its thread, and the watchdog's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Look {
  /// The shard's step count.
  pub steps: u64,
  /// Whether it is parked in its driver.
  pub parked: bool,
  /// Its thread's CPU clock, when it has one: `(spent_ns, progress)`.
  pub cpu: Option<(u64, u64)>,
  /// What the scheduler says of its thread, when it can be read.
  pub run: Option<RunReading>,
  /// The host's memory stall at this look, when it was read (A-336, [`slates_machine::memory_stall::read`]); the
  /// watchdog reads it only at a look that suspects a shard.
  pub memory: Option<StallReading>,
  /// The watchdog's clock, nanoseconds.
  pub now_ns: u64,
}

impl ShardWatch {
  /// A shard's history before the watchdog's first look, which began at `began_ns`: whatever the shard does after it,
  /// the watchdog can bound, so the first look's "look before" is the watchdog's own start (A-232). A shard that the
  /// first look already finds still may have stopped before the watchdog began; its verdict's lateness is then measured
  /// from the start, which bounds only what came after it.
  pub fn begun_at(began_ns: u64) -> ShardWatch {
    ShardWatch {
      looked_ns: began_ns,
      ..ShardWatch::default()
    }
  }

  /// The watchdog's look before the one that last saw this shard move: whatever it did last, it did after this look.
  /// A wedge verdict is due a window after the last movement, and looks come a beat apart, so the verdict comes no
  /// later than a window and a beat after this look, past which is the verdict's own lateness (A-232).
  pub fn still_after_ns(&self) -> u64 {
    self.still_after_ns
  }

  /// When the shard last moved, as the watchdog saw it: the look that last saw it step, park, progress, run or wait.
  pub fn moved_ns(&self) -> u64 {
    self.moved_ns
  }

  /// Whether `look` leaves the shard under suspicion, so the scheduler is asked about its thread: it is not parked, has
  /// completed no step since the last look, and its CPU clock did not rise. A shard that steps or parks costs nothing.
  fn suspects(&self, look: &Look) -> bool {
    !look.parked
      && look.steps == self.steps
      && self.stepped_ns != 0
      && look
        .cpu
        .is_none_or(|(_, progress)| self.progress.is_none_or(|before| progress <= before))
  }

  /// The history after a look at a shard that neither stepped nor parked: `rose` whether its CPU progress rose, `moved`
  /// whether it moved in any way (that, or runnable, or credited a wait, or held for memory), `excused` what it was
  /// found waiting for, if anything.
  fn after_still_look(
    self,
    look: &Look,
    progress: Option<u64>,
    (rose, moved): (bool, bool),
    excused: Option<Want>,
  ) -> ShardWatch {
    ShardWatch {
      memory: look.memory,
      starved_of: excused.unwrap_or(self.starved_of),
      progress: if rose { progress } else { self.progress },
      progressed_ns: if rose {
        look.now_ns
      } else {
        self.progressed_ns
      },
      moved_ns: if moved { look.now_ns } else { self.moved_ns },
      waited_ns: look.run.and_then(|run| run.waited_ns),
      looked_ns: look.now_ns,
      still_after_ns: if moved {
        self.looked_ns
      } else {
        self.still_after_ns
      },
      ..self
    }
  }

  /// Judges one look against the history (A-65's rule; `window_ns` is the liveness budget, `spin_window_ns` the budget of
  /// CPU a poll may run, the liveness budget held to this build, [`spin_window_ns`]) and returns the verdict and the
  /// history for the next look. Pure, so the rule is tested on every host with no thread under it.
  pub fn judge(self, look: Look, window_ns: u64, spin_window_ns: u64) -> (Verdict, ShardWatch) {
    let (spent, progress) = look.cpu.map_or((None, None), |(spent, progress)| {
      (Some(spent), Some(progress))
    });
    let waited_ns = look.run.and_then(|run| run.waited_ns);
    if look.parked || look.steps != self.steps || self.stepped_ns == 0 {
      return (
        Verdict::Alive,
        ShardWatch {
          steps: look.steps,
          stepped_ns: look.now_ns.max(1),
          cpu_at_step_ns: spent,
          progress,
          progressed_ns: look.now_ns,
          moved_ns: look.now_ns,
          waited_ns,
          looked_ns: look.now_ns,
          still_after_ns: self.looked_ns,
          memory: look.memory,
          starved_of: Want::Cpu,
        },
      );
    }
    let rose = match (self.progress, progress) {
      (Some(before), Some(now)) => now > before,
      // No thread clock (Miri): the wall window stands alone, as for observations.
      _ => false,
    };
    // Linux credits a wait only once the thread reaches a CPU or leaves the queue, so a thread waiting now shows no
    // rise; the state says so instead. A rise says it waited between two looks, seen in neither.
    let waiting = look.run.is_some_and(|run| run.state == RunState::Runnable)
      || matches!((self.waited_ns, waited_ns), (Some(before), Some(now)) if now > before);
    // Off the CPU in a wait memory shows as while the host reported a memory stall: starved for memory, not stuck
    // (A-336). Before, a shard waiting on a page-in at a few hundred megabytes free was judged wedged, never having been
    // runnable, and a lone daemon ended itself. A stuck shard is still judged a window after the stall clears.
    let memory = !waiting && held_for_memory(self.memory, look.memory, look.run);
    let excused = if waiting {
      Some(Want::Cpu)
    } else {
      memory.then_some(Want::Memory)
    };
    // A look a window or more after the watchdog's last one saw nothing in between: the watchdog's own thread was kept off
    // its CPU, as a process frozen whole or paging is, and whether the shard waited for a CPU in that gap was never asked.
    // Its stillness there is the process's, so the shard's measures start again at this look, its still time and its time
    // without CPU alike, and a wedge is judged only over a window the watchdog watched (A-308). Before, a fleet test binary
    // was ended with exit 4 on a wedge verdict that came 146 s past its window, when the whole process had gone
    // unscheduled at 0.2 to 1 GB free (g231, load 62 to 131). A stuck shard is still judged a window after the watchdog's
    // looks come back.
    let unwatched = look.now_ns.saturating_sub(self.looked_ns) >= window_ns;
    let next = self.after_still_look(
      &look,
      progress,
      (rose || unwatched, rose || excused.is_some() || unwatched),
      excused,
    );
    let still_for = look.now_ns.saturating_sub(next.moved_ns);
    if still_for >= window_ns {
      return (Verdict::Wedged { for_ns: still_for }, next);
    }
    let cpu_since_step = match (self.cpu_at_step_ns, spent) {
      (Some(at_step), Some(now)) => now.saturating_sub(at_step),
      _ => 0,
    };
    // A poll is judged in this build's CPU: a build that runs compiled code slower runs every poll slower, so its window
    // of CPU is the budget scaled by that slowdown, measured (A-318). Before, ThreadSanitizer's polls of 1,054 and 1,060
    // ms ended in-process daemons' test binaries on CI.
    if cpu_since_step >= spin_window_ns {
      return (
        Verdict::Spinning {
          cpu_ns: cpu_since_step,
          window_ns: spin_window_ns,
        },
        next,
      );
    }
    let no_cpu_for = look.now_ns.saturating_sub(next.progressed_ns);
    if no_cpu_for >= window_ns {
      return (
        Verdict::Starved {
          for_ns: no_cpu_for,
          of: next.starved_of,
        },
        next,
      );
    }
    (Verdict::Alive, next)
  }
}

/// The thread's loop: each beat, look at every shard, decide the turn ([`decide`]) and do it: beat the anchor's word,
/// withhold it, or end the process.
fn run(
  segment: &AnchorSegment,
  holders: &[SlotHolder],
  (control, stop): (&Receiver<()>, &Receiver<()>),
  supervision: Supervision,
  (turns, spin_window_ns, memory): (&Turns, u64, &MemoryStallSource),
) {
  let mut clock = HostClock::new();
  let mut watches = vec![ShardWatch::begun_at(clock.monotonic_ns().max(1)); holders.len()];
  let mut reported = vec![None; holders.len()];
  let mut control_ended = false;
  let mut said_control_ended = false;
  let mut stopping = false;
  let mut lateness = Lateness::default();
  loop {
    match stop.recv_timeout(std::time::Duration::from_nanos(HEARTBEAT_NS)) {
      Err(RecvTimeoutError::Timeout) => {}
      // The daemon is stopping: the control loop's end that follows is the stop's.
      Ok(()) => stopping = true,
      Err(RecvTimeoutError::Disconnected) => return,
    }
    if !control_ended && matches!(control.try_recv(), Err(TryRecvError::Disconnected)) {
      control_ended = true;
    }
    let now = clock.monotonic_ns();
    let late_ns = lateness.look(now);
    let mut looks: Vec<Option<Look>> = holders.iter().map(|holder| look_at(*holder, now)).collect();
    read_runs(holders, &watches, &mut looks, memory);
    // In shard order, aligned with `reported`: `None` for a slot that now holds another registration.
    let verdicts: Vec<Option<(u16, Verdict)>> = holders
      .iter()
      .zip(looks)
      .zip(watches.iter_mut())
      .map(|((holder, look), watch)| {
        let (verdict, next) = watch.judge(look?, LIVENESS_BUDGET_NS, spin_window_ns);
        *watch = next;
        Some((holder.shard(), verdict))
      })
      .collect();
    let stuck_at = verdicts
      .iter()
      .position(|found| found.is_some_and(|(_, verdict)| !verdict.beats()));
    let stuck = stuck_at.and_then(|index| verdicts.get(index).copied().flatten());
    let turn = decide(supervision, stuck, control_ended, stopping);
    if let Some(index) = stuck_at {
      diag_spin::dump(holders, index, now);
    }
    if let Turn::End(cause) = turn {
      turns.looked(stuck.is_some());
      let verdict_late_ns = stuck_at
        .and_then(|index| watches.get(index))
        .and_then(|watch| verdict_lateness_ns(now, watch.still_after_ns()));
      end_process(cause, lateness.since_beat(false, late_ns), verdict_late_ns);
    }
    for (((found, said), watch), holder) in verdicts
      .iter()
      .zip(reported.iter_mut())
      .zip(&watches)
      .zip(holders)
    {
      if let Some((shard, verdict)) = found {
        report(said, (*shard, *verdict, turn), (watch, now), || {
          memory_wait_words(*holder, watch)
        });
      }
    }
    turns.looked(stuck.is_some());
    lateness.since_beat(turn == Turn::Beat, late_ns);
    mark(segment, turn, now);
    match turn {
      Turn::Beat => {}
      Turn::Withhold(Cause::ControlEnded) => {
        if !stopping && !said_control_ended {
          said_control_ended = true;
          slates_machine::log_line!(
            "slates daemon: the control loop ended; the watchdog withholds the beat so the anchor restarts the daemon"
          );
        }
      }
      // Ended above, before any line that takes the log's lock.
      Turn::Withhold(Cause::Stuck { .. }) | Turn::End(_) => {}
    }
  }
}

/// Writes a turn into the segment at `now_ns`: the look, at every turn, beating or not (A-232 step 5: a beat that lapses
/// while the looks go on is the watchdog's verdict, and one that lapses with them is this thread kept off its CPU, which
/// the anchor tells apart by this word, `SUP_LOOKED`); and the beat, when the turn beats.
fn mark(segment: &AnchorSegment, turn: Turn, now_ns: u64) {
  if let Ok(supervision) = segment.supervision() {
    supervision.look(now_ns);
    if turn == Turn::Beat {
      supervision.beat(now_ns);
    }
  }
}

/// How late the first look after a wedge came (A-232 step 5): the look that last saw the shard move, past a beat after
/// the look before it (`still_after_ns`, the watchdog's start before its first look). An anchored daemon's last beat is
/// the look before its verdict, within a window of that look, so it comes within `W + B` and this of the wedge: the
/// term an anchor's bound needs. `None` when the shard last moved at the watchdog's first look of it.
fn first_look_lateness_ns(watch: &ShardWatch) -> Option<u64> {
  (watch.still_after_ns() != 0).then(|| {
    watch
      .moved_ns()
      .saturating_sub(watch.still_after_ns())
      .saturating_sub(HEARTBEAT_NS)
  })
}

/// A stuck verdict's lateness, for its withhold line: the verdict's own ([`verdict_lateness_ns`]) and the first look's
/// ([`first_look_lateness_ns`]), in the end line's words.
fn stuck_lateness_words(watch: &ShardWatch, now_ns: u64) -> String {
  match (
    verdict_lateness_ns(now_ns, watch.still_after_ns()),
    first_look_lateness_ns(watch),
  ) {
    (Some(verdict), Some(first)) => format!(
      "the verdict came {} us past a window and a beat after the watchdog's last look before the shard last moved; \
       the look that last saw it move came {} us past a beat after the one before it",
      verdict / NS_PER_US,
      first / NS_PER_US
    ),
    _ => "the shard last moved at the watchdog's first look of it, so the verdict's lateness is not known".to_owned(),
  }
}

/// How late the watchdog's own looks run, past the beat they wait for: the thread is itself scheduled, and a busy
/// machine delays it. Kept since the last turn that beat, so the end names the lateness of the very looks its verdict
/// rests on (the one that last saw the shard move, and each after it), for a test to bound the end by (A-232).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Lateness {
  /// The last look's time on the watchdog's clock; zero before the first.
  last_look_ns: u64,
  /// The largest lateness of a look since the last beat, the beating look's own included.
  since_beat_ns: u64,
}

impl Lateness {
  /// Records a look at `now_ns` and returns how late it was: its gap from the last look past one beat.
  fn look(&mut self, now_ns: u64) -> u64 {
    let late_ns = if self.last_look_ns == 0 {
      0
    } else {
      now_ns
        .saturating_sub(self.last_look_ns)
        .saturating_sub(HEARTBEAT_NS)
    };
    self.last_look_ns = now_ns.max(1);
    late_ns
  }

  /// Folds this look's lateness into the episode's and returns the episode's: a beating look starts the episode
  /// afresh, since the verdicts that follow rest on what it saw.
  fn since_beat(&mut self, beat: bool, late_ns: u64) -> u64 {
    self.since_beat_ns = if beat {
      late_ns
    } else {
      self.since_beat_ns.max(late_ns)
    };
    self.since_beat_ns
  }
}

/// How late a stuck verdict at `now_ns` came: past a window and a beat after `still_after_ns`, the watchdog's look before
/// the one that last saw the shard move ([`ShardWatch::still_after_ns`]). The shard last moved after that look, so a
/// wedge ends a lone daemon within a window, a beat and this lateness of the wedge: the look's phase against the wedge
/// and the thread's own lateness, both measured here (A-232).
/// `None` when the shard's last movement was seen at the watchdog's first look of it, which has no look before it to
/// measure from.
fn verdict_lateness_ns(now_ns: u64, still_after_ns: u64) -> Option<u64> {
  (still_after_ns != 0).then(|| {
    now_ns
      .saturating_sub(still_after_ns)
      .saturating_sub(LIVENESS_BUDGET_NS)
      .saturating_sub(HEARTBEAT_NS)
  })
}

/// Ends the daemon's process on the watchdog's verdict (A-232): nobody reads this daemon's beat, so nobody else would end
/// it. A deliberate exit with [`FAILED_EXIT_CODE`], not a panic: a stuck shard's thread can be neither joined nor
/// stopped, so an orderly stop would wait on it for good. The exit runs no destructor; what the process held, the kernel
/// releases, and its clients see it as they see a kill.
///
/// The verdict is logged first, but the end never waits on the log for longer than one beat: a shard can be wedged in a
/// write to a standard error nobody drains, holding the lock every log line takes, and a watchdog that waited on that
/// lock would never end anything. So the line is written by a thread of its own. The end joins it once it says the line
/// is written, which it does as its last act, so the join returns at once; a writer still blocked after a beat cannot be
/// joined, and the exit ends it, so the exit is its owner then. Joined, a written line leaves no thread that finished
/// unjoined: ThreadSanitizer reported that one as a leak at the exit, after its own one-second exit sleep, and turned
/// the exit code into its own 66 (diag-g218-tsan-2, run 44217144677: exit 66 seen 27.4 s after the hold, where the
/// verdict came at 1.09 s).
fn end_process(cause: Cause, late_ns: u64, verdict_late_ns: Option<u64>) -> ! {
  let line = match cause {
    Cause::Stuck { shard, verdict } => format!(
      "slates daemon: no anchor reads the beat, so the watchdog ends the daemon's process (exit {FAILED_EXIT_CODE}): \
       shard {shard} is {}; {}; the watchdog's looks ran at most {} us late since its last beat",
      describe(verdict),
      verdict_late_ns.map_or_else(
        || "the shard last moved at the watchdog's first look of it, so the verdict's lateness is not known".to_owned(),
        |late| format!(
          "the verdict came {} us past a window and a beat after the watchdog's last look before the shard last moved",
          late / NS_PER_US
        )
      ),
      late_ns / NS_PER_US
    ),
    Cause::ControlEnded => format!(
      "slates daemon: the control loop ended and no anchor restarts the daemon, so the watchdog ends its process \
       (exit {FAILED_EXIT_CODE})"
    ),
  };
  let (written, logged) = channel::<()>();
  let writer = std::thread::Builder::new()
    .name("slates-watchdog-end".to_owned())
    .spawn(move || {
      slates_machine::log_line!("{line}");
      let _ = written.send(());
    });
  if let Ok(writer) = writer
    && logged
      .recv_timeout(std::time::Duration::from_nanos(HEARTBEAT_NS))
      .is_ok()
  {
    // The line is written and the writer's last act is done; its end is all that is left to wait for.
    let _ = writer.join();
  }
  std::process::exit(i32::from(FAILED_EXIT_CODE))
}

/// A stuck verdict in the end's words.
fn describe(verdict: Verdict) -> String {
  match verdict {
    Verdict::Wedged { for_ns } => format!(
      "wedged, unmoved for {} ms against the {} ms window",
      for_ns / NS_PER_MS,
      LIVENESS_BUDGET_NS / NS_PER_MS
    ),
    Verdict::Spinning { cpu_ns, window_ns } => format!(
      "spinning, {} ms of CPU without a step against the {} ms window",
      cpu_ns / NS_PER_MS,
      window_ns / NS_PER_MS
    ),
    Verdict::Alive | Verdict::Starved { .. } => "not stuck".to_owned(),
  }
}

/// One look at the shard `holder` names, at `now_ns`; `None` once its slot holds another registration.
fn look_at(holder: SlotHolder, now_ns: u64) -> Option<Look> {
  let (steps, parked) = slates_rt::registry::with_entry(holder.shard(), |entry| {
    (entry.pulse.steps(), entry.parking.parked())
  })?;
  let cpu =
    slates_rt::registry::shard_cpu(holder).map(|reading| (reading.spent_ns, reading.progress));
  Some(Look {
    steps,
    parked,
    cpu,
    run: None,
    memory: None,
    now_ns,
  })
}

/// Asks the scheduler about the shards under suspicion, all at once (at most one Windows snapshot a beat), and reads the
/// host's memory stall once for the look (A-336), into every shard's look: a look that suspects no shard reads neither,
/// so a daemon whose shards step or park costs nothing more than before.
fn read_runs(
  holders: &[SlotHolder],
  watches: &[ShardWatch],
  looks: &mut [Option<Look>],
  memory: &MemoryStallSource,
) {
  let suspected: Vec<usize> = looks
    .iter()
    .zip(watches)
    .enumerate()
    .filter_map(|(index, (look, watch))| {
      look
        .as_ref()
        .is_some_and(|look| watch.suspects(look))
        .then_some(index)
    })
    .collect();
  if suspected.is_empty() {
    return;
  }
  let stall = memory.read();
  for look in looks.iter_mut().flatten() {
    look.memory = stall;
  }
  let (at, asked): (Vec<usize>, Vec<SlotHolder>) = suspected
    .into_iter()
    .filter_map(|index| holders.get(index).map(|holder| (index, *holder)))
    .unzip();
  let readings = slates_rt::registry::shard_run_readings(&asked);
  for (index, reading) in at.into_iter().zip(readings) {
    if let Some(Some(look)) = looks.get_mut(index) {
      look.run = reading;
    }
  }
}

/// What an episode's log line said of a shard: its verdict's kind, and for a starved one what it waited for, so a shard
/// starved for a CPU that then waits on memory is logged again.
type Said = (std::mem::Discriminant<Verdict>, Option<Want>);

/// The [`Said`] of a verdict.
fn said_of(verdict: Verdict) -> Said {
  let of = match verdict {
    Verdict::Starved { of, .. } => Some(of),
    Verdict::Alive | Verdict::Wedged { .. } | Verdict::Spinning { .. } => None,
  };
  (std::mem::discriminant(&verdict), of)
}

/// Logs what the watchdog found of a shard the first look it finds it: `said` holds what was last logged, cleared once
/// the shard is alive. A starved shard that then blocks is logged again as wedged. `turn` is what the watchdog does this
/// turn, which a stuck shard's line names.
/// A stuck shard's line names the lateness its verdict rests on, from `watch` at `now_ns` ([`stuck_lateness_words`]); a
/// shard starved for memory, its wait in the `waiting` words ([`memory_wait_words`]).
fn report(
  said: &mut Option<Said>,
  (shard, verdict, turn): (u16, Verdict, Turn),
  (watch, now_ns): (&ShardWatch, u64),
  waiting: impl FnOnce() -> String,
) {
  if verdict == Verdict::Alive {
    *said = None;
    return;
  }
  let kind = said_of(verdict);
  if *said != Some(kind) {
    *said = Some(kind);
    log_verdict(
      shard,
      (verdict, turn),
      || stuck_lateness_words(watch, now_ns),
      waiting,
    );
  }
}

/// A shard starved for memory in its log line's words: the kernel wait its thread is in, where the platform names one
/// (Linux's `wchan`), and the host's reading at the look.
fn memory_wait_words(holder: SlotHolder, watch: &ShardWatch) -> String {
  let wait = slates_rt::registry::shard_kernel_wait_name(holder).map_or_else(
    || "off the CPU".to_owned(),
    |name| format!("in the kernel ({name})"),
  );
  format!(
    "its thread waits {wait}, and the host's reading is {:?}",
    watch.memory
  )
}

/// What a turn does, in a stuck shard's log line.
fn consequence(turn: Turn) -> &'static str {
  match turn {
    Turn::Beat => "the watchdog goes on beating",
    Turn::Withhold(_) => "the watchdog withholds the beat",
    Turn::End(_) => "the watchdog ends the daemon's process",
  }
}

/// Logs one episode's verdict, with what the watchdog saw; a stuck one with the lateness `late` words, one starved for
/// memory with its wait's `waiting` words.
fn log_verdict(
  shard: u16,
  (verdict, turn): (Verdict, Turn),
  late: impl FnOnce() -> String,
  waiting: impl FnOnce() -> String,
) {
  match verdict {
    Verdict::Starved {
      for_ns,
      of: Want::Cpu,
    } => slates_machine::log_line!(
      "slates daemon: shard {shard} is starved: no step and no CPU progress for {} ms, past the {} ms window, while its \
       thread waits for a CPU; the watchdog goes on beating",
      for_ns / NS_PER_MS,
      LIVENESS_BUDGET_NS / NS_PER_MS
    ),
    Verdict::Starved {
      for_ns,
      of: Want::Memory,
    } => slates_machine::log_line!(
      "slates daemon: shard {shard} is starved for memory: no step and no CPU progress for {} ms, past the {} ms \
       window, while the host reports a memory stall; {}; the watchdog goes on beating, and judges it wedged a window \
       after the stall clears",
      for_ns / NS_PER_MS,
      LIVENESS_BUDGET_NS / NS_PER_MS,
      waiting()
    ),
    Verdict::Wedged { for_ns } => slates_machine::log_line!(
      "slates daemon: shard {shard} is wedged: no step, no CPU progress and no wait for a CPU for {} ms, past the {} ms \
       window; {}; {}",
      for_ns / NS_PER_MS,
      LIVENESS_BUDGET_NS / NS_PER_MS,
      consequence(turn),
      late()
    ),
    Verdict::Spinning { cpu_ns, window_ns } => slates_machine::log_line!(
      "slates daemon: shard {shard} is spinning: {} ms of CPU without a step, past the {} ms window; {}; {}",
      cpu_ns / NS_PER_MS,
      window_ns / NS_PER_MS,
      consequence(turn),
      late()
    ),
    Verdict::Alive => {}
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: the window the rule is tested at (a second, the liveness budget).
  const WINDOW: u64 = 1_000_000_000;
  /// Shape: the watchdog's look cadence in the tests (a tenth of the window).
  const TURN: u64 = WINDOW / 10;

  fn look(steps: u64, parked: bool, cpu: Option<(u64, u64)>, now_ns: u64) -> Look {
    Look {
      steps,
      parked,
      cpu,
      run: None,
      memory: None,
      now_ns,
    }
  }

  /// What the scheduler says of a thread on a CPU or waiting for one.
  fn runnable(waited_ns: Option<u64>) -> Option<RunReading> {
    Some(RunReading {
      state: RunState::Runnable,
      waited_ns,
      uninterruptible: Some(false),
    })
  }

  /// What the scheduler says of a thread off the CPU and not ready, asleep (Linux `S`).
  fn blocked(waited_ns: Option<u64>) -> Option<RunReading> {
    Some(RunReading {
      state: RunState::Blocked,
      waited_ns,
      uninterruptible: Some(false),
    })
  }

  /// What the scheduler says of a thread off the CPU in a wait whose kind is `uninterruptible`: held in the kernel
  /// (Linux `D`), asleep (`S`), or not told (`None`, macOS and Windows).
  fn waiting_in(uninterruptible: Option<bool>) -> Option<RunReading> {
    Some(RunReading {
      state: RunState::Blocked,
      waited_ns: None,
      uninterruptible,
    })
  }

  /// Replays `readings`, one a turn from the second turn on, over a shard that took its last step at the first turn and
  /// whose CPU clock never moves again; the verdicts in order, the first of them the second turn's.
  fn replay(readings: &[Option<RunReading>]) -> Vec<Verdict> {
    let (_, mut watch) =
      ShardWatch::default().judge(look(5, false, Some((10, 10)), TURN), WINDOW, WINDOW);
    let mut verdicts = Vec::new();
    for (turn, run) in (2_u64..).zip(readings) {
      let (verdict, next) = watch.judge(
        Look {
          run: *run,
          ..look(5, false, Some((10, 10)), turn * TURN)
        },
        WINDOW,
        WINDOW,
      );
      watch = next;
      verdicts.push(verdict);
    }
    verdicts
  }

  /// The turn of a replay's first wedged verdict.
  fn first_wedged(verdicts: &[Verdict]) -> Option<u64> {
    (2_u64..)
      .zip(verdicts)
      .find(|(_, verdict)| matches!(verdict, Verdict::Wedged { .. }))
      .map(|(turn, _)| turn)
  }

  /// Runs `turns` looks from `start`, the shard's CPU advancing by `cpu_per_turn` each look and its steps by
  /// `steps_per_turn`; the verdicts in order.
  fn drive(turns: u64, steps_per_turn: u64, cpu_per_turn: u64, parked: bool) -> Vec<Verdict> {
    let mut watch = ShardWatch::default();
    let mut verdicts = Vec::new();
    for turn in 1..=turns {
      let cpu = turn * cpu_per_turn;
      let (verdict, next) = watch.judge(
        look(turn * steps_per_turn, parked, Some((cpu, cpu)), turn * TURN),
        WINDOW,
        WINDOW,
      );
      watch = next;
      verdicts.push(verdict);
    }
    verdicts
  }

  /// A-318, §4.14. Do: watch a shard on a whole CPU inside one poll, judged with a spin window three budgets long (a build
  /// measured three times as slow as machine code, as an unoptimized or ThreadSanitizer's build is), turn by turn for
  /// four budgets. Expect: alive while its CPU is under the spin window, though past one budget (a poll this build runs
  /// slower, no spin); spinning once it reaches the spin window, the verdict naming it. Before A-318 the budget alone was
  /// the window, and the poll was spinning at one budget of CPU, as ThreadSanitizer's 1,054 ms poll was on CI.
  #[test]
  fn a_poll_is_judged_spinning_only_past_the_window_held_to_its_build() {
    let spin_window = 3 * WINDOW;
    let mut watch = ShardWatch::default();
    let mut first_spinning = None;
    for turn in 1..=40_u64 {
      let cpu = turn * TURN;
      let (verdict, next) = watch.judge(
        look(1, false, Some((cpu, cpu)), turn * TURN),
        WINDOW,
        spin_window,
      );
      watch = next;
      if let Verdict::Spinning { cpu_ns, window_ns } = verdict {
        first_spinning.get_or_insert((turn, cpu_ns, window_ns));
      } else {
        assert_eq!(
          verdict,
          Verdict::Alive,
          "turn {turn}: on the CPU, so alive until the spin window"
        );
      }
    }
    let (turn, cpu_ns, window_ns) =
      first_spinning.expect("the poll was judged spinning past the spin window");
    assert!(
      cpu_ns >= spin_window && cpu_ns < spin_window + TURN,
      "turn {turn}: {cpu_ns} ns of CPU"
    );
    assert!(
      turn * TURN > WINDOW,
      "not spinning at one budget of CPU (turn {turn})"
    );
    assert_eq!(window_ns, spin_window, "the verdict names the spin window");
  }

  /// A-318. Do: derive the spin window from a measured build cost twice the machine's, one equal to it, one cheaper,
  /// and a refused measure. Expect: two budgets, the budget, the budget, the budget.
  #[test]
  fn the_spin_window_is_the_budget_held_to_the_builds_measured_cost() {
    use slates_machine::build_cost::{BuildCost, BuildCostRefused};
    let cost = |compiled_ns, machine_ns| {
      Ok(BuildCost {
        compiled_ns,
        machine_ns,
        batch: 1,
      })
    };
    assert_eq!(spin_window_ns(cost(200, 100)), 2 * LIVENESS_BUDGET_NS);
    assert_eq!(spin_window_ns(cost(100, 100)), LIVENESS_BUDGET_NS);
    assert_eq!(spin_window_ns(cost(50, 100)), LIVENESS_BUDGET_NS);
    assert_eq!(
      spin_window_ns(Err(BuildCostRefused::NoMachineCode)),
      LIVENESS_BUDGET_NS
    );
  }

  /// A-228, §4.14. Do: watch a shard that steps every turn, one parked every turn, and one starved (no step for
  /// several windows, but a little CPU each turn, far under a window's worth). Expect: alive throughout, each of them.
  #[test]
  fn a_stepping_parked_or_starved_shard_is_alive() {
    assert!(
      drive(50, 1, 1_000, false)
        .iter()
        .all(|v| *v == Verdict::Alive)
    );
    assert!(drive(50, 0, 0, true).iter().all(|v| *v == Verdict::Alive));
    // Starved: 20 ms of CPU a second (2 % of a core) and no step completes for five windows.
    assert!(
      drive(50, 0, 2_000_000, false)
        .iter()
        .all(|v| *v == Verdict::Alive)
    );
  }

  /// A-228, §4.14. Do: watch a shard that stops stepping and runs no CPU (a deadlock, or a host call that never
  /// returns), and one blocked for less than a window before it steps again (a slow call that finishes). Expect: the
  /// first wedged once a whole window has passed with no CPU progress, not before; the second never.
  #[test]
  fn a_shard_with_no_cpu_for_a_window_is_wedged_and_a_shorter_block_is_not() {
    let mut watch = ShardWatch::default();
    let (_, next) = watch.judge(look(5, false, Some((10, 10)), TURN), WINDOW, WINDOW);
    watch = next;
    let mut first_wedged = None;
    for turn in 2..=30 {
      let (verdict, next) =
        watch.judge(look(5, false, Some((10, 10)), turn * TURN), WINDOW, WINDOW);
      watch = next;
      if matches!(verdict, Verdict::Wedged { .. }) && first_wedged.is_none() {
        first_wedged = Some(turn);
      }
    }
    assert_eq!(
      first_wedged,
      Some(11),
      "wedged a whole window after its last progress, not before"
    );
    // Blocked for 0.9 of a window, then a step.
    let mut watch = ShardWatch::default();
    let (_, next) = watch.judge(look(1, false, Some((10, 10)), TURN), WINDOW, WINDOW);
    watch = next;
    for turn in 2..=10 {
      let (verdict, next) =
        watch.judge(look(1, false, Some((10, 10)), turn * TURN), WINDOW, WINDOW);
      watch = next;
      assert_eq!(
        verdict,
        Verdict::Alive,
        "a block shorter than the window at turn {turn}"
      );
    }
    let (verdict, _) = watch.judge(look(2, false, Some((10, 10)), 11 * TURN), WINDOW, WINDOW);
    assert_eq!(verdict, Verdict::Alive);
  }

  /// A-308, §4.14. Do: watch a shard that stops stepping and runs no CPU, blocked at every look, with a gap of a window
  /// and a half in the watchdog's own looks (its thread kept off its CPU, as a process frozen whole or paging is). Expect:
  /// no verdict at the look after the gap, since nothing watched the shard across it, and wedged a window of looks
  /// later, so a stuck shard is still judged. Before, the look after the gap judged it wedged for the gap's length.
  #[test]
  fn a_gap_in_the_watchdogs_own_looks_is_not_counted_as_a_wedge() {
    let (_, mut watch) =
      ShardWatch::default().judge(look(5, false, Some((10, 10)), TURN), WINDOW, WINDOW);
    let still = |now_ns: u64| Look {
      run: blocked(None),
      ..look(5, false, Some((10, 10)), now_ns)
    };
    let (verdict, next) = watch.judge(still(2 * TURN), WINDOW, WINDOW);
    watch = next;
    assert_eq!(verdict, Verdict::Alive);
    let after_gap = 2 * TURN + WINDOW + WINDOW / 2;
    let (verdict, next) = watch.judge(still(after_gap), WINDOW, WINDOW);
    watch = next;
    assert_eq!(
      verdict,
      Verdict::Alive,
      "the look after the watchdog's own gap judges nothing of the gap"
    );
    let mut first_wedged = None;
    for turn in 1..=20 {
      let (verdict, next) = watch.judge(still(after_gap + turn * TURN), WINDOW, WINDOW);
      watch = next;
      if matches!(verdict, Verdict::Wedged { .. }) && first_wedged.is_none() {
        first_wedged = Some(turn);
      }
    }
    assert_eq!(
      first_wedged,
      Some(10),
      "wedged a whole watched window after the gap, not before"
    );
  }

  /// A-228, §4.14. Do: watch a shard whose thread runs on the CPU without completing a step (a task that never
  /// yields). Expect: spinning once it has run a window of CPU since its last step, and alive before.
  #[test]
  fn a_shard_running_a_window_of_cpu_without_a_step_is_spinning() {
    let verdicts = drive(20, 0, TURN, false);
    let first = verdicts
      .iter()
      .position(|v| matches!(v, Verdict::Spinning { .. }))
      .map(|index| index + 1);
    assert_eq!(
      first,
      Some(11),
      "spinning at a window of CPU since its first look: {verdicts:?}"
    );
    assert!(verdicts.iter().take(10).all(|v| *v == Verdict::Alive));
  }

  /// A-228. Do: ask whether a look leaves the shard under suspicion: a look before any history, after a step, parked,
  /// with its CPU clock risen, and with none of those. Expect: only the last, or the same with no clock to read, since
  /// the scheduler is asked about a shard only when the cheap signs say nothing.
  #[test]
  fn only_a_shard_with_no_step_and_no_cpu_rise_is_suspected() {
    assert!(
      !ShardWatch::default().suspects(&look(5, false, Some((10, 10)), TURN)),
      "no history yet"
    );
    let (_, watch) =
      ShardWatch::default().judge(look(5, false, Some((10, 10)), TURN), WINDOW, WINDOW);
    let at = 2 * TURN;
    assert!(
      !watch.suspects(&look(6, false, Some((10, 10)), at)),
      "stepped"
    );
    assert!(
      !watch.suspects(&look(5, true, Some((10, 10)), at)),
      "parked"
    );
    assert!(
      !watch.suspects(&look(5, false, Some((20, 20)), at)),
      "CPU rose"
    );
    assert!(watch.suspects(&look(5, false, Some((10, 10)), at)));
    assert!(
      watch.suspects(&look(5, false, None, at)),
      "no clock to read"
    );
  }

  /// Shape: the run-queue wait of fixtures' shard 0 as it stood frozen (`rungs9/ab-clean-2.samples`, nanoseconds).
  const FROZEN_WAIT: u64 = 17_746_660_969;
  /// Shape: the same counter once the kernel credited the wait, 100 ms before that shard's wedged line.
  const CREDITED_WAIT: u64 = 18_491_698_042;

  /// A-228, §4.14, fixtures' shard 0 on adacachy at CPUWeight=1. Do: replay a shard with no step and no CPU, runnable at
  /// every look, its run-queue wait frozen for 900 ms and then credited all at once (Linux adds a wait only when the
  /// task reaches a CPU or leaves the queue). Expect: never wedged, however long it goes on.
  #[test]
  fn a_runnable_shard_whose_wait_is_credited_late_is_not_wedged() {
    let mut readings = vec![runnable(Some(FROZEN_WAIT)); 9];
    readings.extend(vec![runnable(Some(CREDITED_WAIT)); 41]);
    let verdicts = replay(&readings);
    assert_eq!(first_wedged(&verdicts), None, "{verdicts:?}");
    assert!(verdicts.iter().all(|verdict| verdict.beats()));
    assert_eq!(
      verdicts.last(),
      Some(&Verdict::Starved {
        for_ns: 50 * TURN,
        of: Want::Cpu
      })
    );
  }

  /// A-228, §4.14, measured on two Linux kernels: a thread starved for 5.7-7.9 s showed run-queue growth of 0 and state R
  /// on every sample. Do: replay a shard runnable at every look whose wait never moves. Expect: never wedged.
  #[test]
  fn a_runnable_shard_whose_wait_stands_still_is_not_wedged() {
    let verdicts = replay(&vec![runnable(Some(FROZEN_WAIT)); 50]);
    assert_eq!(first_wedged(&verdicts), None, "{verdicts:?}");
  }

  /// A-228, A-336, §4.14. Do: find a shard starved for a CPU, then for memory, then wedged, then alive. Expect: only a
  /// stuck shard stops the beat, and each kind is logged once an episode: a starved shard that then waits on memory, or
  /// blocks, is logged again.
  #[test]
  fn each_kind_of_verdict_is_logged_once_an_episode_and_only_a_stuck_shard_stops_the_beat() {
    let starved = Verdict::Starved {
      for_ns: WINDOW,
      of: Want::Cpu,
    };
    let for_memory = Verdict::Starved {
      for_ns: WINDOW,
      of: Want::Memory,
    };
    let wedged = Verdict::Wedged { for_ns: WINDOW };
    let spinning = Verdict::Spinning {
      cpu_ns: WINDOW,
      window_ns: WINDOW,
    };
    assert!(Verdict::Alive.beats() && starved.beats() && for_memory.beats());
    assert!(!wedged.beats() && !spinning.beats());
    let mut said = None;
    let watch = (&ShardWatch::default(), WINDOW);
    let words = String::new;
    report(&mut said, (1, starved, Turn::Beat), watch, words);
    assert_eq!(said, Some(said_of(starved)));
    report(
      &mut said,
      (
        1,
        Verdict::Starved {
          for_ns: 2 * WINDOW,
          of: Want::Cpu,
        },
        Turn::Beat,
      ),
      watch,
      words,
    );
    assert_eq!(said, Some(said_of(starved)), "the same episode");
    report(&mut said, (1, for_memory, Turn::Beat), watch, words);
    assert_eq!(said, Some(said_of(for_memory)), "now waiting on memory");
    assert_ne!(said_of(for_memory), said_of(starved));
    report(
      &mut said,
      (
        1,
        wedged,
        Turn::Withhold(Cause::Stuck {
          shard: 1,
          verdict: wedged,
        }),
      ),
      watch,
      words,
    );
    assert_eq!(said, Some(said_of(wedged)), "now stuck");
    report(&mut said, (1, Verdict::Alive, Turn::Beat), watch, words);
    assert_eq!(said, None);
  }

  /// A-228, §4.14. Do: replay a shard runnable at every look that then blocks for good. Expect: starved while it waits,
  /// and wedged a whole window after the last look it was runnable, however long it starved first.
  #[test]
  fn a_starved_shard_that_then_blocks_is_wedged_a_window_later() {
    let mut readings = vec![runnable(Some(FROZEN_WAIT)); 20];
    readings.extend(vec![blocked(Some(FROZEN_WAIT)); 20]);
    let verdicts = replay(&readings);
    assert_eq!(first_wedged(&verdicts), Some(31), "{verdicts:?}");
    assert!(
      verdicts
        .iter()
        .any(|verdict| matches!(verdict, Verdict::Starved { .. }))
    );
  }

  /// A-228, §4.14: macOS and Windows keep no per-thread wait, so their reading is the state alone. Do: replay a shard
  /// runnable at every look with no wait to read. Expect: never wedged.
  #[test]
  fn a_runnable_shard_with_no_wait_to_read_is_not_wedged() {
    let verdicts = replay(&vec![runnable(None); 50]);
    assert_eq!(first_wedged(&verdicts), None, "{verdicts:?}");
  }

  /// A-228, §4.14, fixtures' shard 1 (S S S R R S S): asleep, woken, kept waiting for a CPU, asleep again, with its wait
  /// credited once it ran. Do: replay that shape, then let it stay asleep. Expect: not wedged across the wake, and wedged
  /// a whole window after the last sign of it, which here is the wait credited at the eighth reading (turn 9).
  #[test]
  fn a_shard_woken_and_kept_waiting_is_wedged_a_window_after_it_last_moved() {
    let mut readings = vec![blocked(Some(FROZEN_WAIT)); 4];
    readings.extend(vec![runnable(Some(FROZEN_WAIT)); 3]);
    readings.extend(vec![blocked(Some(CREDITED_WAIT)); 20]);
    let verdicts = replay(&readings);
    assert_eq!(first_wedged(&verdicts), Some(19), "{verdicts:?}");
  }

  /// A-232, §4.14. Do: decide the turn for every verdict a shard can have (alive, starved, wedged, spinning) and for a
  /// control loop that ended, for an anchored daemon and a lone one, running and stopping. Expect: alive and starved
  /// beat for both; a stuck shard or an ended control loop withholds the beat of an anchored daemon and ends a lone one;
  /// while stopping nothing ends, the stop being the daemon's end already; and a stuck shard is named over an ended loop.
  #[test]
  fn each_verdict_maps_to_beat_withhold_or_end_by_who_reads_the_beat() {
    for supervision in [Supervision::Anchored, Supervision::Alone] {
      for stopping in [false, true] {
        a_beating_shard_beats(supervision, stopping);
        a_stuck_shard_or_an_ended_loop_does_not(supervision, stopping);
      }
    }
  }

  /// The rule's turn for `cause` in a daemon of `supervision`: only a lone daemon that is not stopping ends.
  fn expected(supervision: Supervision, stopping: bool, cause: Cause) -> Turn {
    if supervision == Supervision::Alone && !stopping {
      Turn::End(cause)
    } else {
      Turn::Withhold(cause)
    }
  }

  /// Alive and starved shards are not handed in as stuck, and the turn beats.
  fn a_beating_shard_beats(supervision: Supervision, stopping: bool) {
    for verdict in [
      Verdict::Alive,
      Verdict::Starved {
        for_ns: WINDOW,
        of: Want::Cpu,
      },
      Verdict::Starved {
        for_ns: WINDOW,
        of: Want::Memory,
      },
    ] {
      assert!(verdict.beats(), "{verdict:?}");
      assert_eq!(
        decide(supervision, None, false, stopping),
        Turn::Beat,
        "{supervision:?} {verdict:?} stopping={stopping}"
      );
    }
  }

  /// A wedged or spinning shard, named over an ended control loop, and an ended control loop alone.
  fn a_stuck_shard_or_an_ended_loop_does_not(supervision: Supervision, stopping: bool) {
    for verdict in [
      Verdict::Wedged { for_ns: WINDOW },
      Verdict::Spinning {
        cpu_ns: WINDOW,
        window_ns: WINDOW,
      },
    ] {
      let want = expected(supervision, stopping, Cause::Stuck { shard: 3, verdict });
      for control_ended in [false, true] {
        assert_eq!(
          decide(supervision, Some((3, verdict)), control_ended, stopping),
          want,
          "{supervision:?} {verdict:?} stopping={stopping} control_ended={control_ended}"
        );
      }
    }
    assert_eq!(
      decide(supervision, None, true, stopping),
      expected(supervision, stopping, Cause::ControlEnded),
      "{supervision:?} the control loop ended, stopping={stopping}"
    );
  }

  /// A-232. Do: replay a shard that steps, then wedges, through the judge and the decision a lone daemon makes each
  /// turn, as the watchdog's loop does. Expect: it beats while the shard steps and never ends it while it is starved
  /// (runnable at every look); once it is wedged, the first turn past a whole window ends the process, at turn 13 here:
  /// the look after the wedge (turn 3) sees its last CPU rise, and the window runs from that look, so a lone daemon
  /// ends up to one window and two looks after its shard last ran.
  #[test]
  fn a_lone_daemon_ends_a_window_after_its_shard_last_moved_and_never_while_it_starves() {
    let first_end = |readings: &[Option<RunReading>], cpu_after_wedge: u64| {
      let mut watch = ShardWatch::default();
      for turn in 1..=2_u64 {
        let (verdict, next) = watch.judge(
          look(turn, false, Some((turn, turn)), turn * TURN),
          WINDOW,
          WINDOW,
        );
        watch = next;
        assert_eq!(
          decide(Supervision::Alone, None, false, false),
          Turn::Beat,
          "{verdict:?}"
        );
      }
      // Wedged at turn 2's step: one last CPU rise, seen at turn 3, then nothing.
      for (turn, run) in (3_u64..).zip(readings) {
        let (verdict, next) = watch.judge(
          Look {
            run: *run,
            ..look(
              2,
              false,
              Some((cpu_after_wedge, cpu_after_wedge)),
              turn * TURN,
            )
          },
          WINDOW,
          WINDOW,
        );
        watch = next;
        let stuck = (!verdict.beats()).then_some((1, verdict));
        if let Turn::End(_) = decide(Supervision::Alone, stuck, false, false) {
          return Some(turn);
        }
      }
      None
    };
    assert_eq!(first_end(&vec![blocked(None); 30], 3), Some(13), "wedged");
    assert_eq!(first_end(&vec![runnable(None); 50], 3), None, "starved");
  }

  /// A-232. Do: replay a shard that last ran between turns 2 and 3 (its CPU rise seen at turn 3) and then never moved,
  /// to its wedged verdict, and read the look the verdict's lateness is measured from. Expect: turn 2's look, the last
  /// before the shard last moved; with looks on time the verdict comes at turn 13, a window and a beat after it, so its
  /// lateness is zero; a verdict one look later is a look late.
  #[test]
  fn a_wedge_verdicts_lateness_is_measured_from_the_look_before_the_last_movement() {
    let mut watch = ShardWatch::default();
    for turn in 1..=2_u64 {
      watch = watch
        .judge(
          look(turn, false, Some((turn, turn)), turn * TURN),
          WINDOW,
          WINDOW,
        )
        .1;
    }
    let mut wedged_at = None;
    for turn in 3..=20_u64 {
      let (verdict, next) = watch.judge(
        Look {
          run: blocked(None),
          ..look(2, false, Some((3, 3)), turn * TURN)
        },
        WINDOW,
        WINDOW,
      );
      watch = next;
      if matches!(verdict, Verdict::Wedged { .. }) {
        wedged_at = Some(turn);
        break;
      }
    }
    assert_eq!(wedged_at, Some(13));
    assert_eq!(
      watch.still_after_ns(),
      2 * TURN,
      "the look before the last movement"
    );
    assert_eq!(
      verdict_lateness_ns(13 * TURN, watch.still_after_ns()),
      Some(0)
    );
    assert_eq!(
      verdict_lateness_ns(14 * TURN, watch.still_after_ns()),
      Some(TURN)
    );
    assert_eq!(
      verdict_lateness_ns(14 * TURN, 0),
      None,
      "no look before the first"
    );
  }

  /// Replays looks whose lateness `late_of(n)` gives (look `n` a beat and that lateness after look `n - 1`, the watchdog
  /// begun at 0), over a shard that steps at every look and runs on its CPU until it wedges at `wedge_ns`. Returns the
  /// verdict's time, the last beating look's (an anchored daemon's last beat), and the watch at the verdict.
  fn wedge_replay(wedge_ns: u64, late_of: &dyn Fn(u64) -> u64) -> (u64, u64, ShardWatch) {
    let mut watch = ShardWatch::begun_at(1);
    let (mut now, mut steps, mut last_beat) = (0, 0, 0);
    for n in 1_u64..1_000 {
      now += TURN + late_of(n);
      // The shard runs until it blocks at the wedge: it steps at each look before it, and its CPU clock runs until it.
      if now <= wedge_ns {
        steps += 1;
      }
      let cpu = now.min(wedge_ns);
      let (verdict, next) = watch.judge(look(steps, false, Some((cpu, cpu)), now), WINDOW, WINDOW);
      watch = next;
      if matches!(verdict, Verdict::Wedged { .. }) {
        return (now, last_beat, watch);
      }
      last_beat = now;
    }
    panic!("the shard was never found wedged");
  }

  /// A-232 step 5, §4.14. Do: wedge a shard at every hundredth of a beat, before the watchdog's first look and long after
  /// it, under looks on time, a late first look after the wedge, every look a little late, and every look half a beat
  /// late. Expect: the verdict within `W + B` and its own lateness (step 2's rule), and an anchored daemon's last beat
  /// within `W + B` and the first look's lateness alone, the term the anchor's bound needs: the anchor ages its lapse
  /// from that beat.
  #[test]
  fn the_verdict_and_an_anchored_daemons_last_beat_come_within_their_bounds_of_a_wedge() {
    type Pattern = Box<dyn Fn(u64) -> Box<dyn Fn(u64) -> u64>>;
    let first_after = |wedge: u64| {
      move |n: u64| {
        if n == wedge / TURN + 1 {
          TURN * 3 / 4
        } else {
          0
        }
      }
    };
    let patterns: [(&str, Pattern); 4] = [
      ("on time", Box::new(|_| Box::new(|_| 0))),
      (
        "a late first look",
        Box::new(move |wedge| Box::new(first_after(wedge))),
      ),
      (
        "each a little late",
        Box::new(|_| Box::new(|n| (n % 7) * TURN / 50)),
      ),
      (
        "each half a beat late",
        Box::new(|_| Box::new(|_| TURN / 2)),
      ),
    ];
    for (name, pattern, start) in patterns
      .iter()
      .flat_map(|(name, pattern)| [(name, pattern, 0), (name, pattern, 20 * TURN)])
    {
      for offset in (1..TURN).step_by(usize::try_from(TURN / 100).unwrap()) {
        let wedge = start + offset;
        let (verdict_ns, last_beat_ns, watch) = wedge_replay(wedge, &*pattern(wedge));
        let verdict_late = verdict_lateness_ns(verdict_ns, watch.still_after_ns()).unwrap();
        assert!(
          verdict_ns - wedge <= WINDOW + TURN + verdict_late,
          "{name}, wedged at {wedge}: the verdict past W + B + its lateness"
        );
        let first = first_look_lateness_ns(&watch).unwrap();
        assert!(
          last_beat_ns.saturating_sub(wedge) <= WINDOW + TURN + first,
          "{name}, wedged at {wedge}: the last beat came {} ns after, past W + B + first = {}",
          last_beat_ns.saturating_sub(wedge),
          WINDOW + TURN + first
        );
      }
    }
  }

  /// A-232. Do: begin a watch when the watchdog starts, and let its first look find the shard already held (a step
  /// count it has not seen, then no movement). Expect: the look its lateness is measured from is the watchdog's start,
  /// not nothing: a lone daemon whose shard wedged within a beat of its watchdog's start still names its lateness.
  #[test]
  fn a_shard_found_still_at_the_first_look_is_measured_from_the_watchdogs_start() {
    let began = TURN / 2;
    let mut watch = ShardWatch::begun_at(began);
    for turn in 1..=12_u64 {
      watch = watch
        .judge(
          Look {
            run: blocked(None),
            ..look(24, false, Some((7, 7)), turn * TURN)
          },
          WINDOW,
          WINDOW,
        )
        .1;
    }
    assert_eq!(watch.still_after_ns(), began);
    assert_eq!(
      verdict_lateness_ns(11 * TURN, watch.still_after_ns()),
      Some(0),
      "on time: within a window and a beat of the start"
    );
    assert_eq!(
      verdict_lateness_ns(12 * TURN, watch.still_after_ns()),
      Some(12 * TURN - began - WINDOW - TURN),
      "a look later"
    );
  }

  /// A-232. Do: record looks a beat apart, one of them late, then a beat, then late looks with no beat. Expect: each
  /// look's lateness is its gap past a beat; the episode's is the largest since the last beating look, that look's own
  /// included, and starts afresh at the next beat.
  #[test]
  fn the_lateness_an_end_names_is_the_largest_since_the_last_beat() {
    let mut lateness = Lateness::default();
    assert_eq!(
      lateness.look(TURN),
      0,
      "the first look has no gap to be late by"
    );
    assert_eq!(lateness.look(2 * TURN), 0);
    let late = TURN / 4;
    assert_eq!(lateness.look(3 * TURN + late), late);
    assert_eq!(
      lateness.since_beat(true, late),
      late,
      "the beating look's own lateness"
    );
    assert_eq!(lateness.look(4 * TURN + late), 0);
    assert_eq!(
      lateness.since_beat(false, 0),
      late,
      "kept until the next beat"
    );
    assert_eq!(lateness.look(5 * TURN + 3 * late), 2 * late);
    assert_eq!(lateness.since_beat(false, 2 * late), 2 * late);
    assert_eq!(
      lateness.since_beat(true, 0),
      0,
      "a beat starts the episode afresh"
    );
  }

  /// A-228, §4.14. Do: replay a shard with no step and no CPU that the scheduler never saw runnable: asleep with a wait
  /// that never moves (Linux), asleep with no wait to read (macOS, Windows), stopped (which reads as blocked, so a frozen
  /// process is still killed), and with no reading at all (Miri). Expect: each wedged a whole window after its last
  /// step, as before, at turn 11.
  #[test]
  fn a_shard_the_scheduler_never_saw_runnable_is_wedged_within_the_window() {
    let cases = [
      ("asleep, with its wait", blocked(Some(FROZEN_WAIT))),
      ("asleep, no wait to read", blocked(None)),
      ("stopped", blocked(Some(FROZEN_WAIT))),
      ("no reading", None),
    ];
    for (name, reading) in cases {
      let verdicts = replay(&vec![reading; 20]);
      assert_eq!(first_wedged(&verdicts), Some(11), "{name}: {verdicts:?}");
    }
  }

  /// A pressed level, as macOS and Windows read the host.
  fn pressed(pressed: bool) -> Option<StallReading> {
    Some(StallReading::Level { pressed })
  }

  /// The host's memory reading at each turn of a replay.
  type MemoryAt<'a> = &'a dyn Fn(u64) -> Option<StallReading>;

  /// Replays a shard that took its last step at the first turn and whose CPU clock never moves again, its thread off
  /// the CPU as `run` says at every look, while the host's reading at turn `n` is `memory(n)`; the verdicts in order, the
  /// first of them the second turn's.
  fn replay_with_memory(run: Option<RunReading>, memory: MemoryAt<'_>, turns: u64) -> Vec<Verdict> {
    let (_, mut watch) = ShardWatch::default().judge(
      Look {
        memory: memory(1),
        ..look(5, false, Some((10, 10)), TURN)
      },
      WINDOW,
      WINDOW,
    );
    (2..=turns)
      .map(|turn| {
        let (verdict, next) = watch.judge(
          Look {
            run,
            memory: memory(turn),
            ..look(5, false, Some((10, 10)), turn * TURN)
          },
          WINDOW,
          WINDOW,
        );
        watch = next;
        verdict
      })
      .collect()
  }

  /// Shape: the turn of the last look at which the host reports a memory stall in the A-336 replays.
  const LAST_STALLED_TURN: u64 = 30;

  /// A-336, §4.14, the starved extreme for memory. Do: replay a shard that stops stepping and runs no CPU, off the CPU
  /// in a wait memory shows as (held in the kernel, Linux `D`; or a wait the platform does not tell, macOS and Windows),
  /// while the host reports a memory stall at every look up to turn 30 (a pressed level; a PSI total rising), then
  /// reports none. Expect: never wedged while the host stalls, three windows of it, and starved for memory once a
  /// window has passed with no CPU; wedged a window after the last stalled look, at turn 40, so a stuck shard is still
  /// found once the stall clears. Before, it was wedged at turn 11, a window after its last step, as a lone daemon at a
  /// few hundred megabytes free was ended (g231, 2026-11-17).
  #[test]
  fn a_shard_waiting_on_memory_while_the_host_stalls_is_starved_and_wedged_a_window_after_the_stall_clears()
   {
    let level = |turn: u64| pressed(turn <= LAST_STALLED_TURN);
    let total = |turn: u64| {
      Some(StallReading::Cumulative {
        stalled_us: 1_000 * turn.min(LAST_STALLED_TURN),
      })
    };
    let readings: [(&str, MemoryAt<'_>); 2] =
      [("a pressed level", &level), ("a rising total", &total)];
    for (source, memory) in readings {
      for (wait, run) in [
        ("held in the kernel", waiting_in(Some(true))),
        ("a wait the platform does not tell", waiting_in(None)),
      ] {
        let verdicts = replay_with_memory(run, memory, 60);
        assert_eq!(
          first_wedged(&verdicts),
          Some(LAST_STALLED_TURN + 10),
          "{source}, {wait}: {verdicts:?}"
        );
        assert!(
          verdicts
            .iter()
            .take_while(|verdict| !matches!(verdict, Verdict::Wedged { .. }))
            .all(|verdict| verdict.beats()),
          "{source}, {wait}: every verdict before the wedge beats"
        );
        assert!(
          verdicts.contains(&Verdict::Starved {
            for_ns: (LAST_STALLED_TURN - 1) * TURN,
            of: Want::Memory
          }),
          "{source}, {wait}: starved for memory at the last stalled look: {verdicts:?}"
        );
      }
    }
  }

  /// A-336, §4.14, the stuck extreme where the platform tells the wait. Do: replay a still shard asleep in a futex, a
  /// poll or a sleep (Linux `S`), and one with no reading of its thread at all, while the host reports a memory stall at
  /// every look. Expect: each wedged a window after its last step, at turn 11, as before: a stall on the host does not
  /// excuse a thread whose own state says it is not waiting on memory, nor one nothing was read of.
  #[test]
  fn a_shard_asleep_or_unread_is_wedged_within_the_window_however_the_host_stalls() {
    let stalled = |_| pressed(true);
    for (wait, run) in [("asleep", blocked(None)), ("no reading", None)] {
      let verdicts = replay_with_memory(run, &stalled, 30);
      assert_eq!(first_wedged(&verdicts), Some(11), "{wait}: {verdicts:?}");
    }
  }

  /// A-336, the rule's table. Do: ask whether a still look finds a shard held for memory, for each thread reading (held
  /// in the kernel, a wait not told, asleep, runnable, none) and each host stall (a pressed level, a clear one, a risen
  /// total, an unchanged total, a total with none before it, no reading). Expect: held only where the host stalled and
  /// the thread is blocked in a wait memory can show as.
  #[test]
  fn a_look_finds_a_shard_held_for_memory_only_off_the_cpu_while_the_host_stalls() {
    let total = |stalled_us| Some(StallReading::Cumulative { stalled_us });
    let stalls = [
      ("pressed", None, pressed(true), true),
      ("clear", pressed(true), pressed(false), false),
      ("risen", total(5), total(6), true),
      ("unchanged", total(5), total(5), false),
      ("first total", None, total(5), false),
      ("no reading", pressed(true), None, false),
    ];
    let threads = [
      ("held in the kernel", waiting_in(Some(true)), true),
      ("not told", waiting_in(None), true),
      ("asleep", waiting_in(Some(false)), false),
      ("runnable", runnable(None), false),
      ("unread", None, false),
    ];
    for (stall, before, now, stalled) in stalls {
      for (thread, run, may_wait_on_memory) in threads {
        assert_eq!(
          held_for_memory(before, now, run),
          stalled && may_wait_on_memory,
          "{stall}, {thread}"
        );
      }
    }
  }
}

/// DIAG (diag-g251-spin, never main): what every shard was doing at a stuck verdict, and the stuck shard's stack.
mod diag_spin {
  use std::sync::atomic::{AtomicU64, Ordering};

  use slates_rt::SlotHolder;

  /// Backtraces the handler has written.
  static WRITTEN: AtomicU64 = AtomicU64::new(0);
  /// Whether the dump has run once.
  static DUMPED: AtomicU64 = AtomicU64::new(0);

  #[cfg(unix)]
  fn write_stderr(text: &str) {
    // SAFETY: fd 2 and a live buffer of its length.
    let _ = unsafe { libc::write(2, text.as_ptr().cast(), text.len()) };
  }

  #[cfg(not(unix))]
  fn write_stderr(text: &str) {
    use std::io::Write;
    let _ = std::io::stderr().write_all(text.as_bytes());
  }

  #[cfg(target_os = "linux")]
  extern "C" fn handler(_signal: libc::c_int) {
    let trace = std::backtrace::Backtrace::force_capture();
    let text =
      format!("DIAG backtrace of the stuck shard's thread:\n{trace}\nDIAG backtrace end\n");
    write_stderr(&text);
    WRITTEN.fetch_add(1, Ordering::SeqCst);
  }

  #[cfg(target_os = "linux")]
  fn install_and_signal(tid: u64) {
    // SAFETY: a zeroed sigaction with a handler of the right signature; SIGUSR2 is unused by the daemon.
    unsafe {
      let mut action: libc::sigaction = std::mem::zeroed();
      action.sa_sigaction = handler as extern "C" fn(libc::c_int) as usize;
      action.sa_flags = libc::SA_RESTART;
      libc::sigemptyset(&mut action.sa_mask);
      libc::sigaction(libc::SIGUSR2, &action, std::ptr::null_mut());
    }
    for sample in 0..3u64 {
      let before = WRITTEN.load(Ordering::SeqCst);
      // SAFETY: tgkill to a thread of this process.
      let _ = unsafe {
        libc::syscall(
          libc::SYS_tgkill,
          libc::getpid(),
          libc::pid_t::try_from(tid).unwrap_or(0),
          libc::SIGUSR2,
        )
      };
      let began = std::time::Instant::now();
      while WRITTEN.load(Ordering::SeqCst) == before
        && began.elapsed() < std::time::Duration::from_millis(1500)
      {
        std::thread::sleep(std::time::Duration::from_millis(5));
      }
      write_stderr(&format!(
        "DIAG sample {sample}: written {}\n",
        WRITTEN.load(Ordering::SeqCst)
      ));
      std::thread::sleep(std::time::Duration::from_millis(150));
    }
  }

  #[cfg(not(target_os = "linux"))]
  fn install_and_signal(_tid: u64) {}

  fn proc_line(tid: u64, name: &str) -> String {
    std::fs::read_to_string(format!("/proc/self/task/{tid}/{name}"))
      .unwrap_or_default()
      .trim()
      .to_owned()
  }

  pub(super) fn dump(holders: &[SlotHolder], stuck: usize, now_ns: u64) {
    if DUMPED.swap(1, Ordering::SeqCst) != 0 {
      return;
    }
    let mut stuck_tid = None;
    for (index, holder) in holders.iter().enumerate() {
      let words = slates_rt::registry::diag_pulse_words(*holder);
      let site = slates_rt::registry::diag_site_name(*holder);
      let tid = slates_rt::registry::diag_shard_tid(*holder);
      let cpu = slates_rt::registry::shard_cpu(*holder);
      let (stat, wchan, status) = tid.map_or_else(Default::default, |tid| {
        (
          proc_line(tid, "stat"),
          proc_line(tid, "wchan"),
          proc_line(tid, "status"),
        )
      });
      let switches: Vec<&str> = status
        .lines()
        .filter(|line| line.contains("ctxt_switches"))
        .collect();
      write_stderr(&format!(
        "DIAG shard {} (index {index}{}): pulse [steps, waits, spawns, completed, longest_ns, polling slot+1, poll_began_ns, phase, full_turns, overrun_ns] = {words:?}; task site {site:?}; cpu {cpu:?}; watchdog now {now_ns}; tid {tid:?}; stat {stat:?}; wchan {wchan:?}; {switches:?}\n",
        holder.shard(),
        if index == stuck { ", STUCK" } else { "" },
      ));
      if index == stuck {
        stuck_tid = tid;
      }
    }
    if let Some(tid) = stuck_tid {
      install_and_signal(tid);
      for (index, holder) in holders.iter().enumerate() {
        let words = slates_rt::registry::diag_pulse_words(*holder);
        write_stderr(&format!(
          "DIAG after samples, shard index {index}: pulse {words:?}; task site {:?}; cpu {:?}\n",
          slates_rt::registry::diag_site_name(*holder),
          slates_rt::registry::shard_cpu(*holder)
        ));
      }
    }
  }
}
