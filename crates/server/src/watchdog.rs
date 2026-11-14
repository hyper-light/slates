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
//! The window is the existing liveness budget ([`crate::daemon::LIVENESS_BUDGET_NS`]), a fixed number recorded as R3
//! debt in GAPS. A-228 adds no number to it. While every shard is alive and the control loop lives, the thread beats; once
//! a shard is stuck it logs what it saw and withholds the beat, and the anchor, which judges the beat's age by its own
//! measured lateness, kills the daemon. Every look is marked in the segment too, beating or not (`SUP_LOOKED`,
//! A-232), so the anchor tells a withheld beat (the looks go on) from a watchdog that was itself kept off its CPU (they
//! stop). When the control loop ends (a refused boot, A-100), the thread withholds the beat too, so the anchor restarts
//! the daemon as before. A stop tells the thread first ([`Watchdog::stopping`]), since
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
    slates_machine::log_line!(
      "slates daemon: the watchdog beats every {} ms and finds a shard stuck after a {} ms window without a step, with \
       no CPU progress and no wait for a CPU, or with a window's CPU run (the window is the liveness budget); a shard \
       waiting for a CPU is starved, not stuck, and the beat goes on; {}",
      HEARTBEAT_NS / NS_PER_MS,
      LIVENESS_BUDGET_NS / NS_PER_MS,
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
          turns,
        )
      })?;
    Ok(Watchdog {
      stop: Some(stop),
      handle: Some(handle),
      turns,
    })
  }

  /// What the thread has done so far ([`Turns`]).
  pub fn turns(&self) -> &'static Turns {
    self.turns
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
  /// An anchor holds the segment and reads the beat: across the spawn (`FromEnv`, the `slates anchor` process, which
  /// kills a daemon whose beat lapses) or in this process (`Handoff`, a test or an embedding that plays the anchor and
  /// owns what a lapse means). A stuck shard withholds the beat, and the anchor ends the daemon.
  Anchored,
  /// Nobody reads the beat (`Create`: `slates daemon` run alone, or a test's daemon with no anchor). A stuck shard ends
  /// the daemon's own process, on the verdict that withholds an anchored daemon's beat.
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
    // MUTANT (b), diagnostics only: the unsupervised self-end removed.
    (Supervision::Alone, _) | (Supervision::Anchored, _) => Turn::Withhold(cause),
  }
}

/// What the watchdog found of one shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
  /// Parked, stepping, given a little CPU at a time, or inside one poll for less than a window.
  Alive,
  /// No step and no CPU progress for `for_ns` (at least a window), but the scheduler had it waiting for a CPU. It beats.
  Starved {
    /// How long the shard has run no CPU without a step.
    for_ns: u64,
  },
  /// No step, no CPU progress and no wait for a CPU for `for_ns` (at least a window).
  Wedged {
    /// How long the shard has not moved at all.
    for_ns: u64,
  },
  /// No step while it ran `cpu_ns` of CPU (at least a window).
  Spinning {
    /// The CPU the shard ran since its last step.
    cpu_ns: u64,
  },
}

impl Verdict {
  /// Whether a shard with this verdict lets the watchdog beat: a starved shard is not a stuck one.
  fn beats(self) -> bool {
    matches!(self, Verdict::Alive | Verdict::Starved { .. })
  }
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
  /// The watchdog's clock, nanoseconds.
  pub now_ns: u64,
}

impl ShardWatch {
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

