//! Progress-based deadline extension (§4.8 cluster plane; the "late work" pattern) — the mechanism that
//! tells a **slow-but-progressing** operation apart from a **stuck** one, so the first is given more time
//! rather than declared failed. A long fleet operation (a landing, a merge, a large put or clone) runs
//! under a deadline; as it nears that deadline, if it is still making forward progress it is granted a
//! bounded extension, and only a stalled operation — or one that has spent its extension budget — is left
//! to the hard timeout. This is the difference between a genuinely failed node and one that is merely
//! overloaded, which SWIM's Lifeguard multiplier expresses locally and this expresses per operation.
//!
//! It composes with the SWIM detector rather than duplicating it: SWIM decides a *peer's* liveness; this
//! decides an *operation's*, using a progress measure the operation itself reports. A commit whose quorum
//! is filling, a landing whose bytes are advancing, a merge whose position is climbing — each is
//! progressing and earns its extension; one whose measure has not moved for a stall window is not.
//!
//! Sans-io and oracle-tested at N=1: the caller reports a progress measure and the current time, and the
//! policy returns extend / continue / expire. Every bound is a derived parameter, never a hidden
//! constant.
//!
//! Evidence: hyperscale's worker autonomous extension trigger (AD-26; `nodes/worker/extension_trigger.py`)
//! and its progress witnesses (`health/progress_witness/`, tier C, deployed). hyperscale drives the
//! progress decision with change-point statistics (Bayesian online change-point detection, a
//! Kolmogorov–Smirnov two-sample test, a throughput witness); this slice is the throughput essence —
//! advanced-since-last-observation over a stall window — with the statistical witnesses an owed
//! refinement for distinguishing a slowing trend from an outright stall under noisy progress.

/// A progress witness for one operation: the highest progress measure it has reported and when that
/// measure last advanced. The measure is monotone and operation-defined (bytes landed, records merged,
/// chunks acknowledged); only its *advance* matters, so the witness is unit-agnostic.
pub struct ProgressWitness {
  last_measure: u64,
  last_advance_ns: u64,
  stall_window_ns: u64,
}

impl ProgressWitness {
  /// A witness that starts observing at `now_ns` with no progress yet. `stall_window_ns` is the span of
  /// no advance after which the operation is judged stalled (derived from the operation's expected
  /// per-step latency — long enough that a genuinely working step is not called stuck).
  pub fn new(stall_window_ns: u64, now_ns: u64) -> ProgressWitness {
    ProgressWitness {
      last_measure: 0,
      last_advance_ns: now_ns,
      stall_window_ns,
    }
  }

  /// Reports the operation's current progress `measure` at `now_ns`. A measure greater than the highest
  /// seen advances the witness (the operation moved); an equal or lower one does not (no forward
  /// progress — a stale or reordered report).
  pub fn observe(&mut self, measure: u64, now_ns: u64) {
    if measure > self.last_measure {
      self.last_measure = measure;
      self.last_advance_ns = now_ns;
    }
  }

  /// Whether the operation is progressing at `now_ns`: its measure advanced within the stall window. A
  /// witness that has gone a whole stall window with no advance is stalled.
  pub fn is_progressing(&self, now_ns: u64) -> bool {
    now_ns.saturating_sub(self.last_advance_ns) < self.stall_window_ns
  }

  /// The highest progress measure reported so far.
  pub fn last_measure(&self) -> u64 {
    self.last_measure
  }
}

/// What the extension policy decides for an operation at a point in time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionOutcome {
  /// The operation is not yet near its deadline — keep running under the current one.
  Continue,
  /// The operation is near its deadline and still progressing — its deadline is extended to this value.
  Extend {
    /// The new deadline (nanoseconds since the operation's start).
    deadline_ns: u64,
  },
  /// The operation is stalled, or has spent its extension budget — let the hard timeout end it.
  Expire,
}

/// The deadline-extension policy for one operation: its current deadline, when (as a fraction of the
/// deadline) to begin considering an extension, how much each extension adds, how many are allowed, and
/// how many have been granted. Every field is a derived parameter the caller supplies.
pub struct DeadlineExtender {
  deadline_ns: u64,
  lookahead_numerator: u64,
  lookahead_denominator: u64,
  extension_ns: u64,
  max_extensions: u32,
  granted: u32,
}

impl DeadlineExtender {
  /// A policy for an operation with the given `deadline_ns`. The lookahead fraction is the integer ratio
  /// `lookahead_numerator / lookahead_denominator` of the deadline at which extension is first considered
  /// (kept as a ratio so no floating point enters an operational decision — e.g. `3 / 4` for the last
  /// quarter of the budget, hyperscale's 0.75). `extension_ns` is added per grant, up to `max_extensions`
  /// grants. All are derived from the operation's class (its measured step latency and its budget).
  pub fn new(
    deadline_ns: u64,
    lookahead_numerator: u64,
    lookahead_denominator: u64,
    extension_ns: u64,
    max_extensions: u32,
  ) -> DeadlineExtender {
    DeadlineExtender {
      deadline_ns,
      lookahead_numerator,
      lookahead_denominator,
      extension_ns,
      max_extensions,
      granted: 0,
    }
  }

  /// The operation's current deadline (nanoseconds since its start), which grows as extensions are
  /// granted.
  pub fn deadline_ns(&self) -> u64 {
    self.deadline_ns
  }

  /// The number of extensions granted so far.
  pub fn granted(&self) -> u32 {
    self.granted
  }

