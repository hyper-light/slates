//! A bounded multi-producer single-consumer ring of words, for producers that are not shards:
//! bridge threads on Windows (WinFsp dispatches on its own threads, §4.3), the confirmation
//! surface, tests. Shards talk to each other over per-pair SPSC rings; this ring is the one place
//! a compare-and-swap exists on a wake path, and only foreign threads pay it.
//!
//! Vyukov's bounded queue [C: Dmitry Vyukov, "Bounded MPMC queue", 1024cores.net]: every slot
//! carries a sequence number; a producer claims a slot by compare-and-swap on the tail and
//! publishes by writing the slot's sequence; the consumer reads a slot when its sequence says it
//! is full and releases it by writing the next lap's sequence. The word itself is an atomic, so
//! the ring holds no `UnsafeCell` and no unsafe code. Under `--cfg loom` the test explores every
//! interleaving of two producers and one consumer (T-0.3).
//!
//! Two slots is the smallest ring the protocol admits: a slot's sequence reads "full" at its position plus
//! one and "free for the next lap" at its position plus the capacity, which are one value at capacity one —
//! so a one-slot ring let a second push overwrite the unread first (AUD-29-33, 2026-09-30); it is refused.
//! The consumer is claimed ([`MpscRing::consumer`]; `None` while another is held) and released when the
//! claim drops, and it is `Send` but not `Sync`, so no two threads ever pop at once.

#[cfg(loom)]
use loom::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::cell::Cell;
use std::marker::PhantomData;
#[cfg(not(loom))]
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::error::MemError;

/// Shape: keeps the head and the tail on separate cache lines (the largest line we target).
#[repr(align(128))]
#[derive(Debug)]
struct Padded<T>(T);

#[derive(Debug)]
struct Slot {
  sequence: AtomicUsize,
  word: AtomicU64,
}

/// The ring.
#[derive(Debug)]
pub struct MpscRing {
  slots: Box<[Slot]>,
  mask: usize,
  head: Padded<AtomicUsize>,
  tail: Padded<AtomicUsize>,
  /// Whether a [`Consumer`] is held.
  consumer_held: AtomicBool,
}

/// Derived: the smallest capacity whose slot sequences tell a full slot (position + 1) from one free for
/// the next lap (position + capacity) — they coincide at one (see the module doc).
pub const MIN_CAPACITY: usize = 2;

impl MpscRing {
  /// A ring of `capacity` slots (a power of two, at least [`MIN_CAPACITY`]).
  pub fn new(capacity: usize) -> Result<Self, MemError> {
    if capacity < MIN_CAPACITY || !capacity.is_power_of_two() {
      return Err(MemError::BadCapacity { capacity });
    }
    let slots: Vec<Slot> = (0..capacity)
      .map(|i| Slot {
        sequence: AtomicUsize::new(i),
        word: AtomicU64::new(0),
      })
      .collect();
    Ok(Self {
      slots: slots.into_boxed_slice(),
      mask: capacity.saturating_sub(1),
      head: Padded(AtomicUsize::new(0)),
      tail: Padded(AtomicUsize::new(0)),
      consumer_held: AtomicBool::new(false),
    })
  }

  /// Slots.
  pub const fn capacity(&self) -> usize {
    self.mask.saturating_add(1)
  }

  /// Pushes from any thread; returns the word back when the ring is full.
  pub fn push(&self, word: u64) -> Result<(), u64> {
    let mut tail = self.tail.0.load(Ordering::Relaxed);
    loop {
      // `mask` is the slot count minus one, so the masked index is always a slot; a miss reads as full.
      let Some(slot) = self.slots.get(tail & self.mask) else {
        return Err(word);
      };
      let sequence = slot.sequence.load(Ordering::Acquire);
      if sequence == tail {
        match self.tail.0.compare_exchange_weak(
          tail,
          tail.wrapping_add(1),
          Ordering::Relaxed,
          Ordering::Relaxed,
        ) {
          Ok(_) => {
            slot.word.store(word, Ordering::Relaxed);
            slot.sequence.store(tail.wrapping_add(1), Ordering::Release);
            return Ok(());
          }
          Err(seen) => tail = seen,
        }
      } else if sequence.wrapping_sub(tail) <= self.capacity() {
        // Another producer claimed this slot and moved the tail on; follow it.
        tail = self.tail.0.load(Ordering::Relaxed);
      } else {
        // The sequence is a lap behind: the consumer has not released the slot; the ring is full.
        return Err(word);
      }
    }
  }

