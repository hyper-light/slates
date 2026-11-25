//! A-924 piece 2a's tests: a shard's conversation with a spawner its runtime started ([`super::TestSpawner`]): a child
//! started through it has its exit answered; a shard holding no end is refused typed; and the spawner's death answers a
//! running child's exit and every later spawn typed, never a hang.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::mpsc::channel;

use slates_ipc::confined_spawn::{ChildStream, Environment, Spawn};
use slates_rt::runtime::{LocalRuntime, RuntimeConfig};

use super::{SpawnRefused, TestSpawner, spawn};
use crate::daemon::LIVENESS_BUDGET_NS;

/// Shape: how long a test lets its runtime's work take before giving up on it: ten liveness windows.
const TEST_BOUND_NS: u64 = 10 * LIVENESS_BUDGET_NS;

/// A one-shard runtime for a test's own work.
fn runtime() -> LocalRuntime {
  LocalRuntime::new(&RuntimeConfig {
    shards: 1,
    tasks_per_shard: 8,
    timers_per_shard: 8,
    ring_entries: 8,
    step_budget_ns: 1_000_000,
    timer_tick_ns: 100_000,
    batch: 8,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  })
  .expect("a local runtime")
}

/// Runs `work` on a runtime of its own, within [`TEST_BOUND_NS`]: what it answered.
fn on_runtime<T: Send + 'static>(work: impl Future<Output = T> + 'static) -> T {
  let runtime = runtime();
  let (sent, got) = channel();
  runtime
    .spawn(async move {
      let answered = slates_rt::futures::within(TEST_BOUND_NS, work).await;
      let _ = sent.send(answered.ok().flatten());
    })
    .expect("the test's task");
  runtime.run_until_idle();
  got
    .recv()
    .ok()
    .flatten()
    .expect("the work answered within the test's bound")
}

