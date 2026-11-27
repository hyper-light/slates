//! The daemon's reaper (A-920, main's ruling after release probe 17): a thread of the daemon's own that drops what its
//! shards' releases took ([`slates_merge::reaped`]), so the allocator's return of freed pages is paid off every shard's
//! step.
//!
//! **Why.** A release of a retired green frees what it held a unit at a time, each unit far under the shard's step, but
//! the allocator gives a batch of freed pages back to the kernel inside whichever free crosses its threshold. Release
//! probe 17 read copy's longest release unit at p50 1,226 to 1,302 µs, all of it system time (user 3 to 5 µs). In debug
//! the task's counters at that one free read `phys_footprint` down 39-44 MB and `reusable` up 28-33 MB, and the free
//! that paid it was a different one from run to run, so no shaping of the walk could choose it. A swap of the global
//! allocator was weighed and rejected by main's ruling: a C dependency on every platform, Windows included, and every
//! memory figure rebaselined, for a 1.2 ms free.
//!
//! **What.** The release's walk stays on the shard; what each unit frees is moved into a `Reaped` instead of dropped.
//! The caller hands a slice's `Reaped` to [`reap`], which sends it to the reaper over a bounded channel. The reaper drops
//! it on its own thread, where the kernel's work costs no shard a step.
//!
//! **Its bound.** The channel holds [`queue_bound`] batches: two a shard, since each shard hands at most one a slice and
//! the reaper drops one while the next waits. At the bound, or on a thread with no reaper (a test's engine outside a
//! daemon, a refused thread), the shard drops the batch in place and counts it ([`ReapCounts::inline`]): it never waits,
//! and nothing queues past the bound.
//!
//! **Its owner.** The daemon starts it with its shards and stops it after them ([`Reaper::stop`]): a stop message behind
//! every batch queued, then the join. Each shard thread holds its own sender, installed at its start as the spawner's
//! end is ([`install`]), and gone with the thread.
//!
//! **What it reports.** Each shard thread counts what it handed and what it dropped in place (`counts`). The reaper
//! reads its own thread's CPU, user and system time around each batch's drop and keeps the totals in its daemon's slot
//! (`spent`), one a control shard as the doorbell's flags are, so its time is reported apart from the shards'.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::thread::JoinHandle;

use slates_merge::reaped::Reaped;

/// What the reaper's channel carries: a batch to drop, or its stop, sent behind every batch queued.
enum Reaping {
  /// What a release step's caller handed over.
  Batch(Reaped),
  /// The daemon's stop.
  Stop,
}

/// The batches the reaper's channel holds for a daemon of `shards` shards.
///
/// Derived: two a shard, at least two: each shard hands at most one batch a slice of its release, and the reaper drops
/// one while the next waits, so a reaper that keeps up never fills it and one that falls behind is met at a bound that
/// grows with what can hand it work.
fn queue_bound(shards: usize) -> usize {
  shards.saturating_mul(2).max(2)
}

/// The daemon's reaper: its thread and the sender its stop goes over.
pub(crate) struct Reaper {
  /// The daemon's own sender, for each shard's end and for the stop.
  send: SyncSender<Reaping>,
  /// The thread, until it is joined.
  thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Reaper {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Reaper")
      .field("running", &self.thread.is_some())
      .finish()
  }
}

/// A shard thread's sender to its daemon's reaper ([`install`]).
pub(crate) struct ReaperEnd(SyncSender<Reaping>);

impl std::fmt::Debug for ReaperEnd {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("ReaperEnd")
  }
}

impl Reaper {
  /// Starts the reaper of a daemon of `shards` shards whose control shard is `control`. None when the system refuses a
  /// thread (logged): each shard then drops what its releases took in place, counted ([`ReapCounts::inline`]).
  pub(crate) fn start(control: u16, shards: usize) -> Option<Reaper> {
    let (send, receive) = std::sync::mpsc::sync_channel(queue_bound(shards));
    let slot = spent_slot(control);
    slot.clear();
    match std::thread::Builder::new()
      .name("slates-reaper".to_owned())
      .spawn(move || run(&receive, slot))
    {
      Ok(thread) => Some(Reaper {
        send,
        thread: Some(thread),
      }),
      Err(error) => {
        slates_machine::log_line!(
          "slates-server: the reaper's thread could not start ({error}); shards drop what their releases free in place"
        );
        None
      }
    }
  }

