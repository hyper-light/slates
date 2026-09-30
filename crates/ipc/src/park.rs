//! The park protocol between a client waiting for its reply and the daemon shard that writes it (§4.7
//! "Wake strategy"; D-19): the daemon wakes a client only while it is parked, so a reply to a spinning
//! client costs no syscall. Each side writes and then reads. The client raises its parked flag and then
//! re-checks its reply ring; the daemon publishes the reply and then reads the flag. That is the
//! store-buffering shape the doorbell has (`crate::doorbell`), so each side fences with `SeqCst` between its
//! write and its read, or both reads can miss: the client finds no reply and waits, and the daemon finds no
//! parked client and wakes no one.
//!
//! Until 2026-09-29 this side used `Release`/`Acquire` only, as the doorbell did until the day before. And
//! the async SDKs' bridges (macOS: a thread on the wake word's `__ulock`; Windows: one on the region's
//! Event) were edge-triggered on the wake word: a change they saw while the client was not yet armed was
//! used up, so a later wake by a daemon that had seen the client parked found "no change" and nudged
//! nothing. The Python async lifecycle hung on the macOS runner (`cead594`, past 1 h 47 min), and here 1 of
//! 320 runs under contention hung: the client's event loop in `kevent`, its bridge in `__ulock_wait2`,
//! the shard parked (docs/bugs/2026-09-29-an-async-client-waited-forever-for-a-reply-that-had-landed.md).
//!
//! The rule now:
//! - **The client** raises its flag, calls [`after_arming`], then re-checks its ring: the sync `wait`, and
//!   the async SDKs' pump after `arm_async`.
//! - **The daemon** publishes the reply and advances the wake word, then asks [`after_publishing_armed`].
//!   When the client is armed, it advances the word once more and wakes. The second advance lets a wake
//!   that lands while a bridge is between two waits reach its next wait, which is checked against the
//!   word's value.
//! - **A bridge**, after every return from its wait, asks [`reply_waiting_for_armed`]: nudge while the
//!   client is armed and a reply waits in its ring. That is a level, never an edge, so nothing is used up.
//!
//! The loom model at the end drives these functions with a client, a daemon and a bridge, and keeps the
//! old protocol as its witness that it can see the lost wake.

#[cfg(loom)]
use loom::sync::atomic::{Ordering, fence};
#[cfg(not(loom))]
use std::sync::atomic::{Ordering, fence};

/// The client's half, called right after it raised its parked flag: every read it makes after this (the
/// re-check of its reply ring, the wake word's value) is ordered after the flag in the one total order the
/// daemon's fence also takes part in. So a reply the re-check misses was published after the daemon's
/// fence, and the daemon sees the flag and wakes.
pub fn after_arming() {
  fence(Ordering::SeqCst);
}

/// The daemon's half, called right after it published a reply and advanced the wake word: whether the
/// client is parked (so it must be woken). The fence orders the publication before the read of the flag,
/// so a client this read finds unparked re-checks after its own fence and finds the reply.
pub fn after_publishing_armed(armed: impl FnOnce() -> bool) -> bool {
  fence(Ordering::SeqCst);
  armed()
}

/// A bridge's check after each return from its wait, whether woken or timed out: whether to nudge its
/// event loop — the client is armed and a reply waits in its ring. It is a level, judged afresh each time,
/// so a reply seen while the client was not yet armed is never used up. The fence orders the bridge's read
/// of the wake word (which the daemon advanced after its own fence) before these reads.
pub fn reply_waiting_for_armed(
  armed: impl FnOnce() -> bool,
  waiting: impl FnOnce() -> bool,
) -> bool {
  fence(Ordering::SeqCst);
  armed() && waiting()
}

// Two attributes rather than `all(test, loom)`: clippy's test-context rule (unwrap allowed in tests)
// recognizes only a bare `cfg(test)`, and an unwrap here is a failed model, as it should be.
#[cfg(test)]
#[cfg(loom)]
mod loom_tests {
  use std::sync::atomic::{AtomicU64 as StdAtomicU64, Ordering as StdOrdering};

  // loom's `Notify` stands in for the bridge's wait on the wake word (`__ulock`, the Windows Event) and for
  // the completion fd the bridge makes readable: both sticky, as the value-checked wait is for a wake that
  // follows an advance of the word. Test scaffolding under `cfg(loom)` only (D-8 exception 3); the owners
  // are the model's client, daemon and bridge.
  use loom::sync::Notify;
  use loom::sync::atomic::AtomicU32;
  use slates_mem::loom_bounds;

  use super::*;

  /// Replies taken during the spin, before any park.
  static SPUN: StdAtomicU64 = StdAtomicU64::new(0);
  /// Replies the client's own re-check after arming found.
  static RECHECKED: StdAtomicU64 = StdAtomicU64::new(0);
  /// Replies the bridge's nudge brought to the parked client.
  static NUDGED: StdAtomicU64 = StdAtomicU64::new(0);

  /// The shared words of one model run: the reply ring's depth, the client's parked flag, the wake word,
  /// the bridge's stop, the wake the bridge waits on, and the completion fd its nudge makes readable.
  struct Words {
    depth: AtomicU32,
    armed: AtomicU32,
    word: AtomicU32,
    stop: AtomicU32,
    bell: Notify,
    pipe: Notify,
  }

