//! The owner's spin-before-park decision ([`Gaps`], [`Wake`]) held to its module docs' rule, rent or buy: spin, at most
//! a park's wake, only where the next ask is predicted within it (main's ruling of 2026-11-25 on the 57k split at
//! 7a9cb385e). The decision is pure, so it is held here over fixed histories; `tests/owner.rs` holds the owner itself to
//! it where the machine's timing allows (reads 20 ms apart park at once).

use super::{Gaps, Wake};

/// Shape: an asker's gap back to back, as a shard's task asks again after taking an answer: 20 us.
const BACK_TO_BACK_NS: u64 = 20_000;
/// Shape: the gap and the wake main's 57k split measured at 7a9cb385e: asks 12.7 ms apart, a park's wake 1.36 ms.
const SPLIT_GAP_NS: u64 = 12_700_000;
/// Shape: that split's wake.
const SPLIT_WAKE_NS: u64 = 1_360_000;
/// Shape: the profile's idle wake the daemon seeds with (`slates profile --quick` on the MacBook at load 27).
const PROFILE_WAKE_NS: u64 = 9_385;
/// Shape: the gaps a test learns before it judges, past the smoothing's settling (gain 1/8).
const SETTLED: usize = 64;

/// `gap_ns` learned `count` times.
fn gaps_of(gap_ns: u64, count: usize) -> Gaps {
  let mut gaps = Gaps::default();
  for _ in 0..count {
    gaps.learn(gap_ns);
  }
  gaps
}

/// Do: decide with no gap learned yet. Expect: park at once, however long the wake: a spin needs a prediction.
#[test]
fn nothing_learned_parks_at_once() {
  assert_eq!(Gaps::default().window(&Wake::seeded(SPLIT_WAKE_NS)), 0);
}

/// Do: learn back-to-back gaps, against a wake longer than them (the split's). Expect: spin, for the wake's length, at
/// most: a park would cost more than the gap.
#[test]
fn a_gap_predicted_within_the_wake_is_spun_for_at_most_the_wake() {
  let gaps = gaps_of(BACK_TO_BACK_NS, SETTLED);
  assert_eq!(gaps.window(&Wake::seeded(SPLIT_WAKE_NS)), SPLIT_WAKE_NS);
}

/// Do: learn the split's gaps against its wake. Expect: park at once, no spin. Red before: the spin was the predicted gap
/// capped (at the wake here), so every wait spun 1.36 ms and missed.
#[test]
fn a_gap_predicted_past_the_wake_parks_at_once() {
  let gaps = gaps_of(SPLIT_GAP_NS, SETTLED);
  assert_eq!(gaps.window(&Wake::seeded(SPLIT_WAKE_NS)), 0);
}

/// Do: learn back-to-back gaps against the profile's idle wake, shorter than them. Expect: park at once: a park costs
/// less than the gap, so a spin would only burn the owner's core.
#[test]
fn a_wake_cheaper_than_the_gap_parks_at_once() {
  let gaps = gaps_of(BACK_TO_BACK_NS, SETTLED);
  assert_eq!(gaps.window(&Wake::seeded(PROFILE_WAKE_NS)), 0);
}

/// Do: seed the wake at the profile's, then learn the split's wake [`SETTLED`] times, as its parks teach it. Expect: the
/// learned wake within an eighth of the split's (the seed displaced), so back-to-back gaps are then spun through.
#[test]
fn the_wake_learned_from_parks_displaces_the_seed() {
  let mut wake = Wake::seeded(PROFILE_WAKE_NS);
  for _ in 0..SETTLED {
    wake.learn(SPLIT_WAKE_NS);
  }
  assert!(
    wake.mean_ns.abs_diff(SPLIT_WAKE_NS) <= SPLIT_WAKE_NS / 8,
    "learned {} ns, the split's {SPLIT_WAKE_NS} ns",
    wake.mean_ns
  );
  assert!(gaps_of(BACK_TO_BACK_NS, SETTLED).window(&wake) > 0);
}

/// Do: learn one idle gap of the split's length, then back-to-back gaps, against the split's wake. Expect: parked while
/// the idle gap weighs on the prediction, then spun again within [`SETTLED`] gaps of the burst: an idle moment never
/// keeps a burst parked for good.
#[test]
fn a_burst_after_an_idle_gap_is_spun_through_again() {
  let mut gaps = gaps_of(SPLIT_GAP_NS, 1);
  let wake = Wake::seeded(SPLIT_WAKE_NS);
  assert_eq!(gaps.window(&wake), 0, "parked right after the idle gap");
  let spun_again = (1..=SETTLED).find(|_| {
    gaps.learn(BACK_TO_BACK_NS);
    gaps.window(&wake) > 0
  });
  assert!(spun_again.is_some(), "spun again within {SETTLED} gaps");
}
