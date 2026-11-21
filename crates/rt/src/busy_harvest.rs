//! How often a busy shard asks its driver for I/O (§4.3 "Loop"; A-218; 85a, g228, 2026-11-21).
//!
//! A shard that stays busy never reaches its park, where the driver is asked for ready I/O, so it asks between steps
//! with a zero-timeout wait, so an I/O-bound task is not starved behind CPU-bound ones. On Linux and Windows that wait
//! returns at once. On macOS a zero-timeout `kevent` that finds nothing ready sleeps 12.3-13.1 µs with an involuntary
//! switch, while one that finds an event ready returns in 206 ns (g228's probe, Studio-4, 2026-11-21; A-218 measured
//! the sleep at 14.6-15.5 µs). The shard asked once a quantum (the wake estimate, 3-8 µs on Apple silicon) had passed
//! since its last wait, and stamped the ask before the ask's own sleep, so on macOS every busy step after the first
//! slept: a live write's exchange hands work between two of the control shard's own tasks twice and paid about 30 µs
//! for it (`fleet.live.hop.exchange.{spawn,wake}` 15.4 µs each at the median, adas-mac-studio-2) against Linux's 6.
//!
//! So a busy shard asks no more often than asking costs ([`busy_harvest_due`]): the interval is the longer of the
//! quantum and the shard's measured empty harvest (its learned idle turn, [`crate::held_turn`]), measured from the
//! harvest's end. Where a harvest is cheap the quantum governs, as before; where it sleeps, a busy shard spends at most
//! half its time asking, and an I/O task waits at most one interval and one harvest. It spins nothing: it only skips
//! asks that would find nothing. Counted (`crate::shard::Counters::harvests_deferred`).
//!
//! Measured beside a user-space ring watch first (main's ruling (c), BENCHMARKS "Measured and rejected: the macOS busy
//! harvest's interval and a user-space ring watch"): the pair lost under contention, and the counters put the quiet win
//! (collect 30 to 10 µs) on this interval and the contended harm on the watch, whose bound grew with the wake estimate.

/// Whether a busy shard should ask its driver for I/O now: the longer of its quantum and its measured harvest cost has
/// passed since its last wait or harvest ended (`since_ns`). A harvest cost of 0 (none measured yet) leaves the quantum.
#[must_use]
pub fn busy_harvest_due(since_ns: u64, now_ns: u64, quantum_ns: u64, harvest_cost_ns: u64) -> bool {
  now_ns.saturating_sub(since_ns) >= quantum_ns.max(harvest_cost_ns)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: a kicked park's wake on Apple silicon (A-387's row), a busy shard's quantum.
  const WAKE_NS: u64 = 5_000;
  /// Shape: an empty zero-timeout `kevent` on Apple silicon (g228's probe), and an empty `epoll_wait` on Linux.
  const NAPPING_HARVEST_NS: u64 = 12_500;
  const POLLING_HARVEST_NS: u64 = 400;

  /// 85a. Do: ask whether a busy harvest is due, with a harvest that sleeps and one that polls, just before and at each
  /// interval, and with no harvest cost measured. Expect: a polling harvest due at the quantum, as before; a sleeping one
  /// only once its own cost has passed; no cost measured, the quantum.
  #[test]
  fn a_busy_shard_asks_its_driver_no_more_often_than_asking_costs() {
    let since = 1_000_000;
    assert!(!busy_harvest_due(
      since,
      since + WAKE_NS - 1,
      WAKE_NS,
      POLLING_HARVEST_NS
    ));
    assert!(busy_harvest_due(
      since,
      since + WAKE_NS,
      WAKE_NS,
      POLLING_HARVEST_NS
    ));
    assert!(!busy_harvest_due(
      since,
      since + WAKE_NS,
      WAKE_NS,
      NAPPING_HARVEST_NS
    ));
    assert!(!busy_harvest_due(
      since,
      since + NAPPING_HARVEST_NS - 1,
      WAKE_NS,
      NAPPING_HARVEST_NS
    ));
    assert!(busy_harvest_due(
      since,
      since + NAPPING_HARVEST_NS,
      WAKE_NS,
      NAPPING_HARVEST_NS
    ));
    assert!(busy_harvest_due(since, since + WAKE_NS, WAKE_NS, 0));
    // A clock read before the stamp (another shard's) is no time busy.
    assert!(!busy_harvest_due(since, since - 1, WAKE_NS, 0));
  }
}
