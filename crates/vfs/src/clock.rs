//! Time for the volume (§4.4) and host supervision (§2.6): a monotonic clock for the journal and a wall clock for POSIX timestamps,
//! behind one trait so the simulation drives both deterministically and the tests never depend
//! on the host's clock.

/// A source of time.
pub trait Clock {
  /// Monotonic nanoseconds.
  fn monotonic_ns(&mut self) -> u64;
  /// Wall-clock nanoseconds since the Unix epoch, for `atime`, `mtime`, `ctime` and `btime`.
  fn wall_ns(&mut self) -> i64;
}

/// The host's clocks. Monotonic readings share one OS boot/time namespace across instances,
/// shards and processes; warm recovery preserves deadlines. They cannot be compared across hosts.
#[derive(Debug)]
pub struct HostClock {}

impl HostClock {
  /// A clock using the host's common monotonic origin.
  pub fn new() -> Self {
    Self {}
  }
}

impl Default for HostClock {
  fn default() -> Self {
    Self::new()
  }
}

impl Clock for HostClock {
  fn monotonic_ns(&mut self) -> u64 {
    slates_machine::clock::monotonic_ns()
  }

  fn wall_ns(&mut self) -> i64 {
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
      .unwrap_or(0)
  }
}

/// A deterministic clock that advances by a fixed step per read, for models and simulations.
#[derive(Debug, Clone)]
pub struct StepClock {
  now: u64,
  step: u64,
}

impl StepClock {
  /// A clock starting at `start` that advances `step` nanoseconds per read.
  pub const fn new(start: u64, step: u64) -> Self {
    Self { now: start, step }
  }
}

impl Clock for StepClock {
  fn monotonic_ns(&mut self) -> u64 {
    self.now = self.now.saturating_add(self.step);
    self.now
  }

  fn wall_ns(&mut self) -> i64 {
    i64::try_from(self.monotonic_ns()).unwrap_or(i64::MAX)
  }
}
