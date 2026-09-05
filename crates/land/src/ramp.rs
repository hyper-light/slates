//! The online concurrency ramp of a landing (§4.15 step 7): the in-flight depth starts at the
//! pool's cores and doubles while measured throughput rises by more than its variance and the
//! per-entry latency p99 stays within the previous step's by the measured variance fraction
//! (Little's law applied online); it backs off otherwise. The policy is pure and tested here;
//! the writer applies it once the runtime runs entries concurrently (Phase 2), and until then
//! every landing runs with a depth of one and records what the policy would have chosen.

/// One step's samples: entries completed, their wall time, and the latency p99.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepSample {
  /// Entries completed in the step.
  pub entries: u64,
  /// The step's wall time in nanoseconds.
  pub wall_ns: u64,
  /// The per-entry latency p99 in nanoseconds.
  pub p99_ns: u64,
}

impl StepSample {
  /// Entries per second.
  fn throughput(&self) -> u64 {
    /// Format: nanoseconds per second.
    const NS_PER_S: u64 = 1_000_000_000;
    self.entries.saturating_mul(NS_PER_S) / self.wall_ns.max(1)
  }
}

/// The ramp's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ramp {
  /// The current in-flight depth.
  pub depth: u32,
  /// The most depth the pool can use.
  pub max_depth: u32,
  /// The previous step's throughput and p99, once one ran.
  previous: Option<(u64, u64)>,
  /// Measured: the variance fraction of throughput and latency samples in parts per thousand,
  /// from the landing's own samples (the caller measures; a step's change inside it is noise).
  variance_permille: u64,
  settled: bool,
}

impl Ramp {
  /// A ramp starting at the pool's cores.
  pub fn new(cores: u32, max_depth: u32, variance_permille: u64) -> Self {
    Self {
      depth: cores.clamp(1, max_depth.max(1)),
      max_depth: max_depth.max(1),
      previous: None,
      variance_permille,
      settled: false,
    }
  }

  /// Feeds one step's samples; returns the depth for the next step.
  pub fn observe(&mut self, sample: StepSample) -> u32 {
    /// Format: parts per thousand.
    const PERMILLE: u64 = 1000;
    let throughput = sample.throughput();
    let Some((last_throughput, last_p99)) = self.previous else {
      self.previous = Some((throughput, sample.p99_ns));
      self.depth = (self.depth * 2).min(self.max_depth);
      return self.depth;
    };
    let rose = throughput
      .saturating_sub(last_throughput)
      .saturating_mul(PERMILLE)
      > last_throughput.saturating_mul(self.variance_permille);
    let latency_held = sample.p99_ns.saturating_mul(PERMILLE)
      <= last_p99.saturating_mul(PERMILLE.saturating_add(self.variance_permille));
    if rose && latency_held && self.depth < self.max_depth && !self.settled {
      self.depth = (self.depth * 2).min(self.max_depth);
    } else if !rose || !latency_held {
      self.depth = (self.depth / 2).max(1);
      self.settled = true;
    }
    self.previous = Some((throughput, sample.p99_ns));
    self.depth
  }

  /// Whether the ramp found its plateau.
  pub fn settled(&self) -> bool {
    self.settled
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_ramp_doubles_while_throughput_rises_and_backs_off_at_the_plateau() {
    let mut ramp = Ramp::new(4, 64, 50);
    let step = |entries: u64, wall_ns: u64, p99_ns: u64| StepSample {
      entries,
      wall_ns,
      p99_ns,
    };
    assert_eq!(ramp.observe(step(100, 1_000_000, 10_000)), 8);
    assert_eq!(
      ramp.observe(step(190, 1_000_000, 10_200)),
      16,
      "throughput nearly doubled"
    );
    assert_eq!(ramp.observe(step(350, 1_000_000, 10_400)), 32);
    // The plateau: no rise, latency up.
    assert_eq!(ramp.observe(step(355, 1_000_000, 20_000)), 16);
    assert!(ramp.settled());
    assert_eq!(
      ramp.observe(step(700, 1_000_000, 10_000)),
      16,
      "settled: no more doubling"
    );
  }
}