  /// Judges one look against the history (A-65's rule; `window_ns` is the liveness budget) and returns the verdict
  /// and the history for the next look. Pure, so the rule is tested on every host with no thread under it.
  pub fn judge(self, look: Look, window_ns: u64) -> (Verdict, ShardWatch) {
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
    let next = ShardWatch {
      progress: if rose { progress } else { self.progress },
      progressed_ns: if rose {
        look.now_ns
      } else {
        self.progressed_ns
      },
      moved_ns: if rose || waiting {
        look.now_ns
      } else {
        self.moved_ns
      },
      waited_ns,
      ..self
    };
    let still_for = look.now_ns.saturating_sub(next.moved_ns);
    if still_for >= window_ns {
      return (Verdict::Wedged { for_ns: still_for }, next);
    }
    let cpu_since_step = match (self.cpu_at_step_ns, spent) {
      (Some(at_step), Some(now)) => now.saturating_sub(at_step),
      _ => 0,
    };
    if cpu_since_step >= window_ns {
      return (
        Verdict::Spinning {
          cpu_ns: cpu_since_step,
        },
        next,
      );
    }
    let no_cpu_for = look.now_ns.saturating_sub(next.progressed_ns);
    if no_cpu_for >= window_ns {
      return (Verdict::Starved { for_ns: no_cpu_for }, next);
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
  turns: &Turns,
) {
  let mut clock = HostClock::new();
  let mut watches = vec![ShardWatch::default(); holders.len()];
  let mut reported = vec![None; holders.len()];
  let mut control_ended = false;
  let mut said_control_ended = false;
  let mut stopping = false;
  let mut lateness = Lateness::new();
  // The thread's start is where its first wait begins, so it counts as the look before the first: a shard wedged before
  // the first look is first seen within a beat and that look's lateness of the start too.
  lateness.look(clock.monotonic_ns());
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
    lateness.look(now);
    let mut looks: Vec<Option<Look>> = holders.iter().map(|holder| look_at(*holder, now)).collect();
    read_runs(holders, &watches, &mut looks);
    // In shard order, aligned with `reported`: `None` for a slot that now holds another registration.
    let verdicts: Vec<Option<(u16, Verdict)>> = holders
      .iter()
      .zip(looks)
      .zip(watches.iter_mut())
      .map(|((holder, look), watch)| {
        let (verdict, next) = watch.judge(look?, LIVENESS_BUDGET_NS);
        *watch = next;
        Some((holder.shard(), verdict))
      })
      .collect();
    let stuck = verdicts
      .iter()
      .flatten()
      .copied()
      .find(|(_, verdict)| !verdict.beats());
    let turn = decide(supervision, stuck, control_ended, stopping);
    if let Turn::End(cause) = turn {
      turns.looked(stuck.is_some());
      end_process(cause, late_since_moved(cause, holders, &watches, &lateness));
    }
    for ((found, said), watch) in verdicts.iter().zip(reported.iter_mut()).zip(&watches) {
      if let Some((shard, verdict)) = found {
        report(said, *shard, *verdict, turn, lateness.of(watch.moved_ns));
      }
    }
    turns.looked(stuck.is_some());
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

/// Writes a turn into the segment at `now_ns`: the look, at every turn, beating or not (A-232: a beat that lapses while
/// the looks go on is the watchdog's verdict, and one that lapses with them is this thread kept off its CPU, which the
/// anchor tells apart by this word); and the beat, when the turn beats.
fn mark(segment: &AnchorSegment, turn: Turn, now_ns: u64) {
  if let Ok(supervision) = segment.supervision() {
    supervision.look(now_ns);
    if turn == Turn::Beat {
      supervision.beat(now_ns);
    }
  }
}

/// How late the watchdog's own looks run, past the beat they wait for: the thread is itself scheduled, and a busy
/// machine delays it. A stuck verdict names the lateness of the very looks it rests on, summed, so a test can bound the
/// verdict from the shard's wedge by the rule's own terms (A-232):
///
/// A shard wedged at `t0` is first seen to have moved at the next look, `a1`, which came one beat and its own lateness
/// after the look before it, so `a1 - t0 <= B + late(a1)`. It is found wedged at the first look `ak` a whole window
/// after `a1`; a beat divides the window, so at most `W / B` looks follow `a1` before one lies a window past it, and
/// `ak - a1 <= W + late(a2) + ... + late(ak)`. So the verdict comes within `W + B + late(a1) + ... + late(ak)` of the
/// wedge: the window, a beat, and the lateness this names. Until 2026-11-14 the end named the largest lateness since
/// the last beat, which leaves out `a1` and all but the last two looks, and so bounded nothing. The thread's start
/// counts as a look, the one before its first: a shard wedged before the first look has no other `a0`, and the first
/// look's lateness was counted as none, which put a wedge 2.8 ms after a daemon began serving 187 ms before its first
/// look while the line named 0 us (a loaded Mac, 2026-11-13).
///
/// An anchored daemon's beat is withheld from the verdict on, so its last beat is the look before the verdict, still
/// within a window of `a1`: it comes within `W + B + late(a1)` of the wedge, and the anchor's lapse is aged from it. So
/// a stuck verdict names `late(a1)` on its own too ([`Lateness::first`]), the term an anchor's bound needs.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Lateness {
  /// The recent looks, oldest first: when each ran on the watchdog's clock, and how late it was.
  looks: std::collections::VecDeque<(u64, u64)>,
  /// How many looks are kept ([`Lateness::new`]).
  kept: usize,
}

impl Lateness {
  /// Derived: as many looks as a verdict can rest on, the look that last saw the shard move and each after it until a
  /// window has passed: `ceil(W / B) + 1`, and one more for the look before them (the thread's start, before the first),
  /// so `W / B + 2` with the window and beat this daemon runs.
  fn new() -> Lateness {
    let beats =
      usize::try_from(LIVENESS_BUDGET_NS.div_ceil(HEARTBEAT_NS.max(1))).unwrap_or(usize::MAX);
    let kept = beats.saturating_add(2);
    Lateness {
      looks: std::collections::VecDeque::with_capacity(kept),
      kept,
    }
  }

  /// Records a look at `now_ns`: how late it was is its gap from the last look past one beat (none for the first).
  fn look(&mut self, now_ns: u64) {
    let late_ns = self.looks.back().map_or(0, |(last_ns, _)| {
      now_ns.saturating_sub(*last_ns).saturating_sub(HEARTBEAT_NS)
    });
    if self.looks.len() >= self.kept {
      self.looks.pop_front();
    }
    self.looks.push_back((now_ns, late_ns));
  }

  /// The lateness of the first look from `since_ns` on (a shard's last movement, which is a look's time): `late(a1)`.
  fn first(&self, since_ns: u64) -> u64 {
    self
      .looks
      .iter()
      .find(|(look_ns, _)| *look_ns >= since_ns)
      .map_or(0, |(_, late_ns)| *late_ns)
  }

  /// The lateness of the looks from `since_ns` on (a shard's last movement, which is a look's time), summed.
  fn since(&self, since_ns: u64) -> u64 {
    self
      .looks
      .iter()
      .filter(|(look_ns, _)| *look_ns >= since_ns)
      .fold(0, |sum, (_, late_ns)| sum.saturating_add(*late_ns))
  }
}

/// The lateness of the looks a verdict on a shard last moved at `moved_ns` rests on: all of them summed, and the first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LateLooks {
  sum_ns: u64,
  first_ns: u64,
}

impl Lateness {
  /// [`LateLooks`] since `moved_ns`.
  fn of(&self, moved_ns: u64) -> LateLooks {
    LateLooks {
      sum_ns: self.since(moved_ns),
      first_ns: self.first(moved_ns),
    }
  }
}

/// The lateness an end names: for a stuck shard, that of the looks since it last moved ([`Lateness`]); for a control loop
/// that ended, none (nothing was judged).
fn late_since_moved(
  cause: Cause,
  holders: &[SlotHolder],
  watches: &[ShardWatch],
  lateness: &Lateness,
) -> LateLooks {
  match cause {
    Cause::Stuck { shard, .. } => holders
      .iter()
      .zip(watches)
      .find(|(holder, _)| holder.shard() == shard)
      .map_or(LateLooks::default(), |(_, watch)| {
        lateness.of(watch.moved_ns)
      }),
    Cause::ControlEnded => LateLooks::default(),
  }
}

/// Ends the daemon's process on the watchdog's verdict (A-232): nobody reads this daemon's beat, so nobody else would end
/// it. A deliberate exit with [`FAILED_EXIT_CODE`], not a panic: a stuck shard's thread can be neither joined nor
/// stopped, so an orderly stop would wait on it for good. The exit runs no destructor; what the process held, the kernel
/// releases, and its clients see it as they see a kill.
///
/// The verdict is logged first, but the end never waits on the log for longer than one beat: a shard can be wedged in a
/// write to a standard error nobody drains, holding the lock every log line takes, and a watchdog that waited on that
/// lock would never end anything. So the line is written by a thread of its own, which the exit ends if it is still
/// blocked; the exit is that thread's owner.
fn end_process(cause: Cause, late: LateLooks) -> ! {
  let line = match cause {
    Cause::Stuck { shard, verdict } => format!(
      "slates daemon: no anchor reads the beat, so the watchdog ends the daemon's process (exit {FAILED_EXIT_CODE}): \
       shard {shard} is {}; {}",
      describe(verdict),
      lateness_words(late)
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
  if writer.is_ok() {
    let _ = logged.recv_timeout(std::time::Duration::from_nanos(HEARTBEAT_NS));
  }
  std::process::exit(i32::from(FAILED_EXIT_CODE))
}

/// The lateness a stuck verdict names ([`Lateness`]), in its line's words.
fn lateness_words(late: LateLooks) -> String {
  format!(
    "the looks since it last moved ran {} us late in all, the first of them {} us",
    late.sum_ns / NS_PER_US,
    late.first_ns / NS_PER_US
  )
}

/// A stuck verdict in the end's words.
fn describe(verdict: Verdict) -> String {
  match verdict {
    Verdict::Wedged { for_ns } => format!(
      "wedged, unmoved for {} ms against the {} ms window",
      for_ns / NS_PER_MS,
      LIVENESS_BUDGET_NS / NS_PER_MS
    ),
    Verdict::Spinning { cpu_ns } => format!(
      "spinning, {} ms of CPU without a step against the {} ms window",
      cpu_ns / NS_PER_MS,
      LIVENESS_BUDGET_NS / NS_PER_MS
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
    now_ns,
  })
}

/// Asks the scheduler about the shards under suspicion, all at once: at most one Windows snapshot a beat.
fn read_runs(holders: &[SlotHolder], watches: &[ShardWatch], looks: &mut [Option<Look>]) {
  let suspects: Vec<(&mut Look, SlotHolder)> = looks
    .iter_mut()
    .zip(watches)
    .zip(holders)
    .filter_map(|((look, watch), holder)| {
      let look = look.as_mut()?;
      watch.suspects(look).then_some((look, *holder))
    })
    .collect();
  if suspects.is_empty() {
    return;
  }
  let asked: Vec<SlotHolder> = suspects.iter().map(|(_, holder)| *holder).collect();
  let readings = slates_rt::registry::shard_run_readings(&asked);
  for ((look, _), reading) in suspects.into_iter().zip(readings) {
    look.run = reading;
  }
}

/// Logs what the watchdog found of a shard the first look it finds it: `said` holds the kind last logged, cleared once
/// the shard is alive. A starved shard that then blocks is logged again as wedged. `turn` is what the watchdog does this
/// turn, which a stuck shard's line names, with `late`, the lateness of the looks its verdict rests on.
fn report(
  said: &mut Option<std::mem::Discriminant<Verdict>>,
  shard: u16,
  verdict: Verdict,
  turn: Turn,
  late: LateLooks,
) {
  if verdict == Verdict::Alive {
    *said = None;
    return;
  }
  let kind = std::mem::discriminant(&verdict);
  if *said != Some(kind) {
    *said = Some(kind);
    log_verdict(shard, verdict, turn, late);
  }
}

/// What a turn does, in a stuck shard's log line.
fn consequence(turn: Turn) -> &'static str {
  match turn {
    Turn::Beat => "the watchdog goes on beating",
    Turn::Withhold(_) => "the watchdog withholds the beat",
    Turn::End(_) => "the watchdog ends the daemon's process",
  }
}

/// Logs one episode's verdict, with what the watchdog saw; a stuck one names `late` too ([`Lateness`]).
fn log_verdict(shard: u16, verdict: Verdict, turn: Turn, late: LateLooks) {
  match verdict {
    Verdict::Starved { for_ns } => slates_machine::log_line!(
      "slates daemon: shard {shard} is starved: no step and no CPU progress for {} ms, past the {} ms window, while its \
       thread waits for a CPU; the watchdog goes on beating",
      for_ns / NS_PER_MS,
      LIVENESS_BUDGET_NS / NS_PER_MS
    ),
    Verdict::Wedged { for_ns } => slates_machine::log_line!(
      "slates daemon: shard {shard} is wedged: no step, no CPU progress and no wait for a CPU for {} ms, past the {} ms \
       window; {}; {}",
      for_ns / NS_PER_MS,
      LIVENESS_BUDGET_NS / NS_PER_MS,
      consequence(turn),
      lateness_words(late)
    ),
    Verdict::Spinning { cpu_ns } => slates_machine::log_line!(
      "slates daemon: shard {shard} is spinning: {} ms of CPU without a step, past the {} ms window; {}; {}",
      cpu_ns / NS_PER_MS,
      LIVENESS_BUDGET_NS / NS_PER_MS,
      consequence(turn),
      lateness_words(late)
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
      now_ns,
    }
  }

  /// What the scheduler says of a thread on a CPU or waiting for one.
  fn runnable(waited_ns: Option<u64>) -> Option<RunReading> {
    Some(RunReading {
      state: RunState::Runnable,
      waited_ns,
    })
  }

  /// What the scheduler says of a thread off the CPU and not ready.
  fn blocked(waited_ns: Option<u64>) -> Option<RunReading> {
    Some(RunReading {
      state: RunState::Blocked,
      waited_ns,
    })
  }

  /// Replays `readings`, one a turn from the second turn on, over a shard that took its last step at the first turn and
  /// whose CPU clock never moves again; the verdicts in order, the first of them the second turn's.
  fn replay(readings: &[Option<RunReading>]) -> Vec<Verdict> {
    let (_, mut watch) = ShardWatch::default().judge(look(5, false, Some((10, 10)), TURN), WINDOW);
    let mut verdicts = Vec::new();
    for (turn, run) in (2_u64..).zip(readings) {
      let (verdict, next) = watch.judge(
        Look {
          run: *run,
          ..look(5, false, Some((10, 10)), turn * TURN)
        },
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
      );
      watch = next;
      verdicts.push(verdict);
    }
    verdicts
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
    let (_, next) = watch.judge(look(5, false, Some((10, 10)), TURN), WINDOW);
    watch = next;
    let mut first_wedged = None;
    for turn in 2..=30 {
      let (verdict, next) = watch.judge(look(5, false, Some((10, 10)), turn * TURN), WINDOW);
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
    let (_, next) = watch.judge(look(1, false, Some((10, 10)), TURN), WINDOW);
    watch = next;
    for turn in 2..=10 {
      let (verdict, next) = watch.judge(look(1, false, Some((10, 10)), turn * TURN), WINDOW);
      watch = next;
      assert_eq!(
        verdict,
        Verdict::Alive,
        "a block shorter than the window at turn {turn}"
      );
    }
    let (verdict, _) = watch.judge(look(2, false, Some((10, 10)), 11 * TURN), WINDOW);
    assert_eq!(verdict, Verdict::Alive);
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
    let (_, watch) = ShardWatch::default().judge(look(5, false, Some((10, 10)), TURN), WINDOW);
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
      Some(&Verdict::Starved { for_ns: 50 * TURN })
    );
  }

  /// A-228, §4.14, measured on two Linux kernels: a thread starved for 5.7-7.9 s showed run-queue growth of 0 and state R
  /// on every sample. Do: replay a shard runnable at every look whose wait never moves. Expect: never wedged.
  #[test]
  fn a_runnable_shard_whose_wait_stands_still_is_not_wedged() {
    let verdicts = replay(&vec![runnable(Some(FROZEN_WAIT)); 50]);
    assert_eq!(first_wedged(&verdicts), None, "{verdicts:?}");
  }

  /// A-228, §4.14. Do: find a shard starved, then wedged, then alive, and starved again. Expect: only a stuck shard
  /// stops the beat, and each kind is logged once an episode, a starved shard that then blocks being logged again.
  #[test]
  fn each_kind_of_verdict_is_logged_once_an_episode_and_only_a_stuck_shard_stops_the_beat() {
    let starved = Verdict::Starved { for_ns: WINDOW };
    let wedged = Verdict::Wedged { for_ns: WINDOW };
    let spinning = Verdict::Spinning { cpu_ns: WINDOW };
    assert!(Verdict::Alive.beats() && starved.beats());
    assert!(!wedged.beats() && !spinning.beats());
    let mut said = None;
    report(&mut said, 1, starved, Turn::Beat, LateLooks::default());
    assert_eq!(said, Some(std::mem::discriminant(&starved)));
    report(
      &mut said,
      1,
      Verdict::Starved { for_ns: 2 * WINDOW },
      Turn::Beat,
      LateLooks::default(),
    );
    assert_eq!(
      said,
      Some(std::mem::discriminant(&starved)),
      "the same episode"
    );
    report(
      &mut said,
      1,
      wedged,
      Turn::Withhold(Cause::Stuck {
        shard: 1,
        verdict: wedged,
      }),
      LateLooks::default(),
    );
    assert_eq!(said, Some(std::mem::discriminant(&wedged)), "now stuck");
    report(
      &mut said,
      1,
      Verdict::Alive,
      Turn::Beat,
      LateLooks::default(),
    );
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
    for verdict in [Verdict::Alive, Verdict::Starved { for_ns: WINDOW }] {
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
      Verdict::Spinning { cpu_ns: WINDOW },
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
        let (verdict, next) =
          watch.judge(look(turn, false, Some((turn, turn)), turn * TURN), WINDOW);
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

  /// Replays looks whose lateness `late_of(n)` gives, look `n` coming one beat and that lateness after look `n - 1`, over
  /// a shard that steps at every look and runs on its CPU until it wedges at `wedge_ns` (no step, no CPU, never
  /// runnable after). Returns
  /// the verdict look's time, the lateness the verdict names ([`Lateness::since`] of the shard's last movement), and the
  /// lateness the rule before 2026-11-14 named: the largest of the verdict's look and the beating look before it; and
  /// the last beating look's time (an anchored daemon's last beat) with the lateness of the first look the verdict rests
  /// on ([`Lateness::first`]).
  fn wedged_at(wedge_ns: u64, late_of: &dyn Fn(u64) -> u64) -> (u64, u64, u64, (u64, u64)) {
    let mut lateness = Lateness::new();
    // The thread's start, as `run` records it.
    lateness.look(0);
    let mut watch = ShardWatch::default();
    let mut now = 0;
    let mut steps = 0;
    let mut last_beat = 0;
    for n in 1_u64..1_000 {
      now += TURN + late_of(n);
      lateness.look(now);
      // The shard runs until it blocks at the wedge: it steps at each look before it, and its CPU clock runs until it,
      // so the first look after the wedge sees the CPU it ran since the look before.
      if now <= wedge_ns {
        steps += 1;
      }
      let cpu = now.min(wedge_ns);
      let (verdict, next) = watch.judge(look(steps, false, Some((cpu, cpu)), now), WINDOW);
      watch = next;
      if matches!(verdict, Verdict::Wedged { .. }) {
        let last_two = lateness
          .looks
          .iter()
          .rev()
          .take(2)
          .map(|(_, late)| *late)
          .max()
          .unwrap_or(0);
        return (
          now,
          lateness.since(watch.moved_ns),
          last_two,
          (last_beat, lateness.first(watch.moved_ns)),
        );
      }
      last_beat = now;
    }
    panic!("the shard was never found wedged");
  }

  /// A-232, §4.14. Do: wedge a shard at every point of a beat (a hundredth apart), before the watchdog's first look and
  /// long after it, under looks on time, one late first look after the wedge, every look a little late, and every look
  /// half a beat late. Expect: the verdict within
  /// `W + B + L` of the wedge every time, `L` the lateness it names, the looks since the shard last moved summed, and
  /// the late first look counted in it: the largest lateness since the last beat (the rule before 2026-11-14) misses it,
  /// and the bound with that fails. And the last beat, an anchored daemon's, within `W + B + late(a1)` of the wedge, the
  /// first look's lateness alone: the anchor's lapse is aged from it.
  #[test]
  fn a_wedged_verdict_comes_within_a_window_a_beat_and_the_lateness_it_names() {
    let first_after = |wedge: u64| {
      move |n: u64| {
        if n == wedge / TURN + 1 {
          TURN * 3 / 4
        } else {
          0
        }
      }
    };
    // A pattern: given the wedge's time, each look's lateness by its number.
    type Pattern = Box<dyn Fn(u64) -> Box<dyn Fn(u64) -> u64>>;
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
    let mut missed_by_the_largest = 0;
    // Wedged before the watchdog's first look (its start at 0), and long after it.
    for (name, pattern, start) in patterns
      .iter()
      .flat_map(|(name, pattern)| [(name, pattern, 0), (name, pattern, 20 * TURN)])
    {
      for offset in (0..TURN).step_by(usize::try_from(TURN / 100).unwrap()) {
        let wedge = start + offset;
        let late_of = pattern(wedge);
        let (verdict_ns, late_ns, largest_ns, (last_beat_ns, first_ns)) =
          wedged_at(wedge, &*late_of);
        let beat_after = last_beat_ns.saturating_sub(wedge);
        assert!(
          beat_after <= WINDOW + TURN + first_ns,
          "{name}, wedged at {wedge}: the last beat came {beat_after} ns later, past W + B + late(a1) = {}",
          WINDOW + TURN + first_ns
        );
        let after = verdict_ns - wedge;
        missed_by_the_largest += usize::from(after > WINDOW + TURN + largest_ns);
        assert!(
          after <= WINDOW + TURN + late_ns,
          "{name}, wedged at {wedge}: the verdict came {after} ns later, past W + B + L = {}",
          WINDOW + TURN + late_ns
        );
        assert!(after >= WINDOW, "{name}: never before a whole window");
      }
    }
    assert!(
      missed_by_the_largest > 0,
      "the patterns include one the largest lateness since the last beat does not bound, or they prove nothing"
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
}
