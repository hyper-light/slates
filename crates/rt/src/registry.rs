//! The process-wide shard registry: how a wake finds its target (§4.3, "wake from another shard
//! enqueues (slot, generation) on the target's ring and kicks the driver").
//!
//! Each shard registers into one of [`MAX_SHARDS`] **slots** and gives it back when its runtime
//! shuts down: a slot holds the shard's multi-producer wake ring (for foreign threads), the sending
//! end of its control channel, its kick, its parking word and its pulse, plus a `generation` word
//! that is **even while live and odd while free**. The slot's memory is process-static and is never
//! freed, so a waker that outlives its shard reads a live-or-free word, never freed memory; what a
//! stale waker finds is either a free slot (its wake is dropped and counted) or a *new* shard that
//! reused the slot (its wake is delivered and refused by that shard's task arena, whose generations
//! continue from where the old shard's ended — [`Slot::generation_base`] — so a stale word can
//! never name a live task). The kick descriptor is owned by the slot and closed at unregistration,
//! so a process that starts runtimes repeatedly holds exactly the descriptors of its live shards.
//! Before 2026-09-14 every registration leaked its entry, its descriptor and its context for the
//! process lifetime ("one runtime in production"), which filled the table at the 895th shard of a
//! test process and leaked two descriptors per shard
//! (`docs/bugs/2026-09-14-shard-registry-leaks-every-slot-for-the-process-lifetime.md`).
//!
//! Registration takes the lowest free slot under one atomic exchange on the slot's generation
//! (free → claimed), so concurrent runtimes never share a slot; lookup is one `Acquire` load of the
//! generation and a parity check. Shard-to-shard wakes take the single-producer ring of the
//! (source, target) pair, which the current shard's thread-local context holds, and never a
//! compare-and-swap.
//!
//! Routing: on the owning shard's thread the wake goes straight to the local queue; on another
//! shard's thread it goes to that pair's ring and kicks; on a foreign thread it goes to the
//! target's multi-producer ring and kicks. A full ring spins until the consumer drains it, and
//! counts the event: a wake to a live shard is never dropped.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

use slates_mem::{Encoded, MpscRing};

use crate::control::Control;
use crate::driver::Kick;
#[cfg(unix)]
use crate::driver::KickFd;
use crate::error::RtError;
use crate::parking::Parking;
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
  /// The task-arena generation the shard that holds this slot starts its handles from: one past the
  /// highest generation the previous holder issued, so a wake word minted for that shard can never
  /// match a task of this one (the slot-reuse safety, see the module doc). Zero for a first use.
  pub generation_base: u32,
  /// Set by a sender, cleared by the shard once the channel is drained: the shard polls the
  /// channel only when this says something was sent, one atomic load per step otherwise.
  pub control_pending: AtomicBool,
  /// The kick that wakes the shard's driver (its descriptor form points at `kick_fd` below).
  pub kick: Kick,
  /// The kick's descriptor, owned by this entry and closed at unregistration (Unix); the entry
  /// outlives the shard and is dropped only by the slot's next registration.
  #[cfg(unix)]
  pub kick_fd: Option<KickFd>,
  /// The single-producer rings this shard sends on, one per other shard of its runtime, owned here
  /// and lent as `&'static` to those shards' contexts; retired with the entry (every shard of a
  /// runtime unregisters after every thread of it joined, so no borrower outlives them).
  pub pair_rings: Vec<Box<slates_mem::SpscRing>>,
  /// How many times a producer found the ring full and had to spin (a tripwire, GAPS §7).
  pub ring_full_events: AtomicU64,
  /// The shard's parking announcement and the kicks it saved: a sender kicks only a parked
  /// shard, so a message to a spinning shard costs no syscall (§4.7 "Wake strategy"; the
  /// protocol and its loom model live in [`crate::parking`]).
  pub parking: Parking,
  /// The shard's forward-progress pulse, for an observer on any thread (see [`Pulse`]).
  pub pulse: Pulse,
}

