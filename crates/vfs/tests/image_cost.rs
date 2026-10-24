//! A-160's bounded steps, held by count (§4.3): no step of an image's build or merge — the snapshot's walk, the
//! image's layout and writing, the image's walk back out of its file, the merge, and the release of what they built —
//! allocates more, frees more, or allocates larger in a large tree than in a small one. A daemon runs each of these a
//! unit a call in slices bounded by its clock, so a unit whose work follows a directory or a tree holds its shard for
//! as long as that work takes. The allocator calls are counted per thread, so the verdict is exact and independent of
//! the host's load (the pattern of `delta_cost.rs`).
//!
//! Measured before this test (2026-10-24, `image_bench`'s least time of each unit over five rounds, Apple M5 Max): a
//! directory of 100,000 entries was opened by the snapshot's walk in one unit of 12,035 µs (every name copied and
//! sorted), and closed by the image's walk in one of 540 µs; a node vector's doubling took 271 µs at 16,384 nodes;
//! releasing a finished merge and its builder took 4.5–15 ms at 100,000 nodes, all against a slice of 23.7 µs.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use slates_image::{Builder, Identity, Image, Progress, Timestamp, Walk, Walked};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::StepClock;
use slates_vfs::image::{BuildIo, ImageContent, Merge, SnapshotFile, SnapshotWalk};
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

struct Counting;
thread_local! {
  // Const initialization and no destructor: observing an allocation never allocates recursively.
  static CALLS: Cell<usize> = const { Cell::new(0) };
  static FREES: Cell<usize> = const { Cell::new(0) };
  static LARGEST: Cell<usize> = const { Cell::new(0) };
}

/// Notes one allocation or reallocation of `size` bytes.
fn note_allocation(size: usize) {
  CALLS.set(CALLS.get() + 1);
  LARGEST.set(LARGEST.get().max(size));
}

// SAFETY: forwards the allocator contract unchanged; the counters observe calls only.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    note_allocation(layout.size());
    // SAFETY: forwarded unchanged.
    unsafe { System.alloc(layout) }
  }
  unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
    FREES.set(FREES.get() + 1);
    // SAFETY: forwarded unchanged.
    unsafe { System.dealloc(pointer, layout) }
  }
  unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
    note_allocation(size);
    // SAFETY: forwarded unchanged.
    unsafe { System.realloc(pointer, layout, size) }
  }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// One step's allocator traffic on this thread: its allocation calls (a reallocation among them), its frees, and its
/// largest single allocation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Traffic {
  calls: usize,
  frees: usize,
  largest: usize,
}

impl Traffic {
  /// Each count's larger of the two.
  fn worst(self, other: Traffic) -> Traffic {
    Traffic {
      calls: self.calls.max(other.calls),
      frees: self.frees.max(other.frees),
      largest: self.largest.max(other.largest),
    }
  }
}

/// The allocator traffic `work` makes on this thread, and its result.
fn counted<T>(work: impl FnOnce() -> T) -> (Traffic, T) {
  let (calls, frees) = (CALLS.get(), FREES.get());
  LARGEST.set(0);
  let out = work();
  let traffic = Traffic {
    calls: CALLS.get() - calls,
    frees: FREES.get() - frees,
    largest: LARGEST.get(),
  };
  (traffic, out)
}

