//! AUD-29-08 (§4.3, R2, D-8): a shard's context and the values it keeps are reached only through a borrow
//! that proves them alive. A kept value is named by a [`Kept`] handle and lent inside a closure while its own
//! context runs on the calling thread; the thread's current shard is published only while its owner steps
//! it. Until 2026-09-30 the runtime lent `&'static` references to both, which safe code could hold past the
//! owner's drop that freed them. These run on the simulated driver, so Miri runs them too (the owner
//! teardown with held handles, stale wakers and kept values the audit asks it to see).

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::channel;

use slates_rt::error::RtError;
use slates_rt::registry::with_current;
use slates_rt::runtime::RuntimeConfig;
use slates_rt::shard::Kept;
use slates_rt::sim::SimRuntime;

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 16,
    timers_per_shard: 16,
    ring_entries: 16,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: 100_000,
    batch: 16,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  }
}

/// A kept value that counts its own drop, so a test sees it dropped exactly once, with its context.
struct Counted {
  word: u64,
  drops: &'static AtomicU64,
}

impl Drop for Counted {
  fn drop(&mut self) {
    self.drops.fetch_add(1, Ordering::SeqCst);
  }
}

/// Keeps a [`Counted`] from a task of a fresh simulated runtime and returns the runtime and the handle.
fn runtime_keeping(word: u64, drops: &'static AtomicU64) -> (SimRuntime, Kept<Counted>) {
  let mut sim = SimRuntime::new(&config(), 1).unwrap();
  let shard = sim.shard_ids()[0];
  let (tx, rx) = channel();
  sim
    .spawn_on(shard, async move {
      let kept = with_current(|context| context.keep(Counted { word, drops })).unwrap();
      let _ = tx.send(kept);
    })
    .unwrap();
  sim.run_until_idle();
  (sim, rx.recv().unwrap().unwrap())
}

/// Resolves `kept` from a task of `sim`'s shard: what the value's word reads there.
fn resolve_in_task(sim: &mut SimRuntime, kept: Kept<Counted>) -> Option<u64> {
  let shard = sim.shard_ids()[0];
  let (tx, rx) = channel();
  sim
    .spawn_on(shard, async move {
      let _ = tx.send(kept.with(|counted| counted.word));
    })
    .unwrap();
  sim.run_until_idle();
  rx.recv().unwrap()
}

/// AUD-29-08: do: keep a value from a task, hold its handle, drop the runtime; expect the handle to resolve
/// in a task while the runtime lives, the value dropped exactly once with the runtime, and the same handle
/// to answer `None` afterwards — from this thread and from another — never a read of the freed value.
#[test]
fn a_kept_handle_held_past_its_runtime_answers_none() {
  static DROPS: AtomicU64 = AtomicU64::new(0);
  let (mut sim, kept) = runtime_keeping(7, &DROPS);
  assert_eq!(resolve_in_task(&mut sim, kept), Some(7));
  assert_eq!(
    kept.with(|counted| counted.word),
    None,
    "no shard runs between steps"
  );
  assert_eq!(DROPS.load(Ordering::SeqCst), 0);
  drop(sim);
  assert_eq!(
    DROPS.load(Ordering::SeqCst),
    1,
    "the value dropped with its context"
  );
  assert_eq!(kept.with(|counted| counted.word), None);
  let elsewhere = std::thread::spawn(move || kept.with(|counted| counted.word))
    .join()
    .unwrap();
  assert_eq!(elsewhere, None);
}

/// AUD-29-08: do: keep a value on one runtime, then resolve its handle in a task of another runtime — one
/// alive beside it, and one built after the first dropped (which takes the freed registry slot); expect
/// `None` both times: a handle names its context's registration, never a slot or a thread.
#[test]
fn a_kept_handle_resolves_only_on_its_own_context() {
  static FIRST: AtomicU64 = AtomicU64::new(0);
  static SECOND: AtomicU64 = AtomicU64::new(0);
  static THIRD: AtomicU64 = AtomicU64::new(0);
  let (first, kept) = runtime_keeping(11, &FIRST);
  let (mut beside, _) = runtime_keeping(12, &SECOND);
  assert_eq!(resolve_in_task(&mut beside, kept), None);
  drop(first);
  let (mut after, _) = runtime_keeping(13, &THIRD);
  assert_eq!(resolve_in_task(&mut after, kept), None);
  assert_eq!(FIRST.load(Ordering::SeqCst), 1);
}