  /// Whether no word is waiting (a racy read for spin loops; the consumer's `pop` is exact).
  pub fn is_empty(&self) -> bool {
    let head = self.head.0.load(Ordering::Acquire);
    self
      .slots
      .get(head & self.mask)
      .is_none_or(|slot| slot.sequence.load(Ordering::Acquire) != head.wrapping_add(1))
  }

  /// Claims the consumer half: `None` while another claim is held, so there is exactly one at a time.
  pub fn consumer(&self) -> Option<Consumer<'_>> {
    if self.consumer_held.swap(true, Ordering::AcqRel) {
      return None;
    }
    Some(Consumer {
      ring: self,
      one_thread: PhantomData,
    })
  }
}

/// The consumer half: the ring's only consumer while held, used from one thread at a time (`Send`, not
/// `Sync`); dropping it releases the claim. It cannot be shared between threads:
///
/// ```compile_fail,E0277
/// fn shared<T: Sync>(_: &T) {}
/// let ring = slates_mem::MpscRing::new(2).unwrap();
/// shared(&ring.consumer().unwrap());
/// ```
#[derive(Debug)]
pub struct Consumer<'a> {
  ring: &'a MpscRing,
  one_thread: PhantomData<Cell<()>>,
}

impl Drop for Consumer<'_> {
  fn drop(&mut self) {
    self.ring.consumer_held.store(false, Ordering::Release);
  }
}

impl Consumer<'_> {
  /// Pops the oldest word, if any.
  pub fn pop(&self) -> Option<u64> {
    let head = self.ring.head.0.load(Ordering::Relaxed);
    let slot = self.ring.slots.get(head & self.ring.mask)?;
    let sequence = slot.sequence.load(Ordering::Acquire);
    if sequence != head.wrapping_add(1) {
      return None;
    }
    let word = slot.word.load(Ordering::Relaxed);
    slot.sequence.store(
      head.wrapping_add(self.ring.mask).wrapping_add(1),
      Ordering::Release,
    );
    self
      .ring
      .head
      .0
      .store(head.wrapping_add(1), Ordering::Relaxed);
    Some(word)
  }
}

#[cfg(all(test, not(loom)))]
mod tests {
  use super::*;

  #[test]
  fn fifo_per_producer_and_full_is_a_refusal() {
    let ring = MpscRing::new(4).unwrap();
    let c = ring.consumer().unwrap();
    assert_eq!(c.pop(), None);
    for i in 0..4 {
      ring.push(i).unwrap();
    }
    assert_eq!(ring.push(9), Err(9));
    assert_eq!(c.pop(), Some(0));
    ring.push(4).unwrap();
    let rest: Vec<u64> = std::iter::from_fn(|| c.pop()).collect();
    assert_eq!(rest, vec![1, 2, 3, 4]);
    assert_eq!(ring.capacity(), 4);
    assert!(ring.is_empty());
  }

  /// AUD-29-33: do: build a one-slot ring; expect `BadCapacity` (the protocol cannot tell its full slot
  /// from its free one — the old ring let a second push overwrite the first, unread).
  #[test]
  fn a_one_slot_ring_is_refused() {
    assert!(matches!(
      MpscRing::new(1),
      Err(MemError::BadCapacity { capacity: 1 })
    ));
  }