/// Shape: the small tree's files: past the first segment of every segmented array, so the small tree's steps grow
/// each array as the large tree's do.
const SMALL: usize = 2_048;
/// Shape: the large tree's files: eight times the small tree's, so a step whose cost follows the tree is thousands of
/// allocations apart, not a few.
const LARGE: usize = 8 * SMALL;
/// Shape: the files per directory of the many-directory trees.
const PER_DIRECTORY: usize = 64;
/// Shape: the bytes of each file in the tree whose files hold data: under a block, so each file is one data block of
/// the image and the image file grows a block a file.
const FILE_BYTES: usize = 64;
/// Shape: what one step may cost more in the large tree than in the small one: a map's split or merge cascades a node
/// a level, and eight times the entries is at most two more levels of an ordered map whose nodes hold six keys or more
/// (log₆ 8 < 2); a segmented array's list of segments grows by doubling, once per array a step extends.
const SLACK: usize = 16;
/// Format: the page size (the store's granule).
const PAGE: usize = 4096;
/// Shape: the RAM the store maps: room for the large tree's source, its image and its clone.
const REGION_BYTES: usize = 1 << 28;
/// Shape: the store's inode cap, above the large tree's nodes in its source, its image volume and its clone.
const MAX_INODES: usize = 1 << 17;
/// Shape: the store's directory-node, directory-block and chunk caps.
const MAX_OBJECTS: usize = 1 << 16;
/// Shape: each volume's quota, far above what the large tree holds.
const QUOTA: u64 = 1 << 30;
/// Shape: the journal's starting budget, the daemon's floor of one page (`journal_floor_bytes`, §4.5, A-157): with no
/// reader open, the oldest record leaves as the newest lands, so a volume no mount reads keeps a page of records. A
/// larger start measures a ring the daemon never grows: its doublings copied the whole log in one write (2.75 MiB at
/// 16,384 records, a 302 µs unit, 2026-10-24).
const JOURNAL_BYTES: usize = PAGE;
/// Shape: the entries a walk may read: past the large tree's.
const MOST: usize = 1 << 20;
/// Shape: the smallest step budget: one unit a call, as the daemon steps.
const SMALLEST: u64 = 1;
/// Shape: the image's capacity: past the large tree's inodes and blocks.
const CAPACITY: u64 = 256 << 20;
/// Format: the images' identity; the time is in the past, as a written image's must be.
const IDENTITY: Identity = Identity {
  uuid: [5; 16],
  created: Timestamp {
    secs: 1_700_000_000,
    nanos: 0,
  },
};

fn store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(REGION_BYTES, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 64,
      max_dirs: MAX_OBJECTS,
      max_inodes: MAX_INODES,
      max_chunks: MAX_OBJECTS,
      max_dir_blocks: MAX_OBJECTS,
      dir_cutover: 4,
    },
    arena,
    0,
  )
}

fn config(prefix: u16, names: NameEquivalence) -> VolumeConfig {
  VolumeConfig {
    prefix,
    names,
    quota: Quota::Bounded { limit: QUOTA },
    journal_bytes: JOURNAL_BYTES,
    clock: Box::new(StepClock::new(1_000 * 1_000_000_000, 1_000)),
  }
}

/// The worst step of each phase of one tree's build and merge.
#[derive(Clone, Copy, Debug, Default)]
struct Costs {
  walk: Traffic,
  write: Traffic,
  read: Traffic,
  merge: Traffic,
  release: Traffic,
}

