//! The costs of a volume's disk image (A-160; §4.6): what `slates image build` and `slates image merge` spend, phase
//! by phase, on trees of small, medium and large files. Each phase runs as the daemon runs it: in slices bounded
//! against the clock ([`SLICE_NS`], half a step quantum; `DaemonConfig::copy_slice_ns`), one unit of work a call. The
//! longest slice and the longest unit are reported beside the total: the evidence that no step of an image's build or
//! merge holds its shard past a slice and one unit (§4.3).
//!
//! The phases:
//! - **walk**: the snapshot walked into the tree the writer takes (`SnapshotWalk`);
//! - **write**: the image laid out and written into a file of another volume (`Builder` over `BuildIo`);
//! - **read**: the image opened and its tree walked back out of that file (`Image::open`, `Walk` over
//!   `SnapshotFile`);
//! - **merge**: the unchanged image merged into a clone of the snapshot (`Merge` over `ImageContent`), every byte
//!   compared, none written.
//!
//! `cargo run --release -p slates-vfs --example image_bench` prints one row per case and phase: the best of
//! [`ROUNDS`] totals with every round shown, the rate, and the longest slice of the best round that the scheduler left
//! alone. A slice during which the process was switched out involuntarily (`getrusage`'s count moved) is not judged
//! by its wall time, as `vfs_bench`'s `destroy_rows` judges: on a loaded machine the scheduler's own quantum would
//! read as the work's. Those slices are counted, with the longest of them, so none is hidden. **Failures** (the
//! process exits non-zero): an image that reads back with another node count, or a merge of an unchanged image that
//! writes a byte or compares fewer bytes than the files hold.

// A benchmark harness: an unwrap here is a failed run, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use slates_image::{Builder, Identity, Image, Progress, Timestamp, Walk, Walked};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::StepClock;
use slates_vfs::ids::SnapshotId;
use slates_vfs::image::{BuildIo, ImageContent, Merge, SnapshotFile, SnapshotWalk};
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Shape: the rounds recorded per case; the best is reported (BENCHMARKS.md best-of-N, all N shown).
const ROUNDS: usize = 5;
/// Shape: a slice's time: half the box's step quantum, 47,469 ns as its daemon derived it on 2026-10-24
/// (`copy_slice_bytes` 262,753 B at its measured memcpy of 11.07 GB/s, over the copy share of one half).
const SLICE_NS: u64 = 23_700;
/// Format: the byte budget each unit's call is given: one, so every call does exactly one unit.
const ONE_UNIT: u64 = 1;
/// Shape: the cases — a name, the files, each file's bytes, and the files per directory.
const CASES: [(&str, usize, usize, usize); 3] = [
  ("10,000 files of 4 KiB", 10_000, 4 << 10, 250),
  ("1,000 files of 64 KiB", 1_000, 64 << 10, 100),
  ("64 files of 1 MiB", 64, 1 << 20, 64),
];
/// Format: the page size (the store's granule).
const PAGE: usize = 4096;
/// Shape: the RAM region the store maps (address space; only what is written is touched).
const REGION_BYTES: usize = 1 << 31;
/// Shape: the store's inode and directory caps, above the largest case's population in its three volumes.
const MAX_INODES: usize = 1 << 17;
/// Shape: the store's directory-node and block caps.
const MAX_DIRS: usize = 1 << 15;
/// Shape: the store's chunk cap, above what the largest case writes into its source and its image.
const MAX_CHUNKS: usize = 1 << 17;
/// Shape: each volume's quota, far above what any case writes.
const QUOTA: u64 = 1 << 30;
/// Shape: the journal's bytes, so the largest case's operations stay recorded.
const JOURNAL_BYTES: usize = 1 << 26;
/// Format: the cache line.
const CACHE_LINE: usize = 64;
/// Shape: the small-directory cut-over, the store's measured default range.
const DIR_CUTOVER: usize = 32;
/// Shape: the image's room past the files: the file system's own structures and a guest's room to write.
const IMAGE_ROOM: u64 = 64 << 20;
/// Format: the images' identity; the time is in the past, as a written image's must be.
const IDENTITY: Identity = Identity {
  uuid: [3; 16],
  created: Timestamp {
    secs: 1_700_000_000,
    nanos: 0,
  },
};
/// Format: the nanoseconds a clock starts at, a thousand seconds past the epoch, inside the image format's range.
const CLOCK_START_NS: u64 = 1_000 * 1_000_000_000;

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

fn config(prefix: u16) -> VolumeConfig {
  VolumeConfig {
    prefix,
    names: NameEquivalence::Exact,
    quota: Quota::Bounded { limit: QUOTA },
    journal_bytes: JOURNAL_BYTES,
    clock: Box::new(StepClock::new(CLOCK_START_NS, 1)),
  }
}

