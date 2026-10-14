//! The writable+readable DAX access path, measured on the real content arena through the gate (§4.6 A-9,
//! AC-4.12), against the mediated FUSE/NFS round-trip that DAX replaces.
//!
//! What DAX buys (BENCHMARKS.md 2026-10-11): slates over FUSE is 12–325× slower per op than the kernel's own
//! overlayfs, and the gap is the **userspace round-trip** (the kernel↔daemon FUSE/NFS hop), not the store — slates'
//! content arena is already RAM. Writable+readable DAX removes that hop for data access: the guest maps a file's
//! content block into its DAX window and reaches the arena bytes through its **own page table**, so a read or write
//! is a memory access, not a round-trip. A one-time `FUSE_SETUPMAPPING` establishes the mapping; every access after
//! is free of the daemon.
//!
//! This benchmark measures, on the real arena block the gate pins and exposes:
//!   - the **DAX per-access cost**: a 4 KiB read (memcpy out of the arena block) and a 4 KiB write (memcpy in) —
//!     exactly the bytes a guest's page table moves, with no daemon call;
//!   - the **setup cost**: minting a mapping (`dax_map_read` / `dax_map_write`, the latter copying up first).
//!
//! It then prints the **amortised model** against the recorded mediated round-trip: for a file touched N times,
//!   mediated = N × round-trip;  DAX = 1 × setup-round-trip + N × memory-access.
//!
//! HONEST SCOPE: the per-access and setup numbers are measured here, in process, on the real arena + gate. The
//! mediated round-trip is the **recorded** FUSE/NFS op cost from BENCHMARKS.md (a measurement, not a guess). The
//! wired device window (the `MAP_FIXED` of the arena's shared-object fd into a VMM cache region) and a live
//! virtio-fs DAX guest A/B against overlayfs are the owed end-to-end validation — this quantifies the mechanism's
//! read+write win that the integration unlocks, grounded in measured components.
//!
//! Command: `cargo run --release -p slates-vfs --example dax_bench`. Best-of-N with all runs shown.

// Measurement harness (as `vfs_bench.rs`): fixed-size indexing into buffers this file owns, and an `expect` on setup
// that must succeed or the measurement is meaningless — neither is a production path.
#![allow(clippy::indexing_slicing, clippy::expect_used)]

use std::hint::black_box;
use std::time::Instant;

use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::StepClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// The host/content page and the store granule.
const PAGE: usize = 4096;
/// The content region: 16 MiB, enough for the mapped file and the copy-up.
const REGION_PAGES: usize = 4096;
/// The canonical filesystem operation size the mediated baselines are per-op for.
const OP_BYTES: usize = 4096;
/// Best-of-N runs (all shown), each the mean of this many inner iterations so a single memory access is timed
/// above the clock's resolution.
const RUNS: usize = 5;
const ITERS: u64 = 100_000;

/// Recorded mediated round-trip costs (BENCHMARKS.md, Apple M5 Max, 2026-10-11 / 2026-10-08), the per-op cost DAX
/// replaces with one setup plus memory access. FUSE read/write p50 over the loopback mount; the local NFS op p99.
const FUSE_READ_NS: u64 = 43_000;
const FUSE_WRITE_NS: u64 = 277_000;
const NFS_OP_NS: u64 = 5_700;

fn store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).expect("region"))
    .expect("add region");
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 64,
      max_dirs: 1 << 16,
      max_inodes: 1 << 16,
      max_chunks: 1 << 16,
      max_dir_blocks: 1 << 16,
      dir_cutover: 4,
    },
    arena,
    0,
  )
}

fn volume(store: &mut Store) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 7,
      names: NameEquivalence::Fold,
      quota: Quota::Bounded {
        limit: (PAGE * REGION_PAGES) as u64,
      },
      journal_bytes: 1 << 20,
      clock: Box::new(StepClock::new(0, 1_000)),
    },
  )
  .map(|mut v| {
    v.enable_writable_dax(); // L0: this bench measures writable-mapping cost, so opt the volume in.
    v
  })
  .expect("volume")
}

/// The best (minimum) per-iteration nanoseconds over `RUNS` runs of `iters` iterations each, and every run's value.
fn best_of<F: FnMut()>(iters: u64, mut body: F) -> (u64, Vec<u64>) {
  let mut runs = Vec::with_capacity(RUNS);
  for _ in 0..RUNS {
    let start = Instant::now();
    for _ in 0..iters {
      body();
    }
    let total = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    runs.push(total / iters.max(1));
  }
  let best = runs.iter().copied().min().unwrap_or(0);
  (best, runs)
}

