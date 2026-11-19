//! A receive worker's spin window, learned from its own wakes (A-560; A-212's rule kept: a worker waiting for the
//! shard's reply spins for as long as a wake costs, then parks, the 2-competitive rule the shards use, §4.3).
//!
//! **What was there (A-212).** Each receive worker timed fifteen one-way channel wakes once as it started and adopted
//! their median for its whole life, with no bound and no check that the fifteen agreed. A noisy start set it high (CI's
//! arm64 runner: 143.9 and 108.1 µs, against wakes of 4.1 to 11.5 µs), and the worker then spun that long on every slow
//! reply, a core taken from the shards for as long as the mount lived.
//!
//! **The estimate.** The window is learned from the worker's own wakes, each reply it parked for timed from the reply's
//! send to the worker running again, and it keeps learning for the worker's life. The estimator is TCP's round-trip
//! estimator (Jacobson 1988, as RFC 6298 §2 states it): a smoothed wake `S` and its smoothed mean deviation `V`, with
//! the RFC's gains `α = 1/8` and `β = 1/4` (§2.3), seeded by the first sample `R` as `S = R`, `V = R/2` (§2.2), kept
//! in Jacobson's fixed point (`S` scaled by 8, `V` by 4) so no update rounds away. A scheduling delay only ever
//! lengthens a wake, so a sample past the RFC's own band, `S + K·V` with `K = 4` (its retransmission bound, §2.3), is a
//! delay rather than a wake and is folded as the band's edge: one descheduled wake moves the estimate a bounded step.
//!
//! **Agreement.** The RFC seeds the deviation at half the first sample, its stand-in for "nothing known of the
//! spread". The samples agree once the deviation has fallen below that (`V < S/2`): later wakes have confirmed the
//! first more closely than the RFC assumes of one sample. Until they agree the window is zero and the worker parks at
//! once, which costs at most one wake a reply and never a core.
//!
//! **The bound.** The window never exceeds the ceiling the machine allows a spin: the step quantum of the shard that
//! started the mount, which the daemon derives from the machine's measured wake (`slates_rt::futures::step_budget_ns`).
//! A spin longer than a shard's step takes more than a step of CPU from the shards; a worker started off any shard has
//! no quantum and never spins.

/// The spin window a receive worker learns from its own wakes (the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpinWindow {
  /// The smoothed wake, scaled by [`SMOOTHED_SCALE`] (Jacobson's fixed point); `None` before the first wake.
  smoothed: Option<u128>,
  /// The smoothed mean deviation, scaled by [`DEVIATION_SCALE`].
  deviation: u128,
  /// The most the machine allows a spin, nanoseconds.
  ceiling_ns: u64,
  /// Wakes folded in, the non-vacuity count of the learning.
  samples: u64,
}

/// Format: the smoothed wake's fixed-point scale, `1/α` for RFC 6298's `α = 1/8` (§2.3).
const SMOOTHED_SCALE: u128 = 8;
/// Format: the deviation's fixed-point scale, `1/β` for RFC 6298's `β = 1/4` (§2.3).
const DEVIATION_SCALE: u128 = 4;
/// Format: RFC 6298's `K`, the deviations past the smoothed value at which its retransmission bound stands (§2.3).
const BAND_DEVIATIONS: u128 = 4;
/// Format: the divisor of the first sample that seeds the deviation, `V = R/2` (RFC 6298 §2.2), and so the share of
/// the smoothed wake the deviation must fall below before the samples agree.
const SEED_DIVISOR: u128 = 2;

impl SpinWindow {
  /// A window that has learned nothing, under a ceiling of `ceiling_ns` (the shard's step quantum; zero never spins).
  pub const fn new(ceiling_ns: u64) -> Self {
    Self {
      smoothed: None,
      deviation: 0,
      ceiling_ns,
      samples: 0,
    }
  }

  /// Folds in one wake the worker paid: a reply it parked for, timed from its send to the worker running again.
  pub fn record(&mut self, wake_ns: u64) {
    let sample = u128::from(wake_ns);
    self.samples = self.samples.saturating_add(1);
    let Some(smoothed) = self.smoothed else {
      self.smoothed = Some(sample.saturating_mul(SMOOTHED_SCALE));
      self.deviation = sample
        .checked_div(SEED_DIVISOR)
        .unwrap_or(0)
        .saturating_mul(DEVIATION_SCALE);
      return;
    };
    let wake = smoothed.checked_div(SMOOTHED_SCALE).unwrap_or(0);
    let spread = self.deviation.checked_div(DEVIATION_SCALE).unwrap_or(0);
    // A delay is not a wake: past the band the sample is its edge.
    let sample = sample.min(wake.saturating_add(spread.saturating_mul(BAND_DEVIATIONS)));
    let error = sample.abs_diff(wake);
    // V ← V + (|R − S| − V)·β and S ← S + (R − S)·α, in the scaled forms.
    self.deviation = self
      .deviation
      .saturating_sub(self.deviation.checked_div(DEVIATION_SCALE).unwrap_or(0))
      .saturating_add(error);
    self.smoothed = Some(if sample >= wake {
      smoothed.saturating_add(error)
    } else {
      smoothed.saturating_sub(error)
    });
  }

