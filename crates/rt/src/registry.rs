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
//! (`docs/bugs/2026-09-14-shard-registry-leaks-every-slot-for-the-process-lifetime.md`). The
//! context — the task arena, run queue and timer wheel sized to the shard's task budget — is owned
//! by the slot from its build and freed by its own thread when its loop ends ([`reclaim_context`]);
//! it needs no retirement because nothing foreign ever dereferences it.
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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence};
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
  /// The registration that owns this entry; checked under the reader pin.
  holder: SlotHolder,
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
  /// The generational kick that wakes this registration's driver.
  pub kick: Kick,
  /// The kick descriptor, closed after the owning contexts and foreign borrows end (Unix).
  #[cfg(unix)]
  pub(crate) kick_fd: Option<std::os::fd::OwnedFd>,
  /// A simulated shard's flags, owned until its contexts and foreign kick borrows end.
  /// A copied kick carries the registration, never a reference to these flags.
  pub sim_shared: Option<Box<crate::sim::SimShared>>,
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
  /// Set by the shard at its loop's exit ([`note_exited`]): its rings will never be drained again, so
  /// a sender that finds one full stops spinning and counts the wake stale instead of waiting for a
  /// consumer that is gone (the livelock the registry stress test found on 2026-09-14).
  pub exited: AtomicBool,
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
  /// An application loop's own forward-progress count on this shard (the fleet coordinator's period
  /// count), bumped by the shard and read by an observer on any thread — kept on the entry so it
  /// needs no allocation of its own and outlives the shard as the entry does.
  progress: AtomicU64,
  /// The shard's measured scheduler overrun (`ShardContext::scheduler_overrun_ns`), mirrored here at
  /// each wait it folds in, so a stall diagnosis on another thread can tell a shard the operating
  /// system is not scheduling (this climbs) from one held inside its own work (it does not).
  scheduler_overrun_ns: AtomicU64,
  /// The shard's online wake estimate (`ShardContext::wake_cost_ns`), mirrored at each wake it folds
  /// in, for a reader on another thread (the daemon's status, a stall diagnosis).
  wake_cost_ns: AtomicU64,
}

impl Pulse {
  /// The owning shard mirrors its measured scheduler overrun after folding a wait into it.
  pub fn record_scheduler_overrun(&self, overrun_ns: u64) {
    self
      .scheduler_overrun_ns
      .store(overrun_ns, Ordering::Relaxed);
  }

  /// The shard's measured scheduler overrun, nanoseconds (see [`Pulse::record_scheduler_overrun`]).
  pub fn scheduler_overrun_ns(&self) -> u64 {
    self.scheduler_overrun_ns.load(Ordering::Relaxed)
  }

  /// The owning shard mirrors its online wake estimate after folding a wake into it.
  pub fn record_wake_cost(&self, wake_cost_ns: u64) {
    self.wake_cost_ns.store(wake_cost_ns, Ordering::Relaxed);
  }

  /// The shard's online wake estimate, nanoseconds; 0 until its first measured wake (see
  /// [`Pulse::record_wake_cost`]).
  pub fn wake_cost_ns(&self) -> u64 {
    self.wake_cost_ns.load(Ordering::Relaxed)
  }

  /// The owning shard's application loop marks one period of forward progress.
  pub fn beat(&self) {
    self.progress.fetch_add(1, Ordering::Relaxed);
  }

  /// Periods the shard's application loop has completed (see [`Pulse::beat`]).
  pub fn progress(&self) -> u64 {
    self.progress.load(Ordering::Relaxed)
  }

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
/// one slot. Claiming precedes publication; the entry pointer stays null until initialization
/// finishes, then is published with Release and read under a counted borrow.
/// Shape: one slot per cache line (the largest line we target, Apple silicon's 128 bytes): the
/// generation and the reader count are written by every foreign waker of that shard, so two shards'
/// slots on one line would bounce it between their wakers (vorpal measured four global atomics doubling
/// kernel-scale CPU on ping-pong). 1,024 slots make a 128 KiB static.
#[repr(align(128))]
struct Slot {
  generation: AtomicU32,
  /// Foreign readers inside [`with_entry`]. Retirement clears the pointer, then waits for
  /// zero before freeing the entry, including when a borrower was descheduled.
  readers: AtomicU32,
  entry: std::sync::atomic::AtomicPtr<Entry>,
  /// The shard's context, owned here from its build until its owning thread reclaims it at the
  /// loop's end ([`reclaim_context`]): the raw pointer `Box::into_raw` produced, kept whole so the
  /// box is freed with the provenance it was made with. Null while no context is attached. Only the
  /// owning thread stores or takes it — a context is that thread's (`!Send`), and nothing foreign
  /// dereferences one: a wake routes by id through the entry, never through the context.
  context: std::sync::atomic::AtomicPtr<ShardContext>,
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
      readers: AtomicU32::new(0),
      entry: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
      context: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
      arena_generation: AtomicU32::new(0),
      stale_wakes: AtomicU64::new(0),
    }
  }
}

