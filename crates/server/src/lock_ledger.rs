//! The lock ledger (§4.2 D-12): every strict (`--locked`) volume's entitlement reserved against the process's lock
//! capacity, so admission refuses a strict volume whose content the OS will not let it lock, instead of admitting it
//! and refusing its writes later.
//!
//! A strict volume locks only its own blocks, as it allocates them (`slates_mem::arena::ChunkArena::alloc_locked`), so
//! an empty strict volume locks nothing and its entitlement is a promise about the future. The OS's limit
//! (`RLIMIT_MEMLOCK`, macOS's `vm.user_wire_limit`, a Windows working set) is per process, across every shard, so the
//! promises are counted per daemon, not per shard: one word, checked and acquired on a strict volume's admission (a
//! create, a clone, a takeover, a recovery) and released when its credit drops (a refusal after it, the volume's
//! teardown). A cold path, so a shared atomic word is what CLAUDE.md §3 allows; it is a table indexed by the daemon's
//! control shard, as the doorbell is, because a process may run several daemons (the test suites), and each daemon
//! resets its entry at start, before any shard recovers.
//!
//! Before this, a strict create locked its shard's whole content arena: on macOS, which has no lock-on-fault, a 4 MiB
//! volume wired 16 GiB and stalled its shard about 2 s (measured 2026-10-06,
//! `docs/bugs/2026-10-06-a-locked-volume-wired-its-shards-whole-arena.md`).

use std::sync::atomic::{AtomicU64, Ordering};

/// One daemon's reserved bytes, on its own cache line (two daemons' entries never share one).
#[repr(align(128))]
struct Entry(AtomicU64);

/// Shape: one entry per registry slot, indexed by the daemon's control shard (as `daemon::DOORBELL_RANG`).
static RESERVED: [Entry; slates_rt::registry::MAX_SHARDS] =
  [const { Entry(AtomicU64::new(0)) }; slates_rt::registry::MAX_SHARDS];

fn entry(control: u16) -> Option<&'static AtomicU64> {
  RESERVED.get(usize::from(control)).map(|entry| &entry.0)
}

/// Clears the daemon's entry: called at its start, before any shard recovers and reserves again.
pub(crate) fn reset(control: u16) {
  if let Some(word) = entry(control) {
    word.store(0, Ordering::Release);
  }
}

/// The bytes the daemon whose control shard is `control` has promised its strict volumes.
#[cfg(test)]
pub(crate) fn reserved(control: u16) -> u64 {
  entry(control).map_or(0, |word| word.load(Ordering::Acquire))
}

/// A strict volume's entitlement, held against its daemon's lock capacity until it drops.
#[derive(Debug)]
pub struct LockCredit {
  control: u16,
  bytes: u64,
}

impl LockCredit {
  /// Reserves `bytes` against `capacity` for the daemon whose control shard is `control`, or refuses with the bytes
  /// still available. Checked, never wrapped.
  pub(crate) fn reserve(control: u16, bytes: u64, capacity: u64) -> Result<LockCredit, u64> {
    let word = entry(control).ok_or(0u64)?;
    word
      .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
        held.checked_add(bytes).filter(|after| *after <= capacity)
      })
      .map(|_| LockCredit { control, bytes })
      .map_err(|held| capacity.saturating_sub(held))
  }

  /// The bytes it holds.
  pub fn bytes(&self) -> u64 {
    self.bytes
  }
}

impl Drop for LockCredit {
  fn drop(&mut self) {
    if let Some(word) = entry(self.control) {
      let bytes = self.bytes;
      let _ = word.fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
        Some(held.saturating_sub(bytes))
      });
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: a registry slot no other test in this binary uses as a control shard.
  const CONTROL: u16 = 61;

  /// Do: reserve against a capacity of 100 three times, drop one credit, reserve again. Expect: 60 and 40 fit, a
  /// further 1 is refused with 0 available, dropping the 40 frees exactly 40, a reset clears the entry, and a credit
  /// dropped after the reset leaves it at zero.
  #[test]
  fn credits_fit_the_capacity_and_return_it_when_dropped() {
    reset(CONTROL);
    let first = LockCredit::reserve(CONTROL, 60, 100).unwrap();
    let second = LockCredit::reserve(CONTROL, 40, 100).unwrap();
    assert_eq!(
      LockCredit::reserve(CONTROL, 1, 100).map(|c| c.bytes()),
      Err(0)
    );
    drop(second);
    assert_eq!(reserved(CONTROL), 60);
    assert_eq!(
      LockCredit::reserve(CONTROL, 50, 100).map(|c| c.bytes()),
      Err(40)
    );
    drop(first);
    assert_eq!(reserved(CONTROL), 0);
    let held = LockCredit::reserve(CONTROL, 100, 100).unwrap();
    reset(CONTROL);
    assert_eq!(reserved(CONTROL), 0);
    // A credit outliving its daemon's reset (a slot reused) returns nothing below zero: it saturates, never wraps.
    drop(held);
    assert_eq!(reserved(CONTROL), 0);
  }
}
