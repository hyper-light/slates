//! A bounded single-producer single-consumer ring of words: the carrier for cross-shard wakes and
//! message-passing frees (§4.2 "freeing a slot from another shard is a message to the owner";
//! §4.3 `inbound: [SpscRing<Msg>; N_SHARDS]`).
//!
//! Lamport's ring [A: Lamport, "Proving the correctness of multiprocess programs", 1977] with
//! head and tail on separate cache lines and a power-of-two capacity so the index wraps by mask.
//! Every slot is an atomic word, so the ring holds no `UnsafeCell` and no unsafe code at all: the
//! producer stores the slot (relaxed) and publishes the tail (release); the consumer reads the tail
//! (acquire), loads the slot (relaxed), and publishes the head (release). The release/acquire pair
//! on the indices orders the slot accesses; the slot's own atomicity only rules out torn words.
//! Under `--cfg loom` the atomics are loom's and the test explores every interleaving (AC-0.7,
//! T-0.3).
//!
//! The ring is `Sync` for exactly one producer and one consumer; the [`Producer`] and [`Consumer`] halves
//! enforce that and are what shards hold for the ring's life. [`SpscRing::split`] hands the halves out once
//! (a second call is `None`), and each half is `Send` but not `Sync`, so it moves to its thread whole and is
//! never used from two threads at once — which is why `push` and `pop` take `&self`. Until 2026-09-30
//! `split` could be called any number of times, and the runtime called it on every send and every drain:
//! the single producer and consumer were a convention, not a property of the type (AUD-29-33).

#[cfg(loom)]
use loom::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::cell::Cell;
use std::marker::PhantomData;
#[cfg(not(loom))]
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::error::MemError;

/// Shape: the alignment that keeps the head and the tail on separate cache lines on every
/// target we build for (the largest line, Apple silicon's 128 bytes; the profile's measured line
/// is checked against it at boot).
#[repr(align(128))]
#[derive(Debug)]
struct Padded<T>(T);

/// The ring.
#[derive(Debug)]
pub struct SpscRing {
  slots: Box<[AtomicU64]>,
  mask: usize,
  head: Padded<AtomicUsize>,
  tail: Padded<AtomicUsize>,
  /// Whether the halves were handed out ([`SpscRing::split`]); set once, never cleared.
  split: AtomicBool,
}

impl SpscRing {
  /// A ring of `capacity` slots (a power of two, at least one).
  pub fn new(capacity: usize) -> Result<Self, MemError> {
    if capacity == 0 || !capacity.is_power_of_two() {
      return Err(MemError::BadCapacity { capacity });
    }
    let slots: Vec<AtomicU64> = (0..capacity).map(|_| AtomicU64::new(0)).collect();
    Ok(Self {
      slots: slots.into_boxed_slice(),
      mask: capacity - 1,
      head: Padded(AtomicUsize::new(0)),
      tail: Padded(AtomicUsize::new(0)),
      split: AtomicBool::new(false),
    })
  }

  /// Slots.
  pub const fn capacity(&self) -> usize {
    self.mask + 1
  }

  /// Splits the ring into its producer and consumer halves — once in its life: `None` if they were
  /// already handed out, so no second producer or consumer can exist.
  pub fn split(&self) -> Option<(Producer<'_>, Consumer<'_>)> {
    if self.split.swap(true, Ordering::AcqRel) {
      return None;
    }
    Some((
      Producer {
        ring: self,
        one_thread: PhantomData,
      },
      Consumer {
        ring: self,
        one_thread: PhantomData,
      },
    ))
  }

  /// Words waiting.
  pub fn len(&self) -> usize {
    self
      .tail
      .0
      .load(Ordering::Acquire)
      .wrapping_sub(self.head.0.load(Ordering::Acquire))
  }

  /// Whether nothing waits.
  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }
}

/// The producer half: the ring's only producer, used from one thread at a time (`Send`, not `Sync`). It
/// cannot be shared between threads:
///
/// ```compile_fail,E0277
/// fn shared<T: Sync>(_: &T) {}
/// let ring = slates_mem::SpscRing::new(2).unwrap();
/// let (producer, _consumer) = ring.split().unwrap();
/// shared(&producer);
/// ```
#[derive(Debug)]
pub struct Producer<'a> {
  ring: &'a SpscRing,
  one_thread: PhantomData<Cell<()>>,
}

/// The consumer half: the ring's only consumer, used from one thread at a time (`Send`, not `Sync`).
#[derive(Debug)]
pub struct Consumer<'a> {
  ring: &'a SpscRing,
  one_thread: PhantomData<Cell<()>>,
}

impl Producer<'_> {
  /// Pushes a word; returns it back when the ring is full.
  pub fn push(&self, word: u64) -> Result<(), u64> {
    let tail = self.ring.tail.0.load(Ordering::Relaxed);
    let head = self.ring.head.0.load(Ordering::Acquire);
    if tail.wrapping_sub(head) == self.ring.capacity() {
      return Err(word);
    }
    self.ring.slots[tail & self.ring.mask].store(word, Ordering::Relaxed);
    self
      .ring
      .tail
      .0
      .store(tail.wrapping_add(1), Ordering::Release);
    Ok(())
  }
}