/// Contexts reclaimed by their owning threads since the process started (see [`reclaim_context`]);
/// a test's non-vacuity counter that a shut-down runtime's context heap was given back.
static CONTEXTS_RECLAIMED: AtomicU64 = AtomicU64::new(0);

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
    let holder = SlotHolder {
      shard: id,
      generation: live,
    };
    let mut entry = Box::new(Entry {
      holder,
      inbound,
      control,
      generation_base: slot.arena_generation.load(Ordering::Acquire),
      control_pending: AtomicBool::new(false),
      kick: Kick::None,
      #[cfg(unix)]
      kick_fd: None,
      sim_shared: None,
      pair_rings: Vec::new(),
      ring_full_events: AtomicU64::new(0),
      parking: Parking::new(),
      pulse: Pulse::default(),
      exited: AtomicBool::new(false),
    });
    entry.kick = match kick {
      RegisterKick::Kick(kick) => kick,
      #[cfg(unix)]
      RegisterKick::Descriptor(fd, form) => {
        entry.kick_fd = Some(fd);
        form(KickFd::new(holder))
      }
      RegisterKick::Sim(shared) => {
        entry.sim_shared = Some(shared);
        Kick::Sim(holder)
      }
    };
    // Unregistration clears the pointer and waits out readers before publishing a free slot.
    // Store only the raw pointer: moving the Box after lending fields would invalidate borrows.
    slot.entry.store(Box::into_raw(entry), Ordering::Release);
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
  Descriptor(std::os::fd::OwnedFd, fn(KickFd) -> Kick),
  /// A simulated shard's driver flags, owned by the slot; the kick is minted over them.
  Sim(Box<crate::sim::SimShared>),
}

/// Records the highest task-arena generation a shard issued (the shard's own thread, at its loop's
/// exit), so the slot's next holder starts past it.
pub fn note_arena_generation(shard: u16, high: u32) {
  if let Some(slot) = SLOTS.get(usize::from(shard)) {
    slot.arena_generation.fetch_max(high, Ordering::AcqRel);
  }
}

/// Hands `shard`'s slot the context its owning thread just built (`ShardContext::build`), as the raw
/// pointer `Box::into_raw` produced, so the same thread can free it at the loop's end with the
/// provenance it was made with ([`reclaim_context`]). Called once per build, on the owning thread.
pub(crate) fn attach_context(shard: u16, context: *mut ShardContext) {
  if let Some(slot) = SLOTS.get(usize::from(shard)) {
    slot.context.store(context, Ordering::Release);
  }
}