  /// AUD-29-33: do: for every admitted geometry up to 64 slots, fill the ring, push once more, pop one, push
  /// again, then alternate push and pop for three laps; expect the extra push refused with its word handed
  /// back, the oldest word never lost or replaced, and FIFO order across every wrap.
  #[test]
  fn every_admitted_geometry_refuses_when_full_and_keeps_fifo_across_wraps() {
    /// Shape: the largest geometry the sweep covers — enough laps of the sequence arithmetic at every
    /// power of two a small bound admits.
    const LARGEST: usize = 64;
    let mut capacity = MIN_CAPACITY;
    while capacity <= LARGEST {
      let ring = MpscRing::new(capacity).unwrap();
      let consumer = ring.consumer().unwrap();
      let words = u64::try_from(capacity).unwrap();
      for word in 0..words {
        ring.push(word).unwrap();
      }
      assert_eq!(
        ring.push(u64::MAX),
        Err(u64::MAX),
        "capacity {capacity}: full"
      );
      assert_eq!(consumer.pop(), Some(0), "capacity {capacity}: oldest kept");
      ring.push(words).unwrap();
      let drained: Vec<u64> = std::iter::from_fn(|| consumer.pop()).collect();
      assert_eq!(
        drained,
        (1..=words).collect::<Vec<u64>>(),
        "capacity {capacity}"
      );
      for word in 0..words * 3 {
        ring.push(word).unwrap();
        assert_eq!(
          consumer.pop(),
          Some(word),
          "capacity {capacity}: across the wrap"
        );
      }
      assert_eq!(consumer.pop(), None);
      capacity *= 2;
    }
  }

  /// AUD-29-33: do: claim the consumer, claim it again, drop the first claim, claim again; expect the second
  /// claim `None` while the first is held and a fresh claim after it drops.
  #[test]
  fn the_consumer_is_claimed_by_one_holder_at_a_time() {
    let ring = MpscRing::new(MIN_CAPACITY).unwrap();
    let first = ring.consumer().unwrap();
    assert!(
      ring.consumer().is_none(),
      "a second consumer while one is held"
    );
    ring.push(5).unwrap();
    drop(first);
    let second = ring.consumer().unwrap();
    assert_eq!(second.pop(), Some(5));
  }

  #[test]
  fn four_producer_threads_lose_nothing_and_keep_each_producers_order() {
    let ring = MpscRing::new(64).unwrap();
    let per_producer = 20_000u64;
    let producers = 4u64;
    std::thread::scope(|s| {
      for p in 0..producers {
        let ring = &ring;
        s.spawn(move || {
          for i in 0..per_producer {
            let word = (p << 32) | i;
            while ring.push(word).is_err() {
              std::hint::spin_loop();
            }
          }
        });
      }
      let c = ring.consumer().unwrap();
      let mut next = vec![0u64; usize::try_from(producers).unwrap()];
      let mut received = 0u64;
      while received < per_producer * producers {
        if let Some(word) = c.pop() {
          let p = usize::try_from(word >> 32).unwrap();
          assert_eq!(word & 0xFFFF_FFFF, next[p], "producer {p} out of order");
          next[p] += 1;
          received += 1;
        } else {
          std::hint::spin_loop();
        }
      }
    });
  }
}

// Two attributes rather than `all(test, loom)`: clippy's test-context rule (unwrap allowed in
// tests) recognizes only a bare `cfg(test)`, and an unwrap here is a failed model, as it should be.
#[cfg(test)]
#[cfg(loom)]
mod loom_tests {
  use std::sync::atomic::{AtomicU64, Ordering as StdOrdering};

  use super::*;
  use crate::loom_bounds;

  /// Format: a word is `producer << 32 | sequence`, so the consumer can attribute it.
  const PRODUCER_SHIFT: u32 = 32;

  /// Full-ring refusals met across every explored interleaving of the lapping model (a plain
  /// counter outside loom's model: the non-vacuity check that the refusal path ran).
  static REFUSALS: AtomicU64 = AtomicU64::new(0);

