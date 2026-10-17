//! §4.6 DAX sealed-chunk backing A/B, measured as WORKLOADS, not microbenchmarks. The same bench runs on both arms
//! (the compiled `dax_map_read_at` decides the behaviour), so the numbers are the measured head-to-head:
//!
//!   - arm A (branch writable-dax-view): a sealed chunk is backed by a decrypted read-only THROWAWAY the mapping
//!     owns; the canonical ciphertext stays, a chunk-sized plaintext block is allocated and decrypted into per map
//!     and freed on revoke.
//!   - arm B (branch writable-dax-reopen): a sealed chunk is REOPENED IN PLACE — decrypted in its own block, mapped
//!     directly; the idle sweep re-encrypts it once the window is revoked.
//!
//! The workloads, each on sealed content with the server's AES-256-GCM cipher, reading every byte the way a guest
//! does (a memcpy out of the mapped arena block through its own page table — no daemon call):
//!
//!   W1 STREAM: read a whole file through a bounded DAX window (map the window's chunks, read all their bytes,
//!      revoke, let the idle sweep run, advance). This is what a guest reading a large file does: the DAX cache is a
//!      fixed region, so only a window is mapped at a time. Reported: wall-clock, MB/s, and the peak extra arena
//!      bytes a live window costs.
//!   W2 HOT RE-READ: the same working set read N times, the window dropped between reads but the sweep NOT yet run
//!      (an agent grepping the same files repeatedly). This is where reopen amortises — the chunk is still plaintext,
//!      so a re-map is a pin; the view re-decrypts every time.
//!   W3 COLD RE-READ: the same, but the idle sweep runs between reads (the window lapsed long enough to re-seal).
//!      This is reopen's worst case: decrypt on every map and re-encrypt on every sweep.
//!
//! Command: `cargo run --release -p slates-vfs --example dax_backing_ab`.

// Measurement harness (as `dax_bench.rs`): fixed-size indexing and an `expect` on setup that must succeed or the
// measurement is meaningless — neither is a production path.
#![allow(clippy::indexing_slicing, clippy::expect_used)]

use std::time::Instant;

use hyper_seal::Secret32;
use hyper_seal::stream::VersionKey;
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::StepClock;
use slates_vfs::content::{ChunkCipher, KeyIdentity, Tag};
use slates_vfs::error::VfsError;
use slates_vfs::ids::InodeNo;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// The host/content page and the store granule.
const PAGE: usize = 4096;
/// The file a workload reads: 64 MiB of sealed content.
const FILE_BYTES: usize = 64 << 20;
/// The live mapped set W1 streams through, swept rather than assumed: the bench reports the cost at each size, so the
/// view arm's extra RAM is a measured CURVE over the simultaneously-mapped bytes, not a single point. A guest's own
/// per-request granularity is the guest kernel's (measured separately against a real guest); what slates must bound
/// is how many of those are live at once, and that bound is derived from this curve.
const WINDOWS: [usize; 6] = [256 << 10, 1 << 20, 2 << 20, 8 << 20, 32 << 20, 64 << 20];
/// The working set W2/W3 re-read, and how many times.
const HOT_SET_BYTES: usize = 2 << 20;
const REREADS: usize = 20;
/// Best-of-N rounds (all shown).
const RUNS: usize = 3;
/// Keys held at once in hyper-seal's locked region.
const KEYS_HELD: usize = 4;

/// The server's cipher over one key: hyper-seal's `VersionKey` (AES-256-GCM), as `sealed_read_bench`.
struct Cipher(VersionKey);

impl ChunkCipher for Cipher {
  fn seal(
    &self,
    _: u32,
    version: u64,
    index: u32,
    last: bool,
    segment: &mut [u8],
  ) -> Result<Tag, VfsError> {
    self
      .0
      .seal(version, index, last, segment)
      .map_err(|_| VfsError::Integrity)
  }
  fn open(
    &self,
    _: u32,
    version: u64,
    index: u32,
    last: bool,
    segment: &mut [u8],
    tag: &Tag,
  ) -> Result<(), VfsError> {
    self
      .0
      .open(version, index, last, segment, tag)
      .map_err(|_| VfsError::Integrity)
  }
  fn identity(&self, _: u32) -> Option<KeyIdentity> {
    Some(KeyIdentity::default())
  }
  fn reference(&mut self, _: &KeyIdentity) -> Result<u32, VfsError> {
    Ok(0)
  }
  fn key_for_volume(&mut self, _: [u8; 16]) -> Result<u32, VfsError> {
    Ok(0)
  }
}

