//! What a held shard's idle turn costs a request, learned online from the shard's own spins, and whether a hold pays
//! (§4.7; A-387, which amends A-326).
//!
//! A forward made for a client in R9's closed loop asks the loop's owner to hold its idle window open for the loop's
//! return (A-326), on the premise that a held shard polls: it sees the loop's next request at the end of the idle turn
//! the request lands in. A parked shard sees it once a kick wakes it, and the shard's online wake estimate measures what
//! that costs (A-223). So a hold pays only while the turn a request lands in ends sooner than a kicked park wakes. Where
//! that holds depends on the machine, not the platform's name: Linux's epoll and Windows' completion port answer a
//! zero-timeout poll at once, so a turn is well under a microsecond, while macOS's zero-timeout `kevent` sleeps 12–15 µs
//! with an involuntary switch (A-218), longer than a kicked park's 3–8 µs wake; and a loaded machine of any kind can
//! preempt a turn for a scheduler quantum. A rule fixed per platform (A-387's first form, 2026-11-18) is right only
//! where its platform behaves as measured, so every shard learns its own turns from every idle spin, its window's and
//! its holds' alike: it keeps learning while it declines holds, and holds again if its turns become cheap.
//!
//! A request meets a turn with probability in proportion to the turn's length, so whenever turns vary, the turn it
//! lands in is longer than the mean turn: its expected length is E[T²] / E[T], the length-biased mean (the inspection
//! paradox). A preemption that takes one turn in a hundred to a millisecond leaves the mean turn near its usual length
//! but makes most requests meet the long turn, and the length-biased mean says so. Both moments are learned over the
//! number of samples the shard's wake estimate weights (`2^shift`, derived from the boot probe's spread, §4.1), so both
//! sides of the comparison forget at one count: a plain mean over the first `2^shift` turns, which assumes no prior and
//! lets one atypical first turn count as one in many, then exponentially weighted means seeded with it
//! ([`WakeEstimate`]).

use slates_machine::wake::WakeEstimate;

/// Format: the longest turn the estimator holds, nanoseconds: the largest whose square fits the 64-bit sample a
/// [`WakeEstimate`] takes (`u32::MAX`, about 4.29 s). A longer turn (a thread stopped or descheduled for seconds) counts
/// as this long, which already declines any hold whose wake estimate is shorter.
pub const LONGEST_TURN_NS: u64 = u32::MAX as u64;

/// The idle turns a shard has measured, as a request meets them (see the module documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeldTurn {
  shift: u32,
  learned: Learned,
}

/// The moments learned so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Learned {
  /// Fewer than `2^shift` turns: their count and the sums of their lengths and squared lengths.
  Counting {
    turns: u64,
    length_sum: u128,
    square_sum: u128,
  },
  /// `2^shift` turns or more: exponentially weighted means of a turn's length and of its square, over about `2^shift`.
  Weighted {
    length: WakeEstimate,
    square: WakeEstimate,
  },
}

impl HeldTurn {
  /// No turn measured yet, to be weighted over about `2^shift` turns (the shard's wake estimate's shift).
  pub fn new(shift: u32) -> HeldTurn {
    HeldTurn {
      shift: shift.min(WakeEstimate::MAX_SHIFT),
      learned: Learned::Counting {
        turns: 0,
        length_sum: 0,
        square_sum: 0,
      },
    }
  }

  /// The turns the plain mean covers before the weighted means take over: `2^shift`.
  /// Derived: one shifted left `shift` places is `2^shift` (saturating at `u64::MAX`), the wake estimate's own window.
  fn window(&self) -> u64 {
    1u64.checked_shl(self.shift).unwrap_or(u64::MAX)
  }

  /// Folds one idle turn's measured length in, nanoseconds (at most [`LONGEST_TURN_NS`]).
  pub fn record(&mut self, turn_ns: u64) {
    let turn_ns = turn_ns.min(LONGEST_TURN_NS);
    let square_ns = turn_ns.saturating_mul(turn_ns);
    self.learned = match self.learned {
      Learned::Counting {
        turns,
        length_sum,
        square_sum,
      } => {
        let turns = turns.saturating_add(1);
        let length_sum = length_sum.saturating_add(u128::from(turn_ns));
        let square_sum = square_sum.saturating_add(u128::from(square_ns));
        if turns >= self.window() {
          Learned::Weighted {
            length: WakeEstimate::new(mean(length_sum, turns), self.shift),
            square: WakeEstimate::new(mean(square_sum, turns), self.shift),
          }
        } else {
          Learned::Counting {
            turns,
            length_sum,
            square_sum,
          }
        }
      }
      Learned::Weighted {
        mut length,
        mut square,
      } => {
        length.record(turn_ns);
        square.record(square_ns);
        Learned::Weighted { length, square }
      }
    };
  }

