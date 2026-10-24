//! The costs of a volume's disk image (A-160; §4.6): what `slates image build` and `slates image merge` spend, phase
//! by phase, on trees of small, medium and large files, and on the extremes a directory and a tree reach. Each phase
//! runs as the daemon runs it: in slices bounded against the clock ([`SLICE_NS`], half a step quantum;
//! `DaemonConfig::copy_slice_ns`), one unit of work a call. The longest unit is reported beside the total: the evidence
//! that no step of an image's build or merge holds its shard past a slice and one small unit (§4.3).
//!
//! The phases:
//! - **walk**: the snapshot walked into the tree the writer takes (`SnapshotWalk`);
//! - **write**: the image laid out and written into a file of another volume (`Builder` over `BuildIo`);
//! - **read**: the image opened and its tree walked back out of that file (`Image::open`, `Walk` over
//!   `SnapshotFile`);
//! - **merge**: the unchanged image merged into a clone of the snapshot (`Merge` over `ImageContent`), every byte
//!   compared, none written;
//! - **release**: what the walks, the build and the merge made, released a piece a unit (`shed`), as the daemon
//!   releases a finished job's after its reply.
//!
//! `cargo run --release -p slates-vfs --example image_bench` prints one row per case and phase: the best of
//! [`ROUNDS`] totals with every round shown, the rate, the best round's slices and the longest of them the machine left
//! alone, and the units' work. **A unit's work is its least time over the rounds** ([`work_units`]): every round runs
//! the same units in the same order, so interference must strike one unit in every round to survive the minimum, while
//! what a unit itself costs — its own allocations' faults among it — is in every round. The units' p50, p99 and longest
//! work are reported, the longest with its index, beside the longest unit any round saw (the raw). A slice's wall time
//! cannot tell a unit's cost from the machine's: setting aside slices the process was switched out in or faulted in
//! (`getrusage`) hid exactly the units that allocate, which were the units that grew with a directory (2026-10-24);
//! those slices are still counted, with the longest of them. A noise row per case runs trivial units the same way: its
//! work stays near 0.1 µs while its raw longest meets the machine's interference. The region is faulted in before each
//! round, as a daemon's arena is (§4.2, D-12). **Failures** (the process exits non-zero): an image that reads back with
//! another node count, a merge of an unchanged image that writes a byte or compares fewer bytes than the files hold,
//! or rounds that ran different units.

// A benchmark harness: an unwrap here is a failed run, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use slates_image::{Builder, Identity, Image, Progress, Timestamp, Walk, Walked};
use slates_mem::arena::ChunkArena;
use slates_mem::prefault::{PreFault, Progress as Faulted};
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
/// Shape: the pieces a release unit frees, as the daemon's (`slates-server`'s `SHED_PIECES`).
const SHED_PIECES: usize = 64;
/// Shape: the cases — a name, the files, each file's bytes, and the files per directory. The first three are trees of
/// small, medium and large files; the last three the extremes, a directory of 20,000 and of 100,000 entries and a tree of
/// 100,000 nodes, where any unit whose work grows with a directory or a tree shows.
const CASES: [(&str, usize, usize, usize); 6] = [
  ("10k files of 4 KiB", 10_000, 4 << 10, 250),
  ("1k files of 64 KiB", 1_000, 64 << 10, 100),
  ("64 files of 1 MiB", 64, 1 << 20, 64),
  ("20k files of 64 B in one directory", 20_000, 64, 20_000),
  ("100k empty files in one directory", 100_000, 0, 100_000),
  ("100k empty files in 100 directories", 100_000, 0, 1_000),
];
/// Format: the page size (the store's granule).
const PAGE: usize = 4096;
/// Shape: the RAM region the store maps, every page faulted in before a round (as a daemon's arena is, by its
/// pre-fault scheduler, §4.2, D-12): room for the largest case's source and image several times over.
const REGION_BYTES: usize = 1 << 29;
/// Shape: the store's inode and directory caps, above the largest case's population in its three volumes.
const MAX_INODES: usize = 1 << 17;
/// Shape: the store's directory-node and block caps.
const MAX_DIRS: usize = 1 << 15;
/// Shape: the store's chunk cap, above what the largest case writes into its source and its image.
const MAX_CHUNKS: usize = 1 << 17;
/// Shape: each volume's quota, far above what any case writes.
const QUOTA: u64 = 1 << 30;
/// Shape: the journal's starting budget, the daemon's floor of one page (`journal_floor_bytes`, §4.5, A-157): with no
/// reader open, the oldest record leaves as the newest lands, so a volume no mount reads keeps a page of records. A
/// larger start measures a ring the daemon never grows: its doublings copied the whole log in one write (2.75 MiB at
/// 16,384 records, a 302 µs unit, 2026-10-24).
const JOURNAL_BYTES: usize = PAGE;
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