/// A volume of `files` files of `size` bytes, `per_dir` to a directory, each a pattern that fills every block (no
/// block of an image is left unwritten for being zero); and its snapshot.
fn source(
  store: &mut Store,
  (files, size, per_dir): (usize, usize, usize),
) -> (Volume, SnapshotId) {
  let mut volume = Volume::create(store, config(7)).unwrap();
  let root = volume.root_inode(store).unwrap();
  let mut directory = root;
  for index in 0..files {
    if index % per_dir == 0 {
      directory = volume
        .mkdir_no(store, root, &format!("d{:05}", index / per_dir), 0o755)
        .unwrap();
    }
    let file = volume
      .create_file_no(store, directory, &format!("f{index:06}"), 0o644)
      .unwrap();
    let bytes: Vec<u8> = (0..size)
      .map(|at| u8::try_from((at + index) % 251 + 1).unwrap())
      .collect();
    volume.write(store, file, 0, &bytes).unwrap();
  }
  let snapshot = volume.snapshot(store).unwrap();
  (volume, snapshot)
}

/// One phase's time: its total and slices, its longest slice the scheduler left alone and its longest unit, and the
/// slices it did not leave alone.
#[derive(Clone, Default)]
struct Timed {
  total: Duration,
  slices: usize,
  longest: Duration,
  longest_unit: Duration,
  preempted: usize,
  longest_preempted: Duration,
  /// Every unit's time in the slices the scheduler left alone, for the units' percentiles.
  units: Vec<Duration>,
}

impl Timed {
  /// Counts a slice of `took` whose longest unit took `unit`, begun when the involuntary-switch count read `before`.
  fn slice(&mut self, took: Duration, before: Option<u64>, units: &[Duration]) {
    self.total += took;
    self.slices += 1;
    // A preempted slice's units are not judged either: the switch may have fallen inside any of them.
    if slates_machine::bench::involuntary_switches_since(before)
      .is_some_and(|switches| switches > 0)
    {
      self.preempted += 1;
      self.longest_preempted = self.longest_preempted.max(took);
    } else {
      self.longest = self.longest.max(took);
      for unit in units {
        self.longest_unit = self.longest_unit.max(*unit);
      }
      self.units.extend_from_slice(units);
    }
  }

  /// The units' time at quantile `per_mille`, in the slices the scheduler left alone.
  fn unit_quantile(&self, per_mille: usize) -> Duration {
    let mut sorted = self.units.clone();
    sorted.sort_unstable();
    let at = (sorted.len().saturating_sub(1) * per_mille) / 1_000;
    sorted.get(at).copied().unwrap_or_default()
  }
}

/// Runs `unit` — one unit of a phase, `Some` once the phase is whole — in slices of [`SLICE_NS`], as the daemon's
/// `sliced` does, timing each into `timed`; the phase's result.
fn sliced<T>(timed: &mut Timed, mut unit: impl FnMut() -> Option<T>) -> T {
  let budget = Duration::from_nanos(SLICE_NS);
  loop {
    let (started, before) = (
      Instant::now(),
      slates_machine::bench::involuntary_switches(),
    );
    let mut units = Vec::new();
    let done = loop {
      let unit_started = Instant::now();
      let done = unit();
      units.push(unit_started.elapsed());
      if done.is_some() || started.elapsed() >= budget {
        break done;
      }
    };
    timed.slice(started.elapsed(), before, &units);
    if let Some(done) = done {
      return done;
    }
  }
}

/// One round's four phases.
#[derive(Clone, Default)]
struct Round {
  walk: Timed,
  write: Timed,
  read: Timed,
  merge: Timed,
}

/// What a round measured besides its times: the nodes, the image's bytes in use (what was written: the file system's
/// structures and the files' blocks, not its free space) and the bytes the merge compared.
#[derive(Clone, Copy, Default)]
struct Sizes {
  nodes: usize,
  image_bytes: u64,
  compared: u64,
}

/// Format: the image's block, which `Written::blocks_used` counts.
const IMAGE_BLOCK_BYTES: u64 = slates_image::layout::BLOCK_BYTES;

