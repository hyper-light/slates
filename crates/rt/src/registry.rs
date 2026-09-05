//! The process-wide shard registry: how a wake finds its target (§4.3, "wake from another shard
//! enqueues (slot, generation) on the target's ring and kicks the driver").
//!
//! Each shard registers once: a leaked entry holding its multi-producer inbound ring (for
//! foreign threads) and its kick. Entries are never freed, so a waker that outlives its shard
//! kicks a closed driver and is ignored rather than touching freed memory; the leak is bounded
//! by the number of shards a process ever creates (one runtime in production; a handful in
//! tests). Shard-to-shard wakes take the single-producer ring of the (source, target) pair,
//! which the current shard's thread-local context holds, and never a compare-and-swap.
//!
//! Routing: on the owning shard's thread the wake goes straight to the local queue; on another
//! shard's thread it goes to that pair's ring and kicks; on a foreign thread it goes to the
//! target's multi-producer ring and kicks. A full ring spins until the consumer drains it, and
//! counts the event: a wake is never dropped.

use std::cell::Cell;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicPtr, AtomicU16, AtomicU64, Ordering};

use slates_mem::{Encoded, MpscRing};

use crate::driver::Kick;
use crate::error::RtError;
use crate::msg::Msg;
use crate::shard::ShardContext;

/// Shape: the bound on shard ids per process: more than any host's core count, few enough that
/// the registry is a small static table (8 KiB of pointers) and a packed word's top bits stay
/// free for the message kind.
pub const MAX_SHARDS: usize = 1024;

/// A registered shard: its foreign-producer ring and its kick.
#[derive(Debug)]
pub struct Entry {
  /// The inbound ring foreign threads push to.
  pub inbound: MpscRing<u64>,
  /// The kick that wakes the shard's driver.
  pub kick: Kick,
  /// How many times a producer found a ring full and had to spin (a tripwire, GAPS §7).
  pub ring_full_events: AtomicU64,
}

static ENTRIES: [AtomicPtr<Entry>; MAX_SHARDS] =
  [const { AtomicPtr::new(std::ptr::null_mut()) }; MAX_SHARDS];
static NEXT_SHARD: AtomicU16 = AtomicU16::new(0);

thread_local! {
  static CURRENT: Cell<Option<NonNull<ShardContext>>> = const { Cell::new(None) };
}

/// Registers a new shard with an inbound ring of `ring_entries` words and its kick.
pub fn register(ring_entries: usize, kick: Kick) -> Result<u16, RtError> {
  let max = u16::try_from(MAX_SHARDS).unwrap_or(u16::MAX);
  let id = NEXT_SHARD.fetch_add(1, Ordering::AcqRel);
  if usize::from(id) >= MAX_SHARDS {
    return Err(RtError::TooManyShards { max });
  }
  let entry = Box::leak(Box::new(Entry {
    inbound: MpscRing::new(ring_entries)?,
    kick,
    ring_full_events: AtomicU64::new(0),
  }));
  ENTRIES[usize::from(id)].store(entry, Ordering::Release);
  Ok(id)
}

/// The entry of a registered shard.
pub fn entry(shard: u16) -> Option<&'static Entry> {
  let ptr = ENTRIES.get(usize::from(shard))?.load(Ordering::Acquire);
  // SAFETY: entries are leaked boxes, never freed, and a non-null pointer was stored by
  // `register` from a valid box.
  unsafe { ptr.as_ref() }
}

/// Publishes the running shard's context for the current thread (the shard loop calls this).
pub(crate) fn set_current(ctx: Option<NonNull<ShardContext>>) {
  CURRENT.with(|c| c.set(ctx));
}

/// Runs `f` with the current shard's context, if this thread runs a shard. The context's
/// surface is shared (`&self`); its mutable state sits behind its own borrow flag.
pub fn with_current<R>(f: impl FnOnce(&ShardContext) -> R) -> Option<R> {
  let ptr = CURRENT.with(Cell::get)?;
  // SAFETY: the pointer was published by the shard loop for its own thread and stays valid until
  // the loop clears it; only shared access is handed out.
  Some(f(unsafe { &*ptr.as_ptr() }))
}

/// The current shard's id, if this thread runs one.
pub fn current_shard() -> Option<u16> {
  with_current(|ctx| ctx.id)
}

/// Wakes the task named by `word` from wherever the caller is.
pub fn wake(word: Encoded) {
  let target = word.shard();
  let handled = with_current(|ctx| {
    if ctx.id == target {
      ctx.local.push(word.slot());
      true
    } else {
      ctx.send_to(target, Msg::Wake(word).into_word())
    }
  });
  if handled != Some(true) {
    send_foreign(target, Msg::Wake(word).into_word());
  }
}

/// Sends a message word to a shard from a foreign thread (or from a shard without a pair ring).
pub fn send_foreign(target: u16, word: u64) {
  let Some(entry) = entry(target) else { return };
  let mut pending = word;
  loop {
    match entry.inbound.push(pending) {
      Ok(()) => break,
      Err(back) => {
        pending = back;
        entry.ring_full_events.fetch_add(1, Ordering::Relaxed);
        entry.kick.kick();
        std::thread::yield_now();
      }
    }
  }
  entry.kick.kick();
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn registration_hands_out_distinct_ids_and_entries() {
    let a = register(8, Kick::none()).unwrap();
    let b = register(8, Kick::none()).unwrap();
    assert_ne!(a, b);
    assert!(entry(a).is_some());
    assert!(entry(b).is_some());
    assert_eq!(entry(a).unwrap().inbound.capacity(), 8);
  }

  #[test]
  fn a_wake_from_a_foreign_thread_lands_in_the_target_ring() {
    let id = register(4, Kick::none()).unwrap();
    let word = Encoded::pack(id, 5, 1).unwrap();
    wake(word);
    let mut consumer = entry(id).unwrap().inbound.consumer();
    // SAFETY: the word is a wake, which carries no box.
    let msg = unsafe { Msg::from_word(consumer.pop().unwrap()) };
    assert!(matches!(msg, Msg::Wake(w) if w == word));
    assert_eq!(current_shard(), None);
  }

  #[test]
  fn a_full_foreign_ring_spins_and_counts_without_losing_the_word() {
    let id = register(2, Kick::none()).unwrap();
    let entry = entry(id).unwrap();
    send_foreign(id, 1);
    send_foreign(id, 2);
    let filler = std::thread::spawn(move || send_foreign(id, 3));
    // Wait until the sender has spun at least once, then drain one slot so it can finish.
    while entry.ring_full_events.load(Ordering::Relaxed) == 0 {
      std::thread::yield_now();
    }
    let mut c = entry.inbound.consumer();
    assert_eq!(c.pop(), Some(1));
    filler.join().unwrap();
    assert_eq!(c.pop(), Some(2));
    assert_eq!(c.pop(), Some(3));
  }
}
