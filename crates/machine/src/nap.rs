//! Naps while work is expected (84a's Linux arm G; §4.7 "Wake strategy"): the shared terms of a shard's and a client's
//! short timed waits.
//!
//! **Why a nap.** Under Linux's lazy preemption a wakeup onto a CPU that runs a fair neighbour's user code is honoured
//! only when that CPU next enters the kernel, at its tick (kernel 7.0: `wakeup_preempt_fair` reschedules lazily, and
//! `__resched_curr` sends no interrupt for the lazy bit; a kicked shard ran 998 µs after its kick at the p99 on CI's
//! arm64 runner, run 45087231374). A thread that waits on a timer of its own instead gets that kernel entry on its own
//! CPU: the timer stays on the busy CPU it was armed on (`get_nohz_timer_target`), and the interrupt's return to user
//! mode reschedules (`exit_to_user_mode_loop` honours the lazy bit), so a wake that landed meanwhile runs within one
//! nap rather than at the tick.
//!
//! **How long.** A thread naps only while it expects work: until [`EXPECTED_FRACTION`] of its measured gaps (a shard's
//! idle gaps, a client's reply waits), read as exponential with their measured mean, would have ended
//! ([`expectation_ns`]). Each nap lasts R9's floor (the caller's idle window). Past the expectation it parks as before.
//!
//! **Slack.** Linux lets a thread's timers fire up to its slack late to batch them, 50 µs by default (prctl(2),
//! `PR_GET_TIMERSLACK`); a napping thread lowers its own to the least, with no privilege ([`set_least_timer_slack`]).

/// Derived: the share of a thread's gaps its naps cover, R9's percentile: it expects work until 99 in 100 of them would
/// have ended.
pub const EXPECTED_FRACTION: f64 = 0.99;

/// How long a thread expects work, given its gaps' measured mean: `mean · ln(1 / (1 − EXPECTED_FRACTION))`, the
/// exponential's quantile.
pub fn expectation_ns(mean_gap_ns: u64) -> u64 {
  #[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
  )]
  let expected = (mean_gap_ns as f64 * (1.0 / (1.0 - EXPECTED_FRACTION)).ln()) as u64;
  expected
}

/// Format: the least slack the kernel takes, 1 ns: `PR_SET_TIMERSLACK` with 0 asks for the thread's default back.
#[cfg(target_os = "linux")]
pub const LEAST_TIMER_SLACK_NS: u64 = 1;

/// Sets the calling thread's timer slack to [`LEAST_TIMER_SLACK_NS`]; the `errno` of a refusal.
#[cfg(target_os = "linux")]
pub fn set_least_timer_slack() -> Result<(), i32> {
  rustix::thread::set_current_timer_slack(std::num::NonZeroU64::new(LEAST_TIMER_SLACK_NS))
    .map_err(|e| e.raw_os_error())
}

/// Not Linux: timers keep their own slack.
#[cfg(not(target_os = "linux"))]
pub fn set_least_timer_slack() -> Result<(), i32> {
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 84a arm G. Do: ask the expectation of gaps with a mean of 1 ms. Expect: the exponential's 99th percentile,
  /// `ln(100)` means (4.605 ms) to the nanosecond's rounding, and none for a mean of none.
  #[test]
  fn the_expectation_is_the_exponentials_ninety_ninth_percentile() {
    let mean_ns = 1_000_000;
    let expected = expectation_ns(mean_ns);
    assert!((4_605_000..=4_606_000).contains(&expected), "{expected}");
    assert_eq!(expectation_ns(0), 0);
  }
}
