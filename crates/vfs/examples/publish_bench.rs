//! What one §4.8 barrier's publication costs as the shard's content grows (A-64). Every barrier (a FUSE
//! `flush`/`fsync`, an NFS `COMMIT` or `FILE_SYNC` write, a control verb) re-images the whole shard:
//! [`Volume::to_image`] captures every inode, the image is encoded, and [`ShardImage::write_after`] checksums it
//! and copies it into the content object's free slot, after the committed slot the last publish returned (as the
//! daemon publishes, so neither slot is re-read). Since A-64 a file's chunks are named by reference, so the
//! image grows with the number of chunks, not their bytes; before it, the image carried every byte (the
//! 2026-10-03 baseline in BENCHMARKS.md: 75 ms at 256 MiB). This measures the three steps, separately and
//! together, for one volume holding one file of each size, and the image's size.
//!
//! `cargo run --release -p slates-vfs --example publish_bench` prints one CSV row per size, each step the
//! best of [`ROUNDS`] with every round shown. **Failure** (the process exits non-zero): an image that does
//! not read back as the one written.

// A benchmark harness: an unwrap here is a failed run, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Instant;

use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::StepClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::recover::{KeyedImage, ShardImage};
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Shape: the rounds recorded per step; the best is reported (BENCHMARKS.md best-of-N, all N shown).
const ROUNDS: usize = 5;
/// Shape: the file sizes measured, in MiB: a small project file set to a large build output.
const SIZES_MIB: [usize; 5] = [1, 4, 16, 64, 256];
/// Format: one MiB.
const MIB: usize = 1 << 20;
/// Format: the page size (the store's granule).
const PAGE: usize = 4096;
/// Shape: the RAM region the store maps: room for the largest file (address space; only what is written is
/// touched).
const REGION_BYTES: usize = 1 << 29;
/// Shape: the store's slab caps, far above one file's chunks.
const MAX_OBJECTS: usize = 1 << 16;
/// Shape: the volume's quota, above the largest file.
const QUOTA: u64 = 1 << 30;
/// Shape: the journal's bytes.
const JOURNAL_BYTES: usize = 1 << 20;
/// Format: the cache line.
const CACHE_LINE: usize = 64;
/// Shape: the small-directory cut-over, the store's measured default range.
const DIR_CUTOVER: usize = 32;
/// Shape: the bytes written per call, a FUSE write's usual maximum.
const WRITE_BYTES: usize = 128 * 1024;

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

fn volume(store: &mut Store) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Fold,
      quota: Quota::Bounded { limit: QUOTA },
      journal_bytes: JOURNAL_BYTES,
      clock: Box::new(StepClock::new(0, 1)),
    },
  )
  .unwrap()
}

/// The best of `ROUNDS` timings of `round` in microseconds, every round's figure, and the last round's
/// result.
fn best_of<T>(mut round: impl FnMut() -> T) -> (f64, Vec<f64>, T) {
  let mut all = Vec::with_capacity(ROUNDS);
  let mut last = None;
  for _ in 0..ROUNDS {
    let start = Instant::now();
    let value = round();
    all.push(start.elapsed().as_secs_f64() * 1e6);
    last = Some(value);
  }
  let best = all.iter().copied().fold(f64::INFINITY, f64::min);
  (best, all, last.unwrap())
}

fn shown(all: &[f64]) -> String {
  all
    .iter()
    .map(|value| format!("{value:.1}"))
    .collect::<Vec<_>>()
    .join(" ")
}

fn main() {
  println!(
    "size_mib,capture_us_best,encode_us_best,publish_us_best,total_us_best,image_bytes,capture_rounds,encode_rounds,publish_rounds"
  );
  for size_mib in SIZES_MIB {
    let mut store = store();
    let mut vol = volume(&mut store);
    let root = vol.root_inode(&store).unwrap();
    let file = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
    let block: Vec<u8> = (0..WRITE_BYTES)
      .map(|index| index.wrapping_mul(2_654_435_761).to_le_bytes()[0])
      .collect();
    let total = size_mib * MIB;
    let mut offset = 0;
    while offset < total {
      vol
        .write(&mut store, file, u64::try_from(offset).unwrap(), &block)
        .unwrap();
      offset += WRITE_BYTES;
    }
    let (capture, capture_rounds, image) = best_of(|| vol.to_image(&store, None).unwrap());
    let shard = ShardImage::new(vec![KeyedImage {
      key: [1; 16],
      image,
    }]);
    let (encode, encode_rounds, encoded) = best_of(|| shard.to_content());
    // Two slots plus their headers, as the content object's slice holds them.
    let mut slots = vec![0_u8; 2 * encoded.len() + 2 * PAGE];
    // As the daemon publishes: after the committed slot its last publish returned, so the frame is one pass.
    let (_, first) = shard.write_after(slots.as_mut_slice(), None).unwrap();
    let mut known = Some(first);
    let (publish, publish_rounds, _) = best_of(|| {
      let (total, committed) = shard.write_after(slots.as_mut_slice(), known).unwrap();
      known = Some(committed);
      total
    });
    let read = ShardImage::read_from(slots.as_slice()).unwrap().unwrap();
    if read != shard {
      eprintln!("{size_mib} MiB: the published image does not read back as written");
      std::process::exit(1);
    }
    println!(
      "{size_mib},{capture:.1},{encode:.1},{publish:.1},{:.1},{},{},{},{}",
      capture + encode + publish,
      encoded.len(),
      shown(&capture_rounds),
      shown(&encode_rounds),
      shown(&publish_rounds)
    );
  }
}