/// Frees `shard`'s context: **only its owning thread calls this, after its loop has returned** (the
/// multi-thread runtime's worker after `run`, `LocalRuntime` and `SimRuntime` in their `Drop`). At that
/// point the context has no other holder: the loop cleared the thread's current-context cell, its task
/// arena is empty (the loop exits only once every task, including the cancelled ones, is dropped), and
/// nothing foreign ever dereferences a context — a waker carries a packed word and routes by shard id
/// through the entry, and cross-shard rings are owned by entries, not contexts. The entry itself is
/// untouched here (retirement waits for foreign readers in [`unregister`]). Idempotent: a slot with no attached context is a no-op. Before
/// 2026-09-14 every build leaked its context for the process lifetime — a task arena, run queue and
/// timer wheel each sized to the shard's task budget, per shard, per runtime start.
pub fn reclaim_context(shard: u16) {
  let Some(slot) = SLOTS.get(usize::from(shard)) else {
    return;
  };
  let context = slot.context.swap(std::ptr::null_mut(), Ordering::AcqRel);
  if context.is_null() {
    return;
  }
  // Nothing will drain this shard's rings again: a sender spinning on a full one must stop.
  note_exited(shard);
  // A step leaves this thread's current-context cell pointing at the context it stepped (`run` and
  // `run_until_idle` clear it at their exit, a bare `step` does not), so a same-thread runtime — a
  // `LocalRuntime`, a simulation — could be dropped with the cell still naming the context freed
  // here, and the thread's next wake would dereference freed memory. Cleared here, once, for every
  // owner: the cell is this thread's, and the context is this thread's to free.
  CURRENT.with(|current| {
    if current
      .get()
      .is_some_and(|ctx| std::ptr::eq(ctx, context.cast_const()))
    {
      current.set(None);
    }
  });
  // SAFETY: `context` is the pointer `Box::into_raw` produced in `ShardContext::build`, stored by
  // `attach_context` and taken back exactly once here (the swap leaves null). The caller is the
  // context's owning thread after its loop returned, so no reference to the context is live (see the
  // doc above), and this thread may drop the `!Send` value it built.
  drop(unsafe { Box::from_raw(context) });
  CONTEXTS_RECLAIMED.fetch_add(1, Ordering::Relaxed);
}

/// Contexts freed by their owning threads since the process started (a shut-down runtime's per-shard
/// context heap given back): a test's non-vacuity counter.
pub fn contexts_reclaimed() -> u64 {
  CONTEXTS_RECLAIMED.load(Ordering::Relaxed)
}

/// Retires a shard after all of its runtime's contexts have ended. Remove the entry from
/// lookup, wait for foreign borrowers, then drop its resources and publish the free generation.
/// A new registration cannot claim the slot before retirement has finished (§4.3).
pub fn unregister(shard: u16) {
  let Some(slot) = SLOTS.get(usize::from(shard)) else {
    return;
  };
  let live = slot.generation.load(Ordering::Acquire);
  if live & 1 == 1 {
    return;
  }
  note_exited(shard);
  let retired = slot.entry.swap(std::ptr::null_mut(), Ordering::SeqCst);
  fence(Ordering::SeqCst);
  if !retired.is_null() {
    while slot.readers.load(Ordering::SeqCst) != 0 {
      std::thread::yield_now();
    }
    // SAFETY: this pointer came from Box::into_raw in register. All owning contexts ended,
    // and the swap/fence/readers protocol excludes every foreign borrow of the retired entry.
    // New readers see null. The generation remains claimed until the resources are dropped.
    drop(unsafe { Box::from_raw(retired) });
  }
  slot
    .generation
    .store(live.wrapping_add(1), Ordering::Release);
}

/// Runs `f` on the live entry of `shard` — the form every **foreign** reader uses (a wake or a control
/// message from another thread, an observer reading the pulse or copying the kick): the slot counts the
/// reader for the call's span. Unregistration frees an entry only after its pointer was removed
/// and no reader is counted, so `f` never sees freed memory however long its thread is
/// descheduled. `None` for a free slot (a wake to it is stale). The shard's own context holds an
/// unguarded reference to its entry instead ([`entry`]): its slot cannot be re-registered while it
/// lives, since unregistration follows its thread's join.
pub fn with_entry<R>(shard: u16, f: impl FnOnce(&Entry) -> R) -> Option<R> {
  let slot = SLOTS.get(usize::from(shard))?;
  slot.readers.fetch_add(1, Ordering::SeqCst);
  fence(Ordering::SeqCst);
  let _reader = Reader(&slot.readers);
  read_counted(slot, f)
}

/// Release the reader pin even when a test callback unwinds.
struct Reader<'a>(&'a AtomicU32);
impl Drop for Reader<'_> {
  fn drop(&mut self) {
    self.0.fetch_sub(1, Ordering::SeqCst);
  }
}

/// A pinned read of one particular registration, never a replacement in the same slot.
pub(crate) fn with_holder<R>(holder: SlotHolder, f: impl FnOnce(&Entry) -> R) -> Option<R> {
  with_entry(holder.shard, |entry| {
    (entry.holder == holder).then(|| f(entry))
  })
  .flatten()
}

