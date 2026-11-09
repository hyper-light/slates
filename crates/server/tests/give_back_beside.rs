//! A give-back makes way for what else the shard is doing (A-226, §4.3): a task that allocates nothing, as a granted
//! landing does, looks idle to the arena, so nothing else stops the give-back's steps from taking turns with it.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;

/// Shape: the idle steps a give-back beside another task may take: the one before it sees the neighbour ran, and one
/// more for the daemon's own reap tick landing inside the probe.
const STEPS_BESIDE_A_NEIGHBOUR: u64 = 2;

/// A-226, §4.3. Do: on a one-shard daemon, give back the whole of the dead recovery memory from a page slice, once with
/// another task taking turns on the shard the whole time, allocating nothing as a granted landing does, and once alone.
/// Expect: beside the neighbour the give-back stops after a step or two and leaves the rest for the next reap tick;
/// alone it takes more. Under load a landing was 4-5 times slower, a give-back step of a whole quantum between each of
/// its short steps: median 20.9 s against 4.3 s, and the give-back's 3,925 to 6,900 steps took 26 to 68% of the
/// landing's time. Where a give-back takes so few steps that the two cannot differ (Linux's hole punch: tens of
/// microseconds for 11 GB, learned in one step) the test says so and passes; macOS's CI lane is where it can go red.
#[test]
fn a_give_back_leaves_the_rest_for_the_next_tick_when_another_task_is_running() {
  let profile = common::machine_profile();
  let instance = format!("srv-give-back-beside-{}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, &instance, Some(1)),
    SegmentSource::Create {
      name: format!("slates-seg-give-back-beside-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon.bootstrap(true).unwrap();
  let alone = daemon.give_back_beside(false).unwrap();
  let beside = daemon.give_back_beside(true).unwrap();
  daemon.stop();
  eprintln!("a give-back took {alone} idle steps alone and {beside} beside another task");
  if alone <= STEPS_BESIDE_A_NEIGHBOUR {
    eprintln!(
      "SKIPPED: this host gives its memory back in {alone} steps, so a neighbour cannot change them"
    );
    return;
  }
  assert!(
    beside <= STEPS_BESIDE_A_NEIGHBOUR,
    "a give-back beside another task leaves the rest for the next tick: {beside} steps (alone: {alone})"
  );
}