fn store(pages: usize) -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * pages, PAGE, false).expect("region"))
    .expect("add region");
  let mut store = Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 64,
      max_dirs: 1 << 16,
      max_inodes: 1 << 16,
      max_chunks: 1 << 18,
      max_dir_blocks: 1 << 16,
      dir_cutover: 4,
    },
    arena,
    0,
  );
  let secret = Secret32::from_bytes(&[7u8; 32]).expect("secret");
  store.content.set_cipher(Box::new(Cipher(
    VersionKey::new(&secret, [1u8; 16]).expect("key"),
  )));
  store
}

fn volume(store: &mut Store, pages: usize) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 7,
      names: NameEquivalence::Fold,
      quota: Quota::Bounded {
        limit: (PAGE * pages) as u64,
      },
      journal_bytes: 1 << 20,
      clock: Box::new(StepClock::new(0, 1_000)),
    },
  )
  .map(|mut v| {
    v.set_seal_key(Some(0));
    v
  })
  .expect("volume")
}

/// A file of `bytes` with every chunk sealed (ciphertext at rest).
fn sealed_file(store: &mut Store, vol: &mut Volume, bytes: usize) -> InodeNo {
  let root = vol.root_inode(store).expect("root");
  let no = vol.create_file_no(store, root, "f", 0o644).expect("create");
  let data = vec![0xABu8; bytes];
  vol.write(store, no, 0, &data).expect("write");
  for _ in 0..4 {
    vol.age_writes();
    vol.seal_pending(store, u64::MAX);
  }
  no
}

/// Maps the chunks of `[from, from + span)`, reads EVERY byte of each the way a guest does (a memcpy out of the
/// mapped arena block, no daemon call), then revokes. Returns the bytes read and the peak extra arena bytes the live
/// window cost over `baseline`.
#[allow(
  clippy::too_many_arguments,
  reason = "a measurement harness: the window geometry, the store, the volume and the sink are all inputs to one pass"
)]
fn read_window(
  store: &mut Store,
  vol: &mut Volume,
  no: InodeNo,
  chunk: usize,
  from: usize,
  span: usize,
  baseline: usize,
  sink: &mut Vec<u8>,
) -> (usize, usize) {
  let mut maps = Vec::with_capacity(span / chunk + 1);
  let mut read = 0usize;
  let mut off = from;
  while off < from + span {
    let m = vol.dax_map_read_at(store, no, off).expect("map chunk");
    maps.push(m);
    off += chunk;
  }
  let peak = store.content.allocated_bytes().saturating_sub(baseline);
  // The guest's own page-table access: copy every mapped byte out of the arena block.
  for m in &maps {
    let bytes = store.content.arena().bytes(m.block).expect("block bytes");
    let valid = &bytes[..m.file_len.min(bytes.len())];
    sink.clear();
    sink.extend_from_slice(valid);
    read += valid.len();
    std::hint::black_box(&sink[0]);
  }
  for m in &maps {
    store.dax_revoke(m);
  }
  (read, peak)
}

struct Workload {
  seconds: f64,
  peak_extra: usize,
}

/// W1: read the whole file through a bounded live mapped set of `window` bytes, the idle sweep running after each
/// window (the steady state).
fn stream(
  store: &mut Store,
  vol: &mut Volume,
  no: InodeNo,
  chunk: usize,
  window: usize,
) -> Workload {
  let baseline = store.content.allocated_bytes();
  let mut sink = Vec::with_capacity(chunk);
  let mut peak_extra = 0usize;
  let started = Instant::now();
  let mut at = 0usize;
  while at < FILE_BYTES {
    let span = window.min(FILE_BYTES - at);
    let (_, peak) = read_window(store, vol, no, chunk, at, span, baseline, &mut sink);
    peak_extra = peak_extra.max(peak);
    vol.seal_idle(store, u64::MAX);
    at += span;
  }
  Workload {
    seconds: started.elapsed().as_secs_f64(),
    peak_extra,
  }
}