/// A store over one region, its pages faulted in first. A daemon's arena is pre-faulted on its idle time (§4.2, D-12)
/// and never faults on a write; a raw region's first touch would charge the kernel's fault to the image's work (on the
/// box, with transparent huge pages, one fault zeroing 2 MiB in 80–112 µs every 512 blocks written, 2026-10-24).
fn store() -> Store {
  let mut region = Region::map(REGION_BYTES, PAGE, false).unwrap();
  let mut prefault = PreFault::new(u64::MAX, 1);
  while prefault.step(&mut region) != Faulted::Done {}
  let mut arena = ChunkArena::new(PAGE);
  arena.add_region(region).unwrap();
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

/// One phase's time in one round: its total and slices, its longest slice the machine left alone, the slices it did
/// not, and every unit's time in the order the units ran.
#[derive(Clone, Default)]
struct Timed {
  total: Duration,
  slices: usize,
  longest: Duration,
  disturbed: usize,
  longest_disturbed: Duration,
  /// Every unit's time, in order. Every round runs the same units in the same order (one snapshot, one layout, one
  /// merge), so a unit's least time over the rounds is its work's ([`work_units`]).
  units: Vec<Duration>,
}

/// The process's context switches (involuntary and voluntary: preempted, or made to wait in the kernel) and minor page
/// faults so far, as `getrusage` counts them; `None` if it refuses.
#[cfg(unix)]
fn disturbances() -> Option<(i64, i64)> {
  let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
  // SAFETY: `getrusage` fills the `rusage` the pointer names and returns zero on success; the buffer is ours, fully
  // sized and zeroed before the call, and read only once it succeeded.
  let usage = unsafe {
    if libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) != 0 {
      return None;
    }
    usage.assume_init()
  };
  Some((
    usage.ru_nivcsw.saturating_add(usage.ru_nvcsw),
    usage.ru_minflt,
  ))
}

/// The Unix twin's counts, which Windows has no `getrusage` for: `None`, so no slice is set aside as disturbed there and
/// the disturbed columns read zero. The work units, each one's least time over the rounds, do not use them.
#[cfg(not(unix))]
fn disturbances() -> Option<(i64, i64)> {
  None
}

impl Timed {
  /// Counts a slice of `took` whose units took `units`, begun when the disturbances read `before`.
  fn slice(&mut self, took: Duration, before: Option<(i64, i64)>, units: &[Duration]) {
    self.total += took;
    self.slices += 1;
    self.units.extend_from_slice(units);
    let disturbed = match (before, disturbances()) {
      (Some(before), Some(after)) => after.0 > before.0 || after.1 > before.1,
      _ => false,
    };
    if disturbed {
      self.disturbed += 1;
      self.longest_disturbed = self.longest_disturbed.max(took);
    } else {
      self.longest = self.longest.max(took);
    }
  }
}

/// Each unit's work: its least time over the rounds. Interference has to strike the same unit in every round to
/// survive the minimum, while what the unit itself costs — the faults its own allocations take among it — is in every
/// round's time. A slice's wall time cannot tell the two apart: a unit that allocates is a disturbed slice by its own
/// doing, and setting such slices aside hid exactly the units that grow with a directory (2026-10-24).
fn work_units(rounds: &[&Timed]) -> Vec<Duration> {
  let count = rounds.first().map_or(0, |timed| timed.units.len());
  assert!(
    rounds.iter().all(|timed| timed.units.len() == count),
    "every round runs the same units"
  );
  (0..count)
    .map(|index| {
      rounds
        .iter()
        .filter_map(|timed| timed.units.get(index))
        .min()
        .copied()
        .unwrap_or_default()
    })
    .collect()
}

/// The time at quantile `per_mille` of `units`.
fn quantile(units: &[Duration], per_mille: usize) -> Duration {
  let mut sorted = units.to_vec();
  sorted.sort_unstable();
  let at = (sorted.len().saturating_sub(1) * per_mille) / 1_000;
  sorted.get(at).copied().unwrap_or_default()
}

/// Runs `unit` — one unit of a phase, `Some` once the phase is whole — in slices of [`SLICE_NS`], as the daemon's
/// `sliced` does, timing each into `timed`; the phase's result.
fn sliced<T>(timed: &mut Timed, mut unit: impl FnMut() -> Option<T>) -> T {
  let budget = Duration::from_nanos(SLICE_NS);
  loop {
    let (started, before) = (Instant::now(), disturbances());
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

/// One round's five phases.
#[derive(Clone, Default)]
struct Round {
  walk: Timed,
  write: Timed,
  read: Timed,
  merge: Timed,
  release: Timed,
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
  let blocks = u64::try_from(size).unwrap().div_ceil(IMAGE_BLOCK_BYTES);
  let by_blocks = u64::try_from(files).unwrap() * blocks * IMAGE_BLOCK_BYTES + IMAGE_ROOM;
  // Groups enough for every node's inode, one group's inodes past the files for the directories and reserved ones.
  let groups = u64::try_from(files).unwrap() / slates_image::layout::MAX_INODES_PER_GROUP + 2;
  let by_inodes = groups * slates_image::layout::BLOCKS_PER_GROUP * IMAGE_BLOCK_BYTES;
  let capacity = by_blocks.max(by_inodes);
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
      Walked::Done(tree) => Some(*tree),
    }
  });
  let nodes = tree.tree.node_count();
  assert_eq!(nodes, base.tree.node_count(), "the image reads back whole");

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
  sliced(&mut times.release, || {
    let more = (0..SHED_PIECES)
      .all(|_| merge.shed() || builder.shed() || walk.shed() || image_walk.shed() || image.shed());
    (!more).then_some(())
  });
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

