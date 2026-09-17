//! What a wait makes of one observation, and the time a wait is charged in — the two pure rules of the
//! fleet harness's polls (`tests/fleet.rs`), kept here so `tests/observe.rs` proves them by use with
//! plain numbers (§4.14; `slates_server::observe`).

use slates_server::observe::ObserveError;

/// What one ask of a wait's condition established.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
  /// The condition holds on observed state.
  Holds,
  /// Observed state on which the condition does not hold yet: the wait continues.
  Observed,
  /// The observation could not be made, for a reason that can clear — a shard starved past the observe
  /// budget, a state momentarily borrowed: the wait continues, paced, and never reads the silence as
  /// the change it waits for.
  Unavailable(ObserveError),
  /// The observation can never be made — the daemon or its shard is gone: the wait ends, naming why.
  Terminal(ObserveError),
}

/// The verdict of one observed condition: what it saw, or why it could not see, sorted by whether
/// asking again could ever answer ([`ObserveError::is_terminal`]).
pub(crate) fn verdict(observed: Result<bool, ObserveError>) -> Verdict {
  match observed {
    Ok(true) => Verdict::Holds,
    Ok(false) => Verdict::Observed,
    Err(refusal) if refusal.is_terminal() => Verdict::Terminal(refusal),
    Err(refusal) => Verdict::Unavailable(refusal),
  }
}

/// The time a wait is charged in: the observed daemons' own coordinator periods. Each daemon's advance
/// is counted from **its own** period count when the wait began, and the wait is charged the **least**
/// advance among them — so a daemon that began far ahead can never pay for one that has stalled. The
/// rule this replaced charged the least *absolute* count's advance, which under unequal starts charged
/// a stalled daemon's wait to whichever peer was behind it in absolute terms and kept ticking.
#[derive(Debug)]
pub(crate) struct ProgressCharge {
  starts: Vec<u64>,
}

impl ProgressCharge {
  /// Begins the charge from each daemon's current period count, in the order the wait observes them.
  pub(crate) fn begin(progress: impl IntoIterator<Item = u64>) -> ProgressCharge {
    ProgressCharge {
      starts: progress.into_iter().collect(),
    }
  }

  /// Each daemon's advance since the wait began, in the same order (a daemon behind its start, a
  /// restarted coordinator, counts as no advance).
  pub(crate) fn each(&self, progress: impl IntoIterator<Item = u64>) -> Vec<u64> {
    self
      .starts
      .iter()
      .zip(progress)
      .map(|(start, now)| now.saturating_sub(*start))
      .collect()
  }

  /// The advance the wait is charged: the least of [`ProgressCharge::each`]; zero with nothing observed.
  pub(crate) fn advanced(&self, progress: impl IntoIterator<Item = u64>) -> u64 {
    self.each(progress).into_iter().min().unwrap_or(0)
  }
}
