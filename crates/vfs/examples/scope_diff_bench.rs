//! The two measurements AUD-29-76's closure listed as follow-ups (§4.6 scoped exports; §4.4 `advance` of an
//! immutable reader):
//!
//! - **The per-object scope check** ([`Volume::within`]). A scoped mount checks every object a request names
//!   by climbing its parents to the scope, so its cost grows with the object's depth below the scope; a scoped
//!   listing checks every entry of a page. Measured per call against depth, for a directory, a file, and an
//!   object outside the scope (which climbs to the root, the worst case), and per listing page.
//! - **The snapshot diff on a large span** ([`Volume::paths_changed_between`]), which a re-pinned snapshot
//!   mount reports. It enters only the inode-table nodes the span copied, so its cost should follow the number
//!   of changes, not the volume's size. Measured against both, plus a moved directory (whose subtree it names
//!   in both snapshots).
//!
//! `cargo run --release -p slates-vfs --example scope_diff_bench` prints one CSV row per case, each the best of
//! [`ROUNDS`] with every round shown. **Failures** (the process exits non-zero): a scope answer that is not
//! the one the tree implies, or a diff that does not name the changed paths.

// A benchmark harness: an unwrap here is a failed run, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Instant;

use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::StepClock;
use slates_vfs::ids::InodeNo;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Shape: the rounds recorded per case; the best is reported (BENCHMARKS.md best-of-N, all N shown).
const ROUNDS: usize = 7;
/// Shape: scope checks per round, so one round is far above the clock's resolution at the shallowest depth.
const CHECKS_PER_ROUND: usize = 20_000;
/// Shape: the depths below the scope measured, from a flat export to a deep build tree and past it.
const DEPTHS: [usize; 5] = [1, 8, 64, 512, 4_096];
/// Shape: the entries in one listing page (NFS READDIR and FUSE pages are a few hundred to a few thousand).
const PAGE_ENTRIES: usize = 1_024;
/// Shape: the volume sizes the diff is measured over, in files.
const VOLUME_FILES: [usize; 2] = [10_000, 100_000];
/// Shape: the changes between the two snapshots.
const CHANGES: [usize; 3] = [1, 100, 10_000];
/// Shape: files per directory in the diff's volume, so its tree has both depth and breadth.
const FILES_PER_DIR: usize = 256;
/// Format: the page size (the store's granule).
const PAGE: usize = 4096;
/// Shape: the RAM region the store maps (address space; only what is written is touched).
const REGION_BYTES: usize = 1 << 28;
/// Shape: the store's inode-version and directory caps, above the largest case's population.
const MAX_INODES: usize = 1 << 19;
/// Shape: the store's directory-node and block caps.
const MAX_DIRS: usize = 1 << 16;
/// Shape: the store's chunk cap, above the largest case's written files.
const MAX_CHUNKS: usize = 1 << 16;
/// Shape: the volume's quota, far above what any case writes.
const QUOTA: u64 = 1 << 30;
/// Shape: the journal's bytes, so the largest case's operations stay recorded.
const JOURNAL_BYTES: usize = 1 << 26;
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
      max_dirs: MAX_DIRS,
      max_inodes: MAX_INODES,
      max_chunks: MAX_CHUNKS,
      max_dir_blocks: MAX_DIRS,
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

/// The best of `ROUNDS` timings of `round`, in nanoseconds per `per` operations, and every round's figure.
fn best_of(per: usize, mut round: impl FnMut()) -> (f64, Vec<f64>) {
  let mut all = Vec::with_capacity(ROUNDS);
  for _ in 0..ROUNDS {
    let start = Instant::now();
    round();
    all.push(start.elapsed().as_nanos() as f64 / per as f64);
  }
  let best = all.iter().copied().fold(f64::INFINITY, f64::min);
  (best, all)
}

fn shown(all: &[f64]) -> String {
  all
    .iter()
    .map(|value| format!("{value:.1}"))
    .collect::<Vec<_>>()
    .join(" ")
}

/// The scope check against depth: a chain `/scope/d1/…/dD` with a file at the bottom, and a sibling chain
/// outside the scope of the same depth.
fn scope_rows() {
  println!("case,depth,ns_per_check_best,rounds_ns");
  for depth in DEPTHS {
    let mut store = store();
    let mut vol = volume(&mut store);
    let root = vol.root_inode(&store).unwrap();
    let scope = vol.mkdir_no(&mut store, root, "scope", 0o755).unwrap();
    let outside_top = vol.mkdir_no(&mut store, root, "outside", 0o755).unwrap();
    let mut inside = scope;
    let mut outside = outside_top;
    for level in 0..depth {
      let name = format!("d{level}");
      inside = vol.mkdir_no(&mut store, inside, &name, 0o755).unwrap();
      outside = vol.mkdir_no(&mut store, outside, &name, 0o755).unwrap();
    }
    let file = vol.create_file_no(&mut store, inside, "f", 0o644).unwrap();
    let cases: [(&str, InodeNo, bool); 3] = [
      ("directory_inside", inside, true),
      ("file_inside", file, true),
      ("directory_outside", outside, false),
    ];
    for (case, object, expected) in cases {
      assert_eq!(
        vol.within(&store, object, scope).unwrap(),
        expected,
        "{case} at depth {depth}"
      );
      let (best, all) = best_of(CHECKS_PER_ROUND, || {
        for _ in 0..CHECKS_PER_ROUND {
          std::hint::black_box(
            vol
              .within(&store, std::hint::black_box(object), scope)
              .unwrap(),
          );
        }
      });
      println!("{case},{depth},{best:.1},{}", shown(&all));
    }
  }
}

