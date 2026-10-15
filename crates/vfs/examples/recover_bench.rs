//! What a daemon restart's volume rebuild costs as a volume's file count grows (§4.8, A-64). A restart rebuilds each
//! volume from its published image ([`Volume::from_image`]) before its shard serves, and every held NFS connection
//! (A-113) and waiting client (A-114) waits for it: on 2026-10-06 a 50,000-file volume took 61 ms of a shard's 61.2 ms
//! start (`restart-big.sh`, macOS release), about 1.2 µs a file. This builds one volume of `N` 100-byte files in
//! directories of 1,000 (the shape of that run), images it, then claims the image's blocks and rebuilds it into a
//! fresh store, and reports both steps per size, the best of [`ROUNDS`] with every round shown.
//!
//! `cargo run --release -p slates-vfs --example recover_bench` prints one CSV row per size. `RECOVER_BENCH_LOOP=N`
//! rebuilds the largest size N times and prints nothing else, so a sampling profiler can be attached.

// A benchmark harness: an unwrap here is a failed run, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::time::Instant;

use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::checkpoint_log::{Journal, KeyedRecord, ShardDelta};
use slates_vfs::clock::StepClock;
use slates_vfs::delta::VolumeRecord;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::recover::{Claims, KeyedImage, ShardImage, VolumeImage};
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Shape: the rounds recorded per size; the best is reported (BENCHMARKS.md best-of-N, all N shown).
const ROUNDS: usize = 5;
/// Shape: the file counts measured: a project, the measured run, and four times it.
const FILES: [usize; 3] = [10_000, 50_000, 200_000];
/// Shape: files per directory, as the measured run made them.
const PER_DIR: usize = 1_000;
/// Shape: each file's bytes, as the measured run wrote them.
const FILE_BYTES: usize = 100;
/// Format: the page size (the store's granule).
const PAGE: usize = 4096;
/// Shape: the RAM region the store maps: room for the largest size's blocks (address space; only what is written
/// is touched).
const REGION_BYTES: usize = 1 << 31;
/// Shape: the store's slab caps, above the largest size's inodes, directories and chunks.
const MAX_OBJECTS: usize = 1 << 19;
/// Shape: the volume's quota, above the largest size.
const QUOTA: u64 = 1 << 31;
/// Shape: the journal's bytes.
const JOURNAL_BYTES: usize = 1 << 20;
/// Format: the cache line.
const CACHE_LINE: usize = 64;
/// Shape: the small-directory cut-over, the store's measured default range.
const DIR_CUTOVER: usize = 32;

fn store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(REGION_BYTES, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: CACHE_LINE,
      max_dirs: MAX_OBJECTS,
      max_inodes: MAX_OBJECTS,
      max_chunks: MAX_OBJECTS,
      max_dir_blocks: MAX_OBJECTS,
      dir_cutover: DIR_CUTOVER,
    },
    arena,
    0,
  )
}

/// A volume of `files` files in directories of [`PER_DIR`], imaged, and the directory blocks it holds (created in name
/// order, as a client creates).
fn image_of(files: usize) -> (VolumeImage, usize) {
  let mut store = store();
  let mut vol = Volume::create(
    &mut store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Fold,
      quota: Quota::Bounded { limit: QUOTA },
      journal_bytes: JOURNAL_BYTES,
      clock: Box::new(StepClock::new(0, 1)),
    },
  )
  .unwrap();
  let root = vol.root_inode(&store).unwrap();
  let bytes = vec![b'x'; FILE_BYTES];
  for dir in 0..files.div_ceil(PER_DIR) {
    let dir_no = vol
      .mkdir_no(&mut store, root, &format!("d{dir:04}"), 0o755)
      .unwrap();
    for file in 0..PER_DIR.min(files - dir * PER_DIR) {
      let no = vol
        .create_file_no(&mut store, dir_no, &format!("f{file:04}"), 0o644)
        .unwrap();
      vol.write(&mut store, no, 0, &bytes).unwrap();
    }
  }
  (vol.to_image(&store, None).unwrap(), store.blocks.len())
}

/// Claims `image`'s blocks in a fresh store and rebuilds the volume: (claim µs, rebuild µs, directory blocks).
fn rebuild(image: &VolumeImage) -> (u128, u128, usize) {
  let mut fresh = store();
  let started = Instant::now();
  let claims = Claims::prepare(&mut fresh, [image]).unwrap();
  let claimed = started.elapsed().as_micros();
  let started = Instant::now();
  let vol = Volume::from_image(
    &mut fresh,
    image,
    &claims,
    Box::new(StepClock::new(0, 1)),
    JOURNAL_BYTES,
    None,
  )
  .unwrap();
  let rebuilt = started.elapsed().as_micros();
  assert!(
    vol.accounting().referenced_bytes > 0,
    "the rebuilt volume holds its files"
  );
  (claimed, rebuilt, fresh.blocks.len())
}

/// The volume's image as a committed checkpoint in a journal's memory, as a publication leaves it.
fn checkpointed(image: &VolumeImage) -> Vec<u8> {
  let shard = ShardImage::new(vec![KeyedImage {
    key: [1; 16],
    image: image.clone(),
  }]);
  let mut room = 1usize << 20;
  loop {
    let mut slots = vec![0u8; room];
    if Journal::default().checkpoint(&mut slots, &shard).is_ok() {
      return slots;
    }
    room *= 2;
  }
}

/// Shape: the delta log's bytes in the journal bench: larger than any checkpoint here, so the checkpoint rule (deltas
/// since the last checkpoint reach its size) decides when to checkpoint, as in the daemon.
const LOG_BYTES: usize = 1 << 28;

