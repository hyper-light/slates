//! A daemon serving FUSE mounts goes back to idle once their work is done (§4.3 "the serve loop's idle window";
//! §4.6 cache posture): a write through one of two mounts of a volume wakes the other mount's task to tell its kernel,
//! and then both rest. Until 2026-10-22 the second mount's task took its change signal's wake for a request on its
//! device, read nothing, never drained the signal, and was woken again at once: one shard used a whole core from the
//! first write on (measured on the box: 397 of 400 ticks over 4 s, about 92,000 empty wakes every 2 s)
//! (`docs/bugs/2026-10-22-a-readiness-wait-took-any-wake-of-its-task-for-its-own.md`).
//!
//! A test binary of its own, so the shard threads it measures are this daemon's alone: libtest runs a binary's
//! tests in parallel threads of one process, and another test's daemon at work would be counted.
//!
//! Linux only, gated: skips loudly without `fusermount3` or `/dev/fuse` (the CI Linux lane has both). The mount
//! points are in the build output (`common::target`; A-50), removed at the end.
#![cfg(target_os = "linux")]
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;
use std::time::{Duration, Instant};

use slates_client::Intent;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::landing::{connect, scratch};
use common::target::target_dir;

/// Shape: two shards, so the two mounts' tasks and the client may land on different shards.
const TEST_SHARDS: u16 = 2;
/// Shape: the volume's name.
const VOLUME: &str = "fuse-idle";
/// Shape: the bytes written through the first mount and read through the second.
const WRITTEN: &[u8] = b"written through the first mount";
/// Shape: how long the second mount is given to serve the write — one invalidation round and one read, which take
/// microseconds; far past them, short enough that a lost delivery fails the test.
const SERVED_WITHIN: Duration = Duration::from_secs(10);
/// Shape: how long the daemon is given to come to rest after the second mount has served the write. A shard spins
/// out an idle window after client activity, a multiple of its wake estimate (§4.3, A-42): microseconds to
/// milliseconds. This is far past it.
const SETTLE: Duration = Duration::from_millis(500);
/// Shape: how long the shard threads' CPU time is watched once the daemon has settled. A spinning shard is on a CPU
/// for nearly all of it and a resting one for almost none, so half of it separates the two whatever the machine's
/// load: a busy machine slows a spinning shard, and cannot make a resting one use CPU.
const IDLE_WATCH: Duration = Duration::from_secs(1);

fn command_available(name: &str) -> bool {
  Command::new("sh")
    .args(["-c", &format!("command -v {name}")])
    .output()
    .map(|o| o.status.success())
    .unwrap_or(false)
}

/// Waits until `done` holds or [`SERVED_WITHIN`] passes; whether it held.
fn eventually(mut done: impl FnMut() -> bool) -> bool {
  let started = Instant::now();
  while started.elapsed() < SERVED_WITHIN {
    if done() {
      return true;
    }
    #[allow(clippy::disallowed_methods)] // a test's own poll interval, not a shipped wait
    std::thread::sleep(Duration::from_millis(20));
  }
  done()
}

/// The CPU time, in clock ticks, this process's runtime shard threads (`slates-shard-*`) have used so far, read
/// from `/proc/self/task/*/stat` (`proc(5)`: `utime` and `stime` are the 14th and 15th fields).
fn shard_cpu_ticks() -> u64 {
  let mut total = 0u64;
  for entry in std::fs::read_dir("/proc/self/task").unwrap() {
    let task = entry.unwrap().path();
    let name = std::fs::read_to_string(task.join("comm")).unwrap_or_default();
    if !name.starts_with("slates-shard") {
      continue;
    }
    let stat = std::fs::read_to_string(task.join("stat")).unwrap_or_default();
    // The fields after the command's closing parenthesis start at the 3rd (state), so `utime` is the 12th of them
    // and `stime` the 13th. The command is parenthesized because it may hold spaces.
    let after = stat.rsplit_once(") ").map_or("", |(_, rest)| rest);
    let fields: Vec<&str> = after.split_whitespace().collect();
    let utime: u64 = fields
      .get(11)
      .and_then(|field| field.parse().ok())
      .unwrap_or(0);
    let stime: u64 = fields
      .get(12)
      .and_then(|field| field.parse().ok())
      .unwrap_or(0);
    total = total.saturating_add(utime).saturating_add(stime);
  }
  total
}

/// §4.3 (a shard parks when it has no work), §4.6 (cache posture); the bug record above. Do: mount one volume
/// twice through FUSE, write a file through the first mount, read it through the second, let the daemon settle, then
/// watch its shard threads' CPU time for [`IDLE_WATCH`]. Expect: the shards used less than half of it. Before the
/// fix one shard used all of it, for as long as the mounts lived.
#[test]
fn a_write_through_one_of_two_fuse_mounts_leaves_the_daemon_idle_afterwards() {
  if !command_available("fusermount3") || !std::path::Path::new("/dev/fuse").exists() {
    eprintln!("SKIP: no fusermount3 or /dev/fuse on this host; the FUSE transport needs both");
    return;
  }
  let profile = common::machine_profile();
  let instance = format!("srv-fuse-idle-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-fuse-idle-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let volume = client.create(&scratch(VOLUME)).unwrap();
  let first = target_dir();
  let second = target_dir();
  let first_mount = client
    .attach_fuse(volume, Intent::Write, &first.path)
    .unwrap();
  let second_mount = client
    .attach_fuse(volume, Intent::Write, &second.path)
    .unwrap();
  #[allow(clippy::disallowed_methods)]
  // the test's own write through the slates mount: RAM, not disk
  std::fs::write(format!("{}/f", first.path), WRITTEN).unwrap();
  let read_back = format!("{}/f", second.path);
  assert!(
    eventually(|| std::fs::read(&read_back).ok().as_deref() == Some(WRITTEN)),
    "the second mount serves the write made through the first"
  );
  #[allow(clippy::disallowed_methods)] // the test's own settling time, not a shipped wait
  std::thread::sleep(SETTLE);
  let before = shard_cpu_ticks();
  #[allow(clippy::disallowed_methods)] // the watched window itself
  std::thread::sleep(IDLE_WATCH);
  let used = shard_cpu_ticks().saturating_sub(before);
  let window = rustix::param::clock_ticks_per_second().saturating_mul(IDLE_WATCH.as_secs());
  client.detach(first_mount.attachment).unwrap();
  client.detach(second_mount.attachment).unwrap();
  daemon.stop();
  assert!(
    used.saturating_mul(2) < window,
    "the daemon's shards used {used} of {window} clock ticks in the {IDLE_WATCH:?} after the mounts had served the \
     write: a shard is spinning"
  );
}
