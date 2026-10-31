//! AC-3.3 by use: a writer outside FUSE is answered only once every FUSE mount of its volume has been told of its
//! change (§4.6 "Cache posture"; `crate::told` in the server), and a write through one FUSE mount reaches the volume's
//! other mounts just after its reply, the window AC-3.3's text states.
//!
//! Each test maps a file read-only through two FUSE mounts of one volume, as a program sharing a file through a mount
//! does, and reads the maps as memory alone the moment a writer's call returns: a map still holding its old page is a
//! reader that saw stale bytes after the mutating call returned. Before AC-3.3's wait, the mounts were told 52–193 µs
//! after the reply (median 82 µs, Linux, 2026-10-21), so the first read after the reply found the old bytes.
//!
//! Linux only, gated: skips loudly without `fusermount3` or `/dev/fuse`. The mount points are in the build output
//! (`common::target`; A-50), removed at the end.
#![cfg(target_os = "linux")]
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::TcpStream;
use std::os::fd::AsFd;
use std::process::Command;
use std::time::{Duration, Instant};

use slates_client::{Client, Intent, VolumeId};
use slates_server::{Daemon, DaemonConfig, SegmentSource};

mod common;
use common::client::{connect, scratch};
use common::target::{TargetDir, target_dir};

/// Shape: the shard counts the SDK test runs on. On one shard the client is served where its volume lives, so an
/// unheld reply is written to its ring before the mount's task has run at all: the case that catches a reply sent
/// early every time. On two the volume's owner — where the reply waits — may not be the client's shard, and the held
/// reply travels home.
const SDK_SHARDS: [u16; 2] = [1, 2];
/// Shape: the other tests' shard count: two, so the NFS connection and the mounts may start on a shard other than the
/// volume's owner.
const TEST_SHARDS: u16 = 2;
/// Shape: the writes each test makes. Without the wait, about one first read in ten after an SDK write on one shard was
/// stale, and one in four after an NFS WRITE (fedora-2, release, 2026-10-30: 7 and 4 of 50, 13 and 10 of 50), since
/// the mount is often told before the writer's thread has woken to read; 200 makes a pass by chance about 0.9^200.
/// Each round is about a millisecond.
const ROUNDS: u32 = 200;
/// Shape: how long a change through one mount may take to reach another's map before the test calls it lost: far past
/// the measured window (hundreds of microseconds), short enough that a lost change fails the test.
const REACHES_WITHIN: Duration = Duration::from_secs(10);
/// Format: the file each test maps.
const FILE: &str = "map.bin";
/// Format: the file's permissions when the SDK makes it.
const FILE_MODE: u32 = 0o644;
/// Format: the daemon's counter of writers whose reply waited for their volume's FUSE mounts (AC-3.3).
const TOLD_WAITS: &str = "fuse.told_waits";
/// Format: the daemon's counter of waits that reached their bound with a mount untold.
const TOLD_OVERDUE: &str = "fuse.told_overdue";
/// Format: the daemon's counter of writers answered unheld although a mount was untold.
const TOLD_UNWAITED: &str = "fuse.told_unwaited";

fn command_available(name: &str) -> bool {
  Command::new("sh")
    .args(["-c", &format!("command -v {name}")])
    .output()
    .map(|o| o.status.success())
    .unwrap_or(false)
}

/// Whether the FUSE tests run here; a skip is printed, never a failure.
fn fuse_available() -> bool {
  let available = command_available("fusermount3") && std::path::Path::new("/dev/fuse").exists();
  if !available {
    eprintln!("SKIP: no fusermount3 or /dev/fuse on this host; the FUSE transport needs both");
  }
  available
}

/// A started, bootstrapped daemon of `shards` shards, and its instance name.
fn daemon(tag: &str, shards: u16) -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(shards));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// The daemon's counter `name`, summed over its shards.
fn counter(daemon: &Daemon, name: &str) -> u64 {
  daemon
    .refusals_on_every_shard()
    .unwrap()
    .get(name)
    .copied()
    .unwrap_or(0)
}