/// A volume of `files` files made one create at a time with a publication after each, as the daemon's barrier does:
/// a checkpoint when the journal wants one, else a delta; the journal's checkpoint memory, its log, and the delta
/// frames after the last checkpoint.
fn journal_of(files: usize) -> (Vec<u8>, Vec<u8>, usize) {
  let mut store = store();
  let mut vol = Volume::create(
    &mut store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Fold,
      quota: Quota::Bounded { limit: QUOTA },
      journal_bytes: JOURNAL_BYTES,
      clock: Box::new(StepClock::new(0, 1)),
    },
  )
  .unwrap();
  let root = vol.root_inode(&store).unwrap();
  let bytes = vec![b'x'; FILE_BYTES];
  let mut journal = Journal::default();
  let mut slots = vec![0u8; 1 << 28];
  let mut log = vec![0u8; LOG_BYTES];
  let mut scratch: Vec<u8> = Vec::new();
  let mut frames = 0usize;
  let mut dir_no = root;
  for file in 0..files {
    if file % PER_DIR == 0 {
      dir_no = vol
        .mkdir_no(&mut store, root, &format!("d{:04}", file / PER_DIR), 0o755)
        .unwrap();
    }
    let no = vol
      .create_file_no(
        &mut store,
        dir_no,
        &format!("f{:04}", file % PER_DIR),
        0o644,
      )
      .unwrap();
    vol.write(&mut store, no, 0, &bytes).unwrap();
    let appended = !journal.wants_checkpoint()
      && match vol.publication(&store, None).unwrap() {
        VolumeRecord::Delta { delta } => journal
          .append(
            &mut log,
            &ShardDelta::new(
              vec![KeyedRecord {
                key: [1; 16],
                record: VolumeRecord::Delta { delta },
              }],
              Vec::new(),
              None,
              None,
            ),
            &mut scratch,
          )
          .is_ok(),
        VolumeRecord::Full { .. } => false,
      };
    if appended {
      frames += 1;
    } else {
      let image = vol.to_image(&store, None).unwrap();
      journal
        .checkpoint(
          &mut slots,
          &ShardImage::new(vec![KeyedImage {
            key: [1; 16],
            image,
          }]),
        )
        .unwrap();
      frames = 0;
    }
    vol.mark_published(&store);
  }
  (slots, log, frames)
}

/// Recovers a journal (its checkpoint, then its deltas): µs.
fn replay(slots: &[u8], log: &[u8]) -> u128 {
  let started = Instant::now();
  let (image, _) = Journal::recover(slots, log).unwrap();
  let took = started.elapsed().as_micros();
  assert!(image.is_some(), "the journal recovers");
  took
}

/// Recovers the journal (the checkpoint, no deltas): µs.
fn decode(slots: &[u8]) -> u128 {
  let started = Instant::now();
  let (image, _) = Journal::recover(slots, &Vec::<u8>::new()).unwrap();
  let took = started.elapsed().as_micros();
  assert!(image.is_some(), "the checkpoint recovers");
  took
}

fn main() {
  if let Some(loops) = std::env::var("RECOVER_BENCH_REPLAY_LOOP")
    .ok()
    .and_then(|value| value.parse::<usize>().ok())
  {
    let (slots, log, _) = journal_of(FILES[1]);
    for _ in 0..loops {
      let _ = replay(&slots, &log);
    }
    return;
  }
  if std::env::var_os("RECOVER_BENCH_JOURNAL").is_some() {
    println!("files,frames_after_checkpoint,replay_us_best,rounds_replay_us");
    for files in FILES {
      let (slots, log, frames) = journal_of(files);
      let rounds: Vec<u128> = (0..ROUNDS).map(|_| replay(&slots, &log)).collect();
      let all: Vec<String> = rounds.iter().map(u128::to_string).collect();
      println!(
        "{files},{frames},{},{}",
        rounds.iter().min().unwrap(),
        all.join(" ")
      );
    }
    return;
  }
  if let Some(loops) = std::env::var("RECOVER_BENCH_DECODE_LOOP")
    .ok()
    .and_then(|value| value.parse::<usize>().ok())
  {
    let (image, _) = image_of(FILES[FILES.len() - 1]);
    let slots = checkpointed(&image);
    for _ in 0..loops {
      let _ = decode(&slots);
    }
    return;
  }
  if let Some(loops) = std::env::var("RECOVER_BENCH_LOOP")
    .ok()
    .and_then(|value| value.parse::<usize>().ok())
  {
    let (image, _) = image_of(FILES[FILES.len() - 1]);
    for _ in 0..loops {
      let _ = rebuild(&image);
    }
    return;
  }
  println!(
    "files,decode_us_best,decode_ns_per_file,claim_us_best,rebuild_us_best,rebuild_ns_per_file,dir_blocks_created,dir_blocks_rebuilt,rounds_rebuild_us"
  );
  for files in FILES {
    let (image, created_blocks) = image_of(files);
    let slots = checkpointed(&image);
    let decoded = (0..ROUNDS).map(|_| decode(&slots)).min().unwrap();
    let rounds: Vec<(u128, u128, usize)> = (0..ROUNDS).map(|_| rebuild(&image)).collect();
    let claim = rounds.iter().map(|r| r.0).min().unwrap();
    let rebuilt = rounds.iter().map(|r| r.1).min().unwrap();
    let all: Vec<String> = rounds.iter().map(|r| r.1.to_string()).collect();
    println!(
      "{files},{decoded},{},{claim},{rebuilt},{},{created_blocks},{},{}",
      decoded * 1000 / files as u128,
      rebuilt * 1000 / files as u128,
      rounds[0].2,
      all.join(" ")
    );
  }
}