impl Consumer<'_> {
  /// Whether nothing waits (a racy read for a loop's check; `pop` is exact).
  pub fn is_empty(&self) -> bool {
    self.ring.is_empty()
  }

  /// Pops the oldest word, if any.
  pub fn pop(&self) -> Option<u64> {
    let head = self.ring.head.0.load(Ordering::Relaxed);
    let tail = self.ring.tail.0.load(Ordering::Acquire);
    if head == tail {
      return None;
    }
    let word = self.ring.slots[head & self.ring.mask].load(Ordering::Relaxed);
    self
      .ring
      .head
      .0
      .store(head.wrapping_add(1), Ordering::Release);
    Some(word)
  }
}

#[cfg(all(test, not(loom)))]
mod tests {
  use super::*;

  #[test]
  fn capacity_must_be_a_power_of_two() {
    assert!(matches!(
      SpscRing::new(0),
      Err(MemError::BadCapacity { capacity: 0 })
    ));
    assert!(matches!(
      SpscRing::new(6),
      Err(MemError::BadCapacity { capacity: 6 })
    ));
    assert_eq!(SpscRing::new(8).unwrap().capacity(), 8);
  }

  #[test]
  fn fifo_with_wraparound_and_full_and_empty_refusals() {
    let ring = SpscRing::new(4).unwrap();
    let (p, c) = ring.split().unwrap();
    assert_eq!(c.pop(), None);
    for i in 0..4 {
      p.push(i).unwrap();
    }
    assert_eq!(p.push(99), Err(99));
    assert_eq!(ring.len(), 4);
    assert_eq!(c.pop(), Some(0));
    p.push(4).unwrap();
    let drained: Vec<u64> = std::iter::from_fn(|| c.pop()).collect();
    assert_eq!(drained, vec![1, 2, 3, 4]);
    assert!(ring.is_empty());
    for round in 0..1000u64 {
      p.push(round).unwrap();
      assert_eq!(c.pop(), Some(round));
    }
  }

  /// AUD-29-33 (the SPSC sibling): do: split a ring, then split it again; expect the second split `None`, so
  /// no second producer or consumer can exist, and the first halves still carry words.
  #[test]
  fn a_ring_splits_once() {
    let ring = SpscRing::new(2).unwrap();
    let (producer, consumer) = ring.split().unwrap();
    assert!(ring.split().is_none(), "a second pair of halves");
    producer.push(3).unwrap();
    assert_eq!(consumer.pop(), Some(3));
  }

  #[test]
  fn one_producer_and_one_consumer_thread_see_every_word_in_order() {
    let ring = SpscRing::new(64).unwrap();
    let total = 100_000u64;
    std::thread::scope(|s| {
      let (p, c) = ring.split().unwrap();
      s.spawn(move || {
        for i in 0..total {
          while p.push(i).is_err() {
            std::hint::spin_loop();
          }
        }
      });
      let mut expected = 0u64;
      while expected < total {
        if let Some(v) = c.pop() {
          assert_eq!(v, expected);
          expected += 1;
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

  /// Shape: the ring's capacity in the model, the smallest power of two at which the producer
  /// meets a full ring and both indices wrap within the model's words.
  const CAPACITY: usize = 2;
  /// Shape: the words pushed, one more than the capacity, so every execution covers a full ring
  /// with its refusal retried and one wrap of the head and the tail.
  const WORDS: u64 = 3;

  /// Full-ring refusals met across every explored interleaving (a plain counter outside loom's
  /// model, so a silently-dead refusal path can never masquerade as a passing model).
  static REFUSALS: AtomicU64 = AtomicU64::new(0);

  /// T-0.3 (AC-0.7): every interleaving of one producer and one consumer over a two-slot ring
  /// delivers the three words in order, none lost, none duplicated; a word refused by a full
  /// ring is handed back and lands on the retry; at least one interleaving met a full ring.
  #[test]
  fn every_interleaving_of_one_producer_and_one_consumer_is_fifo_without_loss() {
    loom_bounds::explore("spsc ring, one producer and one consumer", || {
      let ring: &'static SpscRing = Box::leak(Box::new(SpscRing::new(CAPACITY).unwrap()));
      let (producer, consumer) = ring.split().unwrap();
      let pushing = loom::thread::spawn(move || {
        for word in 1..=WORDS {
          let mut pending = word;
          while let Err(back) = producer.push(pending) {
            REFUSALS.fetch_add(1, StdOrdering::Relaxed);
            pending = back;
            loom::thread::yield_now();
          }
        }
      });
      let mut got = Vec::new();
      while got.len() < usize::try_from(WORDS).unwrap() {
        match consumer.pop() {
          Some(word) => got.push(word),
          None => loom::thread::yield_now(),
        }
      }
      pushing.join().unwrap();
      assert_eq!(
        got,
        (1..=WORDS).collect::<Vec<u64>>(),
        "in order, nothing lost, nothing duplicated"
      );
      assert_eq!(consumer.pop(), None, "nothing beyond the words pushed");
      assert!(ring.is_empty());
    });
    assert!(
      REFUSALS.load(StdOrdering::Relaxed) > 0,
      "some interleaving met a full ring and retried"
    );
  }
}
