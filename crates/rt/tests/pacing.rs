//! A-783: a paced step asks once a quantum whether anyone else waits on its shard (`futures::others_waiting`), and
//! takes another quantum only when no one does. These pin what the probe sees: nothing while a task runs alone; a task
//! spawned and ready, and a due timer, once there; and the counts of asks and finds.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use slates_rt::futures;
use slates_rt::runtime::{LocalRuntime, RuntimeConfig};

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 64,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 1_000_000,
    timer_tick_ns: 100_000,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  }
}

/// A-783. Do: in one task alone on its shard, ask whether anyone waits; spawn a second task and ask again; then set
/// the ceiling. Expect: no one alone; the spawned (ready) task once spawned; the ceiling a quantum until set, then what
/// was set.
#[test]
fn a_step_sees_a_ready_task_and_nothing_when_alone() {
  let runtime = LocalRuntime::new(&config()).unwrap();
  let (out, seen) = std::sync::mpsc::channel::<(bool, bool, u64, u64)>();
  runtime
    .spawn(async move {
      let alone = futures::others_waiting();
      let default_ceiling = futures::step_ceiling_ns().unwrap();
      let task = futures::spawn(async {}).unwrap();
      futures::detach(task).unwrap();
      let after = futures::others_waiting();
      futures::set_step_ceiling_ns(50_000_000);
      out
        .send((
          alone,
          after,
          default_ceiling,
          futures::step_ceiling_ns().unwrap(),
        ))
        .unwrap();
    })
    .unwrap();
  runtime.run_until_idle();
  let (alone, after, default_ceiling, set) = seen.try_recv().unwrap();
  assert!(!alone, "a task alone saw someone waiting");
  assert!(after, "a ready task was not seen");
  assert_eq!(
    default_ceiling,
    futures::step_budget_ns().unwrap_or(1_000_000)
  );
  assert_eq!(set, 50_000_000);
}

/// Spins until `ns` have passed on the shard clock, asking whether anyone waits every `ask_ns` (none: never).
fn spin(ns: u64, ask_ns: Option<u64>) {
  let began = futures::now_ns();
  let mut asked = began;
  while futures::now_ns().saturating_sub(began) < ns {
    if let Some(every) = ask_ns
      && futures::now_ns().saturating_sub(asked) >= every
    {
      assert!(!futures::others_waiting(), "someone waited");
      asked = futures::now_ns();
    }
    std::hint::spin_loop();
  }
}

/// A-783 (§4.3 amended: a paced step that finds no one waiting may run on). Do: in one task alone on its shard, spin
/// five quanta asking every half quantum; then, in a second task, spin three quanta asking never. Expect: the first
/// poll counted past its allowance by no measure (long, preempted or unattributed: each ask, finding no one, allowed it
/// another quantum); the second counted once.
#[test]
fn a_poll_that_asks_and_finds_no_one_is_no_long_step() {
  let runtime = LocalRuntime::new(&config()).unwrap();
  let (out, counts) = std::sync::mpsc::channel::<(u64, u64, u64)>();
  // Every poll past its allowance by the wall clock: the task's, the host's and the unattributed (§4.3).
  let long_steps = || {
    slates_rt::registry::with_current(|ctx| {
      let counters = ctx.counters();
      counters.long_steps + counters.preempted_steps + counters.unattributed_steps
    })
    .unwrap()
  };
  runtime
    .spawn(async move {
      let quantum = futures::step_budget_ns().unwrap();
      let before = long_steps();
      spin(5 * quantum, Some(quantum / 2));
      futures::yield_now().await;
      let after_asking = long_steps();
      spin(3 * quantum, None);
      futures::yield_now().await;
      out.send((before, after_asking, long_steps())).unwrap();
    })
    .unwrap();
  runtime.run_until_idle();
  let (before, after_asking, after_silent) = counts.try_recv().unwrap();
  assert_eq!(
    after_asking, before,
    "a poll that asked and found no one was counted long"
  );
  assert_eq!(
    after_silent,
    after_asking + 1,
    "a poll past its quantum unasked was not counted"
  );
}

/// What [`a_connection_arriving_on_a_busy_shard_is_seen_waiting`] waits for at most: a newcomer missed for this long is
/// missed for good (the probe is asked a thousand times a millisecond in the spin).
const ARRIVAL_BOUND: std::time::Duration = std::time::Duration::from_secs(2);