/// Builds the image of a volume of `files` files of `size` bytes, `per_directory` to a directory under its root, names
/// judged by `names`, reads it back and merges it into a clone of its snapshot, every phase in the smallest steps; the
/// worst step of each.
fn costs(files: usize, size: usize, per_directory: usize, names: NameEquivalence) -> Costs {
  let mut store = store();
  let mut source = Volume::create(&mut store, config(7, names)).unwrap();
  let root = source.root_inode(&store).unwrap();
  let mut directory = root;
  for index in 0..files {
    if index % per_directory == 0 {
      directory = source
        .mkdir_no(
          &mut store,
          root,
          &format!("Dir{:05}", index / per_directory),
          0o755,
        )
        .unwrap();
    }
    let file = source
      .create_file_no(&mut store, directory, &format!("File{index:06}"), 0o644)
      .unwrap();
    if size > 0 {
      let bytes = vec![u8::try_from(index % 251 + 1).unwrap(); size];
      source.write(&mut store, file, 0, &bytes).unwrap();
    }
  }
  let snapshot = source.snapshot(&mut store).unwrap();
  let mut costs = Costs::default();

  let mut walk = SnapshotWalk::new(&source, &store, snapshot, MOST).unwrap();
  let base = loop {
    let (traffic, walked) = counted(|| walk.step(&source, &store, SMALLEST).unwrap());
    costs.walk = costs.walk.worst(traffic);
    if let Some(tree) = walked {
      break tree;
    }
  };

  let mut image_volume = Volume::create(&mut store, config(11, NameEquivalence::Exact)).unwrap();
  let image_root = image_volume.root_inode(&store).unwrap();
  let file = image_volume
    .create_file_no(&mut store, image_root, "image.ext2", 0o600)
    .unwrap();
  let mut builder = Builder::new(base.tree.clone(), CAPACITY, IDENTITY).unwrap();
  let len = builder.image_bytes();
  image_volume.truncate(&mut store, file, len).unwrap();
  loop {
    let (traffic, progress) = counted(|| {
      let mut io = BuildIo {
        store: &mut store,
        source: &source,
        snapshot,
        inodes: &base.inodes,
        image: &mut image_volume,
        file,
      };
      builder.step(&mut io, SMALLEST).unwrap()
    });
    costs.write = costs.write.worst(traffic);
    if let Progress::Done(_) = progress {
      break;
    }
  }

  let image_snapshot = image_volume.snapshot(&mut store).unwrap();
  let mut image_file = SnapshotFile {
    volume: &image_volume,
    store: &store,
    snapshot: image_snapshot,
    file,
    len,
  };
  let (traffic, image) = counted(|| Image::open(&mut image_file).unwrap());
  costs.read = costs.read.worst(traffic);
  let mut image = image;
  let (traffic, image_walk) = counted(|| Walk::new(&mut image, &mut image_file, MOST).unwrap());
  costs.read = costs.read.worst(traffic);
  let mut image_walk = image_walk;
  let tree = loop {
    let (traffic, walked) = counted(|| {
      image_walk
        .step(&mut image, &mut image_file, SMALLEST)
        .unwrap()
    });
    costs.read = costs.read.worst(traffic);
    if let Walked::Done(tree) = walked {
      break *tree;
    }
  };

  let mut clone = Volume::clone_of(&store, &mut source, snapshot, config(9, names)).unwrap();
  let (traffic, merge) = counted(|| Merge::new(base, tree, &clone, &store, true));
  costs.merge = costs.merge.worst(traffic);
  let mut merge = merge;
  loop {
    let (traffic, report) = counted(|| {
      let mut content = ImageContent {
        image: &mut image,
        volume: &image_volume,
        snapshot: image_snapshot,
        file,
        len,
      };
      merge
        .step(&mut clone, &mut store, &mut content, SMALLEST)
        .unwrap()
    });
    costs.merge = costs.merge.worst(traffic);
    if let Some(report) = report {
      assert_eq!(report.bytes_written, 0, "an unchanged image writes nothing");
      break;
    }
  }

  loop {
    let (traffic, more) =
      counted(|| merge.shed() || builder.shed() || walk.shed() || image_walk.shed());
    costs.release = costs.release.worst(traffic);
    if !more {
      break;
    }
  }
  let (traffic, ()) = counted(|| {
    drop(merge);
    drop(builder);
    drop(walk);
    drop(image_walk);
    drop(image);
  });
  costs.release = costs.release.worst(traffic);
  costs
}

/// A-160 (§4.3, bounded steps). Do: build, read back and merge the image of a tree of 2,048 empty files and of one of
/// 16,384 — all in one directory, under exact names and under folded ones, and in directories of 64 — counting each
/// step's allocator traffic, through to the release of what the build and the merge made. Expect no phase's worst step
/// in the large tree to allocate larger than the small tree's worst, nor to make or free more than [`SLACK`] more
/// allocations: a step's work is bounded whatever the tree.
#[test]
fn no_step_of_an_image_build_or_merge_costs_more_in_a_large_tree_than_in_a_small_one() {
  let shapes = [
    (
      "one directory, exact names",
      0,
      usize::MAX,
      NameEquivalence::Exact,
    ),
    (
      "one directory, folded names",
      0,
      usize::MAX,
      NameEquivalence::Fold,
    ),
    (
      "directories of 64",
      0,
      PER_DIRECTORY,
      NameEquivalence::Exact,
    ),
    (
      "files of 64 bytes",
      FILE_BYTES,
      PER_DIRECTORY,
      NameEquivalence::Exact,
    ),
  ];
  let mut excesses = Vec::new();
  for (shape, size, per_directory, names) in shapes {
    let small = costs(SMALL, size, per_directory.min(SMALL), names);
    let large = costs(LARGE, size, per_directory.min(LARGE), names);
    let phases = [
      ("walk", small.walk, large.walk),
      ("write", small.write, large.write),
      ("read", small.read, large.read),
      ("merge", small.merge, large.merge),
      ("release", small.release, large.release),
    ];
    for (phase, small, large) in phases {
      if large.largest > small.largest
        || large.calls > small.calls + SLACK
        || large.frees > small.frees + SLACK
      {
        excesses.push(format!(
          "{shape}, {phase}: worst step {large:?} at {LARGE} files against {small:?} at {SMALL}"
        ));
      }
    }
  }
  assert!(
    excesses.is_empty(),
    "steps whose cost follows the tree:\n{}",
    excesses.join("\n")
  );
}