fn main() {
  let mut store = store();
  let mut vol = volume(&mut store);
  let root = vol.root_inode(&store).expect("root");
  // One mappable file: a single content block (not inline), the shape `dax_map_*` accepts.
  let no = vol
    .create_file_no(&mut store, root, "f", 0o644)
    .expect("create");
  vol
    .write(&mut store, no, 0, &vec![0xABu8; OP_BYTES])
    .expect("write");

  // Setup: mint a read mapping and a (copy-up) write mapping, best-of-N — one-time per file, then free of the
  // daemon. Each iteration revokes so the next mints afresh rather than measuring a re-map no-op.
  let (map_read_ns, map_read_runs) = best_of(ITERS / 100, || {
    let m = vol.dax_map_read(&mut store, no).expect("map read");
    store.dax_revoke(&m);
  });
  let (map_write_ns, map_write_runs) = best_of(ITERS / 100, || {
    let m = vol.dax_map_write(&mut store, no).expect("map write");
    store.dax_revoke(&m);
  });

  // The mapped block the guest's page table points at.
  let mapping = vol.dax_map_read(&mut store, no).expect("map");
  let block = mapping.block;

  // DAX read: the bytes a guest's page table moves out of the arena block — a memcpy, no daemon call.
  let mut dst = vec![0u8; OP_BYTES];
  let (read_ns, read_runs) = best_of(ITERS, || {
    let src = store.content.arena().bytes(block).expect("block bytes");
    dst.copy_from_slice(&src[..OP_BYTES]);
    black_box(dst[0]);
  });

  // DAX write: the bytes a guest's page table moves into the arena block — a memcpy, no daemon call.
  let data = vec![0xCDu8; OP_BYTES];
  let (write_ns, write_runs) = best_of(ITERS, || {
    let dstb = store.content.arena_mut().bytes_mut(block).expect("block bytes");
    dstb[..OP_BYTES].copy_from_slice(&data);
    black_box(dstb[0]);
  });

  println!("# DAX access path on the real arena block ({OP_BYTES}-byte op, best of {RUNS}), ns/op");
  println!("dax_read\t{read_ns}\truns={read_runs:?}");
  println!("dax_write\t{write_ns}\truns={write_runs:?}");
  println!("dax_map_read_setup\t{map_read_ns}\truns={map_read_runs:?}");
  println!("dax_map_write_setup\t{map_write_ns}\truns={map_write_runs:?}");
  println!();
  println!("# Mediated round-trip DAX replaces (recorded, BENCHMARKS.md 2026-10-08/11), ns/op");
  println!("fuse_read\t{FUSE_READ_NS}\nfuse_write\t{FUSE_WRITE_NS}\nnfs_op\t{NFS_OP_NS}");
  println!();
  println!("# Amortised: a file touched N times — mediated N×round-trip vs DAX setup + N×access");
  amortise("read  vs FUSE ", FUSE_READ_NS, map_read_ns, read_ns);
  amortise("write vs FUSE ", FUSE_WRITE_NS, map_write_ns, write_ns);
  amortise("read  vs NFS  ", NFS_OP_NS, map_read_ns, read_ns);
  amortise("write vs NFS  ", NFS_OP_NS, map_write_ns, write_ns);
}

/// Prints, for a few access counts, the mediated total (N × round-trip) against the DAX total (one setup
/// round-trip plus N memory accesses), and the speedup. The DAX setup pays one round-trip (the single
/// SETUPMAPPING) plus the measured in-process mint; the mediated path pays a round-trip per access.
fn amortise(label: &str, round_trip_ns: u64, setup_mint_ns: u64, access_ns: u64) {
  let dax_setup = round_trip_ns.saturating_add(setup_mint_ns);
  for n in [1u64, 16, 256, 4096] {
    let mediated = round_trip_ns.saturating_mul(n);
    let dax = dax_setup.saturating_add(access_ns.saturating_mul(n));
    let speedup = mediated as f64 / dax.max(1) as f64;
    println!("{label} N={n:>5}: mediated {mediated:>12} ns, dax {dax:>12} ns, speedup {speedup:>7.1}x");
  }
}