  fn words() -> &'static Words {
    Box::leak(Box::new(Words {
      depth: AtomicU32::new(0),
      armed: AtomicU32::new(0),
      word: AtomicU32::new(0),
      stop: AtomicU32::new(0),
      bell: Notify::new(),
      pipe: Notify::new(),
    }))
  }

  /// The bridge's value-checked wait: returns at once when the word has moved past `seen`, else sleeps
  /// until rung.
  fn wait_past(words: &Words, seen: u32) {
    if words.word.load(Ordering::Acquire) == seen {
      words.bell.wait();
    }
  }

  /// The client, after its request: the spin's look, then a park — raise the flag, re-check, and wait on
  /// the completion fd if the re-check missed — and at the end the bridge stopped.
  fn client(words: &Words, fenced: bool) {
    if words.depth.load(Ordering::Acquire) != 0 {
      SPUN.fetch_add(1, StdOrdering::Relaxed);
    } else {
      words.armed.store(1, Ordering::Release);
      if fenced {
        after_arming();
      }
      if words.depth.load(Ordering::Acquire) != 0 {
        RECHECKED.fetch_add(1, StdOrdering::Relaxed);
      } else {
        // The SDK's pump re-checks the ring on every readable event and waits on when it finds nothing,
        // as it must: the fd (and loom's `Notify`) can wake without a reply.
        loop {
          words.pipe.wait();
          if words.depth.load(Ordering::Acquire) != 0 {
            break;
          }
        }
        NUDGED.fetch_add(1, StdOrdering::Relaxed);
      }
      words.armed.store(0, Ordering::Release);
    }
    words.stop.store(1, Ordering::Release);
    words.word.fetch_add(1, Ordering::AcqRel);
    words.bell.notify();
  }

  /// AC-0.7 (§4.7 "Wake strategy", D-19): with the client and daemon fenced and the bridge level-triggered,
  /// a reply published while the client parks is never lost — in every interleaving the client takes it in
  /// its spin, finds it on its re-check, or is nudged by the bridge. All three happened, so no half is
  /// vacuous.
  #[test]
  fn a_reply_published_while_an_async_client_parks_is_never_lost() {
    loom_bounds::explore("park: client, daemon and bridge", || {
      let words = words();
      let daemon = loom::thread::spawn(move || {
        words.depth.store(1, Ordering::Release);
        words.word.fetch_add(1, Ordering::AcqRel);
        if after_publishing_armed(|| words.armed.load(Ordering::Relaxed) != 0) {
          words.word.fetch_add(1, Ordering::AcqRel);
          words.bell.notify();
        }
      });
      let bridge = loom::thread::spawn(move || {
        let mut seen = words.word.load(Ordering::Acquire);
        loop {
          wait_past(words, seen);
          if words.stop.load(Ordering::Acquire) != 0 {
            return;
          }
          seen = words.word.load(Ordering::Acquire);
          // Acquire, as the real bridge reads the flag and the ring's hints: its nudge then carries the
          // reply's publication to the client it wakes.
          if reply_waiting_for_armed(
            || words.armed.load(Ordering::Acquire) != 0,
            || words.depth.load(Ordering::Acquire) != 0,
          ) {
            words.pipe.notify();
          }
        }
      });
      client(words, true);
      daemon.join().unwrap();
      bridge.join().unwrap();
    });
    assert!(
      SPUN.load(StdOrdering::Relaxed) > 0,
      "some reply came in the spin"
    );
    assert!(
      RECHECKED.load(StdOrdering::Relaxed) > 0,
      "some re-check after arming found the reply"
    );
    assert!(
      NUDGED.load(StdOrdering::Relaxed) > 0,
      "some reply came by the bridge's nudge"
    );
  }

  /// The protocol as it stood before 2026-09-29, kept as the model's witness that it can see the bug:
  /// `Release`/`Acquire` with no fence on either side, one wake without a second advance of the word, and an
  /// edge-triggered bridge that uses up a change it sees while the client is not armed. loom finds the
  /// interleaving in which the client parks on the completion fd with its reply in the ring and nothing
  /// left to nudge it — a deadlock. If this model ever passes, the model has lost its power to see it.
  #[test]
  #[should_panic(expected = "deadlock")]
  fn the_edge_triggered_unfenced_protocol_loses_a_wake() {
    loom_bounds::explore("park: the old protocol (must fail)", || {
      let words = words();
      let daemon = loom::thread::spawn(move || {
        words.depth.store(1, Ordering::Release);
        words.word.fetch_add(1, Ordering::AcqRel);
        if words.armed.load(Ordering::Acquire) != 0 {
          words.bell.notify();
        }
      });
      let bridge = loom::thread::spawn(move || {
        let mut seen = words.word.load(Ordering::Acquire);
        loop {
          wait_past(words, seen);
          if words.stop.load(Ordering::Acquire) != 0 {
            return;
          }
          let now = words.word.load(Ordering::Acquire);
          if now == seen {
            continue;
          }
          seen = now;
          if words.armed.load(Ordering::Acquire) != 0 {
            words.pipe.notify();
          }
        }
      });
      client(words, false);
      daemon.join().unwrap();
      bridge.join().unwrap();
    });
  }
}
