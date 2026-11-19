//! A wait whose bound is learned from the waits it measured (R3's class B; A-431, A-442): RFC 6298's estimator
//! (`slates_cluster::failover::RunningDuration`, the one the failover estimate's terms use) over the waits that
//! completed, the bound its tail plus a liveness window, the lateness a live party may show before it is judged
//! (A-228). A wait cut short at its bound is no sample (Karn's rule: its true length is unknown), but doubles the bound
//! for the next (RFC 6298 §5.5's back-off), never past the ceiling its caller names once backed off, and the next wait
//! that completes ends the back-off. The learned pace itself is never cut by the ceiling: the ceiling caps a back-off.
//!
//! Five waits learn this way: AC-3.3's told wait, per shard (`crate::told`, A-431); every callback on an NFSv4.1 back
//! channel, per carrying connection (`crate::callback`, A-442); the FUSE helpers' runs, per shard
//! (`crate::helper_runs`, A-525); a FUSE or WinFsp mount's journal reader's patience, per mount, from its own rounds
//! (`crate::reader_patience`, A-548); and a Windows daemon's exchanges with its anchor on the hold channel, per daemon
//! (`crate::anchor_answers`, A-552). One rule, so they never drift apart.

/// One kind of wait's measured lengths and its back-off.
#[derive(Debug, Default)]
pub(crate) struct LearnedWait {
  /// The waits that completed. A wait cut short at its bound is not a sample (Karn's rule).
  waits: slates_cluster::failover::RunningDuration,
  /// The waits that ran out since the last that completed: each doubles the bound (RFC 6298 §5.5).
  backoffs: u32,
}

impl LearnedWait {
  /// Derived: the bound the next wait runs to: the tail of the completed waits (none before the first) plus
  /// `LIVENESS_BUDGET_NS`, doubled for each wait that ran out since the last completed one, and never past
  /// `ceiling_ns` once backed off. The unbacked bound stands whatever the ceiling.
  pub(crate) fn bound_ns(&self, ceiling_ns: u64) -> u64 {
    let paced = self
      .waits
      .tail_ns()
      .unwrap_or(0)
      .saturating_add(crate::daemon::LIVENESS_BUDGET_NS);
    if self.backoffs == 0 {
      return paced;
    }
    paced
      .saturating_mul(BACKOFF_FACTOR.saturating_pow(self.backoffs))
      .min(ceiling_ns.max(paced))
  }

  /// A wait that completed after `waited_ns`: a sample, and the end of any back-off.
  pub(crate) fn completed(&mut self, waited_ns: u64) {
    self.waits.on_sample(waited_ns);
    self.backoffs = 0;
  }

  /// A wait that ran out at its bound: no sample (Karn's rule), and the bound backed off.
  pub(crate) fn ran_out(&mut self) {
    self.backoffs = self.backoffs.saturating_add(1);
  }
}

/// Format: RFC 6298 §5.5's back-off: each timeout doubles the timer.
const BACKOFF_FACTOR: u64 = 2;

#[cfg(test)]
mod tests {
  use super::LearnedWait;
  use crate::daemon::LIVENESS_BUDGET_NS;

  /// Shape: a wait as a healthy party answers one, 40 µs (the median told wait A-191 measured is about 12 µs).
  const QUICK_NS: u64 = 40_000;
  /// Shape: a wait on a machine whose party answers slowly but answers: a liveness window and a half.
  const SLOW_NS: u64 = LIVENESS_BUDGET_NS + LIVENESS_BUDGET_NS / 2;
  /// Shape: the ceiling the back-off is held under: four liveness windows.
  const CEILING_NS: u64 = 4 * LIVENESS_BUDGET_NS;

  /// A-431, A-442. Do: ask the bound with nothing measured; fold quick completed waits; then waits as slow as a window
  /// and a half. Expect: a liveness window at first; a little past it once quick waits were measured; and past the slow
  /// waits once they were, so a party that answers late but answers is waited for at its pace, where a fixed half
  /// window gave every such wait up.
  #[test]
  fn a_learned_bound_follows_the_waits_it_measured() {
    let mut times = LearnedWait::default();
    assert_eq!(times.bound_ns(CEILING_NS), LIVENESS_BUDGET_NS);
    for _ in 0..8 {
      times.completed(QUICK_NS);
    }
    let quick = times.bound_ns(CEILING_NS);
    assert!(
      quick > LIVENESS_BUDGET_NS && quick < LIVENESS_BUDGET_NS + LIVENESS_BUDGET_NS / 10,
      "quick waits put the bound a little past a window: {quick} ns"
    );
    for _ in 0..8 {
      times.completed(SLOW_NS);
    }
    let slow = times.bound_ns(CEILING_NS);
    assert!(
      slow > SLOW_NS + LIVENESS_BUDGET_NS / 2,
      "slow waits that completed put the bound past them: {slow} ns"
    );
  }

  /// A-431, A-442 (RFC 6298 §5.5, Karn's rule). Do: with quick waits measured, let three waits in a row run out, then a
  /// fourth; then let one complete. Expect: each wait that ran out doubles the bound, the back-off never passes the
  /// ceiling and never sinks below the learned bound, and the completed wait ends the back-off.
  #[test]
  fn a_wait_that_ran_out_backs_the_bound_off_to_its_ceiling_and_a_completed_one_ends_it() {
    let mut times = LearnedWait::default();
    for _ in 0..8 {
      times.completed(QUICK_NS);
    }
    let paced = times.bound_ns(CEILING_NS);
    times.ran_out();
    assert_eq!(times.bound_ns(CEILING_NS), 2 * paced);
    times.ran_out();
    assert_eq!(times.bound_ns(CEILING_NS), (4 * paced).min(CEILING_NS));
    times.ran_out();
    times.ran_out();
    assert_eq!(
      times.bound_ns(CEILING_NS),
      CEILING_NS,
      "never past the ceiling"
    );
    assert_eq!(
      times.bound_ns(paced / 2),
      paced,
      "a ceiling below the learned bound caps no back-off below it"
    );
    times.completed(QUICK_NS);
    assert!(
      times.bound_ns(CEILING_NS) < 2 * paced,
      "a completed wait ends the back-off"
    );
  }
}