/// `/bin/sh` with `args`, its streams the null device, as its group's leader.
fn shell<'a>(args: &'a [&'a OsStr]) -> Spawn<'a> {
  Spawn {
    program: Path::new("/bin/sh"),
    args,
    environment: Environment::Inherited,
    overrides: &[],
    directory: Path::new("/"),
    own_group: true,
    stdin: ChildStream::Null,
    stdout: ChildStream::Null,
    stderr: ChildStream::Null,
    handed: &[],
  }
}

/// A-924 piece 2a. Do: install a spawner's end on the shard; start `sh -c 'exit 5'` through it and await its exit.
/// Expect: exited with code 5.
#[test]
fn a_child_started_through_the_shards_spawner_has_its_exit_answered() {
  let code = on_runtime(async {
    let _spawner = TestSpawner::installed();
    let args = [OsStr::new("-c"), OsStr::new("exit 5")];
    let child = spawn(&shell(&args)).await.expect("spawned");
    child.exit().await.map(|status| status.code())
  });
  assert_eq!(code, Ok(Some(5)));
}

/// A-924 piece 2a. Do: on a shard holding no spawner's end, ask for a child. Expect: refused `NoSpawner`, typed.
#[test]
fn a_shard_holding_no_spawner_is_refused_typed() {
  let refused = on_runtime(async {
    let args = [OsStr::new("-c"), OsStr::new("exit 0")];
    spawn(&shell(&args)).await.map(|_| ())
  });
  assert_eq!(refused, Err(SpawnRefused::NoSpawner));
}

/// A-924 piece 2a, the spawner's death (main's ruling of 2026-11-25). Do: start a child that sleeps far past the test
/// through the shard's spawner; kill the spawner; await the child's exit; then ask for another child. Expect: the exit
/// answered `SpawnerGone` and the next spawn refused `SpawnerGone`, both typed and neither waiting past the test's
/// bound; the orphaned child's group is the asker's to kill.
#[test]
fn the_spawners_death_answers_a_running_childs_exit_and_later_spawns_typed() {
  let (exit, later) = on_runtime(async {
    let spawner = TestSpawner::installed();
    let args = [OsStr::new("-c"), OsStr::new("exec sleep 3600")];
    let child = spawn(&shell(&args)).await.expect("spawned");
    spawner.kill();
    let exit = child.exit().await.map(|_| ());
    child.kill_group();
    let later = spawn(&shell(&args)).await.map(|_| ());
    (exit, later)
  });
  assert_eq!(exit, Err(SpawnRefused::SpawnerGone));
  assert_eq!(later, Err(SpawnRefused::SpawnerGone));
}

/// A-924 piece 2b, the restart (main's ruling of 2026-11-25). Do: make this shard the spawner's owner; start a child
/// that sleeps far past the test; kill the spawner; await the child's exit; then ask for `sh -c 'exit 6'`. Expect: the
/// running child answered `SpawnerGone`; the next spawn waits for the owner's restart, is started by a new spawner (its
/// pid not the dead one's), and exits with code 6. Red before: every spawn after the death refused `SpawnerGone`.
#[test]
fn a_dead_spawner_is_restarted_and_a_spawn_asked_after_its_death_gets_its_child() {
  let (exit, dead, restarted, code) = on_runtime(async {
    let spawner = TestSpawner::owned();
    let dead = spawner.pid();
    let sleeping = [OsStr::new("-c"), OsStr::new("exec sleep 3600")];
    let child = spawn(&shell(&sleeping)).await.expect("spawned");
    spawner.kill();
    let exit = child.exit().await.map(|_| ());
    child.kill_group();
    let exiting = [OsStr::new("-c"), OsStr::new("exit 6")];
    let after = spawn(&shell(&exiting))
      .await
      .expect("spawned after the restart");
    let code = after.exit().await.map(|status| status.code());
    (exit, dead, spawner.pid(), code)
  });
  assert_eq!(exit, Err(SpawnRefused::SpawnerGone));
  assert_ne!(restarted, dead, "a new spawner started the child");
  assert_eq!(code, Ok(Some(6)));
}

/// A-924 piece 2b, the bound. Do: on a shard whose spawns in flight are bounded at one, start a child that sleeps far
/// past the test, and while it runs ask for another. Expect: the second refused `Busy`, typed; the first still answered
/// once its group is killed.
#[test]
fn a_spawn_past_its_shards_bound_is_refused_busy() {
  let (second, first) = on_runtime(async {
    let _spawner = TestSpawner::bounded(1);
    let sleeping = [OsStr::new("-c"), OsStr::new("exec sleep 3600")];
    let child = spawn(&shell(&sleeping)).await.expect("spawned");
    let second = spawn(&shell(&sleeping)).await.map(|_| ());
    child.kill_group();
    let first = child.exit().await.map(|status| status.success());
    (second, first)
  });
  assert_eq!(second, Err(SpawnRefused::Busy));
  assert_eq!(first, Ok(false));
}

/// A-924 piece 2b, the back-off (main's question on (c)'s red: a spawner that ended at once was started 12 times in a
/// window and a half, the doubling of a few milliseconds' start slow to reach the window). Do: ask the back-off after
/// one, two, three and forty deaths in a row. Expect: none after one (a spawner that died once is restarted at once),
/// then a whole liveness window after each later one, so a spawner that cannot stay up is started at most twice a
/// window. Red before: a millisecond's start waited a millisecond after two deaths, two after three.
#[test]
fn the_spawners_back_off_is_a_liveness_window_after_its_first_death() {
  let waits: Vec<u64> = [1, 2, 3, 40].into_iter().map(super::back_off_ns).collect();
  assert_eq!(
    waits,
    [
      0,
      LIVENESS_BUDGET_NS,
      LIVENESS_BUDGET_NS,
      LIVENESS_BUDGET_NS
    ]
  );
}

/// Shape: how long [`a_lingering_spawner_role`] lives after it drops its ends: three liveness windows, past the bound a
/// shard is judged stuck by, so a blocking wait on it would wedge the shard.
const LINGER_NS: u64 = 3 * LIVENESS_BUDGET_NS;

/// A stand-in for a spawner that ends its conversation and lingers, as `reap_map`'s did under load (its ends closed at
/// its exit while the kernel took seconds to make it waitable): started as this binary's own test, it drops its ends at
/// once and leaves its group for its parent's, so its owner's kill of its group misses it, and lives [`LINGER_NS`].
#[test]
fn a_lingering_spawner_role() {
  let Some(ends) = slates_ipc::spawner::ends_from_env() else {
    return;
  };
  drop(ends);
  // It leads a group of its own (its start's), which `setsid` refuses a leader: it moves into its parent's group
  // instead, so the kill of its own reaches none.
  if let Some(parent) = rustix::process::getppid()
    && let Ok(group) = rustix::process::getpgid(Some(parent))
  {
    let _ = rustix::process::setpgid(None, Some(group));
  }
  let until = std::time::Instant::now() + std::time::Duration::from_nanos(LINGER_NS);
  while std::time::Instant::now() < until {
    std::thread::park_timeout(until.saturating_duration_since(std::time::Instant::now()));
  }
  std::process::exit(0);
}

thread_local! {
  /// The longest gap between two steps of [`tick`], in nanoseconds.
  static TICK_MOST: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
  /// Tells [`tick`] to stop.
  static TICK_STOP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Shape: the test runtime's timer tick ([`runtime`]'s), what [`tick`] sleeps between its steps.
const TICK_NS: u64 = 100_000;

/// A task that steps once a timer tick until told to stop or [`TEST_BOUND_NS`] has passed, keeping the longest gap
/// between its steps: a shard blocked in a wait shows as a gap that long. It sleeps rather than yields, so the shard
/// still turns to its driver for the descriptors its other tasks wait on.
async fn tick() {
  let began = slates_rt::futures::now_ns();
  let mut last = began;
  while !TICK_STOP.with(std::cell::Cell::get) && last.saturating_sub(began) < TEST_BOUND_NS {
    let _ = slates_rt::futures::sleep(TICK_NS).await;
    let now = slates_rt::futures::now_ns();
    TICK_MOST.with(|most| most.set(most.get().max(now.saturating_sub(last))));
    last = now;
  }
}

/// Main 01a20182's ruling of 2026-11-25 (A-924 2b's regression: `reap_map`'s test process ended by a daemon's watchdog,
/// a shard wedged 1,140 ms in the spawner's restart, which reaped the dead spawner with a blocking wait). Do: make this
/// shard the owner of a spawner that ends its conversation at once and lingers outside its group
/// ([`a_lingering_spawner_role`]); run a task stepping at every turn; ask for `sh -c 'exit 3'` until the restarted
/// spawner starts it. Expect: the child started and exited 3, and the shard never held past a liveness window: the dead
/// spawner reaped by its own task, awaiting its exit, while the shard steps. Red before: the restart waited on the
/// lingering spawner on the shard, a gap of its [`LINGER_NS`].
#[test]
fn a_spawner_that_ends_its_conversation_and_lingers_is_reaped_while_its_shard_steps() {
  let (gap, code) = on_a_thread_within(|| reaped_while_stepping());
  assert_eq!(code, Ok(Some(3)));
  assert!(
    gap < LIVENESS_BUDGET_NS,
    "the shard stepped while the dead spawner was reaped: its longest gap {gap} ns against {LIVENESS_BUDGET_NS}"
  );
}

/// `work` run on a runtime of its own on a thread of its own, its answer awaited here for at most twice
/// [`TEST_BOUND_NS`] of the wall: a shard blocked in a wait cannot fire its own bound's timer, so the test's bound is
/// kept off the shard, and a shard that never answers fails the test rather than hanging it.
fn on_a_thread_within<T: Send + 'static>(work: fn() -> T) -> T {
  let (sent, got) = channel();
  let _runner = std::thread::spawn(move || {
    let _ = sent.send(work());
  });
  got
    .recv_timeout(std::time::Duration::from_nanos(
      TEST_BOUND_NS.saturating_mul(2),
    ))
    .expect("the shard answered within twice the test's bound, never held in a wait")
}

