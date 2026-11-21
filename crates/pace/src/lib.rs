//! Paced work: a step of cooperative units that yields at the quantum when someone waits (§4.3, amended by A-783; main's
//! ruling of 2026-11-20 15:35: "yield at the quantum when someone waits").
//!
//! **Why.** A shard's step quantum is its online wake estimate, a few microseconds, so a step of units that each cost
//! about a quantum yields after nearly every unit: w-git measured a 9,000-object git answer in 8,498 steps (release)
//! and 29,177 steps (debug), and under load every yield costs a scheduler turn, so the answer's wall time was the step
//! count times the host's turn (about 10 s at load 60 for milliseconds of CPU). Yielding helps only when someone waits.
//!
//! **What a step does.** [`Step::more`] grants units while the current quantum has room for the next unit's estimated
//! cost. At the quantum's end (or when the next unit would cross it) it asks its owner's probe once whether anyone
//! waits on the shard (a task ready, a message, a socket, a due timer): if someone does it stops there, so no step runs
//! past its quantum by more than the estimate's error while others wait; if no one does it takes another quantum and
//! goes on, up to its owner's ceiling (the watchdog's look, derived), never a constant. One probe a quantum, never one a
//! unit (D-7, D-8: nothing shared is read per unit). The first unit of a step always runs, so every step moves.
//!
//! **What a unit costs** ([`UnitCost`]): each unit kind's own measures, the time from one grant to the next. The
//! estimate is the running mean until the samples are enough for the mean's relative standard error to reach the
//! target (`n* = ceil(CV^2 / e^2)`, CV the measured spread over the mean, `e` the owner's target relative error, its
//! quantum's own measured jitter over the quantum), then an exponentially weighted mean over that many samples
//! (weight `2 / (n* + 1)`): the spread and the count decide the weight, no constant does (R3). [`UnitCost::inputs`]
//! gives what decided it, for the owner's log.
//!
//! **No runtime.** The clock and the probe are functions the owner gives (the daemon's: `slates_rt::futures::now_ns`
//! and `slates_rt::futures::others_waiting`), so the engine, git, Mercurial and Subversion pace their work with it
//! while none of them binds to the runtime.

#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]
#![forbid(unsafe_code)]

/// What a step's owner gives it: its clock, its probe of whether anyone waits, the quantum, the ceiling and the target
/// relative error of a unit's estimate.
#[derive(Clone, Copy, Debug)]
pub struct Env {
  /// The owner's clock, nanoseconds.
  pub clock: fn() -> u64,
  /// Whether anyone else waits on the owner's shard now (asked once a quantum).
  pub waiting: fn() -> bool,
  /// The quantum, nanoseconds: how long a step runs between two asks.
  pub quantum_ns: u64,
  /// The most a step runs while no one waits, nanoseconds (the owner's derived ceiling).
  pub ceiling_ns: u64,
  /// The target relative error of a unit's estimate, in parts per million (module docs).
  pub target_ppm: u64,
}

/// The parts of a million: the target error's unit.
/// Format: parts per million.
const MILLION: u128 = 1_000_000;

/// One unit kind's measured cost (module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UnitCost {
  /// The samples taken.
  count: u64,
  /// Their sum, nanoseconds.
  sum: u128,
  /// The sum of their squares.
  sum_sq: u128,
  /// The estimate, nanoseconds.
  estimate: u64,
}

/// What decided a unit kind's estimate, for its owner's log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Inputs {
  /// The samples taken.
  pub count: u64,
  /// Their mean, nanoseconds.
  pub mean_ns: u64,
  /// Their standard deviation, nanoseconds.
  pub spread_ns: u64,
  /// The samples the target error needs (`n*`): the estimate's window.
  pub window: u64,
  /// The estimate, nanoseconds.
  pub estimate_ns: u64,
}

impl UnitCost {
  /// None measured: what every kind starts at.
  pub const ZERO: UnitCost = UnitCost {
    count: 0,
    sum: 0,
    sum_sq: 0,
    estimate: 0,
  };

  /// The current estimate of one unit's cost, nanoseconds: none measured yet is zero (the first unit always runs).
  pub fn estimate_ns(&self) -> u64 {
    self.estimate
  }

