//! Parking a shard and kicking it awake: the protocol between a shard about to wait in its driver
//! and whoever sends it a message from another thread (§4.3, "wake from another shard enqueues
//! (slot, generation) on the target's ring and kicks the driver"; §4.7 "Wake strategy": a sender
//! kicks only a parked shard, so a message to a spinning shard costs no syscall, and the saving
//! is counted). The shard announces that it is about to park, then re-checks its inboxes; a
//! sender publishes its message, then checks the announcement. Each side writes and then reads,
//! the store-buffering shape, so the two reads must be ordered against the two writes by one
//! total order or both can miss: the shard sees no message and parks, the sender sees no
//! announcement and skips the kick, and the message waits for a wake that never comes.
//!
//! Each side therefore fences with `SeqCst` between its write and its read. That is the C++20
//! fence rule ([B: `[atomics.order]`, the fence–fence case]: with a `SeqCst` fence after the
//! write on one thread and a `SeqCst` fence before the read on the other, whichever fence comes
//! first in the total order, the later thread's read observes the earlier thread's write), and
//! it holds whatever ordering the message's own publication used — the multi-producer ring
//! publishes with `Release`, the control flag with `SeqCst`. A `SeqCst` store and load on the
//! announcement alone were not enough: the ring's `Release` publication takes no part in the
//! announcement's total order, so the abstract model lets both reads miss, and x86 realizes it
//! by reordering the plain store of the publication past the plain load of the announcement
//! through its store buffer (a `SeqCst` load is a plain `mov` there; only a `SeqCst` store gets
//! the locked instruction). loom found the lost wake in the first executions it explored
//! (`docs/bugs/2026-09-13-parked-shard-loses-a-foreign-wake.md`); arm64 orders a store-release
//! before a later load-acquire and was never exposed.
//!
//! Under `--cfg loom` the model in this module drives exactly this code: one sender publishing
//! into the shard's multi-producer ring against one shard parking, with loom's `Notify` standing
//! in for the driver's kick (sticky like an eventfd count or an `EVFILT_USER` trigger, spurious
//! like a real `kevent` return); a lost wake is a shard blocked with no runnable thread left,
//! which loom reports as a deadlock (AC-0.7).

#[cfg(loom)]
use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};

/// The shard's announcement that it is parked, and the count of kicks the announcement saved.
#[derive(Debug)]
pub struct Parking {
  parked: AtomicBool,
  kicks_skipped: AtomicU64,
}

impl Default for Parking {
  fn default() -> Self {
    Self::new()
  }
}

impl Parking {
  /// Not parked, nothing saved yet.
  pub fn new() -> Self {
    Self {
      parked: AtomicBool::new(false),
      kicks_skipped: AtomicU64::new(0),
    }
  }

  /// The sender's half, called after the message is published (a word in the ring, the control
  /// flag set): kicks the shard when it has announced parking, else counts the kick saved. The
  /// fence orders the publication before the read of the announcement (the module doc).
  pub fn kick_if_parked(&self, kick: impl FnOnce()) {
    fence(Ordering::SeqCst);
    if self.parked.load(Ordering::SeqCst) {
      kick();
    } else {
      self.kicks_skipped.fetch_add(1, Ordering::Relaxed);
    }
  }

  /// The shard's half: announces parking, then asks `pending` whether a message already waits.
  /// When one does, withdraws the announcement and returns `false` without waiting; otherwise
  /// runs `wait` (the driver's blocking wait, which a kick ends), withdraws the announcement
  /// afterwards, and returns `true`. The fence orders the announcement before the re-check.
  pub fn park_unless_pending(&self, pending: impl FnOnce() -> bool, wait: impl FnOnce()) -> bool {
    self.parked.store(true, Ordering::SeqCst);
    fence(Ordering::SeqCst);
    if pending() {
      self.parked.store(false, Ordering::SeqCst);
      return false;
    }
    wait();
    self.parked.store(false, Ordering::SeqCst);
    true
  }