/// A shard's forward-progress pulse, readable from any thread with no shard round-trip (§4.14; the same
/// discipline as the fleet coordinator's period count in `slates-server`): the loop's step count, its
/// driver-wait count, the tasks it has admitted and completed, the admissions it has **refused** because
/// its arena was full, and its longest single poll — stored by the owning shard from its own `Counters`
/// (which live behind the shard's single-threaded borrow) once per step. An observer reads them to tell a
/// shard that is stepping — alive, however slowly under CPU load — from one that has stopped: parked with
/// no kick (a wedge), or held inside one long poll (`longest_step_ns` climbs); and to tell a shard whose
/// task arena is saturating (`admission_refused` climbs, so a new operation's task cannot be spawned) from
/// one merely slow. It is the instrument a stall diagnosis needs precisely when the shard would not answer
/// a query. The only writer is the shard; `Relaxed` on every side, statistics (R2).
///
/// Shape: on its own cache line (the largest line we target, Apple silicon's 128 bytes) — the owning shard
/// stores every step, so the line must be shared with no word another thread writes (the control flag,
/// the ring's tail) or the shard would pay a transfer per step; a foreign read moves the line once.
#[repr(align(128))]
#[derive(Debug, Default)]
pub struct Pulse {
  steps: AtomicU64,
  waits: AtomicU64,
  spawns: AtomicU64,
  completed: AtomicU64,
  admission_refused: AtomicU64,
  longest_step_ns: AtomicU64,
}

impl Pulse {
  /// The owning shard records its step and task counts after a step (one plain store each, a line it owns).
  pub fn record(
    &self,
    steps: u64,
    spawns: u64,
    completed: u64,
    admission_refused: u64,
    longest_step_ns: u64,
  ) {
    self.steps.store(steps, Ordering::Relaxed);
    self.spawns.store(spawns, Ordering::Relaxed);
    self.completed.store(completed, Ordering::Relaxed);
    self
      .admission_refused
      .store(admission_refused, Ordering::Relaxed);
    self
      .longest_step_ns
      .store(longest_step_ns, Ordering::Relaxed);
  }

  /// The owning shard records its driver-wait count as it enters a wait.
  pub fn record_waits(&self, waits: u64) {
    self.waits.store(waits, Ordering::Relaxed);
  }

  /// Loop iterations the shard has run.
  pub fn steps(&self) -> u64 {
    self.steps.load(Ordering::Relaxed)
  }

  /// Driver waits the shard has entered.
  pub fn waits(&self) -> u64 {
    self.waits.load(Ordering::Relaxed)
  }

  /// Tasks the shard has admitted to its arena.
  pub fn spawns(&self) -> u64 {
    self.spawns.load(Ordering::Relaxed)
  }

  /// Tasks whose future returned on the shard.
  pub fn completed(&self) -> u64 {
    self.completed.load(Ordering::Relaxed)
  }

  /// Admissions the shard refused because its task arena was full (the operation's task could not spawn).
  pub fn admission_refused(&self) -> u64 {
    self.admission_refused.load(Ordering::Relaxed)
  }

  /// The shard's longest single poll, nanoseconds (a step longer than a peer's wake starves the shard).
  pub fn longest_step_ns(&self) -> u64 {
    self.longest_step_ns.load(Ordering::Relaxed)
  }
}

/// One registry slot: its generation word and the entry it currently holds. The generation is odd
/// while the slot is free (initially 1) and even while a shard holds it; claiming a slot is one
/// `compare_exchange` from its free value to that value plus one, so two registrations never take
/// one slot. The entry is published under the same word: written before the even generation is
/// stored (`Release`), read after it is loaded (`Acquire`).
struct Slot {
  generation: AtomicU32,
  entry: std::sync::atomic::AtomicPtr<Entry>,
  /// The highest task-arena generation a holder of this slot has issued, carried to the next
  /// holder as its base (see [`Entry::generation_base`]).
  arena_generation: AtomicU32,
  /// Wakes that found the slot free (a waker outliving its shard), for a tripwire.
  stale_wakes: AtomicU64,
}

impl Slot {
  const fn new() -> Slot {
    Slot {
      generation: AtomicU32::new(1),
      entry: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
      arena_generation: AtomicU32::new(0),
      stale_wakes: AtomicU64::new(0),
    }
  }
}

static SLOTS: [Slot; MAX_SHARDS] = [const { Slot::new() }; MAX_SHARDS];