/// AUD-29-08: do: step a runtime's task, then ask for the current shard outside any step; expect `None`.
/// Before 2026-09-30 a bare step published its context and left it published after it returned, so the
/// thread's next lend could name a context whose owner had since dropped.
#[test]
fn the_current_shard_is_published_only_while_its_owner_steps() {
  let mut sim = SimRuntime::new(&config(), 5).unwrap();
  let shard = sim.shard_ids()[0];
  let (tx, rx) = channel();
  sim
    .spawn_on(shard, async move {
      let _ = tx.send(with_current(|context| context.id));
    })
    .unwrap();
  sim.run_until_idle();
  assert_eq!(rx.recv().unwrap(), Some(shard.0), "inside a step");
  let context = sim.context(shard).unwrap();
  let _ = context.step();
  assert_eq!(
    with_current(|context| context.id),
    None,
    "after a bare step"
  );
}

/// AUD-29-08: do: inside a borrow of a kept value, keep another value on the same shard, and keep a value
/// whose builder keeps a second; expect both refused `KeptInUse`, the built value of the second dropped,
/// and the first value still reachable.
#[test]
fn a_keep_inside_a_kept_borrow_is_refused_typed() {
  static DROPS: AtomicU64 = AtomicU64::new(0);
  let mut sim = SimRuntime::new(&config(), 9).unwrap();
  let shard = sim.shard_ids()[0];
  let (tx, rx) = channel();
  sim
    .spawn_on(shard, async move {
      let outer = with_current(|context| context.keep(1_u64))
        .unwrap()
        .unwrap();
      let nested = outer
        .with(|_| with_current(|context| context.keep(2_u64).map(|_| ())))
        .flatten()
        .unwrap();
      let building = with_current(|context| {
        context.keep_with(|_: Kept<Counted>| {
          let _ = context.keep(3_u64);
          Counted {
            word: 4,
            drops: &DROPS,
          }
        })
      })
      .unwrap();
      let _ = tx.send((nested, building.map(|_| ()), outer.with(|value| *value)));
    })
    .unwrap();
  sim.run_until_idle();
  let (nested, building, outer) = rx.recv().unwrap();
  assert_eq!(nested, Err(RtError::KeptInUse { shard: shard.0 }));
  assert_eq!(building, Err(RtError::KeptInUse { shard: shard.0 }));
  assert_eq!(
    DROPS.load(Ordering::SeqCst),
    1,
    "the refused build was dropped"
  );
  assert_eq!(outer, Some(1));
}

/// AUD-29-08: do: inside a task of one runtime, build and run another to idle on the same thread; expect
/// the inner runtime's task to see its own shard as current, and the outer task to see its shard again once
/// the inner run returned (a nested span restores the enclosing one).
#[test]
fn a_nested_runtime_restores_the_enclosing_shard() {
  let mut outer = SimRuntime::new(&config(), 21).unwrap();
  let outer_shard = outer.shard_ids()[0];
  let (tx, rx) = channel();
  outer
    .spawn_on(outer_shard, async move {
      let mut inner = SimRuntime::new(&config(), 22).unwrap();
      let inner_shard = inner.shard_ids()[0];
      let (inner_tx, inner_rx) = channel();
      inner
        .spawn_on(inner_shard, async move {
          let _ = inner_tx.send(with_current(|context| context.id));
        })
        .unwrap();
      inner.run_until_idle();
      let seen_inside = inner_rx.recv().unwrap();
      let _ = tx.send((
        inner_shard.0,
        seen_inside,
        with_current(|context| context.id),
      ));
    })
    .unwrap();
  outer.run_until_idle();
  let (inner_shard, seen_inside, seen_after) = rx.recv().unwrap();
  assert_eq!(seen_inside, Some(inner_shard));
  assert_eq!(seen_after, Some(outer_shard.0));
}
