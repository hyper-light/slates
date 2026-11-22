//! The learned rates' tests (A-845, A-724 piece 2): Poisson arrivals read back, the horizon scale-free, a change of
//! regime followed, bytes and a principal read at the repository's horizon.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::cast_precision_loss,
  clippy::cast_possible_truncation,
  clippy::cast_sign_loss,
  clippy::arithmetic_side_effects
)]

use super::{Decayed, Rates};

/// Shape: the clock grain the tests' ladders start at, a microsecond in nanoseconds.
const BASE_NS: u64 = 1_000;

/// Arrival times, in nanoseconds from `start`, of `count` arrivals of a Poisson process at `per_s` a second, from a
/// xorshift stream seeded by `seed`.
fn poisson(start: u64, per_s: f64, count: usize, seed: u64) -> Vec<u64> {
  let mut state = seed | 1;
  let mut now = start as f64;
  (0..count)
    .map(|_| {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      let uniform = ((state >> 11) as f64 + 0.5) / (1u64 << 53) as f64;
      now += -uniform.ln() / per_s * 1e9;
      now as u64
    })
    .collect()
}

/// A-845. Do: feed 3,000 arrivals of a Poisson process at 10 a second, each bringing 4,096 bytes. Expect: the rate
/// read at the last arrival within a fifth of 10 a second, the bytes within a fifth of 40,960 a second, and the
/// horizon between one mean gap and a thousand of them.
#[test]
fn poisson_arrivals_are_read_back_at_their_rate() {
  let mut rates = Rates::new(BASE_NS);
  let times = poisson(1, 10.0, 3_000, 7);
  for at in &times {
    rates.arrival(*at);
    rates.bytes(*at, 4_096);
  }
  let now = *times.last().unwrap();
  let works = rates.works_per_s(now).unwrap();
  let bytes = rates.bytes_per_s(now).unwrap();
  assert!((works - 10.0).abs() < 2.0, "works {works}");
  assert!((bytes - 40_960.0).abs() < 8_192.0, "bytes {bytes}");
  let horizon = rates.horizon_ns().unwrap();
  assert!(
    (1e8..1e11).contains(&horizon),
    "horizon {horizon} ns for a 1e8 ns mean gap"
  );
}

/// A-845 ("scale-free"). Do: feed one stream of gaps, and the same gaps each doubled. Expect: the second's horizon
/// exactly twice the first's, and its rate exactly half.
#[test]
fn stretching_time_moves_the_horizon_one_rung() {
  let times = poisson(0, 50.0, 2_000, 11);
  let (mut once, mut twice) = (Rates::new(BASE_NS), Rates::new(BASE_NS));
  for at in &times {
    once.arrival(*at);
    twice.arrival(*at * 2);
  }
  let (one, two) = (once.horizon_ns().unwrap(), twice.horizon_ns().unwrap());
  assert_eq!(two, one * 2.0);
  let last = *times.last().unwrap();
  let (fast, slow) = (
    once.works_per_s(last).unwrap(),
    twice.works_per_s(last * 2).unwrap(),
  );
  assert!((fast - slow * 2.0).abs() < fast * 1e-9, "{fast} vs {slow}");
}

/// A-845. Do: feed 500 arrivals at one a second, then 3,000 at a hundred a second. Expect: the rate read at the end
/// within a fifth of a hundred, and the horizon at the end shorter than after the slow regime.
#[test]
fn a_change_of_regime_is_followed() {
  let mut rates = Rates::new(BASE_NS);
  let slow = poisson(0, 1.0, 500, 3);
  for at in &slow {
    rates.arrival(*at);
  }
  let slow_horizon = rates.horizon_ns().unwrap();
  let fast = poisson(*slow.last().unwrap(), 100.0, 3_000, 5);
  for at in &fast {
    rates.arrival(*at);
  }
  let works = rates.works_per_s(*fast.last().unwrap()).unwrap();
  assert!((works - 100.0).abs() < 20.0, "works {works}");
  assert!(rates.horizon_ns().unwrap() < slow_horizon);
}

/// A-845 ("Before any gap"). Do: read rates before any arrival and after one. Expect: no rate and no horizon either
/// time, and a keep of one (everything kept).
#[test]
fn no_rate_is_read_before_a_gap() {
  let mut rates = Rates::new(BASE_NS);
  assert_eq!(rates.works_per_s(0), None);
  rates.arrival(5);
  assert_eq!(rates.works_per_s(10), None);
  assert_eq!(rates.bytes_per_s(10), None);
  assert_eq!(rates.horizon_ns(), None);
  assert_eq!(rates.keep(1_000_000), 1.0);
}

/// A-845 ("A principal's rates"). Do: feed one stream to a repository's rates and, at its horizon, to one principal's
/// counts as the only principal. Expect: the principal's works and bytes a second within a tenth of the repository's;
/// the keep over one horizon `e^-1`.
#[test]
fn a_lone_principal_reads_its_repositorys_rates() {
  let mut rates = Rates::new(BASE_NS);
  let mut principal = Decayed::default();
  for at in poisson(0, 20.0, 2_000, 13) {
    rates.arrival(at);
    rates.bytes(at, 100);
    principal.add(at, (1, 100), rates.horizon_ns());
  }
  let now = rates.horizon_ns().map(|_| 1).unwrap() + poisson(0, 20.0, 2_000, 13).last().unwrap();
  let (works, bytes) = principal.per_s(now, rates.horizon_ns()).unwrap();
  let repository = rates.works_per_s(now).unwrap();
  assert!(
    (works - repository).abs() < repository / 10.0,
    "{works} vs {repository}"
  );
  assert!(
    (bytes - repository * 100.0).abs() < repository * 10.0,
    "{bytes}"
  );
  let horizon = rates.horizon_ns().unwrap() as u64;
  assert!((rates.keep(horizon) - (-1f64).exp()).abs() < 1e-6);
}