  /// The expected length of the turn a request lands in, nanoseconds: E[T²] / E[T] over the turns learned. `None`
  /// before the first turn; 0 while every turn measured took no time (a clock that does not move between reads).
  pub fn met_ns(&self) -> Option<u64> {
    let (length, square) = match self.learned {
      Learned::Counting { turns: 0, .. } => return None,
      // The ratio of the sums is the ratio of the means.
      Learned::Counting {
        length_sum,
        square_sum,
        ..
      } => (length_sum, square_sum),
      Learned::Weighted { length, square } => {
        (u128::from(length.mean_ns()), u128::from(square.mean_ns()))
      }
    };
    Some(
      square
        .checked_div(length)
        .map_or(0, |met| u64::try_from(met).unwrap_or(u64::MAX)),
    )
  }

  /// The turns folded in so far (a non-vacuity count).
  pub fn turns(&self) -> u64 {
    match self.learned {
      Learned::Counting { turns, .. } => turns,
      Learned::Weighted { length, .. } => self.window().saturating_add(length.samples()),
    }
  }
}

/// `sum / count`, held to a `u64` (`count` is never 0 where it is called).
fn mean(sum: u128, count: u64) -> u64 {
  sum
    .checked_div(u128::from(count))
    .map_or(0, |mean| u64::try_from(mean).unwrap_or(u64::MAX))
}

