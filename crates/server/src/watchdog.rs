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
//! and no CPU rise since the last look. Where it has no answer (Miri, or a source that cannot be read), the CPU clock
//! stands alone, as it did before.
//!
//! The window is the existing liveness budget ([`crate::daemon::LIVENESS_BUDGET_NS`]), a fixed number recorded as R3
//! debt in GAPS. A-228 adds no number to it. While every shard is alive and the control loop lives, the thread beats; once
//! a shard is stuck it logs what it saw and withholds the beat, and the anchor, which judges the beat's age by its own
//! measured lateness, kills the daemon. When the control loop ends (a refused boot, A-100), the thread withholds the
//! beat too, so the anchor restarts the daemon as before. A stop tells the thread first ([`Watchdog::stopping`]), since
//! the shutdown ends the control loop before it stops the thread: a control loop that ends then is the stop's, and is
//! not announced as one to restart (under two spinners a core, 8 of 18 daemons stopped in one `nfs_hostile` run said
//! so before, an 18-core Mac, 2026-11-07). Under Miri, which has no thread clock, the wall window stands
//! alone, as it does for observations.

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
  /// `control` is the control loop's life: while its sender lives the loop lives.
  pub fn start(
    segment: AnchorSegment,
    holders: Vec<SlotHolder>,
    control: Receiver<()>,
  ) -> std::io::Result<Watchdog> {
    let (stop, stop_signal) = channel();
    slates_machine::log_line!(
      "slates daemon: the watchdog beats every {} ms and finds a shard stuck after a {} ms window without a step, with \
       no CPU progress and no wait for a CPU, or with a window's CPU run (the window is the liveness budget); a shard \
       waiting for a CPU is starved, not stuck, and the beat goes on",
      HEARTBEAT_NS / NS_PER_MS,
      LIVENESS_BUDGET_NS / NS_PER_MS
    );
    let handle = std::thread::Builder::new()
      .name("slates-watchdog".to_owned())
      .spawn(move || run(&segment, &holders, &control, &stop_signal))?;
    Ok(Watchdog {
      stop: Some(stop),
      handle: Some(handle),
    })
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

/// The thread's loop: each beat, look at every shard, and beat the anchor's word while every shard is alive and the
/// control loop lives.
fn run(
  segment: &AnchorSegment,
  holders: &[SlotHolder],
  control: &Receiver<()>,
  stop: &Receiver<()>,
) {
  let mut clock = HostClock::new();
  let mut watches = vec![ShardWatch::default(); holders.len()];
  let mut reported = vec![None; holders.len()];
  let mut control_ended = false;
  let mut stopping = false;
  loop {
    match stop.recv_timeout(std::time::Duration::from_nanos(HEARTBEAT_NS)) {
      Err(RecvTimeoutError::Timeout) => {}
      // The daemon is stopping: the control loop's end that follows is the stop's.
      Ok(()) => stopping = true,
      Err(RecvTimeoutError::Disconnected) => return,
    }
    if !control_ended && matches!(control.try_recv(), Err(TryRecvError::Disconnected)) {
      control_ended = true;
      if !stopping {
        slates_machine::log_line!(
          "slates daemon: the control loop ended; the watchdog withholds the beat so the anchor restarts the daemon"
        );
      }
    }
    let now = clock.monotonic_ns();
    let mut all_alive = true;
    let mut looks: Vec<Option<Look>> = holders.iter().map(|holder| look_at(*holder, now)).collect();
    read_runs(holders, &watches, &mut looks);
    for (index, (holder, look)) in holders.iter().zip(looks).enumerate() {
      let Some(look) = look else {
        continue;
      };
      let Some(watch) = watches.get_mut(index) else {
        continue;
      };
      let (verdict, next) = watch.judge(look, LIVENESS_BUDGET_NS);
      *watch = next;
      let Some(said) = reported.get_mut(index) else {
        continue;
      };
      report(said, holder.shard(), verdict);
      all_alive &= verdict.beats();
    }
    if all_alive
      && !control_ended
      && let Ok(supervision) = segment.supervision()
    {
      supervision.beat(now);
    }
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
/// the shard is alive. A starved shard that then blocks is logged again as wedged.
fn report(said: &mut Option<std::mem::Discriminant<Verdict>>, shard: u16, verdict: Verdict) {
  if verdict == Verdict::Alive {
    *said = None;
    return;
  }
  let kind = std::mem::discriminant(&verdict);
  if *said != Some(kind) {
    *said = Some(kind);
    log_verdict(shard, verdict);
  }
}

/// Logs one episode's verdict, with what the watchdog saw.
fn log_verdict(shard: u16, verdict: Verdict) {
  match verdict {
    Verdict::Starved { for_ns } => slates_machine::log_line!(
      "slates daemon: shard {shard} is starved: no step and no CPU progress for {} ms, past the {} ms window, while its \
       thread waits for a CPU; the watchdog goes on beating",
      for_ns / NS_PER_MS,
      LIVENESS_BUDGET_NS / NS_PER_MS
    ),
    Verdict::Wedged { for_ns } => slates_machine::log_line!(
      "slates daemon: shard {shard} is wedged: no step, no CPU progress and no wait for a CPU for {} ms, past the {} ms \
       window; the watchdog withholds the beat",
      for_ns / NS_PER_MS,
      LIVENESS_BUDGET_NS / NS_PER_MS
    ),
    Verdict::Spinning { cpu_ns } => slates_machine::log_line!(
      "slates daemon: shard {shard} is spinning: {} ms of CPU without a step, past the {} ms window; the watchdog \
       withholds the beat",
      cpu_ns / NS_PER_MS,
      LIVENESS_BUDGET_NS / NS_PER_MS
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
    report(&mut said, 1, starved);
    assert_eq!(said, Some(std::mem::discriminant(&starved)));
    report(&mut said, 1, Verdict::Starved { for_ns: 2 * WINDOW });
    assert_eq!(
      said,
      Some(std::mem::discriminant(&starved)),
      "the same episode"
    );
    report(&mut said, 1, wedged);
    assert_eq!(said, Some(std::mem::discriminant(&wedged)), "now stuck");
    report(&mut said, 1, Verdict::Alive);
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
