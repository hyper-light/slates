//! A windowed maximum filter (draft-ietf-ccwg-bbr-06 §5.5.5, which names Kathleen Nichols' algorithm as
//! implemented in Linux's `lib/win_minmax.c` [C]): the maximum of a stream of samples over the most recent
//! `window` units of a caller-chosen clock (a round count, or BBR's ProbeBW cycle count), in constant space
//! — three samples, the best, second-best and third-best in successive sub-windows — so the estimate
//! forgets an old peak once it ages out, without keeping every sample.

/// Format: Linux `minmax_subwin_update` — a second choice is taken after a quarter of the window.
const QUARTER: u64 = 4;

/// One kept sample: its value and the time it was taken.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Sample {
  time: u64,
  value: u64,
}

/// The windowed maximum filter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowedMax {
  samples: [Sample; 3],
}

impl WindowedMax {
  /// A filter holding `value` at `time` (Linux `minmax_reset`).
  pub fn new(time: u64, value: u64) -> WindowedMax {
    let sample = Sample { time, value };
    WindowedMax {
      samples: [sample; 3],
    }
  }

  /// The current windowed maximum.
  pub fn get(&self) -> u64 {
    self.samples[0].value
  }

  /// Folds in `value` taken at `time`, keeping the maximum over the last `window` time units, and returns
  /// the new maximum (Linux `minmax_running_max`).
  pub fn update(&mut self, time: u64, window: u64, value: u64) -> u64 {
    let sample = Sample { time, value };
    if sample.value >= self.samples[0].value
      || sample.time.saturating_sub(self.samples[2].time) > window
    {
      // A new maximum, or nothing left in the window: start over.
      *self = WindowedMax::new(time, value);
      return self.get();
    }
    if sample.value >= self.samples[1].value {
      self.samples[1] = sample;
      self.samples[2] = sample;
    } else if sample.value >= self.samples[2].value {
      self.samples[2] = sample;
    }
    self.subwin_update(window, sample)
  }

  /// Linux `minmax_subwin_update`: ages the kept samples through the window's quarters and halves, so the
  /// second and third samples are fresh candidates when the best one expires.
  fn subwin_update(&mut self, window: u64, sample: Sample) -> u64 {
    let dt = sample.time.saturating_sub(self.samples[0].time);
    if dt > window {
      // The best sample expired: promote the second and third, and take the new one as the third.
      self.samples[0] = self.samples[1];
      self.samples[1] = self.samples[2];
      self.samples[2] = sample;
      if sample.time.saturating_sub(self.samples[0].time) > window {
        self.samples[0] = self.samples[1];
        self.samples[1] = self.samples[2];
        self.samples[2] = sample;
      }
    } else if self.samples[1].time == self.samples[0].time && dt > window / QUARTER {
      // A quarter of the window passed with no second choice: take one.
      self.samples[1] = sample;
      self.samples[2] = sample;
    } else if self.samples[2].time == self.samples[1].time && dt > window / 2 {
      // Half the window passed with no third choice: take one.
      self.samples[2] = sample;
    }
    self.samples[0].value
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A larger sample becomes the maximum at once; a smaller one does not displace it while it is in the
  /// window.
  #[test]
  fn the_maximum_holds_within_the_window() {
    let mut filter = WindowedMax::new(0, 10);
    assert_eq!(filter.update(1, 10, 50), 50);
    assert_eq!(filter.update(2, 10, 20), 50);
    assert_eq!(filter.update(9, 10, 30), 50);
  }

  /// Once the maximum ages past the window, the best of the later samples takes over — the filter forgets
  /// an old peak rather than holding it forever.
  #[test]
  fn an_old_maximum_expires() {
    let mut filter = WindowedMax::new(0, 10);
    filter.update(1, 10, 100);
    filter.update(4, 10, 40);
    filter.update(7, 10, 30);
    assert_eq!(
      filter.update(12, 10, 20),
      40,
      "100 (t=1) aged out; 40 (t=4) is the best left"
    );
    assert_eq!(filter.update(15, 10, 20), 30, "40 aged out too");
  }

  /// With a window of two units (BBR's max-bandwidth filter over two probe cycles), a peak from two cycles
  /// ago is forgotten when the third cycle's sample arrives.
  #[test]
  fn a_two_cycle_window_forgets_the_third_cycle_back() {
    let mut filter = WindowedMax::new(0, 0);
    filter.update(0, 2, 1000);
    filter.update(1, 2, 600);
    filter.update(2, 2, 500);
    assert_eq!(
      filter.get(),
      1000,
      "cycles 0–2 are within two units of cycle 2"
    );
    assert_eq!(filter.update(3, 2, 400), 600, "cycle 0 aged out");
  }
}