/// A-783 (§4.3 amended: "yield at the quantum when someone waits"; the A/B's newcomer, measured 2026-11-20: a build
/// turn on the shard of the repository a ref advertisement asked for ran 82 ms asking 7,420 times and found no one,
/// while advertisements arrived there every few milliseconds). Do: on a shard, a listener task awaits its next
/// connection; a busy task on the same shard spins asking whether anyone waits; a task on another shard connects.
/// Expect: the busy task sees someone waiting within the bound, and before the connector is told it connected plus the
/// probe's own interval.
#[test]
fn a_connection_arriving_on_a_busy_shard_is_seen_waiting() {
  use slates_rt::runtime::Runtime;
  use slates_rt::tcp::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
  use std::sync::mpsc::channel;

  let runtime = Runtime::start(&RuntimeConfig {
    shards: 2,
    ..config()
  })
  .unwrap();
  let ids = runtime.shard_ids();
  let (busy_shard, other_shard) = (ids[0], ids[1]);
  let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), 1).unwrap();
  let addr = listener.local_addr().unwrap();
  runtime
    .spawn_on(busy_shard, async move {
      let _accepted = listener.accept().await;
    })
    .unwrap();
  let (spinning_tx, spinning_rx) = channel();
  let (seen_tx, seen_rx) = channel();
  runtime
    .spawn_on(busy_shard, async move {
      // The listener's first poll (its interest registered) comes before this one: spawned first, queued first.
      futures::yield_now().await;
      spinning_tx.send(()).unwrap();
      let began = std::time::Instant::now();
      let mut seen = false;
      while began.elapsed() < ARRIVAL_BOUND {
        if futures::others_waiting() {
          seen = true;
          break;
        }
        std::hint::spin_loop();
      }
      seen_tx.send((seen, began.elapsed())).unwrap();
    })
    .unwrap();
  spinning_rx.recv_timeout(ARRIVAL_BOUND).unwrap();
  runtime
    .spawn_on(other_shard, async move {
      let _stream = TcpStream::connect(addr).await;
      // Held open until the busy shard has had its bound to see it.
      futures::sleep(ARRIVAL_BOUND.as_nanos().try_into().unwrap())
        .await
        .unwrap();
    })
    .unwrap();
  let (seen, after) = seen_rx.recv_timeout(ARRIVAL_BOUND * 2).unwrap();
  let _ = runtime.shutdown();
  assert!(
    seen,
    "a connection arriving on a busy shard went unseen for {after:?}"
  );
}

/// A-783. Do: a busy task on a shard spins asking whether anyone waits; from outside the shard, a task is spawned onto
/// it (a message on its control channel, as another shard's call or a moved connection arrives). Expect: seen waiting
/// within the bound.
#[test]
fn a_spawn_sent_to_a_busy_shard_is_seen_waiting() {
  use slates_rt::runtime::Runtime;
  use std::sync::mpsc::channel;

  let runtime = Runtime::start(&config()).unwrap();
  let busy_shard = runtime.shard_ids()[0];
  let (spinning_tx, spinning_rx) = channel();
  let (seen_tx, seen_rx) = channel();
  runtime
    .spawn_on(busy_shard, async move {
      spinning_tx.send(()).unwrap();
      let began = std::time::Instant::now();
      let mut seen = false;
      while began.elapsed() < ARRIVAL_BOUND {
        if futures::others_waiting() {
          seen = true;
          break;
        }
        std::hint::spin_loop();
      }
      seen_tx.send((seen, began.elapsed())).unwrap();
    })
    .unwrap();
  spinning_rx.recv_timeout(ARRIVAL_BOUND).unwrap();
  runtime.spawn_on(busy_shard, async {}).unwrap();
  let (seen, after) = seen_rx.recv_timeout(ARRIVAL_BOUND * 2).unwrap();
  let _ = runtime.shutdown();
  assert!(
    seen,
    "a spawn sent to a busy shard went unseen for {after:?}"
  );
}

/// A-783 (the A/B's newcomer, found 2026-11-20 by its log: a ref advertisement's connection, moved to its repository's
/// shard just before a build turn there, was taken into the same step's batch of ready tasks as the build, so it was
/// in neither the queue nor the control channel while the turn asked; it waited out the turn's 80 ms, and the probe
/// found no one 4,263 times). Do: from outside a shard, spawn a task that spins asking whether anyone waits, then at
/// once a second task, both admitted in one step and taken into one batch. Expect: the spinner sees the second task
/// waiting.
#[test]
fn a_task_ready_in_the_same_batch_is_seen_waiting() {
  use slates_rt::runtime::Runtime;
  use std::sync::mpsc::channel;

  let runtime = Runtime::start(&config()).unwrap();
  let shard = runtime.shard_ids()[0];
  let (seen_tx, seen_rx) = channel();
  let (gate_tx, gate_rx) = channel::<()>();
  // Both spawns are sent while the shard is held by a first task, so its next step admits both and takes both into
  // one batch, the spinner first.
  runtime
    .spawn_on(shard, async move {
      let _ = gate_rx.recv_timeout(ARRIVAL_BOUND);
    })
    .unwrap();
  runtime
    .spawn_on(shard, async move {
      let began = std::time::Instant::now();
      let mut seen = false;
      while began.elapsed() < ARRIVAL_BOUND {
        if futures::others_waiting() {
          seen = true;
          break;
        }
        std::hint::spin_loop();
      }
      seen_tx.send((seen, began.elapsed())).unwrap();
    })
    .unwrap();
  runtime.spawn_on(shard, async {}).unwrap();
  gate_tx.send(()).unwrap();
  let (seen, after) = seen_rx.recv_timeout(ARRIVAL_BOUND * 2).unwrap();
  let _ = runtime.shutdown();
  assert!(
    seen,
    "a task ready in the same batch went unseen for {after:?}"
  );
}
