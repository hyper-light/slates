//! A task a shard runs alone (§4.3; `slates_rt::futures::spawn_alone`): while it lives the shard polls nothing else,
//! and every yield it makes still ends a step. A daemon's shard start uses it: its recovery yields between slices so
//! the A-228 watchdog sees the shard step, while nothing else may yet reach the shard's state (§2.6 boot step 2).

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use slates_rt::futures;
use slates_rt::runtime::{Runtime, RuntimeConfig};
use slates_rt::sim::SimRuntime;
use slates_rt::task::Admission;

/// The tests of this binary each start a runtime, which claims process-global registry slots, so they run one at a
/// time (a test harness lock, D-8's stated exception).
#[allow(clippy::disallowed_types)] // a test harness lock (D-8's stated exception), poison recovered
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
  SERIAL
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Shape: how long the test waits for anything a shard does; a bound on a hang, never met by a passing run.
const WAIT: Duration = Duration::from_secs(30);

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 16,
    timers_per_shard: 16,
    ring_entries: 16,
    step_budget_ns: 1_000_000,
    timer_tick_ns: 100_000,
    batch: 16,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  }
}

/// What the shard did, in the order it did it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
  /// The task running alone was polled, with the shard's step count then.
  Alone { steps: u64 },
  /// A second hold asked for while the first lives, and what it was told.
  SecondHold { refused: bool },
  /// Another task ran, by name.
  Ran(&'static str),
}

fn steps_now() -> u64 {
  let shard = futures::shard_id().unwrap().0;
  slates_rt::registry::with_entry(shard, |entry| entry.pulse.steps()).unwrap()
}

async fn record(events: Sender<Event>, name: &'static str) {
  let _ = events.send(Event::Ran(name));
}

/// Collects events until `count` of them are `Ran`, or the wait runs out.
fn until_ran(told: &Receiver<Event>, count: usize) -> Vec<Event> {
  let mut events = Vec::new();
  while events
    .iter()
    .filter(|event| matches!(event, Event::Ran(_)))
    .count()
    < count
  {
    match told.recv_timeout(WAIT) {
      Ok(event) => events.push(event),
      Err(_) => panic!("the shard stopped before every task ran: {events:?}"),
    }
  }
  events
}

/// Queues a task, then spawns the test's task alone (`ADMITTED` says when the test has spawned one from its own thread).
fn launch_alone(events: Sender<Event>) {
  let before = futures::spawn(record(events.clone(), "queued before")).unwrap();
  futures::detach(before).unwrap();
  let alone_events = events;
  let alone = futures::spawn_alone(async move {
    let mut spawned_meanwhile = false;
    loop {
      let _ = alone_events.send(Event::Alone { steps: steps_now() });
      if ADMITTED.load(Ordering::SeqCst) {
        if spawned_meanwhile {
          return;
        }
        let meanwhile = futures::spawn(record(alone_events.clone(), "spawned meanwhile")).unwrap();
        futures::detach(meanwhile).unwrap();
        let second = futures::spawn_alone(async {});
        let _ = alone_events.send(Event::SecondHold {
          refused: matches!(second, Err(slates_rt::RtError::ShardHeld { .. })),
        });
        spawned_meanwhile = true;
      }
      futures::yield_now().await;
    }
  })
  .unwrap();
  futures::detach(alone).unwrap();
}

/// Set once the test has seen a task it spawned from its own thread admitted to the shard.
static ADMITTED: AtomicBool = AtomicBool::new(false);

/// §4.3, §2.6. Do: on one shard, queue a launcher and a task behind it in one batch; the launcher queues a task, then
/// spawns a task alone that yields until the test has spawned a task onto the shard from its own thread and seen it
/// admitted; while alone it spawns a task itself and asks for a second hold. Expect: every poll of the task alone
/// comes before any other task runs, though two were queued before it, one in the very batch that started it; each of
/// its polls is in a later step than the last (its yields end steps, so a watchdog judging steps sees the shard
/// alive); the second hold is refused; and once it ends the four others run in the order they were queued.
#[test]
#[cfg_attr(
  miri,
  ignore = "the OS driver (kqueue, epoll) is outside Miri; the simulated test below runs there"
)]
fn a_task_spawned_alone_runs_alone_through_its_yields_and_the_rest_run_after_it_in_order() {
  let _serial = serial();
  ADMITTED.store(false, Ordering::SeqCst);
  let runtime = Runtime::start(&config()).unwrap();
  let shard = *runtime.shard_ids().first().unwrap();
  let (events, told) = channel();
  let starter = events.clone();
  runtime
    .spawn_on(shard, async move {
      // The launcher and a task behind it are queued together, so the next step polls them in one batch.
      let launcher_events = starter.clone();
      let launcher = futures::spawn(async move { launch_alone(launcher_events) }).unwrap();
      futures::detach(launcher).unwrap();
      let batch = futures::spawn(record(starter.clone(), "in the launcher's batch")).unwrap();
      futures::detach(batch).unwrap();
    })
    .unwrap();

  // The task alone is running: spawn one from this thread and wait until the shard has admitted it.
  let first = told.recv_timeout(WAIT).unwrap();
  assert!(
    matches!(first, Event::Alone { .. }),
    "the task alone ran first: {first:?}"
  );
  let receipt = runtime
    .spawn_on_with_receipt(shard, record(events.clone(), "from another thread"))
    .unwrap();
  assert!(matches!(receipt.wait(WAIT), Some(Admission::Admitted(_))));
  ADMITTED.store(true, Ordering::SeqCst);
  let mut seen = vec![first];
  seen.extend(until_ran(&told, 4));
  let ran: Vec<&Event> = seen
    .iter()
    .filter(|event| matches!(event, Event::Ran(_)))
    .collect();
  assert_eq!(
    ran,
    [
      &Event::Ran("in the launcher's batch"),
      &Event::Ran("queued before"),
      &Event::Ran("from another thread"),
      &Event::Ran("spawned meanwhile")
    ],
    "the others run after the task alone, in their order: {seen:?}"
  );
  let first_other = seen
    .iter()
    .position(|event| matches!(event, Event::Ran(_)))
    .unwrap();
  let last_alone = seen
    .iter()
    .rposition(|event| matches!(event, Event::Alone { .. }))
    .unwrap();
  assert!(
    last_alone < first_other,
    "nothing else ran while the task was alone: {seen:?}"
  );
  assert!(
    seen.contains(&Event::SecondHold { refused: true }),
    "{seen:?}"
  );
  let steps: Vec<u64> = seen
    .iter()
    .filter_map(|event| match event {
      Event::Alone { steps } => Some(*steps),
      _ => None,
    })
    .collect();
  assert!(
    steps.windows(2).all(|pair| pair.first() < pair.get(1)),
    "each poll of the task alone is in a step of its own: {steps:?}"
  );
  drop(runtime);
}

