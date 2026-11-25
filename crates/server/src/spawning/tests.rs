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

/// A-924 piece 2b, the back-off (RFC 6298 §5.5's doubling, capped at the liveness window). Do: ask the back-off after
/// one, two, three and forty deaths in a row of a spawner whose start took a millisecond. Expect: none after one (a
/// spawner that died once is restarted at once), then the start's length, then twice it, and at forty the liveness
/// window, never past it.
#[test]
fn the_spawners_back_off_doubles_its_start_up_to_the_liveness_window() {
  let start_ns = 1_000_000;
  let waits: Vec<u64> = [1, 2, 3, 40]
    .into_iter()
    .map(|deaths| super::back_off_ns(deaths, start_ns))
    .collect();
  assert_eq!(waits, [0, start_ns, 2 * start_ns, LIVENESS_BUDGET_NS]);
}