thread_local! {
  static CURRENT: Cell<Option<&'static ShardContext>> = const { Cell::new(None) };
}

/// Registers a new shard with a wake ring of `ring_entries` words, a control channel bounded at
/// `control_bound`, and its kick; returns the id and the control channel's receiving end. Takes the
/// lowest free slot; refuses `TooManyShards` when every slot holds a live shard.
pub fn register(
  ring_entries: usize,
  control_bound: usize,
  kick: RegisterKick,
) -> Result<(u16, Receiver<Control>), RtError> {
  let max = u16::try_from(MAX_SHARDS).unwrap_or(u16::MAX);
  for (index, slot) in SLOTS.iter().enumerate() {
    let free = slot.generation.load(Ordering::Acquire);
    if free & 1 == 0 {
      continue;
    }
    // Claim: free (odd) → claimed (the next even). A loser sees the even value and moves on.
    let live = free.wrapping_add(1);
    if slot
      .generation
      .compare_exchange(free, live, Ordering::AcqRel, Ordering::Acquire)
      .is_err()
    {
      continue;
    }
    let id = u16::try_from(index).unwrap_or(u16::MAX);
    let (control, receiver) = sync_channel(control_bound.max(1));
    let inbound = match MpscRing::new(ring_entries) {
      Ok(ring) => ring,
      Err(e) => {
        // Give the slot back before refusing.
        slot
          .generation
          .store(live.wrapping_add(1), Ordering::Release);
        return Err(e.into());
      }
    };
    let mut entry = Box::new(Entry {
      inbound,
      control,
      generation_base: slot.arena_generation.load(Ordering::Acquire),
      control_pending: AtomicBool::new(false),
      kick: Kick::None,
      #[cfg(unix)]
      kick_fd: None,
      pair_rings: Vec::new(),
      ring_full_events: AtomicU64::new(0),
      parking: Parking::new(),
      pulse: Pulse::default(),
    });
    // The kick's descriptor form points into this very box: the box's address is stable for the
    // entry's life, and the entry is dropped only after the slot's generation moved past every
    // reader (see `entry`). So the `&'static` is a promise the slot protocol keeps, not a leak.
    entry.kick = match kick {
      RegisterKick::Kick(kick) => kick,
      #[cfg(unix)]
      RegisterKick::Descriptor(fd, form) => {
        let owned: &KickFd = entry.kick_fd.insert(KickFd::new(fd));
        // SAFETY: extends the borrow to `'static`: `owned` lives in the boxed entry, whose
        // allocation is never moved (it is reached only through the raw pointer stored in the slot)
        // and never freed while any reader can observe it (the generation protocol in `entry`).
        let owned: &'static KickFd = unsafe { &*(owned as *const KickFd) };
        form(owned)
      }
    };
    // The previous holder's entry, if the slot was reused: retired at unregistration, dropped
    // here — after the generation moved past it, so no reader holds it (a reader validates the
    // generation before and after its use of the entry; see `entry`).
    let previous = slot.entry.swap(Box::into_raw(entry), Ordering::AcqRel);
    if !previous.is_null() {
      // SAFETY: `previous` was produced by `Box::into_raw` in an earlier `register` of this slot,
      // and was retired by `unregister` (the slot's generation went odd) before this claim moved
      // it even again; every reader checks the generation around its use, so none holds it now.
      drop(unsafe { Box::from_raw(previous) });
    }
    return Ok((id, receiver));
  }
  Err(RtError::TooManyShards { max })
}

