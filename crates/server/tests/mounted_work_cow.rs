//! A mounted work costs its divergence, never a copy of its green (A-774 part 2; §4.16 "Role", D-5): N works over one
//! G-byte green, each mounted over the daemon's NFS, reading one existing file through the mount and saving one edit as
//! an editor does, add O(N x (metadata + the file it edits)) resident bytes, never N x G; and making a mount takes as
//! long over a large green as over a small one. Each mount reads its green's pinned version where it lies and copies
//! only what is edited.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
#![cfg(unix)]

use std::net::TcpStream;
use std::time::{Duration, Instant};

use slates_client::{Client, Submitted};
use slates_ipc::protocol::VolumeId;
use slates_machine::facts::Facts;
use slates_server::{Daemon, DaemonConfig};

mod common;
use common::client::connect;
use common::nfs::{NFS_PROGRAM, call, create, lookup, mount, opaque, read_whole, status, write};

/// Shape: the large green's files; with [`FILE_BYTES`] a 64 MiB green, so one copy of it per mount dwarfs any mount's
/// own metadata (`SLATES_MOUNT_COW_FILES` overrides it for the measured grid).
const FILES: usize = 1024;
/// Shape: the small green's files, against which a mount's making over the large one is timed.
const SMALL_FILES: usize = 16;
/// Shape: one file's bytes, small enough that one edit request carries a whole file.
const FILE_BYTES: usize = 64 * 1024;
/// Shape: the works mounted over the large green (`SLATES_MOUNT_COW_WORKS`): enough that N copies of it (512 MiB) and N
/// mounts' own bytes are far apart, few enough that every mount's attachment fits the test profile's partition (each
/// mount here keeps its attachment to the end).
const WORKS: usize = 8;
/// Shape: the mounts over the small green whose median the large green's mounts are timed against.
const SMALL_WORKS: usize = 4;
/// Bound: one mount's own resident bytes besides the file it saved: its slot, its root and the entries it looked up,
/// its op log's floor, its catalog record and attachment, the NFS connection's buffers. A generous ceiling.
const MOUNT_METADATA_BYTES: usize = 256 * 1024;
/// Bound: what the run may add once, whatever the mounts do (the allocator's page granularity, a map's growth past a
/// power of two): fixed, never per mount.
const FIXED_BYTES: usize = 8 * 1024 * 1024;
/// Bound: how much slower a mount over the large green may be made than one over the small, as a ratio of their
/// medians: a copy of the green grows with its 64 times as many files; a mount that reads where the green lies does not.
const MOUNT_TIME_RATIO: u32 = 4;
/// Bound: the timing floor under which two medians are equal whatever their ratio (scheduling jitter on a loaded box).
const MOUNT_TIME_FLOOR: Duration = Duration::from_millis(5);
/// Format: NFSv3 RENAME (RFC 1813 §3.3.14).
const NFSPROC3_RENAME: u32 = 14;

fn named(key: &str, default: usize) -> usize {
  std::env::var(key)
    .ok()
    .map_or(default, |value| value.parse().unwrap())
}

