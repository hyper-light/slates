//! When a busy shard asks its driver for I/O, and how long an idle shard watches its rings in user space before it
//! asks (§4.3 "Loop"; A-218; 85a item 3, main's ruling (c), 2026-11-21).
//!
//! A shard asks its driver for ready I/O with a zero-timeout wait: between busy steps, so an I/O-bound task is not
//! starved behind CPU-bound ones, and on every turn of its idle spin. On Linux and Windows that wait returns at once. On
//! macOS a zero-timeout `kevent` that finds nothing ready sleeps 12.3-13.1 µs with an involuntary switch, while one
//! that finds an event ready returns in 206 ns (g228's probe, Studio-4, 2026-11-21; A-218 measured the sleep at
//! 14.6-15.5 µs). Two rules follow, neither a new system call:
//!
//! - **A busy shard asks no more often than asking costs** ([`busy_harvest_due`]). The busy harvest ran once a quantum
//!   (the wake estimate, 3-8 µs on Apple silicon) had passed since the last wait, and the stamp was taken before the
//!   harvest's own sleep, so on macOS every busy step after the first slept: a live write's exchange hands work between
//!   two of the control shard's own tasks twice, and paid about 30 µs for it (`fleet.live.hop.exchange.{spawn,wake}`
//!   15.4 µs each at the median, adas-mac-studio-2, 2026-11-21) against Linux's 6. Now the interval is the longer of the
//!   quantum and the harvest's own measured cost (the shard's learned idle turn, [`crate::held_turn`]), measured from the
//!   harvest's end. Where a harvest is cheap (Linux, Windows: well under a microsecond) the quantum governs, as before;
//!   where it sleeps, a busy shard spends at most half its time asking, and an I/O task waits at most one interval and
//!   one harvest.
//! - **An idle shard watches its rings in user space for what blocking costs, then asks as before**
//!   ([`spin_keeps_core`]). A message from another shard is an atomic word the shard can see without the driver; a
//!   shard in its idle spin saw one only between `kevent` sleeps, which is most of the macOS excess of a live write's
//!   hop to its control shard (22-31 µs at the median against Linux's 8-9). Each idle moment begins by looking at the
//!   rings and pollers alone for at most the shard's measured park and wake, the cost of blocking; spinning up to that
//!   cost and then blocking is within twice the optimal waiting cost, whatever the wait turns out to be [A: Karlin, Li,
//!   Manasse and Owicki, "Empirical studies of competitive spinning for a shared-memory multiprocessor", SOSP 1991].
//!   Then the spin goes on exactly as it did, driver turns included, and still ends in a park.
//!
//! Counted (`crate::shard::Counters`): busy harvests deferred by the interval, user-space watches that caught a message
//! and those that ran out of their bound, and the nanoseconds spent watching: the CPU the watch costs, against the wait
//! it saves.

/// Whether a busy shard should ask its driver for I/O now: the longer of its quantum and its measured harvest cost has
/// passed since its last wait or harvest ended (`since_ns`). A harvest cost of 0 (none measured yet) leaves the quantum.
#[must_use]
pub fn busy_harvest_due(since_ns: u64, now_ns: u64, quantum_ns: u64, harvest_cost_ns: u64) -> bool {
  now_ns.saturating_sub(since_ns) >= quantum_ns.max(harvest_cost_ns)
}

/// Whether an idle moment that began at `watch_began_ns` may still watch its rings in user space at `now_ns`: while it
/// is within what blocking costs, the shard's measured park and wake (`block_cost_ns`), the competitive bound.
#[must_use]
pub fn spin_keeps_core(watch_began_ns: u64, now_ns: u64, block_cost_ns: u64) -> bool {
  now_ns.saturating_sub(watch_began_ns) < block_cost_ns
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: a kicked park's wake on Apple silicon (A-387's row), a busy shard's quantum and an idle watch's bound.
  const WAKE_NS: u64 = 5_000;
  /// Shape: an empty zero-timeout `kevent` on Apple silicon (g228's probe), and an empty `epoll_wait` on Linux.
  const NAPPING_HARVEST_NS: u64 = 12_500;
  const POLLING_HARVEST_NS: u64 = 400;

  /// 85a item 3. Do: ask whether a busy harvest is due, with a harvest that sleeps and one that polls, just before and at
  /// each interval, and with no harvest cost measured. Expect: a polling harvest due at the quantum, as before; a
  /// sleeping one only once its own cost has passed; no cost measured, the quantum.
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

  /// 85a item 3, the competitive bound. Do: ask a watch at its start, just inside the bound, at it and past it, and with
  /// no cost to block measured. Expect: watching only inside the bound, and not at all with nothing to weigh it against.
  #[test]
  fn an_idle_shard_watches_its_rings_no_longer_than_blocking_costs() {
    let began = 1_000_000;
    assert!(spin_keeps_core(began, began, WAKE_NS));
    assert!(spin_keeps_core(began, began + WAKE_NS - 1, WAKE_NS));
    assert!(!spin_keeps_core(began, began + WAKE_NS, WAKE_NS));
    assert!(!spin_keeps_core(began, began + 10 * WAKE_NS, WAKE_NS));
    assert!(spin_keeps_core(began, began - 1, WAKE_NS));
    assert!(!spin_keeps_core(began, began, 0));
  }
}