  /// Decides what to do for an operation that has run for `elapsed_ns`, given its progress `witness` at
  /// `now_ns`. Before the lookahead point it [`Continue`](ExtensionOutcome::Continue)s. At or past it, a
  /// progressing operation with budget remaining is [`Extend`](ExtensionOutcome::Extend)ed (the deadline
  /// and the grant count advance); a stalled one, or one whose budget is spent, is left to
  /// [`Expire`](ExtensionOutcome::Expire). The lookahead test is the integer cross-multiplication
  /// `elapsed × denominator ≥ deadline × numerator`, so no fraction is ever rounded.
  pub fn evaluate(
    &mut self,
    elapsed_ns: u64,
    witness: &ProgressWitness,
    now_ns: u64,
  ) -> ExtensionOutcome {
    let reached_lookahead = elapsed_ns.saturating_mul(self.lookahead_denominator)
      >= self.deadline_ns.saturating_mul(self.lookahead_numerator);
    if !reached_lookahead {
      return ExtensionOutcome::Continue;
    }
    if self.granted >= self.max_extensions || !witness.is_progressing(now_ns) {
      return ExtensionOutcome::Expire;
    }
    self.granted = self.granted.saturating_add(1);
    self.deadline_ns = self.deadline_ns.saturating_add(self.extension_ns);
    ExtensionOutcome::Extend {
      deadline_ns: self.deadline_ns,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  // Test time values (nanoseconds); a production caller derives these from the operation's class.
  const DEADLINE: u64 = 100;
  const STALL_WINDOW: u64 = 20;
  const EXTENSION: u64 = 50;
  const MAX_EXTENSIONS: u32 = 2;
  // The lookahead ratio 3/4 — extension is considered in the last quarter of the budget.
  const LOOKAHEAD_NUM: u64 = 3;
  const LOOKAHEAD_DEN: u64 = 4;

  fn extender() -> DeadlineExtender {
    DeadlineExtender::new(
      DEADLINE,
      LOOKAHEAD_NUM,
      LOOKAHEAD_DEN,
      EXTENSION,
      MAX_EXTENSIONS,
    )
  }

  /// The witness advances only on a greater measure, and reports stalled once a whole stall window has
  /// passed with no advance.
  #[test]
  fn the_witness_tracks_forward_progress() {
    let mut witness = ProgressWitness::new(STALL_WINDOW, 0);
    witness.observe(10, 5);
    assert_eq!(witness.last_measure(), 10);
    witness.observe(10, 9); // no advance (equal measure)
    witness.observe(7, 11); // no advance (lower measure)
    assert!(
      witness.is_progressing(20),
      "within the stall window of the last advance at t=5"
    );
    assert!(
      !witness.is_progressing(26),
      "a full stall window (20) past the last advance at t=5"
    );
  }

  /// Before the lookahead point (three quarters of the deadline) the policy just continues.
  #[test]
  fn before_the_lookahead_it_continues() {
    let mut extender = extender();
    let mut witness = ProgressWitness::new(STALL_WINDOW, 0);
    witness.observe(1, 10);
    // Elapsed 70 of a 100 deadline: 70 < 75, below the 3/4 lookahead.
    assert_eq!(
      extender.evaluate(70, &witness, 70),
      ExtensionOutcome::Continue
    );
  }

  /// A progressing operation near its deadline is granted an extension, and the deadline grows.
  #[test]
  fn a_progressing_operation_is_extended() {
    let mut extender = extender();
    let mut witness = ProgressWitness::new(STALL_WINDOW, 0);
    witness.observe(5, 80); // progressed recently

    // Elapsed 80 of 100: past the 3/4 lookahead, and progressing → extend.
    assert_eq!(
      extender.evaluate(80, &witness, 85),
      ExtensionOutcome::Extend {
        deadline_ns: DEADLINE + EXTENSION
      }
    );
    assert_eq!(
      extender.deadline_ns(),
      DEADLINE + EXTENSION,
      "the deadline grew"
    );
    assert_eq!(extender.granted(), 1);
  }

  /// A stalled operation near its deadline is left to expire even though the deadline is close.
  #[test]
  fn a_stalled_operation_is_left_to_expire() {
    let mut extender = extender();
    let mut witness = ProgressWitness::new(STALL_WINDOW, 0);
    witness.observe(5, 10); // last advanced long ago

    // Elapsed 80, but no progress for a full stall window (now 80, last advance 10) → expire.
    assert_eq!(
      extender.evaluate(80, &witness, 80),
      ExtensionOutcome::Expire
    );
    assert_eq!(
      extender.granted(),
      0,
      "no extension granted to a stuck operation"
    );
  }

  /// The extension budget is bounded: after the maximum grants, even a progressing operation expires.
  #[test]
  fn the_extension_budget_is_bounded() {
    let mut extender = extender();
    let mut witness = ProgressWitness::new(STALL_WINDOW, 0);

    let mut now = 80;
    for grant in 1..=MAX_EXTENSIONS {
      witness.observe(u64::from(grant), now); // keep progressing
      let outcome = extender.evaluate(extender.deadline_ns().saturating_sub(1), &witness, now);
      assert!(
        matches!(outcome, ExtensionOutcome::Extend { .. }),
        "grant {grant} extends"
      );
      now += 1;
    }
    // A further evaluation, still progressing, is refused — the budget is spent.
    witness.observe(100, now);
    assert_eq!(
      extender.evaluate(extender.deadline_ns().saturating_sub(1), &witness, now),
      ExtensionOutcome::Expire,
      "a progressing operation past its extension budget still expires"
    );
    assert_eq!(extender.granted(), MAX_EXTENSIONS);
  }
}