/// The counted read itself: the generation check, the pointer load and the call, between the reader
/// count's increment and decrement in [`with_entry`].
fn read_counted<R>(slot: &Slot, f: impl FnOnce(&Entry) -> R) -> Option<R> {
  if slot.generation.load(Ordering::Acquire) & 1 == 1 {
    return None;
  }
  let entry = slot.entry.load(Ordering::SeqCst);
  if entry.is_null() {
    return None;
  }
  // SAFETY: a non-null entry pointer was produced by `Box::into_raw` in `register` and is freed only
  // by `unregister`, which waits until the slot's reader count is zero after clearing
  // the pointer; this reader was counted before the pointer load, under the fence protocol
  // described at that wait, so the entry it loaded is valid for the span of `f`.
  Some(f(unsafe { &*entry }))
}

/// Marks `shard`'s entry exited (the shard's own thread, at its loop's exit): a sender that finds its
/// ring full stops spinning, since nothing will drain it again.
pub fn note_exited(shard: u16) {
  let _ = with_entry(shard, |entry| entry.exited.store(true, Ordering::Release));
}

/// The entry of a live shard, unguarded: for the shard's **own** thread (its context keeps the
/// reference for its life; its slot cannot be re-registered before its thread has ended and joined)
/// and for a runtime building its shards before any of them runs. A foreign reader uses
/// [`with_entry`]. `None` for a free slot.
pub fn entry(shard: u16) -> Option<&'static Entry> {
  let slot = SLOTS.get(usize::from(shard))?;
  if slot.generation.load(Ordering::Acquire) & 1 == 1 {
    return None;
  }
  let entry = slot.entry.load(Ordering::SeqCst);
  if entry.is_null() {
    return None;
  }
  // SAFETY: the caller is the owning shard or its bootstrap thread (the contract above).
  // Unregistration cannot run until all owning contexts ended, so this entry outlives its
  // own shard's reference. Foreign threads must use with_entry, which pins reclamation.
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
  let entry = slot.entry.load(Ordering::SeqCst);
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
      return true;
    }
    match ctx.send_to(target, word.word()) {
      PairSend::Sent | PairSend::Gone => true,
      // No pair ring to that shard (another runtime's): the multi-producer path, but as a shard
      // must take it — draining its own rings while the target's is full, never blocking as a
      // plain foreign thread may.
      PairSend::NoRing => {
        ctx.send_foreign_draining(target, word.word());
        true
      }
    }
  });
  if handled != Some(true) {
    send_foreign(target, word.word());
  }
}

/// The outcome of a shard's send over a pair ring (`ShardContext::send_to`).
#[derive(Debug, PartialEq, Eq)]
pub enum PairSend {
  /// The word landed on the pair ring and the target was kicked.
  Sent,
  /// This shard keeps no pair ring to `target` (a shard of another runtime): use the foreign path.
  NoRing,
  /// The target left its loop or its slot is free: the wake was counted stale.
  Gone,
}

/// Sends a wake word to a shard from a foreign thread (or from a shard without a pair ring).
pub fn send_foreign(target: u16, word: u64) {
  let mut pending = word;
  loop {
    match try_send_foreign(target, pending) {
      TrySend::Landed | TrySend::Gone => return,
      TrySend::Full(back) => {
        pending = back;
        std::thread::yield_now();
      }
    }
  }
}

/// One turn of a foreign send: the word landed in the target's ring (and the target was kicked if it
/// was parked); the ring was full (the word is handed back, for the caller to retry after doing its own
/// work — a shard drains its inbound rings meanwhile, so two shards saturating each other's rings never
/// wait on each other for good); or the target is gone — its holder exited, or its slot is free — and
/// the wake was counted stale. Each turn re-reads the target under the reader count, so a spin ends the
/// moment the wake became stale (the livelock the stress test found on 2026-09-14: a holder that left
/// never drains, and a ring only its holder empties).
pub fn try_send_foreign(target: u16, word: u64) -> TrySend {
  let outcome = with_entry(target, |entry| {
    if entry.exited.load(Ordering::Acquire) {
      return TrySend::Gone;
    }
    match entry.inbound.push(word) {
      Ok(()) => {
        entry.parking.kick_if_parked(|| entry.kick.kick());
        TrySend::Landed
      }
      Err(back) => {
        entry.ring_full_events.fetch_add(1, Ordering::Relaxed);
        entry.kick.kick();
        TrySend::Full(back)
      }
    }
  });
  match outcome {
    Some(TrySend::Gone) | None => {
      count_stale(target);
      TrySend::Gone
    }
    Some(turn) => turn,
  }
}