/// A scoped listing page: the check of every entry of a directory of `PAGE_ENTRIES` files at each depth.
fn listing_rows() {
  println!("case,depth,entries,us_per_page_best,rounds_us");
  for depth in DEPTHS {
    let mut store = store();
    let mut vol = volume(&mut store);
    let root = vol.root_inode(&store).unwrap();
    let scope = vol.mkdir_no(&mut store, root, "scope", 0o755).unwrap();
    let mut dir = scope;
    for level in 0..depth {
      dir = vol
        .mkdir_no(&mut store, dir, &format!("d{level}"), 0o755)
        .unwrap();
    }
    let entries: Vec<InodeNo> = (0..PAGE_ENTRIES)
      .map(|index| {
        vol
          .create_file_no(&mut store, dir, &format!("f{index}"), 0o644)
          .unwrap()
      })
      .collect();
    let (best, all) = best_of(1, || {
      for entry in &entries {
        assert!(vol.within(&store, *entry, scope).unwrap());
      }
    });
    let to_us = |value: f64| value / 1_000.0;
    let rounds: Vec<f64> = all.iter().copied().map(to_us).collect();
    println!(
      "listing_page,{depth},{PAGE_ENTRIES},{:.1},{}",
      to_us(best),
      shown(&rounds)
    );
  }
}

/// A volume of `files` empty files, `FILES_PER_DIR` to a directory; returns the files and the directories.
fn populated(vol: &mut Volume, store: &mut Store, files: usize) -> (Vec<InodeNo>, Vec<InodeNo>) {
  let root = vol.root_inode(store).unwrap();
  let mut file_nos = Vec::with_capacity(files);
  let mut dir_nos = Vec::new();
  for index in 0..files {
    if index % FILES_PER_DIR == 0 {
      let dir = vol
        .mkdir_no(store, root, &format!("dir{}", index / FILES_PER_DIR), 0o755)
        .unwrap();
      dir_nos.push(dir);
    }
    let dir = *dir_nos.last().unwrap();
    file_nos.push(
      vol
        .create_file_no(store, dir, &format!("f{index}"), 0o644)
        .unwrap(),
    );
  }
  (file_nos, dir_nos)
}

/// The diff against the volume's size and the span's changes: a write to `changes` files spread across the
/// volume; then a moved directory of `FILES_PER_DIR` files.
fn diff_rows() {
  println!("case,files,changes,paths_named,us_best,rounds_us");
  for files in VOLUME_FILES {
    for changes in CHANGES {
      let mut store = store();
      let mut vol = volume(&mut store);
      let (file_nos, _) = populated(&mut vol, &mut store, files);
      let before = vol.snapshot(&mut store).unwrap();
      let stride = (files / changes).max(1);
      for file in file_nos.iter().step_by(stride).take(changes) {
        vol.write(&mut store, *file, 0, b"x").unwrap();
      }
      let after = vol.snapshot(&mut store).unwrap();
      let named = vol
        .paths_changed_between(&store, before, after)
        .unwrap()
        .len();
      assert!(named >= changes, "{named} paths named for {changes} writes");
      let (best, all) = best_of(1, || {
        std::hint::black_box(vol.paths_changed_between(&store, before, after).unwrap());
      });
      let to_us = |value: f64| value / 1_000.0;
      let rounds: Vec<f64> = all.iter().copied().map(to_us).collect();
      println!(
        "writes,{files},{changes},{named},{:.1},{}",
        to_us(best),
        shown(&rounds)
      );
    }
    let mut store = store();
    let mut vol = volume(&mut store);
    let (_, dir_nos) = populated(&mut vol, &mut store, files);
    let root = vol.root_inode(&store).unwrap();
    let before = vol.snapshot(&mut store).unwrap();
    let moved_into = *dir_nos.last().unwrap();
    vol
      .rename_no(&mut store, root, "dir0", moved_into, "moved")
      .unwrap();
    let after = vol.snapshot(&mut store).unwrap();
    let named = vol
      .paths_changed_between(&store, before, after)
      .unwrap()
      .len();
    assert!(
      named > 2 * FILES_PER_DIR,
      "a moved directory names its subtree in both snapshots: {named}"
    );
    let (best, all) = best_of(1, || {
      std::hint::black_box(vol.paths_changed_between(&store, before, after).unwrap());
    });
    let to_us = |value: f64| value / 1_000.0;
    let rounds: Vec<f64> = all.iter().copied().map(to_us).collect();
    println!(
      "moved_directory,{files},1,{named},{:.1},{}",
      to_us(best),
      shown(&rounds)
    );
  }
}

fn main() {
  scope_rows();
  listing_rows();
  diff_rows();
}
