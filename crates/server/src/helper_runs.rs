//! The helper processes a shard starts and waits on, and how long it waits for each (A-525; R3, A-414's owed list): the
//! FUSE mount's and unmount's `fusermount3` (`crate::fuse`, Linux). Cfg-free, so the rule is tested on every host; its
//! only caller is Linux's.

/// The helper runs one shard waited on.
#[derive(Debug, Default)]
pub(crate) struct HelperRuns {
  runs: crate::learned_wait::LearnedWait,
}

impl HelperRuns {
  /// A helper that finished `ran_ns` after it was started.
  #[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
  pub(crate) fn finished(&mut self, ran_ns: u64) {
    self.runs.completed(ran_ns);
  }

  /// A helper given up at its bound, killed and reaped.
  #[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
  pub(crate) fn given_up(&mut self) {
    self.runs.ran_out();
  }

  /// How long the next helper is waited for before it is given up, under the failover delay `_failover_ns`.
  #[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
  pub(crate) fn bound_ns(&self, _failover_ns: u64) -> u64 {
    crate::config::FAILOVER_SLO_NS
  }
}

#[cfg(test)]
mod tests {
  use super::HelperRuns;
  use crate::daemon::LIVENESS_BUDGET_NS;

  /// Shape: a `fusermount3` run on a quiet machine, 20 ms: one exec of a small setuid program and one system call.
  const QUICK_RUN_NS: u64 = 20_000_000;
  /// Shape: the failover delay the cases run under: two liveness windows, a quiet fleet's order.
  const DELAY_NS: u64 = 2 * LIVENESS_BUDGET_NS;

  /// A-525 (R3, A-414's owed list: the failover placeholder's readers). Do: ask a shard's helper bound with no run
  /// measured, then after quick runs; then give two runs up; then let one finish. Expect: a liveness window at first; a
  /// little past it once quick runs are measured; doubled for the first run given up and held at the failover delay for
  /// the second; and back at the learned bound once a run finishes. Before, every helper was given the operator's
  /// ten-second placeholder, whatever this machine's helpers took.
  #[test]
  fn a_helpers_wait_follows_the_runs_this_shard_measured_and_backs_off_to_the_failover_delay() {
    let mut runs = HelperRuns::default();
    assert_eq!(
      runs.bound_ns(DELAY_NS),
      LIVENESS_BUDGET_NS,
      "a liveness window before any run is measured"
    );
    for _ in 0..8 {
      runs.finished(QUICK_RUN_NS);
    }
    let learned = runs.bound_ns(DELAY_NS);
    assert!(
      learned > LIVENESS_BUDGET_NS && learned < LIVENESS_BUDGET_NS + LIVENESS_BUDGET_NS / 10,
      "quick runs put the bound a little past a window: {learned} ns"
    );
    runs.given_up();
    assert_eq!(
      runs.bound_ns(DELAY_NS),
      2 * learned,
      "a run given up doubles the next bound"
    );
    runs.given_up();
    assert_eq!(
      runs.bound_ns(DELAY_NS),
      DELAY_NS,
      "the back-off stops at the failover delay"
    );
    runs.finished(QUICK_RUN_NS);
    assert!(
      runs.bound_ns(DELAY_NS) < 2 * learned,
      "a run that finished ends the back-off"
    );
  }
}