/// A file mapped read-only through a mount, as a program that only reads it holds it.
struct ReadOnlyMap {
  map: *mut u8,
  length: usize,
  _file: std::fs::File,
}

impl ReadOnlyMap {
  /// Maps `length` bytes of `path`, shared and read-only, and reads it once so its pages are in the mount's cache.
  #[allow(clippy::disallowed_methods)] // the test's own open through the slates mount: RAM, not disk
  fn of(path: &str, length: usize) -> ReadOnlyMap {
    let file = std::fs::File::open(path).unwrap();
    // SAFETY: a fresh shared, read-only map of `length` bytes of a file this value holds open, at an address the kernel
    // chooses; nothing else maps it, and `Drop` unmaps it before the file closes.
    let map = unsafe {
      rustix::mm::mmap(
        std::ptr::null_mut(),
        length,
        rustix::mm::ProtFlags::READ,
        rustix::mm::MapFlags::SHARED,
        file.as_fd(),
        0,
      )
    }
    .unwrap()
    .cast::<u8>();
    let mapped = ReadOnlyMap {
      map,
      length,
      _file: file,
    };
    let _ = mapped.seen();
    mapped
  }

  /// The map's bytes as they read now: a copy taken a byte at a time with volatile loads, since the kernel replaces the
  /// map's pages when the file changes, and a slice held over memory that changes under it is not sound.
  fn seen(&self) -> Vec<u8> {
    (0..self.length)
      .map(|offset| {
        // SAFETY: `map` is `length` readable bytes, mapped in `of` and unmapped only in `Drop`, and `offset` is below
        // `length`; a volatile load of one byte holds no reference across the kernel's page replacement.
        unsafe { std::ptr::read_volatile(self.map.add(offset)) }
      })
      .collect()
  }

  /// Whether every byte reads `fill` now.
  fn shows(&self, fill: u8) -> bool {
    self.seen().iter().all(|byte| *byte == fill)
  }
}

impl Drop for ReadOnlyMap {
  fn drop(&mut self) {
    // SAFETY: unmaps exactly the map made in `of`, which nothing reads after this.
    let _ = unsafe { rustix::mm::munmap(self.map.cast(), self.length) };
  }
}

/// Two FUSE mounts of one volume, each a read-only mount with the file mapped through it.
struct TwoMounts {
  first: ReadOnlyMap,
  second: ReadOnlyMap,
  // The mount points, declared after the maps so the maps are unmapped before the directories are removed.
  _points: [TargetDir; 2],
}

impl TwoMounts {
  /// Mounts `volume` read-only twice and maps [`FILE`] through each.
  fn of(client: &mut Client, volume: VolumeId, length: usize) -> TwoMounts {
    let points = [target_dir(), target_dir()];
    for point in &points {
      client
        .attach_fuse(volume, Intent::Read, &point.path)
        .unwrap();
    }
    let [first, second] = &points;
    TwoMounts {
      first: ReadOnlyMap::of(&format!("{}/{FILE}", first.path), length),
      second: ReadOnlyMap::of(&format!("{}/{FILE}", second.path), length),
      _points: points,
    }
  }

  /// Whether either map, read now, still lacks a write of `fill`: a reader that saw stale bytes.
  fn stale_after(&self, fill: u8) -> bool {
    !(self.first.shows(fill) && self.second.shows(fill))
  }
}

/// The fill of round `round`: a byte no two consecutive rounds share, never the file's first fill.
fn fill_of(round: u32) -> u8 {
  b'B' + u8::try_from(round % 24).unwrap()
}

/// AC-3.3, §4.6 "Cache posture". Do: on a daemon of one shard and then of two ([`SDK_SHARDS`]), map `map.bin`
/// read-only through two FUSE mounts of one volume, then replace it through the SDK [`ROUNDS`] times, reading both maps
/// as memory alone the moment each write returns. Expect: every first read shows the write, since its reply waited
/// until both mounts were told; the daemon counts a wait per write and none overdue. Before the wait the mounts were
/// told after the reply: on one shard the first read found the old bytes nearly every time.
#[test]
fn an_sdk_write_is_answered_once_every_fuse_mount_of_its_volume_was_told() {
  if !fuse_available() {
    return;
  }
  for shards in SDK_SHARDS {
    sdk_writes_wait_for_two_mounts(shards);
  }
}

