//! The helper processes a shard starts and waits on, and how long it waits for each (A-525; R3, A-414's owed list): the
//! FUSE mount's and unmount's `fusermount3` (`crate::fuse`, Linux), each one exec of a small setuid program and one
//! mount or unmount system call. How long that takes is this machine's to measure, not a setting, so a shard learns its
//! helpers' runs as `crate::learned_wait` learns a wait: RFC 6298's estimator over the runs that finished, the bound
//! their tail plus a liveness window (the lateness a live party may show before it is judged, A-228); a run given up at
//! its bound is no sample (Karn's rule) and doubles the next bound (RFC 6298 §5.5), backed off no further than the
//! failover delay the fleet measures (`F = D + E + R`, A-363), the bound it holds a dead owner's volumes to. A helper
//! that has not finished by then is given up, killed and reaped, and its mount refused, typed; the next is waited for
//! longer. Until A-525 every helper was given the operator's ten-second failover placeholder, whatever its machine's
//! helpers took. Cfg-free, so the rule is tested on every host; its only caller is Linux's.

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

  /// Derived: how long the next helper is waited for before it is given up: the learned bound of the runs this shard
  /// waited on, backed off for those given up no further than the failover delay `failover_ns` (the module doc).
  #[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
  pub(crate) fn bound_ns(&self, failover_ns: u64) -> u64 {
    self.runs.bound_ns(failover_ns)
  }
}

#[cfg(test)]
mod tests {
  use super::HelperRuns;
  use crate::daemon::LIVENESS_BUDGET_NS;

  /// Shape: a `fusermount3` run on a quiet machine, 20 ms: one exec of a small setuid program and one system call.
  const QUICK_RUN_NS: u64 = 20_000_000;
  /// Shape: the failover delay the cases run under: three liveness windows, a quiet fleet's order, past one doubling of
  /// the bound quick runs leave (a little over a window) and short of two.
  const DELAY_NS: u64 = 3 * LIVENESS_BUDGET_NS;

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
