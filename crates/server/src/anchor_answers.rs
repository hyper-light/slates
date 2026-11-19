//! The anchor's answers on the Windows hold channel, and how long an exchange waits for one (A-552; R3's class D, D-c).
//!
//! A Windows daemon asks its anchor to hold a mount's handles, a Shards stream's resync end or DAX section, to replace a
//! held entry's record, and to release each (A-198, A-427, A-448), one exchange at a time over the hold channel, which
//! the anchor answers as soon as its loop wakes on the channel's posted read (A-213). How long that takes is this
//! machine's to measure, not a setting, so the daemon learns its anchor's answers as `crate::learned_wait` learns a
//! wait: RFC 6298's estimator over the exchanges answered (a refusal is an answer too), the bound their tail plus a
//! liveness window (the lateness a live party may show before it is judged, A-228); an exchange given up at its bound is
//! no sample (Karn's rule) and doubles the next bound (RFC 6298 §5.5), backed off no further than the failover delay the
//! fleet measures (`F = D + E + R`, A-363), the ceiling the helpers a shard waits on are backed off to (A-525). One
//! estimator for holds and releases alike: one party answers both on one channel. Until A-552 a hold waited the
//! recovery budget and a window and a release a window, whatever the anchor took. Cfg-free, so the rule is tested on
//! every host; its only caller is Windows' (`anchor_hold_windows.rs`).

/// The anchor's answers one daemon waited on.
#[derive(Debug, Default)]
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(crate) struct AnchorAnswers {
  answers: crate::learned_wait::LearnedWait,
}

impl AnchorAnswers {
  /// An exchange the anchor answered `waited_ns` after it began.
  #[cfg_attr(not(any(windows, test)), allow(dead_code))]
  pub(crate) fn answered(&mut self, waited_ns: u64) {
    self.answers.completed(waited_ns);
  }

  /// An exchange given up at its bound, unanswered.
  #[cfg_attr(not(any(windows, test)), allow(dead_code))]
  pub(crate) fn unanswered(&mut self) {
    self.answers.ran_out();
  }

  /// Derived: how long the next exchange waits for the anchor's answer: the learned bound of the answers this daemon
  /// waited on, backed off for those unanswered no further than the failover delay `failover_ns` (the module doc).
  #[cfg_attr(not(any(windows, test)), allow(dead_code))]
  pub(crate) fn bound_ns(&self, failover_ns: u64) -> u64 {
    self.answers.bound_ns(failover_ns)
  }
}

#[cfg(test)]
mod tests {
  use super::AnchorAnswers;
  use crate::daemon::LIVENESS_BUDGET_NS;

  /// Shape: an anchor's answer on a quiet machine, 2 ms: its loop wakes on the channel's posted read (A-213), takes one
  /// message and duplicates a few handles.
  const QUICK_ANSWER_NS: u64 = 2_000_000;
  /// Shape: the failover delay the cases run under: three liveness windows, a quiet fleet's order, past one doubling of
  /// the bound quick answers leave (a little over a window) and short of two.
  const DELAY_NS: u64 = 3 * LIVENESS_BUDGET_NS;

  /// A-552 (R3's class D: the Windows hold channel's two bounds). Do: ask the bound of an exchange with the anchor with
  /// no answer measured, then after quick answers; then let two exchanges go unanswered; then let one be answered.
  /// Expect: a liveness window at first; a little past it once quick answers are measured; doubled for the first
  /// exchange unanswered and held at the failover delay for the second; and back under the doubled bound once one is
  /// answered. Before, a hold waited the recovery budget and a window, and a release a window, whatever this machine's
  /// anchor took.
  #[test]
  fn an_exchanges_wait_follows_the_anchors_answers_and_backs_off_to_the_failover_delay() {
    let mut answers = AnchorAnswers::default();
    assert_eq!(
      answers.bound_ns(DELAY_NS),
      LIVENESS_BUDGET_NS,
      "a liveness window before any answer is measured"
    );
    for _ in 0..8 {
      answers.answered(QUICK_ANSWER_NS);
    }
    let learned = answers.bound_ns(DELAY_NS);
    assert!(
      learned > LIVENESS_BUDGET_NS && learned < LIVENESS_BUDGET_NS + LIVENESS_BUDGET_NS / 10,
      "quick answers put the bound a little past a window: {learned} ns"
    );
    answers.unanswered();
    assert_eq!(
      answers.bound_ns(DELAY_NS),
      2 * learned,
      "an exchange unanswered doubles the next bound"
    );
    answers.unanswered();
    assert_eq!(
      answers.bound_ns(DELAY_NS),
      DELAY_NS,
      "the back-off stops at the failover delay"
    );
    answers.answered(QUICK_ANSWER_NS);
    assert!(
      answers.bound_ns(DELAY_NS) < 2 * learned,
      "an answer ends the back-off"
    );
  }
}