  /// Whether the wakes folded in agree: the deviation has fallen below the share of the smoothed wake the RFC seeds it
  /// at (`V < S/2`).
  pub fn agreed(&self) -> bool {
    self.smoothed.is_some_and(|smoothed| {
      let wake = smoothed.checked_div(SMOOTHED_SCALE).unwrap_or(0);
      let spread = self.deviation.checked_div(DEVIATION_SCALE).unwrap_or(0);
      spread.saturating_mul(SEED_DIVISOR) < wake
    })
  }

  /// How long to spin before parking, nanoseconds: the smoothed wake once the wakes agree, never past the ceiling, and
  /// zero before they agree.
  pub fn window_ns(&self) -> u64 {
    match self.smoothed {
      Some(smoothed) if self.agreed() => {
        let wake = smoothed.checked_div(SMOOTHED_SCALE).unwrap_or(0);
        u64::try_from(wake).unwrap_or(u64::MAX).min(self.ceiling_ns)
      }
      _ => 0,
    }
  }

  /// The wakes folded in since the worker started.
  pub fn samples(&self) -> u64 {
    self.samples
  }
}

#[cfg(test)]
mod tests {
  use super::SpinWindow;

  /// Format: nanoseconds a microsecond.
  const MICROS: u64 = 1_000;
  /// Shape: the scenes' ceiling, a shard step quantum of 50 µs.
  const CEILING: u64 = 50 * MICROS;

  /// A-560, B4 (CI's arm64 runner, diagnostics run 42178889880 run 3: the window came out 143.9 and 108.1 µs against
  /// wakes of 4.1 to 11.5 µs). Do: fold a start in which most wakes were delayed (five of nine past 100 µs), then the
  /// worker's wakes as they settle, 4 to 11 µs. Expect the window never past the ceiling after any wake, zero while the
  /// wakes disagree, and once they agree within the settled wakes' range. The median of the start, the old rule's
  /// window, was 120 µs, spun on every slow reply for the worker's life.
  #[test]
  fn a_start_of_delayed_wakes_never_spins_past_the_ceiling_and_settles_on_the_wakes() {
    let mut window = SpinWindow::new(CEILING);
    let start = [144, 108, 9, 130, 120, 6, 140, 11, 125];
    let settled = [4, 7, 11, 5, 9, 8, 6, 10, 7, 5, 9, 11, 4, 8, 6];
    for (index, wake) in start
      .iter()
      .chain(settled.iter().cycle().take(90))
      .enumerate()
    {
      window.record(wake * MICROS);
      assert!(
        window.window_ns() <= CEILING,
        "after wake {index}: a window of {} ns, past the ceiling: {window:?}",
        window.window_ns()
      );
      assert!(
        window.window_ns() == 0 || window.agreed(),
        "after wake {index}: a window spun before the wakes agreed: {window:?}"
      );
    }
    assert!(window.agreed(), "the settled wakes agree: {window:?}");
    let learned = window.window_ns();
    assert!(
      (4 * MICROS..=11 * MICROS).contains(&learned),
      "the window settles within the wakes: {learned} ns, {window:?}"
    );
    assert_eq!(window.samples(), 99, "every wake was folded in");
  }

  /// A-560: wakes that agree are spun for. Do: fold eight wakes of 8 µs each. Expect the window 8 µs once a second wake
  /// has confirmed the first, and zero before.
  #[test]
  fn wakes_that_agree_are_spun_for() {
    let mut window = SpinWindow::new(CEILING);
    assert_eq!(window.window_ns(), 0, "no wake, no spin");
    window.record(8 * MICROS);
    assert_eq!(window.window_ns(), 0, "one wake is not yet agreement");
    for _ in 0..7 {
      window.record(8 * MICROS);
    }
    assert!(window.agreed());
    assert_eq!(window.window_ns(), 8 * MICROS);
  }

  /// A-560, the bound. Do: fold agreeing wakes slower than the ceiling (300 µs each), and agreeing wakes under a ceiling
  /// of zero (a worker started off any shard). Expect the window at the ceiling, and zero.
  #[test]
  fn a_window_never_exceeds_the_ceiling_the_machine_allows() {
    let mut slow = SpinWindow::new(CEILING);
    let mut off_any_shard = SpinWindow::new(0);
    for _ in 0..8 {
      slow.record(300 * MICROS);
      off_any_shard.record(8 * MICROS);
    }
    assert!(slow.agreed() && off_any_shard.agreed());
    assert_eq!(slow.window_ns(), CEILING);
    assert_eq!(off_any_shard.window_ns(), 0);
  }

  /// A-560, a delay is not a wake. Do: settle on 8 µs wakes, then fold one wake descheduled for 10 ms, then 8 µs wakes
  /// again. Expect the window to move by no more than the band allows (an eighth of four deviations), and to come back
  /// within the wakes once they agree again: one delay neither sets the window nor holds it.
  #[test]
  fn one_descheduled_wake_moves_the_window_a_bounded_step() {
    let mut window = SpinWindow::new(CEILING);
    for wake in [8, 7, 9, 8, 8, 7, 9, 8] {
      window.record(wake * MICROS);
    }
    let before = window.window_ns();
    window.record(10_000 * MICROS);
    assert!(
      window.window_ns() <= before.saturating_add(2 * MICROS),
      "one 10 ms delay moved the window from {before} ns to {} ns: {window:?}",
      window.window_ns()
    );
    for _ in 0..40 {
      window.record(8 * MICROS);
    }
    assert!(window.agreed());
    assert!(
      (7 * MICROS..=9 * MICROS).contains(&window.window_ns()),
      "{window:?}"
    );
  }
}