/// What a registration hands the slot for its kick: a ready [`Kick`] (the simulation's shared flag,
/// Windows' completion port, or none), or a descriptor the slot takes ownership of together with the
/// kick form to mint over it (Unix: an eventfd or a kqueue).
pub enum RegisterKick {
  /// A kick that owns nothing the slot must close.
  Kick(Kick),
  /// A descriptor the slot owns; the function mints the kick over the slot's owned form of it.
  #[cfg(unix)]
  Descriptor(std::os::fd::OwnedFd, fn(&'static KickFd) -> Kick),
}

/// Records the highest task-arena generation a shard issued (the shard's own thread, at its loop's
/// exit), so the slot's next holder starts past it.
pub fn note_arena_generation(shard: u16, high: u32) {
  if let Some(slot) = SLOTS.get(usize::from(shard)) {
    slot.arena_generation.fetch_max(high, Ordering::AcqRel);
  }
}

/// Gives a shard's slot back once its thread has ended (the runtime calls this after the join):
/// closes the kick's descriptor and marks the slot free (its generation goes odd). The entry stays allocated — a waker that outlives
/// the shard may still read it under a stale generation and must find valid memory — and is dropped
/// by the next registration of the slot.
pub fn unregister(shard: u16) {
  let Some(slot) = SLOTS.get(usize::from(shard)) else {
    return;
  };
  let live = slot.generation.load(Ordering::Acquire);
  if live & 1 == 1 {
    return; // already free
  }
  let entry = slot.entry.load(Ordering::Acquire);
  if !entry.is_null() {
    // SAFETY: the pointer came from `Box::into_raw` in `register` and is dropped only by a later
    // `register` of this slot, which cannot run before the generation goes odd below; the shard's
    // own thread has ended (the caller joined it), so this is the one live borrow.
    let entry = unsafe { &*entry };
    entry.kick.close();
  }
  slot
    .generation
    .store(live.wrapping_add(1), Ordering::Release);
}

/// The entry of a live shard: `None` for a free slot. The entry's memory is valid for the process
/// lifetime (a retired entry is dropped only by the slot's next registration, after its generation
/// moved on), so a caller that races a shutdown reads a retired-but-valid entry whose kick is closed
/// and whose control channel is disconnected — both inert by construction.
pub fn entry(shard: u16) -> Option<&'static Entry> {
  let slot = SLOTS.get(usize::from(shard))?;
  if slot.generation.load(Ordering::Acquire) & 1 == 1 {
    return None;
  }
  let entry = slot.entry.load(Ordering::Acquire);
  if entry.is_null() {
    return None;
  }
  // SAFETY: a non-null entry pointer was produced by `Box::into_raw` in `register` and is freed only
  // by a later `register` of the same slot; that `register` runs only after `unregister` turned the
  // generation odd, and we read an even generation just above — a shard that unregisters and a new
  // one that re-registers between our two loads leaves a valid (retired or fresh) entry either way,
  // because the swap in `register` frees the *previous* entry only after installing the new one.
  Some(unsafe { &*entry })
}

/// Stores a pair ring in `shard`'s entry and lends it for the entry's life (see
/// `Entry::pair_rings`); `None` for a free slot.
pub fn lend_pair_ring(
  shard: u16,
  ring: slates_mem::SpscRing,
) -> Option<&'static slates_mem::SpscRing> {
  let slot = SLOTS.get(usize::from(shard))?;
  if slot.generation.load(Ordering::Acquire) & 1 == 1 {
    return None;
  }
  let entry = slot.entry.load(Ordering::Acquire);
  if entry.is_null() {
    return None;
  }
  // SAFETY: the entry is valid (see `entry`); the rings are pushed only here, by the registering
  // thread before any shard thread of the runtime starts (`connect_pairs` runs before the spawns),
  // so no other reference to `pair_rings` exists yet, and the boxed ring's address is stable for the
  // entry's life.
  let entry = unsafe { &mut *entry };
  entry.pair_rings.push(Box::new(ring));
  let lent: &slates_mem::SpscRing = entry.pair_rings.last()?;
  // SAFETY: lifetime extension by the same argument as the kick: the box is never moved or freed
  // while a borrower can exist.
  Some(unsafe { &*(lent as *const slates_mem::SpscRing) })
}

/// The wakes that found their target's slot free (a waker outlived its shard); a tripwire, never a
/// fault: the word it carried had no task to reach.
pub fn stale_wakes(shard: u16) -> u64 {
  SLOTS
    .get(usize::from(shard))
    .map_or(0, |slot| slot.stale_wakes.load(Ordering::Relaxed))
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
  let Some(entry) = entry(target) else {
    if let Some(slot) = SLOTS.get(usize::from(target)) {
      slot.stale_wakes.fetch_add(1, Ordering::Relaxed);
    }
    return;
  };
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
  entry.parking.kick_if_parked(|| entry.kick.kick());
}

