//! Red-first copy of A-570's sampler test, on main's code.

#[cfg(test)]
mod tests {
  use std::time::{Duration, Instant};

  use crate::daemon::LIVENESS_BUDGET_NS;

  /// Shape: the daemon's own resident memory the test pins (any steady reading: the hold subtracts its growth alone).
  const RESIDENT: u64 = 64 << 20;
  /// Shape: the share of the daemon's capacity the host's available memory falls by: an eighth, as the recovery
  /// suite's later drop.
  const FALL_PARTS: u64 = 8;
  /// Shape: how long the test waits for the first sample to fix the baseline: a window and a half, past the reap loop's
  /// first look a window into the start.
  const BASELINE_WITHIN_NS: u64 = LIVENESS_BUDGET_NS / 2 * 3;
  /// Shape: how long the test looks for the hold after the fall: half a window, short of a tick a window on.
  const HELD_WITHIN_NS: u64 = LIVENESS_BUDGET_NS / 2;
  /// Shape: the pause between the test's looks.
  const LOOK_PAUSE_NS: u64 = 1_000_000;

  /// Looks with `look` until it answers true or `within_ns` passes since `since`: how long that took.
  fn look_until(
    since: Instant,
    within_ns: u64,
    mut look: impl FnMut() -> bool,
  ) -> Option<Duration> {
    loop {
      if look() {
        return Some(since.elapsed());
      }
      if since.elapsed() > Duration::from_nanos(within_ns) {
        return None;
      }
      // The harness paces its looks; the daemon under test waits on its wheel.
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(Duration::from_nanos(LOOK_PAUSE_NS));
    }
  }

  /// A-570 (the pressure hold off the tick). Do: on a one-shard daemon, put the sampler's readings in the host's place
  /// (the daemon's own resident memory, and the host's available memory equal to its capacity), wait for the first
  /// sample to fix the baseline, then let available fall by an eighth of the capacity. Expect: that eighth withheld
  /// within half a liveness window of the fall, by the sampler the reading woke, as the kernel's report would. The reap
  /// loop's tick sampled once a window, and its sample was what fixed the baseline, so the fall waited a window.
  #[test]
  fn a_reported_fall_in_available_memory_is_held_back_without_a_tick() {
    let daemon = crate::daemon::audit_daemon_of(1);
    let capacity = crate::daemon::observe_first(&daemon, |state| state.config.reserve_per_shard);
    daemon.inject_resident_memory(RESIDENT).unwrap();
    daemon.inject_available_memory(capacity).unwrap();
    let baseline = look_until(Instant::now(), BASELINE_WITHIN_NS, || {
      daemon.pressure_baseline() == Ok(Some(capacity))
    });
    let fall = capacity / FALL_PARTS;
    let fell = Instant::now();
    daemon.inject_available_memory(capacity - fall).unwrap();
    let held = look_until(
      fell,
      HELD_WITHIN_NS,
      || matches!(daemon.pressure_withheld(), Ok(Ok(withheld)) if withheld == fall),
    );
    let withheld = daemon.pressure_withheld();
    daemon.stop();
    assert!(baseline.is_some(), "the first sample fixed the baseline");
    let held = held.unwrap_or_else(|| {
      panic!("the fall was held back within half a liveness window: {withheld:?} for {fall}")
    });
    println!("a fall of {fall} bytes in available memory held back {held:?} after it");
  }
}