/// [`an_sdk_write_is_answered_once_every_fuse_mount_of_its_volume_was_told`] on a daemon of `shards` shards.
fn sdk_writes_wait_for_two_mounts(shards: u16) {
  let tag = format!("told-sdk{shards}");
  let (daemon, instance) = daemon(&tag, shards);
  let mut client = connect(&instance);
  let volume = client.create(&scratch(&tag)).unwrap();
  let writer = client
    .attach(volume, None, Intent::Write)
    .unwrap()
    .attachment;
  let length = rustix::param::page_size();
  client
    .fs_write((volume, writer), FILE, &vec![b'A'; length], FILE_MODE)
    .unwrap();
  let mounts = TwoMounts::of(&mut client, volume, length);
  let waits_before = counter(&daemon, TOLD_WAITS);
  let mut stale = Vec::new();
  for round in 0..ROUNDS {
    let fill = fill_of(round);
    client
      .fs_write((volume, writer), FILE, &vec![fill; length], FILE_MODE)
      .unwrap();
    if mounts.stale_after(fill) {
      stale.push(round);
    }
  }
  let waited = counter(&daemon, TOLD_WAITS).saturating_sub(waits_before);
  let (overdue, unwaited) = (
    counter(&daemon, TOLD_OVERDUE),
    counter(&daemon, TOLD_UNWAITED),
  );
  eprintln!(
    "AC-3.3 SDK ({shards} shards): {} of {ROUNDS} first reads stale; {waited} writes waited, {overdue} overdue, \
     {unwaited} unwaited",
    stale.len()
  );
  assert!(
    stale.is_empty(),
    "a map read the old bytes right after the SDK write returned ({shards} shards), rounds {stale:?}"
  );
  assert!(
    waited >= u64::from(ROUNDS),
    "non-vacuous: each write waited for the mounts ({waited} waits)"
  );
  assert_eq!(overdue, 0, "no wait reached its bound");
  assert_eq!(unwaited, 0, "every reply was held");
  drop(mounts);
  daemon.stop();
}

/// AC-3.3, §4.6 "Cache posture", the NFS writer. Do: create `map.bin` over the daemon's NFS export, map it read-only
/// through two FUSE mounts of the same volume, then write it over NFS (`FILE_SYNC`) [`ROUNDS`] times, reading both maps
/// as memory alone the moment each WRITE is answered. Expect: every first read shows the write, the daemon counting a
/// wait per write and none overdue.
#[test]
fn an_nfs_write_is_answered_once_every_fuse_mount_of_its_volume_was_told() {
  if !fuse_available() {
    return;
  }
  let (daemon, instance) = daemon("told-nfs", TEST_SHARDS);
  let mut client = connect(&instance);
  let volume = client.create(&scratch("told-nfs")).unwrap();
  let length = rustix::param::page_size();
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let path = daemon
    .mount_capability("told-nfs")
    .expect("the name's owner shard answers")
    .expect("a volume by that name is provisioned there");
  let root = common::nfs::mount(&mut stream, &path, 1);
  let file = common::nfs::create(&mut stream, &root, FILE, 2);
  common::nfs::write(&mut stream, &file, &vec![b'A'; length], 3);
  let mounts = TwoMounts::of(&mut client, volume, length);
  let waits_before = counter(&daemon, TOLD_WAITS);
  let mut stale = Vec::new();
  for round in 0..ROUNDS {
    let fill = fill_of(round);
    common::nfs::write(&mut stream, &file, &vec![fill; length], 4 + round);
    if mounts.stale_after(fill) {
      stale.push(round);
    }
  }
  let waited = counter(&daemon, TOLD_WAITS).saturating_sub(waits_before);
  let (overdue, unwaited) = (
    counter(&daemon, TOLD_OVERDUE),
    counter(&daemon, TOLD_UNWAITED),
  );
  eprintln!(
    "AC-3.3 NFS: {} of {ROUNDS} first reads stale; {waited} writes waited, {overdue} overdue, {unwaited} unwaited",
    stale.len()
  );
  assert!(
    stale.is_empty(),
    "a map read the old bytes right after the NFS WRITE was answered, rounds {stale:?}"
  );
  assert!(
    waited >= u64::from(ROUNDS),
    "non-vacuous: each WRITE waited for the mounts ({waited} waits)"
  );
  assert_eq!(overdue, 0, "no wait reached its bound");
  assert_eq!(unwaited, 0, "every wait was armed");
  drop(stream);
  drop(mounts);
  daemon.stop();
}