/// Reports its drop: a task the shard cancelled.
struct Dropped(Sender<&'static str>, &'static str);
impl Drop for Dropped {
  fn drop(&mut self) {
    let _ = self.0.send(self.1);
  }
}

/// §4.3 (banned item 9: a shutdown leaves no task behind). Do: hold a shard with a task alone that yields forever,
/// with another task queued behind it, then shut the runtime down. Expect: the shutdown ends, and both tasks are
/// dropped: the hold ends with its task's cancellation and does not keep the queued one from its own.
#[test]
#[cfg_attr(
  miri,
  ignore = "the OS driver (kqueue, epoll) is outside Miri; the simulated test below runs there"
)]
fn a_shutdown_cancels_a_task_alone_and_the_tasks_waiting_behind_it() {
  let _serial = serial();
  let runtime = Runtime::start(&config()).unwrap();
  let shard = *runtime.shard_ids().first().unwrap();
  let (dropped, told) = channel();
  let (running, alone_ran) = channel();
  runtime
    .spawn_on(shard, async move {
      let waiting = Dropped(dropped.clone(), "waiting");
      let behind = futures::spawn(async move {
        let _waiting = waiting;
        std::future::pending::<()>().await;
      })
      .unwrap();
      futures::detach(behind).unwrap();
      let holder = Dropped(dropped, "alone");
      let alone = futures::spawn_alone(async move {
        let _holder = holder;
        loop {
          let _ = running.send(());
          futures::yield_now().await;
        }
      })
      .unwrap();
      futures::detach(alone).unwrap();
    })
    .unwrap();
  alone_ran.recv_timeout(WAIT).unwrap();
  runtime.shutdown().unwrap();
  let mut gone = vec![
    told.recv_timeout(WAIT).unwrap(),
    told.recv_timeout(WAIT).unwrap(),
  ];
  gone.sort_unstable();
  assert_eq!(gone, ["alone", "waiting"]);
}

/// Shape: the simulated runtime's seed; the test's order depends on none of its choices.
const SEED: u64 = 1;

/// §4.3, on a simulated shard (deterministic, and so under Miri too). Do: from one task, queue a task and then spawn a
/// task alone that yields one more time than a step's batch, so that without the hold the queued task would run in
/// the holder's first steps. Expect: every poll of the task alone comes before the queued task runs, and the queued
/// task runs once the task alone has ended.
#[test]
fn on_a_simulated_shard_a_task_alone_runs_before_the_task_queued_ahead_of_it() {
  let mut sim = SimRuntime::new(&config(), SEED).unwrap();
  let shard = *sim.shard_ids().first().unwrap();
  let polls = config().batch.saturating_add(1);
  let (events, told) = channel();
  sim
    .spawn_on(shard, async move {
      let before = futures::spawn(record(events.clone(), "queued before")).unwrap();
      futures::detach(before).unwrap();
      let alone_events = events;
      let alone = futures::spawn_alone(async move {
        for _ in 0..polls {
          let _ = alone_events.send(Event::Ran("alone"));
          futures::yield_now().await;
        }
      })
      .unwrap();
      futures::detach(alone).unwrap();
    })
    .unwrap();
  sim.run_until_idle();
  let seen: Vec<Event> = told.try_iter().collect();
  let alone = seen
    .iter()
    .filter(|event| **event == Event::Ran("alone"))
    .count();
  assert_eq!(
    alone, polls,
    "the task alone was polled through every yield: {seen:?}"
  );
  assert_eq!(
    seen.last(),
    Some(&Event::Ran("queued before")),
    "the queued task ran last, once the task alone had ended: {seen:?}"
  );
}