/// The outcome of one foreign send turn ([`try_send_foreign`]).
#[derive(Debug, PartialEq, Eq)]
pub enum TrySend {
  /// The word landed in the target's ring.
  Landed,
  /// The ring was full; the word is handed back to retry.
  Full(u64),
  /// The target's holder exited or its slot is free: the wake was counted stale.
  Gone,
}

/// Counts a wake that found no live consumer (a stale waker, or a holder that exited): a tripwire,
/// never a fault.
pub(crate) fn count_stale(target: u16) {
  if let Some(slot) = SLOTS.get(usize::from(target)) {
    slot.stale_wakes.fetch_add(1, Ordering::Relaxed);
  }
}

/// Sends a control message to a shard from any thread and kicks it; refused when the shard's
/// control channel is full or the shard is gone.
pub fn send_control(target: u16, message: Control) -> Result<(), RtError> {
  with_entry(target, |entry| send_control_to(entry, target, message))
    .ok_or(RtError::ShardGone { shard: target })?
}

/// The send itself, on a counted entry (see [`send_control`]).
fn send_control_to(entry: &Entry, target: u16, message: Control) -> Result<(), RtError> {
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

/// A live shard named together with the registration that holds its slot: what a foreign submitter
/// keeps when its work must reach *that* shard and never a later holder of the slot. A slot freed at
/// unregistration is claimed by the next shard to register (the lowest free slot), so a message
/// addressed by id alone after the shard exited would reach a stranger — an observation of one daemon
/// landing on the next daemon to start. [`send_control_to_holder`] refuses `ShardGone` instead once
/// the slot is free or held by a later registration. Read from a started runtime
/// ([`crate::Runtime::holder_of`]): a registration in progress has claimed its generation before it
/// published its entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SlotHolder {
  shard: u16,
  generation: u32,
}

impl SlotHolder {
  /// The shard id.
  pub fn shard(&self) -> u16 {
    self.shard
  }
}

/// The registration currently holding `shard`'s slot; `None` for a free slot.
pub fn holder_of(shard: u16) -> Option<SlotHolder> {
  with_entry(shard, |entry| entry.holder)
}

/// Sends a control message to the shard `holder` names, from any thread, and kicks it; refused
/// `ShardGone` when the slot is free or held by a later registration (see [`SlotHolder`]) and
/// `ControlFull` when the holder's control channel is full.
pub fn send_control_to_holder(holder: SlotHolder, message: Control) -> Result<(), RtError> {
  with_holder(holder, |entry| {
    send_control_to(entry, holder.shard, message)
  })
  .ok_or(RtError::ShardGone {
    shard: holder.shard,
  })?
}

#[cfg(test)]
mod tests {
  use super::*;

  /// AC-0.7: pause a foreign descriptor borrow, then retire its shard. Retirement must
  /// wait for that borrow, and a copied kick must refuse access after retirement and reuse.
  #[cfg(unix)]
  #[test]
  fn retirement_waits_for_a_foreign_kick_borrow() {
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;

    let (descriptor, _write) = rustix::pipe::pipe().unwrap();
    #[cfg(target_os = "linux")]
    let form = Kick::Eventfd;
    #[cfg(any(target_os = "macos", target_os = "freebsd"))]
    let form = Kick::Kqueue;
    let (shard, _control) = register(2, 1, RegisterKick::Descriptor(descriptor, form)).unwrap();
    let kick = with_entry(shard, |entry| entry.kick).unwrap();
    let descriptor = match kick {
      #[cfg(target_os = "linux")]
      Kick::Eventfd(descriptor) => descriptor,
      #[cfg(any(target_os = "macos", target_os = "freebsd"))]
      Kick::Kqueue(descriptor) => descriptor,
      _ => panic!("registered descriptor"),
    };
    let (entered, borrowing) = sync_channel(0);
    let (release, released) = sync_channel(0);
    let (retiring, retirement_started) = sync_channel(0);
    let (retired, retirement_done) = sync_channel(1);
    let closed_during_borrow = std::thread::scope(|scope| {
      let borrower = scope.spawn(move || {
        descriptor.with(|_| {
          entered.send(()).unwrap();
          released.recv().unwrap();
        })
      });
      borrowing.recv().unwrap();
      let owner = scope.spawn(move || {
        retiring.send(()).unwrap();
        unregister(shard);
        retired.send(()).unwrap();
      });
      retirement_started.recv().unwrap();
      // Shape: a scheduler observation window only; channels force the borrow to span
      // retirement. This does not tune the runtime or delay a production operation.
      let closed = retirement_done
        .recv_timeout(Duration::from_millis(50))
        .is_ok();
      release.send(()).unwrap();
      assert_eq!(borrower.join().unwrap(), Some(()));
      owner.join().unwrap();
      closed
    });
    assert!(
      !closed_during_borrow,
      "unregistration closed a borrowed descriptor"
    );
    assert_eq!(descriptor.with(|_| ()), None);
    let (replacement, _control) = register(2, 1, RegisterKick::Kick(Kick::None)).unwrap();
    assert_eq!(
      descriptor.with(|_| ()),
      None,
      "a stale kick cannot borrow a replacement"
    );
    unregister(replacement);
  }

