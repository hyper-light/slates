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
use proptest::prelude::*;

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
    rates.answered(*at, 4_096);
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
    once.answered(*at, 1);
    twice.answered(*at * 2, 1);
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
    rates.answered(*at, 1);
  }
  let slow_horizon = rates.horizon_ns().unwrap();
  let fast = poisson(*slow.last().unwrap(), 100.0, 3_000, 5);
  for at in &fast {
    rates.answered(*at, 1);
  }
  let works = rates.works_per_s(*fast.last().unwrap()).unwrap();
  assert!((works - 100.0).abs() < 20.0, "works {works}");
  assert!(rates.horizon_ns().unwrap() < slow_horizon);
}

/// A-845 ("Before any gap"). Do: read rates before any work and after one of 77 bytes. Expect: no rate and no horizon
/// either time, a keep of one (everything kept), and the bytes a work none, then the first work's own.
#[test]
fn no_rate_is_read_before_a_gap() {
  let mut rates = Rates::new(BASE_NS);
  assert_eq!(rates.works_per_s(0), None);
  assert_eq!(rates.bytes_per_work(), None);
  rates.answered(5, 77);
  assert_eq!(rates.bytes_per_work(), Some(77));
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
    rates.answered(at, 100);
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

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::with_cases(300)))]

  /// A-845 ("Bytes a work", the property A-724 piece 2a's `Learned` held). Do: answer any works, of any bytes, at any
  /// gaps (repeated instants too). Expect: the bytes a work always within the range of the bytes seen, the first work's
  /// own after one, and the works counted.
  #[test]
  fn the_bytes_a_work_never_leave_the_range_of_the_bytes_seen(
    works in prop::collection::vec((0u32..1_000_000, 0u64..10_000_000_000), 1..200),
  ) {
    let mut rates = Rates::new(BASE_NS);
    let (mut now, mut least, mut most) = (0u64, u64::MAX, 0u64);
    for (at, (bytes, gap)) in works.iter().enumerate() {
      now += gap;
      rates.answered(now, u64::from(*bytes));
      least = least.min(u64::from(*bytes));
      most = most.max(u64::from(*bytes));
      if at == 0 {
        prop_assert_eq!(rates.bytes_per_work(), Some(u64::from(*bytes)));
      }
      let expected = rates.bytes_per_work().unwrap();
      prop_assert!((least..=most).contains(&expected), "{expected} not in {least}..={most}");
    }
    prop_assert_eq!(rates.works(), u64::try_from(works.len()).unwrap());
  }
}

/// A-845 ("Bytes a work", A-724 piece 2a's case). Do: answer a hundred small works at a steady rhythm, then three 64
/// times larger at the same rhythm. Expect: the steady size read exactly, then the new size from its first work, where
/// the arrival horizon's mean (the longest rung, a steady rhythm's most likely) would still read about 2.8 small.
#[test]
fn the_bytes_a_work_follow_a_repositorys_new_size() {
  let mut rates = Rates::new(BASE_NS);
  let (small, large, gap) = (1_000u64, 64_000u64, 1_000_000u64);
  let mut now = 1;
  for _ in 0..100 {
    rates.answered(now, small);
    now += gap;
  }
  assert_eq!(rates.bytes_per_work(), Some(small));
  for _ in 0..3 {
    rates.answered(now, large);
    now += gap;
  }
  assert!(rates.bytes_per_work().unwrap() >= large / 2, "{rates:?}");
}

/// A-845 ("Bytes a work": a work after a silence). Do: answer a hundred small works at a steady rhythm, then after a
/// hundred gaps' silence one large work. Expect: the large size read within a hundredth.
#[test]
fn a_work_after_a_silence_outweighs_what_came_before() {
  let mut rates = Rates::new(BASE_NS);
  let (small, large, gap) = (1_000u64, 64_000u64, 1_000_000u64);
  let mut now = 1;
  for _ in 0..100 {
    rates.answered(now, small);
    now += gap;
  }
  rates.answered(now + 100 * gap, large);
  assert!(
    rates.bytes_per_work().unwrap() > large - large / 100,
    "{rates:?}"
  );
}

/// A-845 ("Bytes a work": noisy sizes). Do: answer 2,000 works at a steady rhythm whose bytes alternate 1,000 and
/// 3,000. Expect: the bytes a work near their mean, 2,000 (within a quarter), not the last one's: a longer rung's mean
/// predicts them better than the shortest's last value.
#[test]
fn noisy_sizes_are_read_as_their_mean() {
  let mut rates = Rates::new(BASE_NS);
  for at in 0..2_000u64 {
    rates.answered(1 + at * 1_000_000, if at % 2 == 0 { 1_000 } else { 3_000 });
  }
  let read = rates.bytes_per_work().unwrap();
  assert!((1_500..=2_500).contains(&read), "{read}");
}