/// W2/W3: re-read the same working set `REREADS` times; `sweep_between` runs the idle sweep between reads (cold) or
/// not (hot — the window lapsed but the chunk has not been re-sealed yet).
fn reread(
  store: &mut Store,
  vol: &mut Volume,
  no: InodeNo,
  chunk: usize,
  sweep_between: bool,
) -> Workload {
  let baseline = store.content.allocated_bytes();
  let mut sink = Vec::with_capacity(chunk);
  let mut peak_extra = 0usize;
  let started = Instant::now();
  for _ in 0..REREADS {
    let (_, peak) = read_window(store, vol, no, chunk, 0, HOT_SET_BYTES, baseline, &mut sink);
    peak_extra = peak_extra.max(peak);
    if sweep_between {
      vol.seal_idle(store, u64::MAX);
    }
  }
  // Whatever is still reopened is re-sealed at the end either way: the work is owed, not avoided.
  vol.seal_idle(store, u64::MAX);
  Workload {
    seconds: started.elapsed().as_secs_f64(),
    peak_extra,
  }
}

fn mib(bytes: usize) -> f64 {
  bytes as f64 / (1024.0 * 1024.0)
}

fn main() {
  hyper_seal::lock_keys(KEYS_HELD).expect("lock keys");
  let chunk = store(64).content.chunk_bytes();
  // Region: the file, plus room for the view arm's throwaways at the LARGEST swept window, and headroom.
  let pages = (FILE_BYTES / PAGE) * 3 + 8192;

  // Every data point gets a FRESH store, volume and sealed file: sharing one across the sweep let later windows
  // inherit the arena churn (reopened/re-sealed chunks, deadlist growth) of earlier ones, which is not the thing
  // being measured.
  let fresh = |chunk_out: &mut usize| {
    let mut store = store(pages);
    let mut vol = volume(&mut store, pages);
    let no = sealed_file(&mut store, &mut vol, FILE_BYTES);
    *chunk_out = store.content.chunk_bytes();
    (store, vol, no)
  };

  let mut sweep: Vec<(usize, Vec<Workload>)> = WINDOWS.iter().map(|w| (*w, Vec::new())).collect();
  let mut hot_runs: Vec<Workload> = Vec::new();
  let mut cold_runs: Vec<Workload> = Vec::new();
  let mut c = chunk;

  for _ in 0..RUNS {
    for (window, runs) in &mut sweep {
      let (mut store, mut vol, no) = fresh(&mut c);
      runs.push(stream(&mut store, &mut vol, no, chunk, *window));
    }
    {
      let (mut store, mut vol, no) = fresh(&mut c);
      hot_runs.push(reread(&mut store, &mut vol, no, chunk, false));
    }
    {
      let (mut store, mut vol, no) = fresh(&mut c);
      cold_runs.push(reread(&mut store, &mut vol, no, chunk, true));
    }
  }

  let best = |runs: &[Workload]| -> (f64, usize) {
    let seconds = runs.iter().map(|w| w.seconds).fold(f64::MAX, f64::min);
    let peak = runs.iter().map(|w| w.peak_extra).max().unwrap_or(0);
    (seconds, peak)
  };
  let all = |runs: &[Workload]| -> Vec<String> {
    runs.iter().map(|w| format!("{:.3}s", w.seconds)).collect()
  };

  let (s2, p2) = best(&hot_runs);
  let (s3, p3) = best(&cold_runs);

  println!(
    "# DAX sealed-chunk backing, WORKLOADS (chunk {chunk} B, AES-256-GCM, best of {RUNS}; runs shown)"
  );
  println!(
    "# W1 sweep: streaming {} MiB, varying the SIMULTANEOUSLY-MAPPED bytes — the view arm's extra RAM is this curve",
    mib(FILE_BYTES)
  );
  for (window, runs) in &sweep {
    let (s, p) = best(runs);
    println!(
      "W1 live-mapped {:>8.2} MiB:\t{s:.3} s\t{:>6.0} MiB/s\tpeak extra RAM {:>7.2} MiB\truns={:?}",
      mib(*window),
      mib(FILE_BYTES) / s,
      mib(p),
      all(runs)
    );
  }
  println!(
    "W2 hot re-read {} MiB x{REREADS} (no sweep between):\t{s2:.3} s\t{:.0} MiB/s\tpeak extra RAM {:.1} MiB\truns={:?}",
    mib(HOT_SET_BYTES),
    mib(HOT_SET_BYTES * REREADS) / s2,
    mib(p2),
    all(&hot_runs)
  );
  println!(
    "W3 cold re-read {} MiB x{REREADS} (sweep between):\t{s3:.3} s\t{:.0} MiB/s\tpeak extra RAM {:.1} MiB\truns={:?}",
    mib(HOT_SET_BYTES),
    mib(HOT_SET_BYTES * REREADS) / s3,
    mib(p3),
    all(&cold_runs)
  );
}
