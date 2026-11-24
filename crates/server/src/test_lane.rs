//! The lane a test runs in, as the lane declares it: `SLATES_TEST_RESTART_SLOWDOWN`, an integer of at least 1 (1 where
//! it is unset), how many times slower than a developer's debug build the lane's runner goes. CI's plain test lanes
//! declare 10 (a loaded shared runner) and ThreadSanitizer's lane 25 (its checks on every access, 10 to 20 times); the
//! client's restart tests read the same variable (`crates/client/tests/client.rs`, `RESTART_SLOWDOWN`), and the
//! Python SDK's tests too.
//!
//! A test multiplies by it a wait whose length is a bet on the daemon's speed: how long its client waits for an answer
//! before it gives up, or how long it holds off on purpose so the daemon's work meets it. A slow lane is then given the
//! time its own slowness takes, and a fast one is held no longer than before. A bound the test asserts the daemon keeps
//! is never scaled: that is the rule under test.

/// The lane's declared slowdown (the module docs): 1 unless `SLATES_TEST_RESTART_SLOWDOWN` names an integer of at
/// least 1.
pub(crate) fn slowdown() -> u64 {
  std::env::var("SLATES_TEST_RESTART_SLOWDOWN")
    .ok()
    .and_then(|value| value.parse::<u64>().ok())
    .filter(|&times| times >= 1)
    .unwrap_or(1)
}
