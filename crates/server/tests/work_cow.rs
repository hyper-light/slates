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

/// Step 4 (GAPS 2026-11-20): a single seed work carrying more than `CARRIED_INCREMENT_MAX` (default
/// `16 × 256 KiB = 4 MiB` in CI; override with `SLATES_WORK_COW_SINGLE_FILES=1000` and
/// `SLATES_WORK_COW_SINGLE_BYTES=1073741` for the full 1 GiB single-changeset seed) submits in ONE
/// call across a multi-shard daemon without splitting the seed and without stalling any shard past
/// the liveness window: `laid_post_state`, `IncrementEncoder`, `AheadHold`, and `Green::make_step`
/// all slice cooperatively on both local and cross-shard owner paths.
#[test]
fn single_large_seed_work_submits_sliced_without_stalling_shards() {
  let env_usize = |key: &str, default: usize| {
    std::env::var(key)
      .ok()
      .map_or(default, |value| value.parse().unwrap())
  };
  let files = env_usize("SLATES_WORK_COW_SINGLE_FILES", 16);
  let file_bytes = env_usize("SLATES_WORK_COW_SINGLE_BYTES", 256 * 1024);
  let shards = env_usize("SLATES_WORK_COW_SINGLE_SHARDS", 4);
  let profile = common::machine_profile();
  let instance = format!("srv-single-seed-{}", std::process::id());
  let daemon = Daemon::start(
    &profile,
    DaemonConfig::derive(&profile, &instance, Some(u16::try_from(shards).unwrap())),
    slates_server::SegmentSource::Create {
      name: format!("slates-seg-single-seed-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon
    .bootstrap(true)
    .expect("the fixture explicitly creates its local consensus group");
  let mut client = connect(&instance);
  let green = client.create_green("single-seed-green", false).unwrap();
  let seed = write_single_large_seed(&mut client, green, files, file_bytes);
  let started = Instant::now();
  match client.submit(seed).unwrap() {
    Submitted::Accepted(version) => assert_eq!(version, 1),
    other => panic!("single large seed submit was not accepted: {other:?}"),
  }
  let submit_wall = started.elapsed();
  let (prepare_slices, ahead_named, submit_longest_us) = single_seed_counters(&mut client);
  let submit_pulse_ms = pulse_longest_ms(&daemon);
  assert!(
    prepare_slices > 0,
    "sliced preparation must run (non-vacuity)"
  );
  assert!(
    ahead_named > 0,
    "chunk-by-chunk AheadHold staging must supply the increment in O(1) (non-vacuity)"
  );
  verify_single_seed_green_readback(&mut client, green, files, file_bytes);
  let land_wall = land_and_verify_single_seed(&daemon, &mut client, green, files, file_bytes);
  let land_pulse_ms = pulse_longest_ms(&daemon);
  println!(
    "single_large_seed: {} files x {} B ({} MiB) on {shards} shards: submit={submit_wall:?} \
     land={land_wall:?} prepare_slices={prepare_slices} ahead_named={ahead_named} \
     submit_longest_us={submit_longest_us}us submit_pulse_longest={submit_pulse_ms}ms \
     land_pulse_longest={land_pulse_ms}ms",
    files,
    file_bytes,
    (files * file_bytes) / (1024 * 1024),
  );
  drop(client);
  daemon.stop();
}

fn write_single_large_seed(
  client: &mut Client,
  green: VolumeId,
  files: usize,
  file_bytes: usize,
) -> VolumeId {
  let (seed, _) = client.create_work(green, "single-seed-work").unwrap();
  let mut payload = vec![0u8; file_bytes];
  let chunk_max = 256 * 1024;
  for index in 0..files {
    let fill = u8::try_from((index % 251) + 1).unwrap();
    payload.fill(fill);
    let mut off = 0usize;
    while off < file_bytes {
      let end = (off + chunk_max).min(file_bytes);
      client
        .edit(
          seed,
          &path(index),
          u64::try_from(off).unwrap(),
          0,
          &payload[off..end],
        )
        .unwrap();
      off = end;
    }
  }
  seed
}

fn single_seed_counters(client: &mut Client) -> (u64, u64, u64) {
  let status = client.daemon_status().unwrap();
  let sum = |name: &str| -> u64 {
    status
      .shards
      .iter()
      .flat_map(|s| s.refusals.iter())
      .filter(|r| r.kind == name)
      .map(|r| r.count)
      .sum()
  };
  let max = |name: &str| -> u64 {
    status
      .shards
      .iter()
      .flat_map(|s| s.refusals.iter())
      .filter(|r| r.kind == name)
      .map(|r| r.count)
      .max()
      .unwrap_or(0)
  };
  (
    sum("merge.submit.prepare_slices"),
    sum("push.held_ahead.named"),
    max("merge.submit.longest_step_us"),
  )
}

fn pulse_longest_ms(daemon: &Daemon) -> u64 {
  daemon
    .shard_pulses()
    .into_iter()
    .map(|p| p.longest_step_ns)
    .max()
    .unwrap_or(0)
    / 1_000_000
}

fn verify_single_seed_green_readback(
  client: &mut Client,
  green: VolumeId,
  files: usize,
  file_bytes: usize,
) {
  for index in [0, files / 2, files - 1] {
    let expected = u8::try_from((index % 251) + 1).unwrap();
    let got = client.read(green, &path(index), ReadAt::Head).unwrap();
    assert_eq!(got.len(), file_bytes, "file {index} length");
    assert!(
      got.iter().all(|&b| b == expected),
      "file {index} bytes mismatch"
    );
  }
}

fn land_and_verify_single_seed(
  daemon: &Daemon,
  client: &mut Client,
  green: VolumeId,
  files: usize,
  file_bytes: usize,
) -> Duration {
  let secret = daemon.segment().issuer_secret().unwrap();
  let target = common::target::target_dir();
  let land_started = Instant::now();
  let grant = common::landing::approve(client, &secret, green, None, &target.path);
  match client.land(
    green,
    None,
    &target.path,
    slates_ipc::protocol::Filter::default(),
    Some(grant),
  ) {
    Ok(slates_client::Landing::Landed(outcome)) => {
      assert_eq!(outcome.state, "done");
      // `outcome.written` counts both files and created parent directories (`dir-00..dir-63`).
      assert!(
        outcome.written >= u64::try_from(files).unwrap(),
        "expected at least {files} written entries, got {}",
        outcome.written
      );
    }
    other => panic!("single large seed disk landing failed: {other:?}"),
  }
  let land_wall = land_started.elapsed();
  for index in [0, files / 2, files - 1] {
    let expected = u8::try_from((index % 251) + 1).unwrap();
    let on_disk = std::fs::read(format!("{}/{}", target.path, path(index))).unwrap();
    assert_eq!(on_disk.len(), file_bytes, "disk file {index} length");
    assert!(
      on_disk.iter().all(|&b| b == expected),
      "disk file {index} bytes mismatch"
    );
  }
  land_wall
}
