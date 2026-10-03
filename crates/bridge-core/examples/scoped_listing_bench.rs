//! What a scoped export pays per request (§4.6 scoped exports; AUD-29-76's follow-up measurement of the
//! per-object scope check), through the seam every transport serves by ([`ScopedBridge`] over
//! [`VolumeBridge`]): one listing page of [`PAGE_ENTRIES`] files, and one lookup of a file, in a directory at
//! each depth below the scope. Each object a request names costs a climb of its parents; the question is
//! whether a listing page pays that climb once or once per entry.
//!
//! `cargo run --release -p slates-bridge-core --example scoped_listing_bench` prints one CSV row per case, the
//! best of [`ROUNDS`] with every round shown. **Failures** (the process exits non-zero): a page that does not
//! list every file, or a lookup that does not find the file.

// A benchmark harness: an unwrap here is a failed run, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Instant;

use slates_bridge_core::scoped::ScopedBridge;
use slates_bridge_core::{Attachments, Bridge, ObjectId, OpContext, Rights, View, VolumeBridge};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Shape: the rounds recorded per case; the best is reported (BENCHMARKS.md best-of-N, all N shown).
const ROUNDS: usize = 7;
/// Shape: lookups per round, so one round is far above the clock's resolution at the shallowest depth.
const LOOKUPS_PER_ROUND: usize = 2_000;
/// Shape: the depths below the scope measured, from a flat export to a deep build tree and past it.
const DEPTHS: [usize; 5] = [1, 8, 64, 512, 4_096];
/// Shape: the files in the listed directory, one page (NFS READDIR and FUSE pages hold hundreds to thousands).
const PAGE_ENTRIES: usize = 1_024;
/// Format: the `.` and `..` entries a listing adds.
const DOT_ENTRIES: usize = 2;
/// Format: the page size (the store's granule).
const PAGE: usize = 4096;
/// Shape: the RAM region the store maps (address space; only what is written is touched).
const REGION_BYTES: usize = 1 << 26;
/// Shape: the store's caps, above the largest case's population.
const MAX_OBJECTS: usize = 1 << 16;
/// Format: the cache line.
const CACHE_LINE: usize = 128;
/// Shape: the small-directory cut-over, the store's measured default range.
const DIR_CUTOVER: usize = 32;
/// Shape: the volume's quota and journal, far above what any case writes.
const QUOTA: u64 = 1 << 30;
/// Shape: the journal's bytes.
const JOURNAL_BYTES: usize = 1 << 24;

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
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: QUOTA },
      journal_bytes: JOURNAL_BYTES,
      clock: Box::new(HostClock::default()),
    },
  )
  .unwrap()
}

fn context() -> OpContext {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      VolumeId { bytes: [0; 16] },
      View::Current,
      Principal::Uid { uid: 0 },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  attachments.context(id).unwrap()
}

fn oid(ino: u64) -> ObjectId {
  ObjectId::new(ino, 0)
}

/// The best of `ROUNDS` timings of `round`, in microseconds per `per` operations, and every round's figure.
fn best_of(per: usize, mut round: impl FnMut()) -> (f64, Vec<f64>) {
  let mut all = Vec::with_capacity(ROUNDS);
  for _ in 0..ROUNDS {
    let start = Instant::now();
    round();
    all.push(start.elapsed().as_nanos() as f64 / 1_000.0 / per as f64);
  }
  (all.iter().copied().fold(f64::INFINITY, f64::min), all)
}

fn shown(all: &[f64]) -> String {
  all
    .iter()
    .map(|value| format!("{value:.2}"))
    .collect::<Vec<_>>()
    .join(" ")
}

fn main() {
  println!("case,depth,us_best,rounds_us");
  for depth in DEPTHS {
    let mut store = store();
    let mut vol = volume(&mut store);
    let mut inner = VolumeBridge::new(VolumeId { bytes: [0; 16] }, &mut vol, &mut store);
    let cx = context();
    let root = inner.root(&cx).unwrap();
    let scope = inner.mkdir(oid(root), &cx, "scope", 0o755).unwrap().ino;
    let mut dir = scope;
    for level in 0..depth {
      dir = inner
        .mkdir(oid(dir), &cx, &format!("d{level}"), 0o755)
        .unwrap()
        .ino;
    }
    for index in 0..PAGE_ENTRIES {
      let (file, fh) = inner
        .create(oid(dir), &cx, &format!("f{index}"), 0o644, 0)
        .unwrap();
      inner.release(oid(file.ino), &cx, fh).unwrap();
    }
    let mut scoped = ScopedBridge::new(&mut inner, scope);
    let fh = scoped.opendir(oid(dir), &cx).unwrap();
    let (page, page_rounds) = best_of(1, || {
      let entries = scoped
        .readdir(oid(dir), &cx, fh, 0, PAGE_ENTRIES + DOT_ENTRIES)
        .unwrap();
      assert_eq!(entries.len(), PAGE_ENTRIES + DOT_ENTRIES, "depth {depth}");
    });
    println!("listing_page,{depth},{page:.2},{}", shown(&page_rounds));
    let (lookup, lookup_rounds) = best_of(LOOKUPS_PER_ROUND, || {
      for _ in 0..LOOKUPS_PER_ROUND {
        std::hint::black_box(scoped.lookup(oid(dir), &cx, "f7").unwrap());
      }
    });
    println!("lookup,{depth},{lookup:.2},{}", shown(&lookup_rounds));
  }
}