/// Shape: how long the noise probe runs its trivial units: about one of the longer phases, so it meets as much of
/// the machine's interference as a phase does.
const NOISE_PROBE: Duration = Duration::from_millis(40);
/// Shape: the arithmetic one trivial unit of the noise probe does: well under a microsecond, so any unit that takes
/// longer was held by the machine, not by its work.
const NOISE_UNIT_STEPS: u64 = 64;

/// One round of the noise probe: trivial units in [`SLICE_NS`] slices until `done` says so, given the units run and the
/// time spent.
fn noise_round(done: impl Fn(usize, Duration) -> bool) -> Timed {
  let mut timed = Timed::default();
  let started = Instant::now();
  let (mut state, mut ran) = (0u64, 0usize);
  sliced(&mut timed, || {
    // Format: Knuth's MMIX multiplier, any odd constant serving: the work only has to be real and tiny.
    for step in 0..NOISE_UNIT_STEPS {
      state = std::hint::black_box(
        state
          .wrapping_mul(6_364_136_223_846_793_005)
          .wrapping_add(step),
      );
    }
    ran += 1;
    done(ran, started.elapsed()).then_some(())
  });
  timed
}

/// The machine's own interference, measured in-process before the phases are judged (as `vfs_bench`'s `destroy_rows`
/// measures its jitter): [`ROUNDS`] rounds of trivial units in [`SLICE_NS`] slices, the first for [`NOISE_PROBE`] and
/// the rest for as many units as it ran, so the rounds' units align as a phase's do. Its raw longest unit is the
/// interference a single round meets; its longest work unit, near its p50, shows the minimum over rounds removes it.
fn noise_floor() -> Vec<Timed> {
  let first = noise_round(|_, spent| spent >= NOISE_PROBE);
  let units = first.units.len();
  std::iter::once(first)
    .chain((1..ROUNDS).map(|_| noise_round(|ran, _| ran >= units)))
    .collect()
}

/// A phase as the report names it: its name, how a round's time for it is found, and what its rate is of.
type Phase = (&'static str, fn(&Round) -> &Timed, String);

/// Milliseconds, to three places.
fn ms(duration: Duration) -> String {
  format!("{:.3}", duration.as_secs_f64() * 1_000.0)
}

/// Microseconds, to one place.
fn us(duration: Duration) -> String {
  format!("{:.1}", duration.as_secs_f64() * 1e6)
}

/// One row: the best round's total with every round's shown, the rate, the best round's slices, and the units' work
/// (their least times over the rounds) beside the longest unit any round saw.
fn row(name: &str, phase: &str, rounds: &[&Timed], rate: &str) {
  let Some(best) = rounds.iter().min_by_key(|timed| timed.total) else {
    return;
  };
  let all: Vec<String> = rounds.iter().map(|timed| ms(timed.total)).collect();
  let work = work_units(rounds);
  let (longest_at, longest) = work
    .iter()
    .enumerate()
    .max_by_key(|(_, unit)| **unit)
    .map(|(at, unit)| (at, *unit))
    .unwrap_or_default();
  let raw = rounds
    .iter()
    .flat_map(|timed| timed.units.iter())
    .max()
    .copied()
    .unwrap_or_default();
  println!(
    "{name},{phase},{},{},{rate},{},{},{},{},{},{},{},{longest_at},{}",
    ms(best.total),
    all.join(" "),
    best.slices,
    us(best.longest),
    best.disturbed,
    us(best.longest_disturbed),
    us(quantile(&work, 500)),
    us(quantile(&work, 990)),
    us(longest),
    us(raw)
  );
}

fn main() {
  println!(
    "case,phase,best_ms,rounds_ms,rate,slices,longest_slice_us,disturbed_slices,longest_disturbed_us,unit_p50_us,unit_p99_us,longest_unit_us,longest_unit_at,longest_raw_unit_us"
  );
  for (name, files, size, per_dir) in CASES {
    let noise = noise_floor();
    row(
      name,
      "noise",
      &noise.iter().collect::<Vec<_>>(),
      "trivial units",
    );
    let rounds: Vec<(Round, Sizes)> = (0..ROUNDS).map(|_| round((files, size, per_dir))).collect();
    let sizes = rounds.first().map(|(_, sizes)| *sizes).unwrap_or_default();
    let phases: [Phase; 5] = [
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
      (
        "release",
        |round| &round.release,
        format!("{} entries", sizes.nodes),
      ),
    ];
    for (phase, of, unit) in phases {
      let timed: Vec<&Timed> = rounds.iter().map(|(round, _)| of(round)).collect();
      let best = timed
        .iter()
        .map(|timed| timed.total)
        .min()
        .unwrap_or_default();
      let seconds = best.as_secs_f64().max(f64::MIN_POSITIVE);
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
      row(name, phase, &timed, &rate);
    }
  }
}
