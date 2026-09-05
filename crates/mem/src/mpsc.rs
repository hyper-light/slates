//! A bounded multi-producer single-consumer ring of `Copy` words, for producers that are not
//! shards: bridge threads on Windows (WinFsp dispatches on its own threads, §4.3), the
//! confirmation surface, tests. Shards talk to each other over per-pair SPSC rings; this ring is
//! the one place a compare-and-swap exists on a wake path, and only foreign threads pay it.
//!
//! Vyukov's bounded queue [C: Dmitry Vyukov, "Bounded MPMC queue", 1024cores.net]: every slot
//! carries a sequence number; a producer claims a slot by compare-and-swap on the tail and
//! publishes by writing the slot's sequence; the consumer reads a slot when its sequence says it
//! is full and releases it by writing the next lap's sequence. Under `--cfg loom` the test below
//! explores every interleaving of two producers and one consumer (T-0.3).

use std::cell::UnsafeCell;

#[cfg(loom)]
use loom::sync::atomic::{AtomicUsize, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::error::MemError;

/// Shape: keeps the head and the tail on separate cache lines (the largest line we target).
#[repr(align(128))]
#[derive(Debug)]
struct Padded<T>(T);

#[derive(Debug)]
struct Slot<T> {
  sequence: AtomicUsize,
  value: UnsafeCell<Option<T>>,
}

/// The ring.
#[derive(Debug)]
pub struct MpscRing<T: Copy> {
  slots: Box<[Slot<T>]>,
  mask: usize,
  head: Padded<AtomicUsize>,
  tail: Padded<AtomicUsize>,
}

// SAFETY: slots are claimed by sequence number, so no two threads touch one slot's value at the
// same time; the consumer is single by the `Consumer` half.
unsafe impl<T: Copy + Send> Sync for MpscRing<T> {}
// SAFETY: a ring of Send words may move between threads.
unsafe impl<T: Copy + Send> Send for MpscRing<T> {}

impl<T: Copy> MpscRing<T> {
  /// A ring of `capacity` slots (a power of two, at least one).
  pub fn new(capacity: usize) -> Result<Self, MemError> {
    if capacity == 0 || !capacity.is_power_of_two() {
      return Err(MemError::BadCapacity { capacity });
    }
    let slots: Vec<Slot<T>> = (0..capacity)
      .map(|i| Slot {
        sequence: AtomicUsize::new(i),
        value: UnsafeCell::new(None),
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
  pub fn push(&self, value: T) -> Result<(), T> {
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
            // SAFETY: the compare-and-swap claimed this slot for this producer alone until the
            // sequence below publishes it.
            unsafe { slot.value.get().write(Some(value)) };
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
        return Err(value);
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
  pub const fn consumer(&self) -> Consumer<'_, T> {
    Consumer { ring: self }
  }
}

/// The consumer half.
#[derive(Debug)]
pub struct Consumer<'a, T: Copy> {
  ring: &'a MpscRing<T>,
}

impl<T: Copy> Consumer<'_, T> {
  /// Pops the oldest word, if any.
  pub fn pop(&mut self) -> Option<T> {
    let head = self.ring.head.0.load(Ordering::Relaxed);
    let slot = &self.ring.slots[head & self.ring.mask];
    let sequence = slot.sequence.load(Ordering::Acquire);
    if sequence != head.wrapping_add(1) {
      return None;
    }
    // SAFETY: the sequence says the producer published this slot and no producer will touch it
    // until the sequence below releases it for the next lap.
    let value = unsafe { slot.value.get().replace(None) };
    slot.sequence.store(
      head.wrapping_add(self.ring.mask).wrapping_add(1),
      Ordering::Release,
    );
    self
      .ring
      .head
      .0
      .store(head.wrapping_add(1), Ordering::Relaxed);
    value
  }
}

#[cfg(all(test, not(loom)))]
mod tests {
  use super::*;

  #[test]
  fn fifo_per_producer_and_full_is_a_refusal() {
    let ring = MpscRing::<u32>::new(4).unwrap();
    let mut c = ring.consumer();
    assert_eq!(c.pop(), None);
    for i in 0..4 {
      ring.push(i).unwrap();
    }
    assert_eq!(ring.push(9), Err(9));
    assert_eq!(c.pop(), Some(0));
    ring.push(4).unwrap();
    let rest: Vec<u32> = std::iter::from_fn(|| c.pop()).collect();
    assert_eq!(rest, vec![1, 2, 3, 4]);
    assert_eq!(ring.capacity(), 4);
  }

  #[test]
  fn four_producer_threads_lose_nothing_and_keep_each_producers_order() {
    let ring = MpscRing::<u64>::new(64).unwrap();
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

#[cfg(all(test, loom))]
mod loom_tests {
  use super::*;

  #[test]
  fn every_interleaving_of_two_producers_and_one_consumer_loses_nothing() {
    loom::model(|| {
      let ring: &'static MpscRing<u8> = Box::leak(Box::new(MpscRing::new(2).unwrap()));
      let a = loom::thread::spawn(move || {
        while ring.push(1).is_err() {
          loom::thread::yield_now();
        }
      });
      let b = loom::thread::spawn(move || {
        while ring.push(2).is_err() {
          loom::thread::yield_now();
        }
      });
      let mut c = ring.consumer();
      let mut got = Vec::new();
      while got.len() < 2 {
        match c.pop() {
          Some(v) => got.push(v),
          None => loom::thread::yield_now(),
        }
      }
      a.join().unwrap();
      b.join().unwrap();
      got.sort_unstable();
      assert_eq!(got, vec![1, 2]);
    });
  }
}
