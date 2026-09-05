//! AC-0.6 and AC-0.9: the same task program runs on the OS driver and on the simulation driver
//! with identical observable results, and the runtime's public surface behaves the same in both.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::{Sender, channel};

use slates_rt::futures::{cancel, join, sleep, spawn, spawn_child, yield_now};
use slates_rt::runtime::{LocalRuntime, RuntimeConfig};
use slates_rt::sim::SimRuntime;
use slates_rt::task::Outcome;

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 64,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: 100_000,
    batch: 64,
    pin: false,
    page_bytes: 4096,
  }
}

/// The program: a root task spawns three children with different shapes, joins them in a fixed
/// order, then spawns and cancels a fourth. Every event goes to `log`.
async fn program(log: Sender<String>) {
  let send = |l: &Sender<String>, s: &str| {
    let _ = l.send(s.to_owned());
  };
  send(&log, "start");
  let l1 = log.clone();
  let c1 = spawn_child(async move {
    yield_now().await;
    yield_now().await;
    let _ = l1.send("c1 after two yields".to_owned());
  })
  .unwrap();
  let l2 = log.clone();
  let c2 = spawn_child(async move {
    sleep(3_000_000).await;
    let _ = l2.send("c2 after sleeping".to_owned());
  })
  .unwrap();
  let l3 = log.clone();
  let c3 = spawn_child(async move {
    let _ = l3.send("c3 immediately".to_owned());
  })
  .unwrap();
  assert_eq!(join(c3).await, Ok(Outcome::Completed));
  send(&log, "joined c3");
  assert_eq!(join(c1).await, Ok(Outcome::Completed));
  send(&log, "joined c1");
  assert_eq!(join(c2).await, Ok(Outcome::Completed));
  send(&log, "joined c2");
  let l4 = log.clone();
  let c4 = spawn(async move {
    sleep(1_000_000_000).await;
    let _ = l4.send("c4 should never print".to_owned());
  })
  .unwrap();
  yield_now().await;
  cancel(c4).unwrap();
  assert_eq!(join(c4).await, Ok(Outcome::Cancelled));
  send(&log, "c4 cancelled");
  send(&log, "end");
}

fn run_on_os() -> Vec<String> {
  let (tx, rx) = channel();
  let rt = LocalRuntime::new(&config()).unwrap();
  let root = rt.spawn(program(tx)).unwrap();
  rt.run_until_idle();
  let counters = rt.context().counters();
  assert_eq!(
    counters.completed, 4,
    "root, c1, c2 and c3 complete; {counters:?}"
  );
  assert_eq!(counters.cancelled, 1, "{counters:?}");
  assert_eq!(counters.nested_borrows, 0, "{counters:?}");
  rt.context().detach(root).unwrap();
  rx.try_iter().collect()
}

fn run_on_sim() -> Vec<String> {
  let (tx, rx) = channel();
  let mut sim = SimRuntime::new(&config(), 7).unwrap();
  let shard = sim.shard_ids()[0];
  sim.spawn_on(shard, program(tx)).unwrap();
  sim.run_until_idle();
  let counters = sim.context(shard).unwrap().counters();
  assert_eq!(
    counters.completed, 4,
    "root, c1, c2 and c3 complete; {counters:?}"
  );
  assert_eq!(counters.cancelled, 1, "{counters:?}");
  assert_eq!(counters.timers_fired, 1, "{counters:?}");
  assert!(
    sim.now_ns() >= 3_000_000,
    "virtual time advanced to the sleep's deadline"
  );
  rx.try_iter().collect()
}

#[test]
fn the_program_produces_the_same_trace_on_the_os_driver_and_the_simulation() {
  let os = run_on_os();
  let sim = run_on_sim();
  let expected = vec![
    "start",
    "c3 immediately",
    "joined c3",
    "c1 after two yields",
    "joined c1",
    "c2 after sleeping",
    "joined c2",
    "c4 cancelled",
    "end",
  ];
  assert_eq!(os, expected, "os driver");
  assert_eq!(sim, expected, "simulation driver");
}

#[test]
fn a_lost_driver_cancels_every_task_with_a_terminal_completion() {
  let (tx, rx) = channel();
  let mut sim = SimRuntime::new(&config(), 3).unwrap();
  let shard = sim.shard_ids()[0];
  for i in 0..5 {
    let tx = tx.clone();
    sim
      .spawn_on(shard, async move {
        sleep(10_000_000 * (i + 1)).await;
        let _ = tx.send(format!("task {i} finished sleeping"));
      })
      .unwrap();
  }
  sim.kill_driver(shard, 1).unwrap();
  sim.run_until_idle();
  let ctx = sim.context(shard).unwrap();
  assert!(ctx.exited());
  let c = ctx.counters();
  assert_eq!(c.driver_lost, 1, "{c:?}");
  assert_eq!(c.cancelled, 5, "{c:?}");
  assert_eq!(c.completed, 0, "{c:?}");
  assert_eq!(ctx.live_tasks(), 0);
  assert_eq!(rx.try_iter().count(), 0, "no task ran to completion");
}

#[test]
fn a_parent_finishing_cancels_and_joins_its_children() {
  let (tx, rx) = channel();
  let mut sim = SimRuntime::new(&config(), 11).unwrap();
  let shard = sim.shard_ids()[0];
  sim
    .spawn_on(shard, async move {
      for i in 0..3 {
        let tx = tx.clone();
        spawn_child(async move {
          sleep(1_000_000_000).await;
          let _ = tx.send(format!("child {i}"));
        })
        .unwrap();
      }
      let _ = tx.send("parent done".to_owned());
    })
    .unwrap();
  sim.run_until_idle();
  let ctx = sim.context(shard).unwrap();
  let c = ctx.counters();
  assert_eq!(c.completed, 1, "{c:?}");
  assert_eq!(c.cancelled, 3, "{c:?}");
  assert_eq!(ctx.live_tasks(), 0, "every slot reaped");
  assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec!["parent done"]);
}

#[test]
fn admission_is_refused_at_the_arena_bound_never_silently() {
  let mut cfg = config();
  cfg.tasks_per_shard = 2;
  let rt = LocalRuntime::new(&cfg).unwrap();
  rt.spawn(async {}).unwrap();
  rt.spawn(async {}).unwrap();
  assert!(matches!(
    rt.spawn(async {}),
    Err(slates_rt::RtError::TooManyTasks { capacity: 2 })
  ));
  rt.run_until_idle();
  assert_eq!(rt.context().counters().admission_refused, 1);
}