/// Sends a control message to a shard from any thread and kicks it; refused when the shard's
/// control channel is full or the shard is gone.
pub fn send_control(target: u16, message: Control) -> Result<(), RtError> {
  let entry = entry(target).ok_or(RtError::ShardGone { shard: target })?;
  match entry.control.try_send(message) {
    Ok(()) => {
      entry.control_pending.store(true, Ordering::SeqCst);
      entry.parking.kick_if_parked(|| entry.kick.kick());
      Ok(())
    }
    Err(TrySendError::Full(_)) => Err(RtError::ControlFull { shard: target }),
    Err(TrySendError::Disconnected(_)) => Err(RtError::ShardGone { shard: target }),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The slot protocol under contention (a stand-in until the loom lane covers this crate, which is
  /// owed): many threads register, wake and unregister against a handful of slots at once; every
  /// wake either lands in a live ring or is counted stale — never a fault — and every slot ends free
  /// with no entry lost (each registration is unregistered by its own thread). Do: 16 threads × 200
  /// register/wake/unregister cycles. Expect: no panic, every wake accounted for, every slot free.
  #[test]
  fn registrations_wakes_and_unregistrations_interleave_without_a_fault() {
    const THREADS: usize = 16;
    const CYCLES: usize = 200;
    // A process-static counter the threads share (R2: no `Arc`; a `&'static` is the sharing form).
    static WAKES_LANDED: AtomicU64 = AtomicU64::new(0);
    let landed: &'static AtomicU64 = &WAKES_LANDED;
    let handles: Vec<_> = (0..THREADS)
      .map(|_| {
        std::thread::spawn(move || {
          for _ in 0..CYCLES {
            let (id, receiver) = register(4, 2, RegisterKick::Kick(Kick::none())).unwrap();
            // A foreign wake to our own slot lands in its ring (drained by the "shard": this thread).
            send_foreign(id, 7);
            if let Some(entry) = entry(id) {
              let mut ring = entry.inbound.consumer();
              while ring.pop().is_some() {
                landed.fetch_add(1, Ordering::Relaxed);
              }
            }
            // A wake to a slot another thread may have freed meanwhile is counted, never a fault.
            send_foreign(
              id.wrapping_add(1) % u16::try_from(MAX_SHARDS).unwrap_or(u16::MAX),
              9,
            );
            let _ = receiver.try_recv();
            unregister(id);
          }
        })
      })
      .collect();
    for handle in handles {
      handle.join().unwrap();
    }
    assert_eq!(
      landed.load(Ordering::Relaxed),
      u64::try_from(THREADS * CYCLES).unwrap(),
      "every wake to a live slot landed"
    );
    let live = SLOTS
      .iter()
      .filter(|slot| slot.generation.load(Ordering::Acquire) & 1 == 0)
      .count();
    assert_eq!(live, 0, "every slot was given back");
  }

  #[test]
  fn registration_hands_out_distinct_ids_and_entries() {
    let (a, _ra) = register(8, 4, RegisterKick::Kick(Kick::none())).unwrap();
    let (b, _rb) = register(8, 4, RegisterKick::Kick(Kick::none())).unwrap();
    assert_ne!(a, b);
    assert!(entry(a).is_some());
    assert!(entry(b).is_some());
    assert_eq!(entry(a).unwrap().inbound.capacity(), 8);
  }

  #[test]
  fn a_wake_from_a_foreign_thread_lands_in_the_target_ring() {
    let (id, _receiver) = register(4, 4, RegisterKick::Kick(Kick::none())).unwrap();
    let word = Encoded::pack(id, 5, 1).unwrap();
    wake(word);
    let mut consumer = entry(id).unwrap().inbound.consumer();
    assert_eq!(consumer.pop(), Some(word.word()));
    assert_eq!(current_shard(), None);
  }

  #[test]
  fn a_full_foreign_ring_spins_and_counts_without_losing_the_word() {
    let (id, _receiver) = register(2, 4, RegisterKick::Kick(Kick::none())).unwrap();
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
    let (id, receiver) = register(2, 1, RegisterKick::Kick(Kick::none())).unwrap();
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
