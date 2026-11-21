//! A work over a green costs its divergence, never a copy of the green (§4.16, A-247's lazy base): creating N works over
//! a G-byte green, each reading one existing file and editing it, adds O(N x (metadata + the file it edits)) resident
//! bytes, never N x G. The green's base is pinned by each work's floor and read where it lies; a work holds only what it
//! changed (`crate::work_files::WorkFiles`).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
#![cfg(unix)]

use std::time::{Duration, Instant};

use slates_client::{Client, Submitted};
use slates_ipc::protocol::{ReadAt, VolumeId};
use slates_machine::facts::Facts;
use slates_server::{Daemon, DaemonConfig};

mod common;
use common::client::connect;

/// Shape: the green's files; with [`FILE_BYTES`] a 64 MiB green, large enough that one copy of it per work dwarfs any
/// work's own metadata. `SLATES_WORK_COW_FILES` overrides it for the measured grid (run-requests).
const FILES: usize = 1024;
/// Shape: one file's bytes, small enough that one edit request carries a whole file (`SLATES_WORK_COW_FILE_BYTES`).
const FILE_BYTES: usize = 64 * 1024;
/// Shape: the works made over the green, enough that N copies of it (1 GiB) and N works' own bytes are far apart
/// (`SLATES_WORK_COW_WORKS`).
const WORKS: usize = 16;
/// Bound: one work's own resident bytes besides the file it changed: its catalog record, floor, journal entry, delta
/// map and the reply buffers its verbs touched. A generous ceiling: measured far below it.
const WORK_METADATA_BYTES: usize = 64 * 1024;
/// Bound: what the run may add once whatever the works do (the allocator's page granularity, a map's growth past a
/// power of two): fixed, never per work.
const FIXED_BYTES: usize = 8 * 1024 * 1024;

fn daemon() -> (Daemon, String) {
  let profile = common::machine_profile();
  let instance = format!("srv-work-cow-{}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, &instance, Some(1)),
    slates_server::SegmentSource::Create {
      name: format!("slates-seg-work-cow-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  (daemon, instance)
}

/// The grid's sizes: the files, one file's bytes, the works; each [`FILES`], [`FILE_BYTES`], [`WORKS`] unless the
/// environment names another.
#[derive(Clone, Copy)]
struct Sizes {
  files: usize,
  file_bytes: usize,
  works: usize,
}

fn sizes() -> Sizes {
  let named = |key: &str, default: usize| {
    std::env::var(key)
      .ok()
      .map_or(default, |value| value.parse().unwrap())
  };
  Sizes {
    files: named("SLATES_WORK_COW_FILES", FILES),
    file_bytes: named("SLATES_WORK_COW_FILE_BYTES", FILE_BYTES),
    works: named("SLATES_WORK_COW_WORKS", WORKS),
  }
}

fn path(index: usize) -> String {
  format!("dir-{:02}/file-{index:05}", index % 64)
}

/// Shape: the most bytes and files one seed work lands: a submit of a whole 1 GiB or 100,000-file green holds its
/// shard past its liveness window, and the watchdog ends the daemon (GAPS, 2026-11-20), so the green is seeded in parts.
const SEED_BYTES: usize = 16 * 1024 * 1024;
/// Shape: see [`SEED_BYTES`].
const SEED_FILES: usize = 5000;

/// A green of `sizes.files` files of `sizes.file_bytes` each, landed by seed works of at most [`SEED_FILES`] files and
/// [`SEED_BYTES`] bytes each.
fn populated_green(client: &mut Client, sizes: Sizes) -> VolumeId {
  let green = client.create_green("cow-green", false).unwrap();
  let per_seed = (SEED_BYTES / sizes.file_bytes.max(1)).clamp(1, SEED_FILES);
  let mut bytes = vec![0u8; sizes.file_bytes];
  for (part, first) in (0..sizes.files).step_by(per_seed).enumerate() {
    let (seed, _) = client
      .create_work(green, &format!("cow-seed-{part}"))
      .unwrap();
    for index in first..(first + per_seed).min(sizes.files) {
      bytes.fill(u8::try_from(index % 251).unwrap());
      client.edit(seed, &path(index), 0, 0, &bytes).unwrap();
    }
    match client.submit(seed).unwrap() {
      Submitted::Accepted(_) => {}
      other => panic!("seed part {part}'s submit was not accepted: {other:?}"),
    }
  }
  green
}

/// One agent's first steps on a work: request it, read one existing file, change one byte of it. The time they took.
fn first_edit(client: &mut Client, green: VolumeId, sizes: Sizes, index: usize) -> Duration {
  let started = Instant::now();
  let (work, _) = client
    .create_work(green, &format!("cow-work-{index}"))
    .unwrap();
  let target = path(index * 61 % sizes.files);
  let read = client.read(work, &target, ReadAt::Head).unwrap();
  assert_eq!(
    read.len(),
    sizes.file_bytes,
    "the work reads the green's file"
  );
  client.edit(work, &target, 0, 1, b"w").unwrap();
  started.elapsed()
}

fn resident() -> usize {
  usize::try_from(Facts::resident_now().expect("this OS reports resident memory")).unwrap()
}

/// The bytes live allocations hold now, as the allocator counts them (`slates_mem::heap`): a freed block the allocator
/// keeps for reuse is not counted, so a copy made into one shows where resident memory hides it.
fn live_heap() -> usize {
  usize::try_from(slates_mem::heap::stats().in_use).unwrap()
}

/// §4.16, A-247: N works over one G-byte green, each requested, reading one base file and editing it, add resident
/// bytes bounded by N x (a work's metadata + the file it changed) plus a fixed term, below even one copy of the green.
#[test]
fn works_over_a_green_cost_their_divergence_not_the_green() {
  let sizes = sizes();
  let green_bytes = sizes.files * sizes.file_bytes;
  let (_daemon, instance) = daemon();
  let mut client = connect(&instance);
  let green = populated_green(&mut client, sizes);
  // One work first, so what a first work alone sets up (a map's first node, a reply buffer) is not charged to the N.
  let warm = first_edit(&mut client, green, sizes, sizes.works);
  let (before, heap_before) = (resident(), live_heap());
  let times: Vec<Duration> = (0..sizes.works)
    .map(|index| first_edit(&mut client, green, sizes, index))
    .collect();
  let added = resident().saturating_sub(before);
  let heap_added = live_heap().saturating_sub(heap_before);
  let bound = sizes.works * (WORK_METADATA_BYTES + sizes.file_bytes) + FIXED_BYTES;
  let slowest = times.iter().max().unwrap();
  let total: Duration = times.iter().sum();
  println!(
    "work_cow: green {green_bytes} B in {} files; {} works, each request -> read -> edit: live heap +{heap_added} B \
     ({} B/work), resident +{added} B, bound {bound} B, one copy per work would be {} B; time warm {warm:?}, mean {:?}, slowest {slowest:?}",
    sizes.files,
    sizes.works,
    heap_added / sizes.works,
    sizes.works * green_bytes,
    total / u32::try_from(sizes.works).unwrap(),
  );
  if sizes.works * green_bytes <= bound {
    println!(
      "work_cow: a copy of the green per work would fit the bound at these sizes; the bound decides nothing"
    );
  }
  assert!(
    heap_added <= bound,
    "{} works over a {green_bytes}-byte green added {heap_added} live heap bytes, past {bound}",
    sizes.works
  );
  assert!(
    added <= bound,
    "{} works over a {green_bytes}-byte green added {added} resident bytes, past {bound}: a work holds more than its \
     divergence",
    sizes.works
  );
}