/// Whether holding a shard's idle window pays (§4.7; A-387): a held shard sees a request at the end of the turn it
/// lands in (`met_turn_ns`, [`HeldTurn::met_ns`]), a parked one once a kick wakes it (`wake_ns`, the shard's online
/// wake estimate), so a hold pays while the first is shorter. A shard that has measured no turn yet has no evidence its
/// turns nap and holds as A-326 asks.
pub fn hold_pays(met_turn_ns: Option<u64>, wake_ns: u64) -> bool {
  met_turn_ns.is_none_or(|turn| turn < wake_ns)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: an estimate weighting about sixteen turns, so a test crosses from the plain mean to the weighted one.
  const SHIFT: u32 = 4;
  /// Shape: a macOS turn, the zero-timeout `kevent`'s nap (A-218), and a Linux one, a zero-timeout `epoll_wait`.
  const NAPPING_TURN_NS: u64 = 13_500;
  const POLLING_TURN_NS: u64 = 400;
  /// Shape: a kicked park's wake on Apple silicon and on a Linux runner (BENCHMARKS, the A-387 and A-223 rows).
  const MAC_WAKE_NS: u64 = 4_500;
  const LINUX_WAKE_NS: u64 = 9_000;
  /// Shape: a turn a scheduler quantum preempted.
  const PREEMPTED_TURN_NS: u64 = 1_000_000;

  /// A-387. Do: ask a fresh estimate for the turn a request meets; fold in one turn; then many more of the same length,
  /// past the plain mean's window into the weighted one. Expect: nothing before the first turn; the first turn alone
  /// after it, with no prior pulling it anywhere; the same length through the window and after it, every turn counted.
  #[test]
  fn the_first_turn_measured_is_the_estimate_and_equal_turns_keep_it() {
    let mut turns = HeldTurn::new(SHIFT);
    assert_eq!(turns.met_ns(), None);
    turns.record(NAPPING_TURN_NS);
    assert_eq!(turns.met_ns(), Some(NAPPING_TURN_NS));
    let window = 1_u64 << SHIFT;
    for _ in 1..window * 4 {
      turns.record(NAPPING_TURN_NS);
      assert_eq!(turns.met_ns(), Some(NAPPING_TURN_NS));
    }
    assert_eq!(turns.turns(), window * 4);
  }

  /// A-387. Do: learn ninety-nine short turns and one a quantum preempted, in each order. Expect: the turn a request
  /// meets is the length-biased mean, E[T²] / E[T], exactly while the plain mean covers the turns (the preempted turn
  /// takes 96% of the time, so most requests meet it), far above the mean turn; and the order of the turns does not
  /// change it.
  #[test]
  fn a_request_meets_a_preempted_turn_in_proportion_to_its_length() {
    let short = 99_u64;
    let length_sum = u128::from(short * POLLING_TURN_NS + PREEMPTED_TURN_NS);
    let square_sum = u128::from(short * POLLING_TURN_NS * POLLING_TURN_NS)
      + u128::from(PREEMPTED_TURN_NS) * u128::from(PREEMPTED_TURN_NS);
    let expected = u64::try_from(square_sum / length_sum).unwrap();
    let mean_turn = u64::try_from(length_sum / u128::from(short + 1)).unwrap();
    // The plain mean covers all hundred turns at a shift of seven (128).
    for preempted_first in [true, false] {
      let mut turns = HeldTurn::new(7);
      if preempted_first {
        turns.record(PREEMPTED_TURN_NS);
      }
      for _ in 0..short {
        turns.record(POLLING_TURN_NS);
      }
      if !preempted_first {
        turns.record(PREEMPTED_TURN_NS);
      }
      assert_eq!(
        turns.met_ns(),
        Some(expected),
        "preempted first: {preempted_first}"
      );
      assert!(
        expected > mean_turn * 50,
        "{expected} against a mean turn of {mean_turn}"
      );
    }
  }

  /// A-387, the estimator's direction. Do: after a run of turns of mixed lengths, past the window, fold in one turn
  /// longer than the turn a request meets, and from the same state one shorter. Expect: the longer raises the estimate
  /// and the shorter lowers it (with weights `w`, the ratio (S2 + w·x²) / (S1 + w·x) exceeds S2 / S1 exactly when x
  /// exceeds it), so the estimate keeps learning in both directions after the plain mean has handed over.
  #[test]
  fn a_longer_turn_raises_the_estimate_and_a_shorter_lowers_it() {
    let mut turns = HeldTurn::new(SHIFT);
    for n in 0..(1_u64 << SHIFT) * 3 {
      turns.record(if n % 5 == 0 {
        NAPPING_TURN_NS
      } else {
        POLLING_TURN_NS
      });
    }
    let met = turns.met_ns().unwrap();
    let mut longer = turns;
    longer.record(met * 2);
    let mut shorter = turns;
    shorter.record(met / 2);
    assert!(longer.met_ns().unwrap() > met, "{longer:?} against {met}");
    assert!(shorter.met_ns().unwrap() < met, "{shorter:?} against {met}");
  }

  /// A-387, the decision. Do: ask whether a hold pays with no turn measured, with turns that nap past the wake (a Mac's),
  /// with turns far under it (a Linux runner's), and with a turn exactly a wake long. Expect: no evidence holds (A-326);
  /// a napping turn declines; a polling turn holds; and a turn as long as the wake declines, since then a parked shard
  /// sees the request no later and spends no core waiting for it.
  #[test]
  fn a_hold_pays_only_while_the_turn_a_request_meets_is_shorter_than_a_wake() {
    assert!(hold_pays(None, MAC_WAKE_NS));
    let mut mac = HeldTurn::new(SHIFT);
    mac.record(NAPPING_TURN_NS);
    assert!(!hold_pays(mac.met_ns(), MAC_WAKE_NS));
    let mut linux = HeldTurn::new(SHIFT);
    linux.record(POLLING_TURN_NS);
    assert!(hold_pays(linux.met_ns(), LINUX_WAKE_NS));
    assert!(!hold_pays(Some(LINUX_WAKE_NS), LINUX_WAKE_NS));
  }

  /// A-387, hostile inputs. Do: fold in turns of zero length, of the largest length, and of the first length whose
  /// square passes 64 bits, at the smallest, the largest and a past-the-largest shift. Expect: no panic and no wrap: a
  /// clock that does not move gives a turn of 0, and a turn longer than the estimator holds counts as the longest it
  /// holds, which declines any hold.
  #[test]
  fn extreme_turns_saturate_and_never_panic() {
    for shift in [0, WakeEstimate::MAX_SHIFT, u32::MAX] {
      let mut still = HeldTurn::new(shift);
      still.record(0);
      still.record(0);
      assert_eq!(still.met_ns(), Some(0), "shift {shift}");
      let mut huge = HeldTurn::new(shift);
      huge.record(u64::MAX);
      huge.record(LONGEST_TURN_NS + 1);
      assert_eq!(
        huge.met_ns(),
        Some(LONGEST_TURN_NS),
        "shift {shift}: {huge:?}"
      );
      assert!(!hold_pays(huge.met_ns(), LONGEST_TURN_NS));
    }
  }
}