  /// A shard's sender to this reaper, for its thread to install at its start.
  pub(crate) fn end(&self) -> ReaperEnd {
    ReaperEnd(self.send.clone())
  }

  /// Stops the reaper once every batch queued before is dropped, and joins it: after the daemon's shards, whose senders
  /// have then gone with their threads. The stop waits for room behind those batches, which the reaper is dropping.
  pub(crate) fn stop(&mut self) {
    let Some(thread) = self.thread.take() else {
      return;
    };
    // A reaper already gone refuses the stop; the join below reads why.
    let _ = self.send.send(Reaping::Stop);
    if thread.join().is_err() {
      slates_machine::log_line!("slates-server: the reaper's thread panicked");
    }
  }
}

impl Drop for Reaper {
  fn drop(&mut self) {
    self.stop();
  }
}

/// The reaper's loop: each batch dropped and its thread time kept in `slot`, until its stop or its last sender is gone.
fn run(receive: &Receiver<Reaping>, slot: &'static ReaperSpent) {
  while let Ok(Reaping::Batch(batch)) = receive.recv() {
    let taken = batch.taken();
    let began = thread_times();
    drop(batch);
    slot.add(taken, thread_times().since(began));
  }
}

/// A thread's CPU, user and system time now, nanoseconds; zero where the platform reads none.
#[derive(Clone, Copy, Default)]
struct ThreadTimes {
  cpu: u64,
  user: u64,
  system: u64,
}

impl ThreadTimes {
  /// What the thread spent from `began` to `self`.
  fn since(self, began: ThreadTimes) -> ThreadTimes {
    ThreadTimes {
      cpu: self.cpu.saturating_sub(began.cpu),
      user: self.user.saturating_sub(began.user),
      system: self.system.saturating_sub(began.system),
    }
  }
}

/// The calling thread's times now.
fn thread_times() -> ThreadTimes {
  let (user, system) = slates_rt::registry::calling_thread_user_system_ns().unwrap_or((0, 0));
  ThreadTimes {
    cpu: slates_rt::registry::calling_thread_cpu().map_or(0, |reading| reading.spent_ns),
    user,
    system,
  }
}

/// A reaper's totals: the batches it dropped, the things in them, and its thread's CPU, user and system time dropping
/// them. Written by the reaper a batch at a time, read by a report; on its own cache line, one a control shard.
#[repr(align(128))]
struct ReaperSpent {
  batches: AtomicU64,
  taken: AtomicU64,
  cpu_ns: AtomicU64,
  user_ns: AtomicU64,
  system_ns: AtomicU64,
}

impl ReaperSpent {
  /// None yet: a fresh slot, made in place for each slot of a static, never shared through a constant.
  const fn none() -> ReaperSpent {
    ReaperSpent {
      batches: AtomicU64::new(0),
      taken: AtomicU64::new(0),
      cpu_ns: AtomicU64::new(0),
      user_ns: AtomicU64::new(0),
      system_ns: AtomicU64::new(0),
    }
  }

  /// Cleared, for a reaper starting on this slot.
  fn clear(&self) {
    for total in [
      &self.batches,
      &self.taken,
      &self.cpu_ns,
      &self.user_ns,
      &self.system_ns,
    ] {
      total.store(0, Ordering::Relaxed);
    }
  }

  /// A batch of `taken` things dropped in `spent`.
  fn add(&self, taken: u64, spent: ThreadTimes) {
    self.batches.fetch_add(1, Ordering::Relaxed);
    self.taken.fetch_add(taken, Ordering::Relaxed);
    self.cpu_ns.fetch_add(spent.cpu, Ordering::Relaxed);
    self.user_ns.fetch_add(spent.user, Ordering::Relaxed);
    self.system_ns.fetch_add(spent.system, Ordering::Relaxed);
  }
}

/// One reaper's totals per control shard, indexed by the shard's registry id: a table, not one total, because a process
/// that runs several daemons (the test suites) must give each its own, as the doorbell's flags do.
static REAPER_SPENT: [ReaperSpent; slates_rt::registry::MAX_SHARDS] =
  [const { ReaperSpent::none() }; slates_rt::registry::MAX_SHARDS];

