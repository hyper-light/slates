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
//! The ring is `Sync` for exactly one producer and one consumer at a time; the [`Producer`] and
//! [`Consumer`] halves enforce that in the type system and are what shards hold.

#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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
    })
  }

  /// Slots.
  pub const fn capacity(&self) -> usize {
    self.mask + 1
  }

  /// Splits the ring into its producer and consumer halves.
  pub fn split(&self) -> (Producer<'_>, Consumer<'_>) {
    (Producer { ring: self }, Consumer { ring: self })
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

/// The producer half.
#[derive(Debug)]
pub struct Producer<'a> {
  ring: &'a SpscRing,
}

/// The consumer half.
#[derive(Debug)]
pub struct Consumer<'a> {
  ring: &'a SpscRing,
}

impl Producer<'_> {
  /// Pushes a word; returns it back when the ring is full.
  pub fn push(&mut self, word: u64) -> Result<(), u64> {
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
  /// Pops the oldest word, if any.
  pub fn pop(&mut self) -> Option<u64> {
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
    let (mut p, mut c) = ring.split();
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

  #[test]
  fn one_producer_and_one_consumer_thread_see_every_word_in_order() {
    let ring = SpscRing::new(64).unwrap();
    let total = 100_000u64;
    std::thread::scope(|s| {
      let (mut p, mut c) = ring.split();
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

#[cfg(all(test, loom))]
mod loom_tests {
  use super::*;

  #[test]
  fn every_interleaving_of_one_producer_and_one_consumer_is_fifo_without_loss() {
    loom::model(|| {
      let ring: &'static SpscRing = Box::leak(Box::new(SpscRing::new(2).unwrap()));
      let (mut p, mut c) = ring.split();
      let producer = loom::thread::spawn(move || {
        for i in 1..=3u64 {
          while p.push(i).is_err() {
            loom::thread::yield_now();
          }
        }
      });
      let mut got = Vec::new();
      while got.len() < 3 {
        match c.pop() {
          Some(v) => got.push(v),
          None => loom::thread::yield_now(),
        }
      }
      producer.join().unwrap();
      assert_eq!(got, vec![1, 2, 3]);
    });
  }
}