fn daemon() -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-mount-cow-{}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, &instance, Some(1)),
    slates_server::SegmentSource::Create {
      name: common::fixture_object("slates-seg", "mount-cow"),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

fn path(index: usize) -> String {
  format!("dir-{:02}/file-{index:05}", index % 64)
}

/// A green named `name` of `files` files of [`FILE_BYTES`] each, landed by one seed work.
fn populated_green(client: &mut Client, name: &str, files: usize) -> VolumeId {
  let green = client.create_green(name, false).unwrap();
  let (seed, _) = client.create_work(green, &format!("{name}-seed")).unwrap();
  let mut bytes = vec![0u8; FILE_BYTES];
  for index in 0..files {
    bytes.fill(u8::try_from(index % 251).unwrap());
    client.edit(seed, &path(index), 0, 0, &bytes).unwrap();
  }
  match client.submit(seed).unwrap() {
    Submitted::Accepted(_) => green,
    other => panic!("the seed's submit was not accepted: {other:?}"),
  }
}

/// Work `name` over `green` mounted over NFS: the time from requesting the work to holding the mount's root, the
/// stream and the root.
fn mounted_work(
  daemon: &Daemon,
  client: &mut Client,
  green: VolumeId,
  name: &str,
) -> (Duration, TcpStream, Vec<u8>) {
  let started = Instant::now();
  client.create_work(green, name).unwrap();
  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let capability = daemon.mount_capability(name).unwrap().unwrap();
  let root = mount(&mut stream, &capability, 1);
  (started.elapsed(), stream, root)
}

/// The handle of `path` under `root`, a directory at a time.
fn lookup_path(stream: &mut TcpStream, root: &[u8], path: &str, xid: &mut u32) -> Vec<u8> {
  let mut handle = root.to_vec();
  for part in path.split('/') {
    *xid += 1;
    handle = lookup(stream, &handle, part, *xid);
  }
  handle
}

/// Reads `path` through the mount and saves it with its first byte changed, as an editor does (a temporary file beside
/// it, renamed over it).
fn read_and_save(stream: &mut TcpStream, root: &[u8], path: &str) {
  let mut xid = 10;
  let file = lookup_path(stream, root, path, &mut xid);
  xid += 1;
  let mut bytes = read_whole(stream, &file, xid);
  assert_eq!(bytes.len(), FILE_BYTES, "the mount reads the green's file");
  bytes[0] = b'w';
  let (dir_path, name) = path.rsplit_once('/').unwrap();
  let dir = lookup_path(stream, root, dir_path, &mut xid);
  let temp = format!(".{name}.swp");
  xid += 1;
  let fh = create(stream, &dir, &temp, xid);
  xid += 1;
  write(stream, &fh, &bytes, xid);
  let mut args = Vec::new();
  opaque(&dir, &mut args);
  opaque(temp.as_bytes(), &mut args);
  opaque(&dir, &mut args);
  opaque(name.as_bytes(), &mut args);
  xid += 1;
  assert_eq!(
    status(&call(stream, NFS_PROGRAM, NFSPROC3_RENAME, &args, xid)),
    0
  );
}

fn resident() -> usize {
  usize::try_from(Facts::resident_now().expect("this OS reports resident memory")).unwrap()
}

fn median(mut times: Vec<Duration>) -> Duration {
  times.sort();
  times[times.len() / 2]
}

/// A-774 part 2 (§4.16, D-5). Do: over a 64 MiB green of 1,024 files, request N works, mount each over NFS, read one
/// file through it and save one edit as an editor does; time each mount's making against mounts over a 16-file green.
/// Expect: the mounts add at most N x (a mount's metadata + the file it saved) plus a fixed term, below even one copy of
/// the green; a mount over the large green is made within a small ratio of one over the small green.
#[test]
fn mounted_works_cost_their_divergence_not_the_green() {
  let files = named("SLATES_MOUNT_COW_FILES", FILES);
  let works = named("SLATES_MOUNT_COW_WORKS", WORKS);
  let green_bytes = files * FILE_BYTES;
  let (daemon, instance) = daemon();
  let mut client = connect(&instance);
  let small = populated_green(&mut client, "cow-small", SMALL_FILES);
  let large = populated_green(&mut client, "cow-large", files);

  // A mount of each first, so what a first mount alone sets up is not charged to the N.
  let _ = mounted_work(&daemon, &mut client, small, "cow-small-warm");
  let (_, mut stream, root) = mounted_work(&daemon, &mut client, large, "cow-large-warm");
  read_and_save(&mut stream, &root, &path(0));
  let small_times: Vec<Duration> = (0..SMALL_WORKS)
    .map(|index| mounted_work(&daemon, &mut client, small, &format!("cow-small-{index}")).0)
    .collect();

  let before = resident();
  let mut large_times = Vec::new();
  let mut streams = Vec::new();
  for index in 0..works {
    let (took, mut stream, root) =
      mounted_work(&daemon, &mut client, large, &format!("cow-large-{index}"));
    large_times.push(took);
    read_and_save(&mut stream, &root, &path(index * 61 % files));
    streams.push(stream);
  }
  let added = resident().saturating_sub(before);
  let bound = works * (MOUNT_METADATA_BYTES + FILE_BYTES) + FIXED_BYTES;

  let (small_median, large_median) = (median(small_times), median(large_times));
  println!(
    "mount_cow: green {green_bytes} B in {files} files; {works} mounts, each request -> mount -> read -> save: \
     resident +{added} B ({} B/mount), bound {bound} B, one copy per mount would be {} B; mount made in median \
     {large_median:?} over the large green, {small_median:?} over the {SMALL_FILES}-file green",
    added / works,
    works * green_bytes,
  );
  assert!(
    works * green_bytes > bound,
    "the bound must rule out a copy of the green per mount"
  );
  assert!(
    added <= bound,
    "{works} mounts over a {green_bytes}-byte green added {added} resident bytes, past {bound}: a mount holds more \
     than its divergence"
  );
  assert!(
    large_median <= MOUNT_TIME_FLOOR.max(small_median * MOUNT_TIME_RATIO),
    "a mount over the {files}-file green took {large_median:?}, the {SMALL_FILES}-file one's {small_median:?}: its \
     making grows with the green"
  );
  drop(streams);
  drop(client);
  daemon.stop();
}