  /// The mean and the variance of the samples so far.
  fn moments(&self) -> (u128, u128) {
    let count = u128::from(self.count.max(1));
    let mean = self.sum.checked_div(count).unwrap_or(0);
    let variance = self
      .sum_sq
      .checked_div(count)
      .unwrap_or(0)
      .saturating_sub(mean.saturating_mul(mean));
    (mean, variance)
  }

  /// The window the target error asks for: `ceil(CV^2 / e^2)`, at least one.
  fn window(&self, target_ppm: u64) -> u64 {
    let (mean, variance) = self.moments();
    let target = u128::from(target_ppm.max(1));
    let below = mean
      .saturating_mul(mean)
      .saturating_mul(target)
      .saturating_mul(target)
      .max(1);
    let above = variance.saturating_mul(MILLION).saturating_mul(MILLION);
    u64::try_from(above.div_ceil(below))
      .unwrap_or(u64::MAX)
      .max(1)
  }

  /// Takes a sample of `ns`, the estimate moved by the window the target error asks for (module docs).
  pub fn add(&mut self, ns: u64, target_ppm: u64) {
    self.count = self.count.saturating_add(1);
    let sample = u128::from(ns);
    self.sum = self.sum.saturating_add(sample);
    self.sum_sq = self.sum_sq.saturating_add(sample.saturating_mul(sample));
    let window = self.window(target_ppm);
    if self.count <= window {
      let (mean, _) = self.moments();
      self.estimate = u64::try_from(mean).unwrap_or(u64::MAX);
      return;
    }
    // weight 2 / (window + 1): the estimate moves that share of the way to the sample.
    let span = u128::from(window).saturating_add(1);
    let estimate = u128::from(self.estimate);
    self.estimate = if sample >= estimate {
      let up = sample
        .saturating_sub(estimate)
        .saturating_mul(2)
        .checked_div(span)
        .unwrap_or(0);
      u64::try_from(estimate.saturating_add(up)).unwrap_or(u64::MAX)
    } else {
      let down = estimate
        .saturating_sub(sample)
        .saturating_mul(2)
        .checked_div(span)
        .unwrap_or(0);
      u64::try_from(estimate.saturating_sub(down)).unwrap_or(0)
    };
  }

  /// What decides the estimate now, under `target_ppm` (module docs).
  pub fn inputs(&self, target_ppm: u64) -> Inputs {
    let (mean, variance) = self.moments();
    Inputs {
      count: self.count,
      mean_ns: u64::try_from(mean).unwrap_or(u64::MAX),
      spread_ns: u64::try_from(variance.isqrt()).unwrap_or(u64::MAX),
      window: self.window(target_ppm),
      estimate_ns: self.estimate,
    }
  }
}

/// A paced path's measured costs (module docs): each unit kind's, and its probe's own (an ask whether anyone waits),
/// kept across its steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Costs<const KINDS: usize> {
  /// Each unit kind's cost.
  pub units: [UnitCost; KINDS],
  /// The probe's cost: a step asks no sooner than an ask costs, so asking takes at most half a step.
  pub probe: UnitCost,
}

impl<const KINDS: usize> Costs<KINDS> {
  /// None measured.
  pub const ZERO: Costs<KINDS> = Costs {
    units: [UnitCost::ZERO; KINDS],
    probe: UnitCost::ZERO,
  };
}

impl<const KINDS: usize> Default for Costs<KINDS> {
  fn default() -> Self {
    Self::ZERO
  }
}

/// Why a step stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
  /// Someone waited at a quantum's end: the step yields to them.
  Waiting,
  /// The step reached its ceiling with no one waiting.
  Ceiling,
}

/// What a step did, for its owner's counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
  /// The units it ran.
  pub units: u64,
  /// The quanta it took: one, and one more each time no one waited.
  pub quanta: u64,
  /// The probes it asked.
  pub asks: u64,
  /// How it stopped, if it was refused a unit (none when its work ended first).
  pub stop: Option<Stop>,
  /// When it began and when it last asked for a unit, on its clock.
  pub span: (u64, u64),
  /// How long it was allowed to run, nanoseconds: to the end of the last quantum it took (or its ceiling, where it
  /// stopped there). A step that ran past this by more than a unit and its owner's own work was mispaced.
  pub allowed_ns: u64,
}

