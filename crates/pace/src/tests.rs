//! A-783's tests: a step over a fake clock, each unit advancing it by its kind's cost.

#![allow(
  clippy::unwrap_used,
  clippy::arithmetic_side_effects,
  clippy::indexing_slicing
)]

use std::cell::Cell;

use super::{Costs, Env, Step, Stop, UnitCost};

thread_local! {
  /// The fake clock, nanoseconds.
  static NOW: Cell<u64> = const { Cell::new(0) };
  /// Whether anyone waits.
  static WAITING: Cell<bool> = const { Cell::new(false) };
  /// The probes asked.
  static ASKED: Cell<u64> = const { Cell::new(0) };
}

fn now() -> u64 {
  NOW.with(Cell::get)
}

fn waiting() -> bool {
  ASKED.with(|asked| asked.set(asked.get() + 1));
  WAITING.with(Cell::get)
}

fn advance(ns: u64) {
  NOW.with(|now| now.set(now.get() + ns));
}

/// The kinds of the mixed workload: a cheap unit and a dear one, nanoseconds.
const COSTS: [u64; 2] = [3, 40];
/// The quantum of the tests, nanoseconds.
const QUANTUM: u64 = 100;

fn env(ceiling_ns: u64) -> Env {
  Env {
    clock: now,
    waiting,
    quantum_ns: QUANTUM,
    ceiling_ns,
    target_ppm: 100_000,
  }
}

/// The mixed workload's `at`th unit's kind: two cheap units, then a dear one.
fn kind_of(at: u64) -> usize {
  usize::from(at % 3 == 2)
}

/// `units` of the mixed workload run in steps of `env`: the steps, and each step's length on the clock.
fn run_paced(units: u64, env: Env) -> Vec<u64> {
  let mut costs = Costs::<2>::ZERO;
  let mut lengths = Vec::new();
  let mut done = 0;
  while done < units {
    let began = now();
    let mut step = Step::begin(env, began);
    while done < units && step.more(kind_of(done), &mut costs) {
      advance(COSTS[kind_of(done)]);
      done += 1;
    }
    step.close(now(), &mut costs);
    lengths.push(now() - began);
  }
  lengths
}

/// Today's fixed slicing: units until the clock passes the quantum, the clock read after each.
fn run_fixed(units: u64) -> Vec<u64> {
  let mut lengths = Vec::new();
  let mut done = 0;
  while done < units {
    let began = now();
    while done < units {
      advance(COSTS[kind_of(done)]);
      done += 1;
      if now() - began >= QUANTUM {
        break;
      }
    }
    lengths.push(now() - began);
  }
  lengths
}

/// A-783 (main's red: under a mixed workload no step runs past the quantum while someone waits). Do: run 3,000 units of
/// a mixed workload (a 3 ns unit twice, then a 40 ns one) in steps of a 100 ns quantum while someone always waits, and
/// the same today's way. Expect: today some steps run past the quantum (by up to a dear unit); paced, none does once the
/// estimates are learned (the first steps, before any dear unit is measured, may), and a probe is asked once a step.
#[test]
fn while_someone_waits_no_step_runs_past_its_quantum() {
  NOW.with(|now| now.set(0));
  WAITING.with(|w| w.set(true));
  ASKED.with(|asked| asked.set(0));
  let fixed = run_fixed(3_000);
  assert!(fixed.iter().any(|length| *length > QUANTUM), "{fixed:?}");
  let paced = run_paced(3_000, env(u64::MAX));
  let learned = &paced[2..];
  assert!(
    learned.iter().all(|length| *length <= QUANTUM),
    "{:?}",
    learned
      .iter()
      .filter(|length| **length > QUANTUM)
      .collect::<Vec<_>>()
  );
  assert!(ASKED.with(Cell::get) <= paced.len() as u64);
}