/// One round of `case`: every phase, each in slices.
fn round(case: (usize, usize, usize)) -> (Round, Sizes) {
  let mut store = store();
  let (mut volume, snapshot) = source(&mut store, case);
  let mut times = Round::default();

  let mut walk = SnapshotWalk::new(&volume, &store, snapshot, usize::MAX).unwrap();
  let base = sliced(&mut times.walk, || {
    walk.step(&volume, &store, ONE_UNIT).unwrap()
  });

  let (files, size, _) = case;
  let capacity = u64::try_from(files.saturating_mul(size)).unwrap() + IMAGE_ROOM;
  let mut image_volume = Volume::create(&mut store, config(11)).unwrap();
  let root = image_volume.root_inode(&store).unwrap();
  let file = image_volume
    .create_file_no(&mut store, root, "image.ext2", 0o600)
    .unwrap();
  let mut builder = Builder::new(base.tree.clone(), capacity, IDENTITY).unwrap();
  let image_bytes = builder.image_bytes();
  image_volume
    .truncate(&mut store, file, image_bytes)
    .unwrap();
  let written = sliced(&mut times.write, || {
    let mut io = BuildIo {
      store: &mut store,
      source: &volume,
      snapshot,
      inodes: &base.inodes,
      image: &mut image_volume,
      file,
    };
    match builder.step(&mut io, ONE_UNIT).unwrap() {
      Progress::More => None,
      Progress::Done(written) => Some(written),
    }
  });

  let image_snapshot = image_volume.snapshot(&mut store).unwrap();
  let mut source_file = SnapshotFile {
    volume: &image_volume,
    store: &store,
    snapshot: image_snapshot,
    file,
    len: image_bytes,
  };
  let mut image = Image::open(&mut source_file).unwrap();
  let mut image_walk = Walk::new(&mut image, &mut source_file, usize::MAX).unwrap();
  let tree = sliced(&mut times.read, || {
    match image_walk
      .step(&mut image, &mut source_file, ONE_UNIT)
      .unwrap()
    {
      Walked::More => None,
      Walked::Done(tree) => Some(tree),
    }
  });
  let nodes = tree.tree.nodes().len();
  assert_eq!(nodes, base.tree.nodes().len(), "the image reads back whole");

  let mut clone = Volume::clone_of(&store, &mut volume, snapshot, config(9)).unwrap();
  let mut merge = Merge::new(base, tree, &clone, &store, true);
  let report = sliced(&mut times.merge, || {
    let mut content = ImageContent {
      image: &mut image,
      volume: &image_volume,
      snapshot: image_snapshot,
      file,
      len: image_bytes,
    };
    merge
      .step(&mut clone, &mut store, &mut content, ONE_UNIT)
      .unwrap()
  });
  let bytes = u64::try_from(files.saturating_mul(size)).unwrap();
  assert_eq!(report.bytes_written, 0, "an unchanged image writes nothing");
  assert_eq!(report.bytes_compared, bytes, "every byte is compared");
  (
    times,
    Sizes {
      nodes,
      image_bytes: written.blocks_used.saturating_mul(IMAGE_BLOCK_BYTES),
      compared: bytes,
    },
  )
}

/// A phase as the report names it: its name, how a round's time for it is found, and what its rate is of.
type Phase = (&'static str, fn(&Round) -> &Timed, String);

/// Milliseconds, to three places.
fn ms(duration: Duration) -> String {
  format!("{:.3}", duration.as_secs_f64() * 1_000.0)
}

fn main() {
  println!(
    "case,phase,best_ms,rounds_ms,rate,slices,longest_slice_us,unit_p50_us,unit_p99_us,longest_unit_us,preempted_slices,longest_preempted_us"
  );
  for (name, files, size, per_dir) in CASES {
    let rounds: Vec<(Round, Sizes)> = (0..ROUNDS).map(|_| round((files, size, per_dir))).collect();
    let sizes = rounds.first().map(|(_, sizes)| *sizes).unwrap_or_default();
    let phases: [Phase; 4] = [
      (
        "walk",
        |round| &round.walk,
        format!("{} entries", sizes.nodes),
      ),
      (
        "write",
        |round| &round.write,
        format!("{} image bytes in use", sizes.image_bytes),
      ),
      (
        "read",
        |round| &round.read,
        format!("{} entries", sizes.nodes),
      ),
      (
        "merge",
        |round| &round.merge,
        format!("{} bytes compared", sizes.compared),
      ),
    ];
    for (phase, of, unit) in phases {
      let Some(best) = rounds
        .iter()
        .map(|(round, _)| of(round))
        .min_by_key(|timed| timed.total)
      else {
        continue;
      };
      let all: Vec<String> = rounds
        .iter()
        .map(|(round, _)| ms(of(round).total))
        .collect();
      let seconds = best.total.as_secs_f64().max(f64::MIN_POSITIVE);
      let rate = match phase {
        "write" => format!(
          "{:.0} MB/s of {unit}",
          sizes.image_bytes as f64 / seconds / 1e6
        ),
        "merge" => format!(
          "{:.0} MB/s of {unit}",
          sizes.compared as f64 / seconds / 1e6
        ),
        _ => format!(
          "{:.0} entries/ms of {unit}",
          sizes.nodes as f64 / seconds / 1e3
        ),
      };
      println!(
        "{name},{phase},{},{},{rate},{},{:.1},{:.1},{:.1},{:.1},{},{:.1}",
        ms(best.total),
        all.join(" "),
        best.slices,
        best.longest.as_secs_f64() * 1e6,
        best.unit_quantile(500).as_secs_f64() * 1e6,
        best.unit_quantile(990).as_secs_f64() * 1e6,
        best.longest_unit.as_secs_f64() * 1e6,
        best.preempted,
        best.longest_preempted.as_secs_f64() * 1e6
      );
    }
  }
}
