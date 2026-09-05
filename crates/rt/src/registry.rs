//! The process-wide shard registry: how a wake finds its target (§4.3, "wake from another shard
//! enqueues (slot, generation) on the target's ring and kicks the driver").
//!
//! Each shard registers once: a leaked entry holding its multi-producer wake ring (for foreign
//! threads), the sending end of its control channel, and its kick. Entries are never freed, so a
//! waker that outlives its shard kicks a closed driver and is ignored rather than touching freed
//! memory; the leak is bounded by the number of shards a process ever creates (one runtime in
//! production; a handful in tests). The table is an array of `OnceLock`s, so registration and
//! lookup are safe code. Shard-to-shard wakes take the single-producer ring of the (source,
//! target) pair, which the current shard's thread-local context holds, and never a
//! compare-and-swap.
//!
//! Routing: on the owning shard's thread the wake goes straight to the local queue; on another
//! shard's thread it goes to that pair's ring and kicks; on a foreign thread it goes to the
//! target's multi-producer ring and kicks. A full ring spins until the consumer drains it, and
//! counts the event: a wake is never dropped.

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

use slates_mem::{Encoded, MpscRing};

use crate::control::Control;
use crate::driver::Kick;
use crate::error::RtError;
use crate::shard::ShardContext;

/// Shape: the bound on shard ids per process: more than any host's core count, few enough that
/// the registry is a small static table and a packed word's top bits stay free.
pub const MAX_SHARDS: usize = 1024;

/// A registered shard: its foreign wake ring, its control channel and its kick.
#[derive(Debug)]
pub struct Entry {
  /// The wake ring foreign threads push to.
  pub inbound: MpscRing,
  /// The control channel's sending end.
  pub control: SyncSender<Control>,
  /// Set by a sender, cleared by the shard once the channel is drained: the shard polls the
  /// channel only when this says something was sent, one atomic load per step otherwise.
  pub control_pending: AtomicBool,
  /// The kick that wakes the shard's driver.
  pub kick: Kick,
  /// How many times a producer found the ring full and had to spin (a tripwire, GAPS §7).
  pub ring_full_events: AtomicU64,
}

static ENTRIES: [OnceLock<&'static Entry>; MAX_SHARDS] = [const { OnceLock::new() }; MAX_SHARDS];
static NEXT_SHARD: AtomicU16 = AtomicU16::new(0);

thread_local! {
  static CURRENT: Cell<Option<&'static ShardContext>> = const { Cell::new(None) };
}

/// Registers a new shard with a wake ring of `ring_entries` words, a control channel bounded at
/// `control_bound`, and its kick; returns the id and the control channel's receiving end.
pub fn register(
  ring_entries: usize,
  control_bound: usize,
  kick: Kick,
) -> Result<(u16, Receiver<Control>), RtError> {
  let max = u16::try_from(MAX_SHARDS).unwrap_or(u16::MAX);
  let id = NEXT_SHARD.fetch_add(1, Ordering::AcqRel);
  if usize::from(id) >= MAX_SHARDS {
    return Err(RtError::TooManyShards { max });
  }
  let (control, receiver) = sync_channel(control_bound.max(1));
  let entry: &'static Entry = Box::leak(Box::new(Entry {
    inbound: MpscRing::new(ring_entries)?,
    control,
    control_pending: AtomicBool::new(false),
    kick,
    ring_full_events: AtomicU64::new(0),
  }));
  let _ = ENTRIES[usize::from(id)].set(entry);
  Ok((id, receiver))
}

/// The entry of a registered shard.
pub fn entry(shard: u16) -> Option<&'static Entry> {
  ENTRIES.get(usize::from(shard))?.get().copied()
}

/// Publishes the running shard's context for the current thread (the shard loop calls this).
pub(crate) fn set_current(ctx: Option<&'static ShardContext>) {
  CURRENT.with(|c| c.set(ctx));
}

/// Runs `f` with the current shard's context, if this thread runs a shard.
pub fn with_current<R>(f: impl FnOnce(&ShardContext) -> R) -> Option<R> {
  CURRENT.with(Cell::get).map(f)
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
      ctx.send_to(target, word.word())
    }
  });
  if handled != Some(true) {
    send_foreign(target, word.word());
  }
}

/// Sends a wake word to a shard from a foreign thread (or from a shard without a pair ring).
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

/// Sends a control message to a shard from any thread and kicks it; refused when the shard's
/// control channel is full or the shard is gone.
pub fn send_control(target: u16, message: Control) -> Result<(), RtError> {
  let entry = entry(target).ok_or(RtError::ShardGone { shard: target })?;
  match entry.control.try_send(message) {
    Ok(()) => {
      entry.control_pending.store(true, Ordering::Release);
      entry.kick.kick();
      Ok(())
    }
    Err(TrySendError::Full(_)) => Err(RtError::ControlFull { shard: target }),
    Err(TrySendError::Disconnected(_)) => Err(RtError::ShardGone { shard: target }),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn registration_hands_out_distinct_ids_and_entries() {
    let (a, _ra) = register(8, 4, Kick::none()).unwrap();
    let (b, _rb) = register(8, 4, Kick::none()).unwrap();
    assert_ne!(a, b);
    assert!(entry(a).is_some());
    assert!(entry(b).is_some());
    assert_eq!(entry(a).unwrap().inbound.capacity(), 8);
  }

  #[test]
  fn a_wake_from_a_foreign_thread_lands_in_the_target_ring() {
    let (id, _receiver) = register(4, 4, Kick::none()).unwrap();
    let word = Encoded::pack(id, 5, 1).unwrap();
    wake(word);
    let mut consumer = entry(id).unwrap().inbound.consumer();
    assert_eq!(consumer.pop(), Some(word.word()));
    assert_eq!(current_shard(), None);
  }

  #[test]
  fn a_full_foreign_ring_spins_and_counts_without_losing_the_word() {
    let (id, _receiver) = register(2, 4, Kick::none()).unwrap();
    let entry = entry(id).unwrap();
    send_foreign(id, 1);
    send_foreign(id, 2);
    let filler = std::thread::spawn(move || send_foreign(id, 3));
    while entry.ring_full_events.load(Ordering::Relaxed) == 0 {
      std::thread::yield_now();
    }
    let mut c = entry.inbound.consumer();
    assert_eq!(c.pop(), Some(1));
    filler.join().unwrap();
    assert_eq!(c.pop(), Some(2));
    assert_eq!(c.pop(), Some(3));
  }

  #[test]
  fn control_is_refused_when_the_channel_is_full_or_the_shard_is_gone() {
    let (id, receiver) = register(2, 1, Kick::none()).unwrap();
    send_control(id, Control::Active(true)).unwrap();
    assert!(matches!(
      send_control(id, Control::Shutdown),
      Err(RtError::ControlFull { .. })
    ));
    drop(receiver);
    assert!(matches!(
      send_control(id, Control::Shutdown),
      Err(RtError::ShardGone { .. })
    ));
    assert!(matches!(
      send_control(u16::MAX, Control::Shutdown),
      Err(RtError::ShardGone { .. })
    ));
  }
}
