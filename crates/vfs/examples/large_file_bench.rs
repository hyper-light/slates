//! What a read and a write cost inside one large file, as the file grows (§4.5 chunked bodies): random 4 KiB reads,
//! random 4 KiB overwrites, and sequential 1 MiB writes, in files of 16, 128 and 512 MiB (1,024 to 8,192 extents of
//! the store's 64 KiB chunk). A cost that grows with the file is a walk over its extents: on 2026-10-06 a read resolved
//! every extent's chunk (22.2 µs in the 512 MiB file) and a write recounted every extent twice (74 µs); both now
//! touch only the extents they overlap. Best of [`ROUNDS`], every round shown.
//!
//! `cargo run --release -p slates-vfs --example large_file_bench` prints one CSV row per file size.

// A benchmark harness: an unwrap here is a failed run, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::time::Instant;

use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::StepClock;
use slates_vfs::ids::InodeNo;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Shape: the rounds recorded per size; the best is reported (BENCHMARKS.md best-of-N, all N shown).
const ROUNDS: usize = 5;
/// Shape: the file sizes measured, in MiB.
const FILE_MIB: [usize; 3] = [16, 128, 512];
/// Shape: one sequential write, and the unit the file sizes are in.
const MIB: usize = 1 << 20;
/// Format: the page size (the store's granule), and the random operations' size.
const PAGE: usize = 4096;
/// Shape: random reads per round.
const READS: u64 = 200_000;
/// Shape: random overwrites per round.
const OVERWRITES: u64 = 20_000;
/// Shape: the RAM region the store maps: room for the largest file and its rewritten windows.
const REGION_BYTES: usize = 1 << 31;
/// Shape: the store's slab caps, above the largest file's chunks.
const MAX_OBJECTS: usize = 1 << 19;
/// Format: the cache line.
const CACHE_LINE: usize = 64;
/// Shape: the small-directory cut-over, the store's measured default range.
const DIR_CUTOVER: usize = 32;
/// Shape: the journal's bytes.
const JOURNAL_BYTES: usize = 1 << 20;
/// Shape: the xorshift seed the offsets are drawn from (any non-zero word).
const SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// A store, a volume and one file of `mib` MiB written sequentially: and the sequential write's µs per MiB.
fn file_of(mib: usize) -> (Store, Volume, InodeNo, u128) {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(REGION_BYTES, PAGE, false).unwrap())
    .unwrap();
  let mut store = Store::new(
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
  );
  let mut vol = Volume::create(
    &mut store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded {
        limit: u64::try_from(REGION_BYTES).unwrap(),
      },
      journal_bytes: JOURNAL_BYTES,
      clock: Box::new(StepClock::new(0, 1)),
    },
  )
  .unwrap();
  let root = vol.root_inode(&store).unwrap();
  let file = vol.create_file_no(&mut store, root, "big", 0o644).unwrap();
  let block = vec![7u8; MIB];
  let started = Instant::now();
  for at in 0..mib {
    vol
      .write(&mut store, file, u64::try_from(at * MIB).unwrap(), &block)
      .unwrap();
  }
  let per_mib = started.elapsed().as_micros() / mib as u128;
  (store, vol, file, per_mib)
}

/// The next offset of a xorshift sequence, page-aligned inside `size`.
fn next_page(state: &mut u64, size: u64) -> u64 {
  *state ^= *state << 13;
  *state ^= *state >> 7;
  *state ^= *state << 17;
  (*state % (size / PAGE as u64)) * PAGE as u64
}

fn main() {
  println!(
    "file_mib,extents,sequential_us_per_mib,read_ns_best,overwrite_ns_best,rounds_read_ns,rounds_overwrite_ns"
  );
  for mib in FILE_MIB {
    let (mut store, mut vol, file, sequential) = file_of(mib);
    let size = u64::try_from(mib * MIB).unwrap();
    let mut buf = vec![0u8; PAGE];
    let fill = vec![9u8; PAGE];
    let (mut reads, mut writes) = (Vec::new(), Vec::new());
    for _ in 0..ROUNDS {
      let mut state = SEED;
      let started = Instant::now();
      for _ in 0..READS {
        let off = next_page(&mut state, size);
        vol.read(&store, file, off, &mut buf).unwrap();
      }
      reads.push(started.elapsed().as_nanos() / u128::from(READS));
      let started = Instant::now();
      for _ in 0..OVERWRITES {
        let off = next_page(&mut state, size);
        vol.write(&mut store, file, off, &fill).unwrap();
      }
      writes.push(started.elapsed().as_nanos() / u128::from(OVERWRITES));
    }
    let show = |all: &[u128]| {
      all
        .iter()
        .map(u128::to_string)
        .collect::<Vec<_>>()
        .join(" ")
    };
    println!(
      "{mib},{},{sequential},{},{},{},{}",
      mib * MIB / store.content.chunk_bytes(),
      reads.iter().min().unwrap(),
      writes.iter().min().unwrap(),
      show(&reads),
      show(&writes)
    );
  }
}
