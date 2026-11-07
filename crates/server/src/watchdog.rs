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
//! - otherwise, since its last step, **stuck (wedged)** once a whole window has passed with no CPU progress at all. A
//!   deadlock, and a host call that never returns, consume none; a starved shard, runnable but given little CPU, keeps
//!   consuming some;
//! - **stuck (spinning)** once it has run a whole window of CPU without completing a step: a task that does not yield.
//!   A cooperative task yields within the shard's step budget, microseconds, so a window of CPU is no poll's;
//! - **alive** otherwise: starved, or busy inside one poll for less than a window.
//!
//! The window is the existing liveness budget ([`crate::daemon::LIVENESS_BUDGET_NS`]), a fixed number recorded as R3
//! debt in GAPS. A-228 adds no number to it. While every shard is alive and the control loop lives, the thread beats; once
//! a shard is stuck it logs what it saw and withholds the beat, and the anchor, which judges the beat's age by its own
//! measured lateness, kills the daemon. When the control loop ends (a refused boot, A-100), the thread withholds the
//! beat too, so the anchor restarts the daemon as before. Under Miri, which has no thread clock, the wall window stands
//! alone, as it does for observations.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::thread::JoinHandle;

use slates_anchor::AnchorSegment;
use slates_rt::SlotHolder;
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
      "slates daemon: the watchdog beats every {} ms and finds a shard stuck after a {} ms window without a step and \
       with no CPU progress, or with a window's CPU run (the window is the liveness budget)",
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
  /// Parked, stepping, starved or inside one poll for less than a window.
  Alive,
  /// No step and no CPU progress for `for_ns` (at least a window).
  Wedged {
    /// How long the shard has run no CPU without a step.
    for_ns: u64,
  },
  /// No step while it ran `cpu_ns` of CPU (at least a window).
  Spinning {
    /// The CPU the shard ran since its last step.
    cpu_ns: u64,
  },
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
}

/// One look at a shard: its step count, whether it is parked, its thread clock reading (CPU nanoseconds, progress),
/// and the watchdog's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Look {
  /// The shard's step count.
  pub steps: u64,
  /// Whether it is parked in its driver.
  pub parked: bool,
  /// Its thread's CPU clock, when it has one: `(spent_ns, progress)`.
  pub cpu: Option<(u64, u64)>,
  /// The watchdog's clock, nanoseconds.
  pub now_ns: u64,
}

impl ShardWatch {
  /// Judges one look against the history (A-65's rule; `window_ns` is the liveness budget) and returns the verdict
  /// and the history for the next look. Pure, so the rule is tested on every host with no thread under it.
  pub fn judge(self, look: Look, window_ns: u64) -> (Verdict, ShardWatch) {
    let (spent, progress) = look.cpu.map_or((None, None), |(spent, progress)| {
      (Some(spent), Some(progress))
    });
    if look.parked || look.steps != self.steps || self.stepped_ns == 0 {
      return (
        Verdict::Alive,
        ShardWatch {
          steps: look.steps,
          stepped_ns: look.now_ns.max(1),
          cpu_at_step_ns: spent,
          progress,
          progressed_ns: look.now_ns,
        },
      );
    }
    let rose = match (self.progress, progress) {
      (Some(before), Some(now)) => now > before,
      // No thread clock (Miri): the wall window stands alone, as for observations.
      _ => false,
    };
    let next = ShardWatch {
      progress: if rose { progress } else { self.progress },
      progressed_ns: if rose {
        look.now_ns
      } else {
        self.progressed_ns
      },
      ..self
    };
    let still_for = look.now_ns.saturating_sub(next.progressed_ns);
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
  let mut reported = vec![false; holders.len()];
  let mut control_ended = false;
  loop {
    match stop.recv_timeout(std::time::Duration::from_nanos(HEARTBEAT_NS)) {
      Err(RecvTimeoutError::Timeout) => {}
      Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
    }
    if !control_ended && matches!(control.try_recv(), Err(TryRecvError::Disconnected)) {
      control_ended = true;
      slates_machine::log_line!(
        "slates daemon: the control loop ended; the watchdog withholds the beat so the anchor restarts the daemon"
      );
    }
    let now = clock.monotonic_ns();
    let mut all_alive = true;
    for (index, holder) in holders.iter().enumerate() {
      let Some(look) = look_at(*holder, now) else {
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
      if verdict == Verdict::Alive {
        *said = false;
        continue;
      }
      all_alive = false;
      if !*said {
        *said = true;
        log_stuck(holder.shard(), verdict);
      }
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
    now_ns,
  })
}

/// Logs a stuck shard once per episode, with what the watchdog saw.
fn log_stuck(shard: u16, verdict: Verdict) {
  match verdict {
    Verdict::Wedged { for_ns } => slates_machine::log_line!(
      "slates daemon: shard {shard} is wedged: no step and no CPU progress for {} ms, past the {} ms window; the \
       watchdog withholds the beat",
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
      now_ns,
    }
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
}