  /// Do: register a slot whose ring holds four words, fill it, mark its holder exited (what the shard
  /// does at its loop's exit) and send a fifth wake from a foreign thread's path. Expect: the send
  /// returns at once, counted stale — where before it spun for a consumer that would never come.
  /// Non-vacuous: the four fills landed (the ring was full), and the stale count moved by one.
  #[test]
  fn a_wake_to_an_exited_holders_full_ring_is_counted_stale_not_spun_on() {
    let (id, _receiver) = register(4, 2, RegisterKick::Kick(Kick::none())).unwrap();
    for word in 0..4u64 {
      send_foreign(id, word);
    }
    let stale_before = stale_wakes(id);
    note_exited(id);
    send_foreign(id, 4);
    assert_eq!(
      stale_wakes(id) - stale_before,
      1,
      "the fifth wake found an exited holder's full ring and was counted stale"
    );
    unregister(id);
  }

  // Every registration below is given back at the test's end. A slot left registered with a ring
  // nobody drains is a live holder to every other test in this binary: the interleaving stress test's
  // neighbour wake to it fills the ring and then spins, as the protocol says it must for a live
  // holder, for good — the binary hung 10 minutes that way on 2026-09-17 (the stress test's last
  // thread in `send_as_shard`, the neighbour a leaked two-word ring).

  #[test]
  fn registration_hands_out_distinct_ids_and_entries() {
    let (a, _ra) = register(8, 4, RegisterKick::Kick(Kick::none())).unwrap();
    let (b, _rb) = register(8, 4, RegisterKick::Kick(Kick::none())).unwrap();
    assert_ne!(a, b);
    assert!(entry(a).is_some());
    assert!(entry(b).is_some());
    assert_eq!(entry(a).unwrap().inbound.capacity(), 8);
    unregister(a);
    unregister(b);
  }

  #[test]
  fn a_wake_from_a_foreign_thread_lands_in_the_target_ring() {
    let (id, _receiver) = register(4, 4, RegisterKick::Kick(Kick::none())).unwrap();
    let word = Encoded::pack(id, 5, 1).unwrap();
    wake(word);
    let mut consumer = entry(id).unwrap().inbound.consumer();
    assert_eq!(consumer.pop(), Some(word.word()));
    assert_eq!(current_shard(), None);
    unregister(id);
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
    unregister(id);
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
    unregister(id);
  }

  /// A submission pinned to a slot's registration is refused as gone once the slot is free, and once
  /// a later registration holds it — never delivered to the new holder.
  #[test]
  fn a_holder_pinned_send_is_refused_once_the_slot_changes_hands() {
    let (id, _receiver) = register(2, 2, RegisterKick::Kick(Kick::none())).unwrap();
    let holder = holder_of(id).unwrap();
    assert_eq!(holder.shard(), id);
    send_control_to_holder(holder, Control::Active(true)).unwrap();
    unregister(id);
    assert_ne!(holder_of(id), Some(holder), "the old registration ended");
    assert!(matches!(
      send_control_to_holder(holder, Control::Active(true)),
      Err(RtError::ShardGone { .. })
    ));
    let (again, _receiver) = register(2, 2, RegisterKick::Kick(Kick::none())).unwrap();
    if again == id {
      assert!(matches!(
        send_control_to_holder(holder, Control::Active(true)),
        Err(RtError::ShardGone { .. })
      ));
      assert_ne!(holder_of(again), Some(holder));
    }
    unregister(again);
  }
}