/// A-783 (main's red: the step's share of the quantum used goes up; fewer steps). Do: run the same 3,000 units while no
/// one waits, paced with a ceiling of 50 quanta, and today's way. Expect: paced, the steps number the work over the
/// ceiling (one more at most), under a tenth of today's, no step passes its ceiling, and the probes asked one a quantum, not one a unit: a quantum asked before a unit
/// that would cross it covers at least the quantum less the dearest unit, so at most `work / (quantum - dearest)` and
/// one a step.
#[test]
fn while_no_one_waits_a_step_takes_more_quanta_to_its_ceiling() {
  NOW.with(|now| now.set(0));
  WAITING.with(|w| w.set(false));
  ASKED.with(|asked| asked.set(0));
  let fixed = run_fixed(3_000);
  let ceiling = 50 * QUANTUM;
  let paced = run_paced(3_000, env(ceiling));
  let work: u64 = (0..3_000).map(|at| COSTS[kind_of(at)]).sum();
  assert!(
    paced.len() as u64 <= work.div_ceil(ceiling) + 1 && paced.len() * 10 < fixed.len(),
    "{} paced steps against {} fixed for {work} ns of work",
    paced.len(),
    fixed.len()
  );
  assert!(paced.iter().all(|length| *length <= ceiling), "{paced:?}");
  let probes = ASKED.with(Cell::get);
  assert!(
    probes <= work.div_ceil(QUANTUM - COSTS[1]) + paced.len() as u64 && probes < 3_000,
    "{probes} probes for {work} ns of work"
  );
}

/// A-783. Do: a step whose next unit would cross the ceiling. Expect: stopped `Ceiling`, the probe not asked.
#[test]
fn a_step_stops_at_its_ceiling_without_asking() {
  NOW.with(|now| now.set(0));
  WAITING.with(|w| w.set(false));
  ASKED.with(|asked| asked.set(0));
  let mut costs = Costs::<1>::ZERO;
  for _ in 0..4 {
    costs.units[0].add(60, 100_000);
  }
  let mut step = Step::begin(env(100), 0);
  assert!(step.more(0, &mut costs));
  advance(60);
  assert!(!step.more(0, &mut costs));
  assert_eq!(step.report().stop, Some(Stop::Ceiling));
  assert_eq!(ASKED.with(Cell::get), 0);
}

/// A-783 (R3: the weight is the spread's and the count's). Do: feed one kind a steady 50 ns, another a spread of 10 and
/// 90 ns. Expect: the steady one's window one and its estimate 50; the spread one's window grows with its spread (the
/// samples its target error asks for), its estimate near its mean of 50; the inputs say so.
#[test]
fn the_estimates_window_follows_its_spread_and_count() {
  let mut steady = UnitCost::default();
  let mut spread = UnitCost::default();
  for at in 0..200u64 {
    steady.add(50, 100_000);
    spread.add(if at % 2 == 0 { 10 } else { 90 }, 100_000);
  }
  let calm = steady.inputs(100_000);
  assert_eq!((calm.window, calm.estimate_ns, calm.spread_ns), (1, 50, 0));
  let wide = spread.inputs(100_000);
  assert_eq!((wide.mean_ns, wide.spread_ns), (50, 40));
  // CV = 0.8, e = 0.1: (0.8 / 0.1)^2 = 64 samples.
  assert_eq!(wide.window, 64);
  assert!(wide.estimate_ns.abs_diff(50) <= 5, "{wide:?}");
}

/// A-783 (what makes a ceiling-long step safe, main's 84a question of 2026-11-24: an import's paced checkpoint step ran
/// 477 ms with no one waiting; a request arriving mid-step must not wait it out). Do: a step of the mixed workload with
/// a ceiling of 50 quanta, no one waiting until 10 quanta in, someone waiting from then on. Expect: the step stops at
/// the first quantum's end after the arrival, so within a quantum and one unit of it, never at its ceiling.
#[test]
fn a_waiter_arriving_mid_step_ends_it_within_a_quantum() {
  const ARRIVES: u64 = 10 * QUANTUM;
  NOW.with(|now| now.set(0));
  WAITING.with(|w| w.set(false));
  let mut costs = Costs::<2>::ZERO;
  let mut done = 0;
  let mut step = Step::begin(env(50 * QUANTUM), now());
  while step.more(kind_of(done), &mut costs) {
    advance(COSTS[kind_of(done)]);
    done += 1;
    if now() >= ARRIVES {
      WAITING.with(|w| w.set(true));
    }
  }
  let ended = now();
  step.close(ended, &mut costs);
  assert!(
    ended >= ARRIVES,
    "the step stopped at {ended} ns, before anyone waited"
  );
  assert!(
    ended <= ARRIVES + QUANTUM + COSTS[1],
    "the step stopped {} ns after someone arrived, past a quantum and a unit",
    ended - ARRIVES
  );
}