/// [`a_spawner_that_ends_its_conversation_and_lingers_is_reaped_while_its_shard_steps`]'s work on its own runtime:
/// the shard's longest gap between steps, and the child's exit code.
fn reaped_while_stepping() -> (u64, Result<Option<i32>, SpawnRefused>) {
  on_runtime(async {
    let here = slates_rt::futures::shard_id().map_or(0, |shard| shard.0);
    let program = std::env::current_exe().expect("the test binary");
    let args = [
      OsStr::new("spawning::tests::a_lingering_spawner_role"),
      OsStr::new("--exact"),
      OsStr::new("--nocapture"),
    ];
    let slates_ipc::spawner::Started { child, ends } =
      slates_ipc::spawner::start(&program, &args, 1).expect("the lingering spawner");
    super::own(child, vec![here], None);
    for end in ends {
      let placement = super::Placement {
        generation: 0,
        owner: here,
        bound: super::TEST_SPAWNS_IN_FLIGHT,
      };
      super::install(end, placement).expect("its end installed");
    }
    TICK_STOP.with(|stop| stop.set(false));
    TICK_MOST.with(|most| most.set(0));
    slates_rt::futures::spawn(tick())
      .and_then(slates_rt::futures::detach)
      .expect("the ticker");
    let exiting = [OsStr::new("-c"), OsStr::new("exit 3")];
    let child = loop {
      match spawn(&shell(&exiting)).await {
        Ok(child) => break child,
        Err(SpawnRefused::SpawnerGone) => slates_rt::futures::yield_now().await,
        Err(refused) => panic!("refused {refused:?}"),
      }
    };
    let code = child.exit().await.map(|status| status.code());
    drop(child);
    TICK_STOP.with(|stop| stop.set(true));
    super::uninstall();
    let _ = super::OWNER.with(|owner| owner.try_borrow_mut().ok().map(|mut owner| owner.take()));
    (TICK_MOST.with(std::cell::Cell::get), code)
  })
}