/// The slot of the daemon whose control shard is `control`: the first for an id past the table, which the registry
/// never hands out.
fn spent_slot(control: u16) -> &'static ReaperSpent {
  REAPER_SPENT
    .get(usize::from(control))
    .or_else(|| REAPER_SPENT.first())
    .unwrap_or(&FALLBACK_SPENT)
}

/// The slot no registry id names, for a table that could not be read (it always can).
static FALLBACK_SPENT: ReaperSpent = ReaperSpent::none();

/// A reaper's totals so far, as [`spent`] reads them.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Spent {
  /// The batches it dropped.
  pub(crate) batches: u64,
  /// The things in them.
  pub(crate) taken: u64,
  /// Its thread's CPU dropping them, nanoseconds.
  pub(crate) cpu_ns: u64,
  /// Of that, its user and system time, where the platform keeps them apart (macOS; else zero).
  pub(crate) user_ns: u64,
  /// Its system time.
  pub(crate) system_ns: u64,
}

/// The totals of the reaper of the daemon whose control shard is `control`: read by the tests that print them apart
/// from a shard's own time.
#[cfg(test)]
pub(crate) fn spent(control: u16) -> Spent {
  let slot = spent_slot(control);
  Spent {
    batches: slot.batches.load(Ordering::Relaxed),
    taken: slot.taken.load(Ordering::Relaxed),
    cpu_ns: slot.cpu_ns.load(Ordering::Relaxed),
    user_ns: slot.user_ns.load(Ordering::Relaxed),
    system_ns: slot.system_ns.load(Ordering::Relaxed),
  }
}

/// What a shard thread handed its reaper and what it dropped in place (`counts`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReapCounts {
  /// The batches handed.
  pub(crate) batches: u64,
  /// The things in them: the non-vacuity count of what left the shard to be dropped.
  pub(crate) handed: u64,
  /// The things dropped in place, the reaper's queue at its bound or no reaper on the thread: zero while the reaper
  /// keeps up.
  pub(crate) inline: u64,
}

std::thread_local! {
  /// This shard thread's sender to its daemon's reaper; none outside a daemon, or where its thread was refused.
  static END: RefCell<Option<SyncSender<Reaping>>> = const { RefCell::new(None) };
  /// What this thread handed its reaper and dropped in place.
  static COUNTS: Cell<ReapCounts> = const {
    Cell::new(ReapCounts {
      batches: 0,
      handed: 0,
      inline: 0,
    })
  };
}

/// The calling shard thread's sender to its daemon's reaper, installed at the shard's start: none leaves it dropping
/// in place.
pub(crate) fn install(end: Option<ReaperEnd>) {
  let _ = END.with(|installed| {
    installed
      .try_borrow_mut()
      .map(|mut installed| *installed = end.map(|end| end.0))
  });
}

/// What a release step's caller took, `reaped`, handed to the calling thread's reaper to be dropped there; dropped in
/// place, counted, when the reaper's queue is at its bound or the thread has none. Nothing taken is nothing sent.
pub(crate) fn reap(reaped: Reaped) {
  if reaped.is_empty() {
    return;
  }
  let taken = reaped.taken();
  let back = END.with(|end| match end.try_borrow() {
    Ok(end) => match end.as_ref() {
      Some(send) => match send.try_send(Reaping::Batch(reaped)) {
        Ok(()) => None,
        Err(TrySendError::Full(back) | TrySendError::Disconnected(back)) => Some(back),
      },
      None => Some(Reaping::Batch(reaped)),
    },
    Err(_) => Some(Reaping::Batch(reaped)),
  });
  let handed = back.is_none();
  // Dropped here: what the reaper could not take.
  drop(back);
  COUNTS.with(|counts| {
    let mut now = counts.get();
    if handed {
      now.batches = now.batches.saturating_add(1);
      now.handed = now.handed.saturating_add(taken);
    } else {
      now.inline = now.inline.saturating_add(taken);
    }
    counts.set(now);
  });
}

/// What the calling shard thread has handed its reaper and dropped in place so far: read by the tests that hold a
/// release to having freed nothing on the shard.
#[cfg(test)]
pub(crate) fn counts() -> ReapCounts {
  COUNTS.with(Cell::get)
}