/// One step of paced work (module docs): copied in and out of its owner's state, so a step across a lend owns nothing.
#[derive(Clone, Copy, Debug)]
pub struct Step {
  /// What its owner gave it.
  env: Env,
  /// The end of its current quantum, on the clock.
  boundary: u64,
  /// The unit under way and when it was granted.
  current: Option<(usize, u64)>,
  /// What it did.
  report: Report,
}

impl Step {
  /// A step beginning at `began` on `env`'s clock: its first quantum.
  pub fn begin(env: Env, began: u64) -> Step {
    Step {
      env,
      boundary: began.saturating_add(env.quantum_ns),
      current: None,
      report: Report {
        quanta: 1,
        span: (began, began),
        allowed_ns: env.quantum_ns,
        ..Report::default()
      },
    }
  }

  /// Whether a unit of `kind` runs now (module docs): the unit before it measured into `costs`, by its kind.
  pub fn more<const KINDS: usize>(&mut self, kind: usize, costs: &mut Costs<KINDS>) -> bool {
    let now = (self.env.clock)();
    self.more_at(now, kind, costs)
  }

  /// [`Step::more`] at `now`, read by its caller (one clock read a unit, whoever reads it).
  pub fn more_at<const KINDS: usize>(
    &mut self,
    now: u64,
    kind: usize,
    costs: &mut Costs<KINDS>,
  ) -> bool {
    self.measure(now, costs);
    let first = self.report.units == 0;
    let estimate = costs.units.get(kind).map_or(0, UnitCost::estimate_ns);
    let ends = now.saturating_add(estimate);
    if !first && ends > self.boundary && !self.another_quantum((now, ends), costs) {
      return false;
    }
    self.report.units = self.report.units.saturating_add(1);
    self.report.span.1 = now;
    self.current = Some((kind, now));
    true
  }

  /// At the quantum's end, or before a unit that would cross it: another quantum when no one waits and the ceiling
  /// has room for the unit, else the stop. The ask is timed into the probe's cost, and the quantum taken is never
  /// shorter than an ask costs, so asking takes at most half a step whatever the quantum.
  fn another_quantum<const KINDS: usize>(
    &mut self,
    (now, ends): (u64, u64),
    costs: &mut Costs<KINDS>,
  ) -> bool {
    let (began, _) = self.report.span;
    if ends.saturating_sub(began) > self.env.ceiling_ns {
      self.report.stop = Some(Stop::Ceiling);
      self.report.allowed_ns = self.env.ceiling_ns;
      return false;
    }
    self.report.asks = self.report.asks.saturating_add(1);
    let waiting = (self.env.waiting)();
    let asked = (self.env.clock)();
    costs
      .probe
      .add(asked.saturating_sub(now), self.env.target_ppm);
    if waiting {
      self.report.stop = Some(Stop::Waiting);
      return false;
    }
    let quantum = self.env.quantum_ns.max(costs.probe.estimate_ns());
    self.boundary = asked.saturating_add(quantum);
    self.report.quanta = self.report.quanta.saturating_add(1);
    self.report.allowed_ns = self.boundary.saturating_sub(began);
    true
  }

  /// The unit under way measured to `now` into `costs`, by its kind.
  fn measure<const KINDS: usize>(&mut self, now: u64, costs: &mut Costs<KINDS>) {
    if let Some((kind, granted)) = self.current.take()
      && let Some(cost) = costs.units.get_mut(kind)
    {
      cost.add(now.saturating_sub(granted), self.env.target_ppm);
    }
  }

  /// Ends the step at `now`: the unit under way measured into `costs`; what it did.
  pub fn close<const KINDS: usize>(&mut self, now: u64, costs: &mut Costs<KINDS>) -> Report {
    self.measure(now, costs);
    self.report
  }

  /// What it did so far.
  pub fn report(&self) -> Report {
    self.report
  }
}

#[cfg(test)]
mod tests;