  /// Kicks skipped because the shard had not announced parking (the saving, counted).
  pub fn kicks_skipped(&self) -> u64 {
    self.kicks_skipped.load(Ordering::Relaxed)
  }
}

// Two attributes rather than `all(test, loom)`: clippy's test-context rule (unwrap allowed in
// tests) recognizes only a bare `cfg(test)`, and an unwrap here is a failed model, as it should be.
#[cfg(test)]
#[cfg(loom)]
mod loom_tests {
  use std::sync::atomic::{AtomicU64, Ordering as StdOrdering};

  // loom's `Notify` is the model's stand-in for the driver's kick: test scaffolding under
  // `cfg(loom)` only (D-8 exception 3, a test harness); the two owners are this model's sender
  // and shard.
  use loom::sync::Notify;
  use slates_mem::{MpscRing, loom_bounds};

  use super::*;

  /// Shape: the shard's foreign ring in the model, the smallest power of two; it carries one
  /// word, so the sender never meets a full ring (the ring's own models cover that).
  const RING_CAPACITY: usize = 2;
  /// Format: the word the sender publishes; the model checks that it arrives.
  const WORD: u64 = 0x5a;

  /// Kicks delivered, across every explored interleaving.
  static KICKS: AtomicU64 = AtomicU64::new(0);
  /// Kicks skipped because the shard was not parked, across every explored interleaving.
  static SKIPS: AtomicU64 = AtomicU64::new(0);
  /// Waits the shard entered, across every explored interleaving.
  static WAITS: AtomicU64 = AtomicU64::new(0);

  /// AC-0.7 (the kick-if-parked protocol of `registry::send_foreign` and `shard::park`): a
  /// sender publishes a word into the shard's ring and kicks only if the shard announced
  /// parking; the shard announces, re-checks the ring, and waits only when it saw nothing. In
  /// every interleaving the shard receives the word: it either saw it before waiting, or was
  /// kicked out of its wait. A lost wake leaves the shard blocked with nothing left to run,
  /// which loom reports as a deadlock. Some interleaving kicked, some skipped the kick, and some
  /// made the shard wait, so neither half of the protocol is vacuous.
  #[test]
  fn a_word_published_while_the_shard_parks_is_never_lost() {
    loom_bounds::explore("parking: one sender against one parking shard", || {
      let ring: &'static MpscRing = Box::leak(Box::new(MpscRing::new(RING_CAPACITY).unwrap()));
      let parking: &'static Parking = Box::leak(Box::new(Parking::new()));
      let kick: &'static Notify = Box::leak(Box::new(Notify::new()));
      let sender = loom::thread::spawn(move || {
        // `send_foreign`'s order: publish, then kick if the shard is parked.
        ring.push(WORD).unwrap();
        parking.kick_if_parked(|| {
          KICKS.fetch_add(1, StdOrdering::Relaxed);
          kick.notify();
        });
      });
      let mut consumer = ring.consumer();
      let mut waited = false;
      // The shard's loop: drain the ring, and park unless something is pending.
      let word = loop {
        if let Some(word) = consumer.pop() {
          break word;
        }
        if parking.park_unless_pending(|| !ring.is_empty(), || kick.wait()) {
          waited = true;
        }
      };
      assert_eq!(word, WORD);
      sender.join().unwrap();
      if waited {
        WAITS.fetch_add(1, StdOrdering::Relaxed);
      }
      SKIPS.fetch_add(parking.kicks_skipped(), StdOrdering::Relaxed);
    });
    assert!(
      KICKS.load(StdOrdering::Relaxed) > 0,
      "some interleaving kicked"
    );
    assert!(
      SKIPS.load(StdOrdering::Relaxed) > 0,
      "some interleaving skipped the kick"
    );
    assert!(
      WAITS.load(StdOrdering::Relaxed) > 0,
      "some interleaving made the shard wait"
    );
  }
}