  /// Runs `producers` threads each pushing `words_per_producer` words through a ring of
  /// `capacity` slots into one consumer, under every explored interleaving, and asserts that
  /// every word arrives exactly once and each producer's order is kept; a word refused by a
  /// full ring is handed back and lands on the retry.
  fn producers_into_one_consumer(
    name: &str,
    capacity: usize,
    producers: u64,
    words_per_producer: u64,
  ) {
    loom_bounds::explore(name, move || {
      let ring: &'static MpscRing = Box::leak(Box::new(MpscRing::new(capacity).unwrap()));
      let pushing: Vec<_> = (0..producers)
        .map(|producer| {
          loom::thread::spawn(move || {
            for sequence in 0..words_per_producer {
              let mut pending = (producer << PRODUCER_SHIFT) | sequence;
              while let Err(back) = ring.push(pending) {
                REFUSALS.fetch_add(1, StdOrdering::Relaxed);
                pending = back;
                loom::thread::yield_now();
              }
            }
          })
        })
        .collect();
      let consumer = ring.consumer().unwrap();
      let mut got = Vec::new();
      let total = usize::try_from(producers * words_per_producer).unwrap();
      while got.len() < total {
        match consumer.pop() {
          Some(word) => got.push(word),
          None => loom::thread::yield_now(),
        }
      }
      for producer in pushing {
        producer.join().unwrap();
      }
      assert_eq!(consumer.pop(), None, "nothing beyond the words pushed");
      assert!(ring.is_empty());
      let mut every_word = got.clone();
      every_word.sort_unstable();
      let expected: Vec<u64> = (0..producers)
        .flat_map(|producer| {
          (0..words_per_producer).map(move |sequence| (producer << PRODUCER_SHIFT) | sequence)
        })
        .collect();
      assert_eq!(every_word, expected, "every word exactly once");
      for producer in 0..producers {
        let in_order: Vec<u64> = got
          .iter()
          .filter(|word| *word >> PRODUCER_SHIFT == producer)
          .map(|word| word & ((1 << PRODUCER_SHIFT) - 1))
          .collect();
        assert_eq!(
          in_order,
          (0..words_per_producer).collect::<Vec<u64>>(),
          "producer {producer}'s order kept"
        );
      }
    });
  }

  /// Shape: producers in the contention model — the design's "two producers" (T-0.3), the
  /// fewest that contend on the tail's compare-and-swap.
  const CONTENDING_PRODUCERS: u64 = 2;
  /// Shape: words per contending producer — two, so each producer has an order the consumer
  /// must keep across the other's interleaved claims.
  const CONTENDING_WORDS: u64 = 2;
  /// Shape: the contention model's capacity — every word fits at once, so the model isolates
  /// the claim race (a producer following a tail another moved) from the full-ring wait, which
  /// the lapping model covers with one spinner; loom explores two spinners' voluntary yields
  /// into executions past any honest bound (measured 2026-09-13, `docs/wip/concurrency.md`).
  const CONTENDING_CAPACITY: usize = 4;

  /// T-0.3 (AC-0.7): every interleaving of two producers and one consumer, the producers
  /// contending for slots, delivers every word exactly once and keeps each producer's order.
  #[test]
  fn every_interleaving_of_two_contending_producers_keeps_each_order_and_loses_nothing() {
    producers_into_one_consumer(
      "mpsc ring, two contending producers",
      CONTENDING_CAPACITY,
      CONTENDING_PRODUCERS,
      CONTENDING_WORDS,
    );
  }

  /// Shape: the lapping model's capacity, the smallest power of two at which the producer meets
  /// a full ring and the sequence numbers lap within the model's words.
  const LAPPING_CAPACITY: usize = 2;
  /// Shape: words in the lapping model, one more than the capacity, so every execution meets a
  /// full ring with its refusal retried and pushes into the second lap.
  const LAPPING_WORDS: u64 = 3;

  /// T-0.3 (AC-0.7): every interleaving of a producer lapping a two-slot ring and its consumer
  /// delivers the words in order, none lost, none duplicated; the full-ring refusal hands the
  /// word back and the retry lands it; at least one interleaving met a full ring.
  #[test]
  fn every_interleaving_of_a_producer_lapping_the_ring_is_fifo_without_loss() {
    producers_into_one_consumer(
      "mpsc ring, one producer lapping the ring",
      LAPPING_CAPACITY,
      1,
      LAPPING_WORDS,
    );
    assert!(
      REFUSALS.load(StdOrdering::Relaxed) > 0,
      "some interleaving met a full ring and retried"
    );
  }
}
