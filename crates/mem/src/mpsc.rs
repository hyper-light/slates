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

#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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
}

impl MpscRing {
  /// A ring of `capacity` slots (a power of two, at least one).
  pub fn new(capacity: usize) -> Result<Self, MemError> {
    if capacity == 0 || !capacity.is_power_of_two() {
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
      mask: capacity - 1,
      head: Padded(AtomicUsize::new(0)),
      tail: Padded(AtomicUsize::new(0)),
    })
  }

  /// Slots.
  pub const fn capacity(&self) -> usize {
    self.mask + 1
  }

  /// Pushes from any thread; returns the word back when the ring is full.
  pub fn push(&self, word: u64) -> Result<(), u64> {
    let mut tail = self.tail.0.load(Ordering::Relaxed);
    loop {
      let slot = &self.slots[tail & self.mask];
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
    let slot = &self.slots[head & self.mask];
    slot.sequence.load(Ordering::Acquire) != head.wrapping_add(1)
  }

  /// The consumer half; there must be exactly one at a time.
  pub const fn consumer(&self) -> Consumer<'_> {
    Consumer { ring: self }
  }
}

/// The consumer half.
#[derive(Debug)]
pub struct Consumer<'a> {
  ring: &'a MpscRing,
}

impl Consumer<'_> {
  /// Pops the oldest word, if any.
  pub fn pop(&mut self) -> Option<u64> {
    let head = self.ring.head.0.load(Ordering::Relaxed);
    let slot = &self.ring.slots[head & self.ring.mask];
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
    let mut c = ring.consumer();
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
      let mut c = ring.consumer();
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
      let mut consumer = ring.consumer();
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