/// The `share`th share of the sorted `values`, in thousandths (500: the median).
fn per_mille(values: &[Duration], share: usize) -> Duration {
  /// Format: parts per thousand.
  const PER_MILLE: usize = 1_000;
  let mut sorted = values.to_vec();
  sorted.sort();
  let at = (sorted.len().saturating_sub(1)).saturating_mul(share) / PER_MILLE;
  sorted.get(at).copied().unwrap_or_default()
}

/// AC-3.3's window (§4.6 "Cache posture"). Do: write `map.bin` through one FUSE mount of a volume [`ROUNDS`] times,
/// each `pwrite` of the whole page, and after each watch the file's read-only map through a second mount of the same
/// volume, as memory alone, until it shows the write. Expect: every write reaches the second map, after the writer's
/// call returned — a write through a FUSE mount is answered before the volume's other FUSE mounts are told, since its
/// reply waiting on them could deadlock — and print the window.
#[test]
#[allow(clippy::disallowed_methods)] // the test's own calls through the slates mount: RAM, not disk
fn a_write_through_one_fuse_mount_reaches_another_just_after_its_reply() {
  use std::os::unix::fs::FileExt;
  if !fuse_available() {
    return;
  }
  let (daemon, instance) = daemon("told-fuse", TEST_SHARDS);
  let mut client = connect(&instance);
  let volume = client.create(&scratch("told-fuse")).unwrap();
  let length = rustix::param::page_size();
  let writing = target_dir();
  client
    .attach_fuse(volume, Intent::Write, &writing.path)
    .unwrap();
  let written = format!("{}/{FILE}", writing.path);
  std::fs::write(&written, vec![b'A'; length]).unwrap();
  let reading = target_dir();
  client
    .attach_fuse(volume, Intent::Read, &reading.path)
    .unwrap();
  let map = ReadOnlyMap::of(&format!("{}/{FILE}", reading.path), length);
  let file = std::fs::OpenOptions::new()
    .write(true)
    .open(&written)
    .unwrap();
  let (mut windows, mut first_reads_stale, mut lost) = (Vec::new(), 0u32, Vec::new());
  for round in 0..ROUNDS {
    let fill = fill_of(round);
    file.write_all_at(&vec![fill; length], 0).unwrap();
    let returned = Instant::now();
    if !map.shows(fill) {
      first_reads_stale = first_reads_stale.saturating_add(1);
    }
    loop {
      if map.shows(fill) {
        windows.push(returned.elapsed());
        break;
      }
      if returned.elapsed() > REACHES_WITHIN {
        lost.push(round);
        break;
      }
      std::hint::spin_loop();
    }
  }
  /// Format: the per-mille shares the window is printed at.
  const MEDIAN: usize = 500;
  /// Format: see [`MEDIAN`].
  const P90: usize = 900;
  eprintln!(
    "AC-3.3 FUSE-TO-FUSE WINDOW: {first_reads_stale} of {ROUNDS} first reads stale; reached the other mount after \
     median {:?}, p90 {:?}, max {:?}",
    per_mille(&windows, MEDIAN),
    per_mille(&windows, P90),
    windows.iter().max().copied().unwrap_or_default()
  );
  assert!(
    lost.is_empty(),
    "a write through one mount never reached the other within {REACHES_WITHIN:?}, rounds {lost:?}"
  );
  drop(file);
  drop(map);
  daemon.stop();
}
