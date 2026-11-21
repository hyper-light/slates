//! DIAG (84a macOS, never main): how many of the runtime's zero-timeout driver harvests took a millisecond or more, on
//! the busy path (between two tasks) and in the idle spin, the share g222 measured at 9-12% of loaded calls.
#![allow(missing_docs)]
use std::sync::atomic::{AtomicU64, Ordering};

pub static BUSY: AtomicU64 = AtomicU64::new(0);
pub static BUSY_SLOW: AtomicU64 = AtomicU64::new(0);
pub static SPIN: AtomicU64 = AtomicU64::new(0);
pub static SPIN_SLOW: AtomicU64 = AtomicU64::new(0);

/// Format: a millisecond, the slow harvest's threshold (g222's split).
pub const SLOW_NS: u64 = 1_000_000;

/// Counts one harvest that took `ns`, on the busy path or in the spin.
pub fn count(busy: bool, ns: u64) {
  let (all, slow) = if busy {
    (&BUSY, &BUSY_SLOW)
  } else {
    (&SPIN, &SPIN_SLOW)
  };
  all.fetch_add(1, Ordering::Relaxed);
  if ns >= SLOW_NS {
    slow.fetch_add(1, Ordering::Relaxed);
  }
}

/// The counts since the last take, zeroing them: (busy, busy slow, spin, spin slow).
pub fn take() -> (u64, u64, u64, u64) {
  (
    BUSY.swap(0, Ordering::Relaxed),
    BUSY_SLOW.swap(0, Ordering::Relaxed),
    SPIN.swap(0, Ordering::Relaxed),
    SPIN_SLOW.swap(0, Ordering::Relaxed),
  )
}
