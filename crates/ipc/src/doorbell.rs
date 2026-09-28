//! The doorbell protocol between a client and the daemon shard serving it (§4.7 "Wake strategy"): the
//! client rings the doorbell only while the shard has announced it is going idle, so a request to a
//! polling shard costs no syscall. The client publishes its request into the command ring, then reads the
//! announcement; the shard announces, then re-checks the rings before idling. Each side writes and then
//! reads — the store-buffering shape — so each side must fence with `SeqCst` between its write and its
//! read, or both reads can miss: the client sees no announcement and does not ring, the shard sees no
//! request and idles, and the request waits for whatever wakes the shard next (a timer, another client).
//!
//! This is the runtime's kick-if-parked rule (`slates_rt::parking`, whose module doc states the C++20
//! fence–fence rule [B: `[atomics.order]`] and why a `SeqCst` store and load alone are not enough) applied
//! to the other cross-thread wake the daemon has. Until 2026-09-28 this side used `Release`/`Acquire` only,
//! and the shard's last look at the client rings came *before* the runtime's own fence (the runtime's
//! post-fence re-check sees only its own inboxes): x86 realizes the lost wake through its store buffer,
//! arm64 orders a store-release before a later load-acquire and does not
//! (`docs/bugs/2026-09-28-a-client-request-waited-for-a-timer-after-a-lost-doorbell.md`). The two halves
//! below are the only place the fences live; the client (`ClientEnd::send`) and the daemon's serve loop
//! call them, and the loom model at the end drives these same functions.

#[cfg(loom)]
use loom::sync::atomic::{Ordering, fence};
#[cfg(not(loom))]
use std::sync::atomic::{Ordering, fence};

/// The client's half, called right after the request is written into the command ring: whether the
/// shard has announced it is going idle (so the doorbell must be rung). The fence orders the publication
/// before the read of the announcement in the one total order the shard's fence also takes part in.
pub fn after_publishing_idle_announced(announced: impl FnOnce() -> bool) -> bool {
  fence(Ordering::SeqCst);
  announced()
}

/// The shard's half, called right after it has announced it is going idle: whether a request is pending
/// in its client rings (so the shard must serve, not idle). The fence orders the announcement before the
/// re-check, so a request the re-check misses was published after the fence, and its client sees the
/// announcement and rings.
pub fn after_announcing_idle_pending(pending: impl FnOnce() -> bool) -> bool {
  fence(Ordering::SeqCst);
  pending()
}

// Two attributes rather than `all(test, loom)`: clippy's test-context rule (unwrap allowed in tests)
// recognizes only a bare `cfg(test)`, and an unwrap here is a failed model, as it should be.
#[cfg(test)]
#[cfg(loom)]
mod loom_tests {
  use std::sync::atomic::{AtomicU64 as StdAtomicU64, Ordering as StdOrdering};

  // loom's `Notify` stands in for the doorbell (an eventfd count on Linux, the bootstrap word and its
  // watcher's kick elsewhere): sticky, like the eventfd. Test scaffolding under `cfg(loom)` only (D-8
  // exception 3); the two owners are the model's client and shard.
  use loom::sync::Notify;
  use loom::sync::atomic::AtomicU32;
  use slates_mem::loom_bounds;

  use super::*;

  /// Rings rung across every explored interleaving of the fenced model.
  static RUNG: StdAtomicU64 = StdAtomicU64::new(0);
  /// Interleavings in which the client skipped the doorbell (the shard was polling).
  static SKIPPED: StdAtomicU64 = StdAtomicU64::new(0);
  /// Interleavings in which the shard idled and a ring brought it back.
  static IDLED: StdAtomicU64 = StdAtomicU64::new(0);

  /// The shared words of one model run: the command ring's published depth, the shard's idle
  /// announcement (`daemon_parked`), and the doorbell.
  struct Words {
    depth: AtomicU32,
    announced: AtomicU32,
    bell: Notify,
  }

  fn words() -> &'static Words {
    Box::leak(Box::new(Words {
      depth: AtomicU32::new(0),
      announced: AtomicU32::new(0),
      bell: Notify::new(),
    }))
  }

  /// AC-0.7 (§4.7 "Wake strategy"): with both halves fenced, a request published while the shard goes
  /// idle is never lost — in every interleaving the shard either sees it on its re-check or is rung out
  /// of its idle. Some interleaving rang, some skipped the ring, and some idled, so no half is vacuous.
  #[test]
  fn a_request_published_while_the_shard_goes_idle_is_never_lost() {
    loom_bounds::explore("doorbell: one client against one idling shard", || {
      let words = words();
      let client = loom::thread::spawn(move || {
        words.depth.store(1, Ordering::Release);
        let ring = after_publishing_idle_announced(|| words.announced.load(Ordering::Relaxed) != 0);
        if ring {
          RUNG.fetch_add(1, StdOrdering::Relaxed);
          words.bell.notify();
        } else {
          SKIPPED.fetch_add(1, StdOrdering::Relaxed);
        }
      });
      // The serve loop: serve if a request is there; else announce, re-check, and idle until rung.
      loop {
        if words.depth.load(Ordering::Acquire) != 0 {
          break;
        }
        words.announced.store(1, Ordering::Relaxed);
        let pending = after_announcing_idle_pending(|| words.depth.load(Ordering::Acquire) != 0);
        if !pending {
          IDLED.fetch_add(1, StdOrdering::Relaxed);
          words.bell.wait();
        }
        words.announced.store(0, Ordering::Relaxed);
      }
      client.join().unwrap();
    });
    assert!(
      RUNG.load(StdOrdering::Relaxed) > 0,
      "some interleaving rang"
    );
    assert!(
      SKIPPED.load(StdOrdering::Relaxed) > 0,
      "some interleaving skipped the ring"
    );
    assert!(
      IDLED.load(StdOrdering::Relaxed) > 0,
      "some interleaving idled"
    );
  }

  /// The protocol as it stood before 2026-09-28, kept as the model's witness that it can see the bug: the
  /// client published with `Release` and read the announcement with `Acquire`, the shard announced with
  /// `Release` and re-checked with `Acquire`, no fence on either side. loom finds the interleaving in
  /// which both reads miss and the shard idles with the request in its ring and no ring coming — a
  /// deadlock. If this model ever passes, the model has lost its power to see the lost wake.
  #[test]
  #[should_panic(expected = "deadlock")]
  fn the_unfenced_protocol_loses_a_wake() {
    loom_bounds::explore("doorbell: the unfenced protocol (must fail)", || {
      let words = words();
      let client = loom::thread::spawn(move || {
        words.depth.store(1, Ordering::Release);
        if words.announced.load(Ordering::Acquire) != 0 {
          words.bell.notify();
        }
      });
      loop {
        if words.depth.load(Ordering::Acquire) != 0 {
          break;
        }
        words.announced.store(1, Ordering::Release);
        if words.depth.load(Ordering::Acquire) == 0 {
          words.bell.wait();
        }
        words.announced.store(0, Ordering::Release);
      }
      client.join().unwrap();
    });
  }
}
