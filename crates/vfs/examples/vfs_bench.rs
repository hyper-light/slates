//! The Phase 1 volume baseline (Phase 1 task 9; AC-1.3, AC-1.5, AC-1.8, T-1.7; BENCHMARKS.md):
//! create, lookup, readdir, write, read, rename, snapshot, clone and destroy costs versus tree
//! size; the directory cut-over measured in place; memory per file against the derived budget;
//! destroy in bounded slices against the shard's step budget; the 190k-file create burst.
//!
//! `cargo run --release -p slates-vfs --example vfs_bench`
//!
//! Trees have the shape of a `cargo build` output tree measured on this workspace (see
//! [`FILES_PER_DIR`] and [`NAME_BYTES`]). Memory is measured through a counting global
//! allocator, so it is every heap byte the process holds for the tree: slabs, names, directory
//! vectors and maps; content lives in the mapped region and is reported separately.

use std::alloc::{GlobalAlloc, Layout, System};
use std::error::Error;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use slates_machine::bench::{Measurement, measure};
use slates_machine::{Derived, derived};
use slates_mem::Handle;
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_mem::slab::Slab;
use slates_vfs::clock::HostClock;
use slates_vfs::dir::{Child, DirNode, MEASURED_CUTOVER};
use slates_vfs::dirtree::{BLOCK_BYTES, DirBlock, ENTRY_BYTES, Retired};
use slates_vfs::ids::{Epoch, InodeNo};
use slates_vfs::inode::Inode;
use slates_vfs::journal::OpRecord;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::trie::{FANOUT, LEVELS, TrieNode};
use slates_vfs::volume::{DestroyProgress, Store, StoreConfig, Volume, VolumeConfig};

/// Measured: files per directory of a `cargo build` output tree. `find target/debug -type f |
/// wc -l` = 40,052 files in `find target/debug -type d | wc -l` = 1,117 directories, 35.9 per
/// directory, on this workspace after `cargo test --workspace`, 2026-09-05.
const FILES_PER_DIR: usize = 36;

/// Measured: the mean file-name length of the same tree, 49.2 bytes over 42,155 files
/// (`find target/debug -type f | awk -F/ '{s+=length($NF); n++} END {print s/n}'`, 2026-09-05).
const NAME_BYTES: usize = 49;

/// Shape: the fan-out of the level above the leaf directories, so a 10^6-file tree is 64
/// groups of 434 directories rather than one directory of 27,778.
const GROUPS: usize = 64;

/// Format: the design's burst size (T-1.7).
const BURST_FILES: usize = 190_000;

/// Format: the tree sizes AC-1.3 names.
const SIZES: [usize; 3] = [1_000, 100_000, 1_000_000];

/// Shape: burst repetitions, best of N with all N shown (CLAUDE.md §5).
const BURST_RUNS: usize = 3;

/// Format: the directory sizes the probe measures: the inline capacity, then doublings up to
/// the cited p99.
const CUTOVER_SIZES: [usize; 7] = [2, 4, 8, 16, 32, 64, 128];

/// Shape: the measurement budget per row (the same as the Phase 0 benches).
const BUDGET: Duration = Duration::from_millis(500);

/// Format: the page size the benches build stores with.
const PAGE: usize = 4096;

/// Shape: content region pages (16 MiB): the content rows use one window of one file.
const REGION_PAGES: usize = 4096;

/// Format: a 64-bit hash's bytes.
const CACHE_LINE: usize = 64;

// ------------------------------------------------------------------ the counting allocator

/// Live heap bytes, kept by the allocator below (a statistic: `Relaxed`, in a bench binary).
static LIVE_HEAP: AtomicUsize = AtomicUsize::new(0);
/// Nanoseconds spent inside the system allocator's `dealloc`, to attribute a slow destroy
/// slice to the allocator or to the volume.
static DEALLOC_NS: AtomicU64 = AtomicU64::new(0);

struct Counting;

// SAFETY: every call forwards to the system allocator unchanged; the counters are statistics
// that do not affect what is returned.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    LIVE_HEAP.fetch_add(layout.size(), Ordering::Relaxed);
    // SAFETY: the layout is the caller's, forwarded as is.
    unsafe { System.alloc(layout) }
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    LIVE_HEAP.fetch_sub(layout.size(), Ordering::Relaxed);
    let started = Instant::now();
    // SAFETY: `ptr` came from `alloc` with this layout.
    unsafe { System.dealloc(ptr, layout) }
    DEALLOC_NS.fetch_add(
      u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
      Ordering::Relaxed,
    );
  }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

// ------------------------------------------------------------------ reporting

fn report(name: &str, m: &Measurement) {
  println!(
    "ratchet\t{}\t{}\t{}\t{}",
    key(name),
    m.interval.lower,
    m.median_ns(),
    m.interval.upper
  );
  println!(
    "{name}: median {} ns [{}, {}] p99 {} ns, {} samples × batch {}{}",
    m.median_ns(),
    m.interval.lower,
    m.interval.upper,
    m.p99_ns,
    m.samples,
    m.batch,
    if m.quick { " (quick)" } else { "" }
  );
}

/// A row from a best-of-N run: lower = best, median = middle, upper = worst, all shown.
fn report_runs(name: &str, unit: &str, mut runs: Vec<u64>) {
  runs.sort_unstable();
  let best = runs.first().copied().unwrap_or(0);
  let worst = runs.last().copied().unwrap_or(0);
  let middle = runs.get(runs.len() / 2).copied().unwrap_or(0);
  println!("ratchet\t{}\t{best}\t{middle}\t{worst}", key(name));
  println!("{name}: best {best} {unit}, all runs {runs:?}");
}

/// A deterministic row (a count or a size): the same value in every column.
fn report_value(name: &str, unit: &str, value: u64) {
  println!("ratchet\t{}\t{value}\t{value}\t{value}", key(name));
  println!("{name}: {value} {unit}");
}

/// A row that the OS scheduler and the profile's per-run budget move more than the code does:
/// printed, never gated (the gate for it is the ac-1.8 verdict, which measures its own floor).
fn report_info_value(name: &str, unit: &str, value: u64) {
  println!("ratchet-info\t{}\t{value}\t{value}\t{value}", key(name));
  println!("{name}: {value} {unit}");
}

/// The ratchet key of a row: `vfs.` plus the row's name in snake case.
fn key(name: &str) -> String {
  let words: Vec<&str> = name
    .split(|c: char| !c.is_ascii_alphanumeric())
    .filter(|w| !w.is_empty())
    .collect();
  format!("vfs.{}", words.join("_").to_ascii_lowercase())
}

// ------------------------------------------------------------------ fixtures

fn store_for(objects: usize, cutover: usize) -> Result<Store, Box<dyn Error>> {
  let mut arena = ChunkArena::new(PAGE);
  arena.add_region(Region::map(PAGE * REGION_PAGES, PAGE, false)?)?;
  Ok(Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: CACHE_LINE,
      max_dirs: objects + GROUPS + 1,
      // Every mutation after a snapshot copies a record; four generations is ample here.
      max_inodes: (objects + GROUPS + 1) * 4,
      max_chunks: REGION_PAGES,
      max_dir_blocks: objects / BLOCK_ENTRIES_LOW + GROUPS + 1,
      dir_cutover: cutover,
    },
    arena,
  ))
}

fn config(prefix: u16) -> VolumeConfig {
  VolumeConfig {
    prefix,
    names: NameEquivalence::Fold,
    quota: Quota::Bounded { limit: 1 << 40 },
    journal_bytes: JOURNAL_BYTES,
    // The host clock: destroy slices are cut by the volume's clock against a real budget.
    clock: Box::new(HostClock::default()),
  }
}

/// A file name of the measured length.
fn file_name(n: usize) -> String {
  format!("{n:0>width$}.rlib.d", width = NAME_BYTES - 7)
}

struct Tree {
  store: Store,
  vol: Volume,
  leaves: Vec<Handle<DirNode>>,
  files: usize,
  /// Leaf and group directories.
  dirs: usize,
  /// Heap bytes the tree took beyond the empty volume.
  heap: usize,
}

/// A tree of `files` files in the measured shape.
fn build(files: usize, cutover: usize) -> Result<Tree, Box<dyn Error>> {
  let dirs = files.div_ceil(FILES_PER_DIR).max(1);
  let mut store = store_for(files + dirs, cutover)?;
  let heap_before = LIVE_HEAP.load(Ordering::Relaxed);
  let mut vol = Volume::create(&mut store, config(1))?;
  let heap_volume = LIVE_HEAP
    .load(Ordering::Relaxed)
    .saturating_sub(heap_before);
  let root = vol.root();
  let mut groups = Vec::with_capacity(GROUPS);
  for g in 0..GROUPS.min(dirs) {
    groups.push(vol.mkdir(&mut store, root, &format!("g{g}"), 0o755)?);
  }
  let heap_groups = LIVE_HEAP
    .load(Ordering::Relaxed)
    .saturating_sub(heap_before);
  let mut leaves = Vec::with_capacity(dirs);
  let mut remaining = files;
  for d in 0..dirs {
    let leaf = vol.mkdir(
      &mut store,
      groups[d % groups.len()],
      &format!("d{d}"),
      0o755,
    )?;
    let count = remaining.min(FILES_PER_DIR);
    for f in 0..count {
      vol.create_file(&mut store, leaf, &file_name(f), 0o644)?;
    }
    remaining -= count;
    leaves.push(leaf);
  }
  let heap = LIVE_HEAP
    .load(Ordering::Relaxed)
    .saturating_sub(heap_before);
  println!(
    "heap for {files} files: {heap_volume} B for the empty volume, {} B after {} group dirs, {} B for the tree ({} B per file), op log {} records",
    heap_groups - heap_volume,
    groups.len(),
    heap - heap_volume,
    (heap - heap_volume) / files.max(1),
    vol.op_log().len()
  );
  Ok(Tree {
    store,
    vol,
    leaves,
    files,
    dirs: dirs + groups.len(),
    heap: heap - heap_volume,
  })
}

/// Derived: the fewest entries a directory block holds after a split at the measured name
/// length, half a block (`BLOCK_BYTES / 2 / (ENTRY_BYTES + NAME_BYTES)`); the block cap and the
/// memory budget count blocks at this occupancy.
const BLOCK_ENTRIES_LOW: usize = BLOCK_BYTES / 2 / (ENTRY_BYTES + NAME_BYTES);
/// Format: the bytes of a journal path before the file name in these trees (`/gNN/dNNNNN/`).
const PATH_PREFIX_BYTES: usize = 12;
/// Shape: the op log budget the bench volumes carry, small enough to be full at the smallest
/// tree, so every size pays the same eviction on each mutation and the size comparison is fair.
const JOURNAL_BYTES: usize = 1 << 16;

/// Derived: the heap budget of a tree (AC-1.5), counted object by object: an inode record per
/// file and per directory; a directory node per directory (its two inline entries and their
/// names included); for every directory past the inline form, its blocks at the lowest
/// occupancy a split leaves (half a block), each a full slab slot; the trie's nodes (one per
/// `FANOUT` inodes at the leaf level and the levels above, `FANOUT / (FANOUT − 1)` per `FANOUT`
/// inodes, plus the spine); one slab segment of slack per slab (blocks: sixty-four pages); and
/// the op log's records, one per create and mkdir with its path and up to twice for the
/// vector's growth, until the log's byte budget is full.
fn heap_budget(files: usize, dirs: usize, page: usize, journal_bytes: usize) -> Derived<usize> {
  let fanout = FANOUT.max(2);
  let records = files + dirs;
  let inodes = records * size_of::<Inode>();
  let nodes = dirs * size_of::<DirNode>();
  let blocks_per_dir = FILES_PER_DIR.div_ceil(BLOCK_ENTRIES_LOW);
  let blocks = files.div_ceil(FILES_PER_DIR) * blocks_per_dir * size_of::<DirBlock>();
  let levels = usize::try_from(LEVELS).unwrap_or(0);
  let trie = (records.div_ceil(fanout) * fanout / (fanout - 1) + levels) * size_of::<TrieNode>();
  let slack = 3 * page + 64 * page;
  let record = 2 * size_of::<OpRecord>() + PATH_PREFIX_BYTES + NAME_BYTES;
  let journal = journal_bytes.min(records * record);
  derived!(
    inodes + nodes + blocks + trie + slack + journal,
    "(files + dirs) × size_of::<Inode>() + dirs × size_of::<DirNode>() + leaf dirs × ceil(FILES_PER_DIR / BLOCK_ENTRIES_LOW) × size_of::<DirBlock>() + ((files + dirs) / FANOUT × FANOUT / (FANOUT − 1) + LEVELS) × size_of::<TrieNode>() + 67 × page + min(journal_bytes, (files + dirs) × (2 × size_of::<OpRecord>() + path))",
    [
      "struct sizes",
      "BLOCK_ENTRIES_LOW",
      "FILES_PER_DIR",
      "NAME_BYTES",
      "page",
      "journal_bytes"
    ]
  )
}

// ------------------------------------------------------------------ rows

/// The numbers behind the recorded cut-over: lookup and insert-and-remove in the inline form
/// at its capacity and in the tree at the probe sizes.
fn cutover_probe() -> usize {
  let policy = NameEquivalence::Fold;
  let mut blocks: Slab<DirBlock> = Slab::new(64, 1 << 16);
  let mut retired = Retired::new();
  for &n in &CUTOVER_SIZES {
    let names: Vec<String> = (0..n).map(file_name).collect();
    // Inline up to the cut-over; the tree from the first entry when the cut-over is one.
    for (label, cutover) in [("an inline", MEASURED_CUTOVER), ("a tree", 1)] {
      if cutover == MEASURED_CUTOVER && n > MEASURED_CUTOVER {
        continue;
      }
      let mut node = DirNode::new(Epoch(0), None, InodeNo::compose(1, 1), "probe");
      for (i, name) in names.iter().enumerate() {
        let child = Child::File(InodeNo::compose(1, u64::try_from(i).unwrap_or(0) + 3));
        let _ = node.insert(
          &mut blocks,
          Epoch(0),
          &mut retired,
          policy,
          name,
          child,
          cutover,
        );
      }
      let mut cursor = 0usize;
      report(
        &format!("lookup in {label} dir of {n}"),
        &measure(
          || {
            cursor = (cursor + 1) % n;
            std::hint::black_box(node.lookup(&blocks, policy, &names[cursor]));
          },
          BUDGET,
        ),
      );
      let probe = Child::File(InodeNo::compose(1, u64::MAX));
      report(
        &format!("insert and remove in {label} dir of {n}"),
        &measure(
          || {
            let _ = node.insert(
              &mut blocks,
              Epoch(0),
              &mut retired,
              policy,
              "probe",
              probe,
              cutover,
            );
            let _ = node.remove(
              &mut blocks,
              Epoch(0),
              &mut retired,
              policy,
              "probe",
              cutover,
            );
          },
          BUDGET,
        ),
      );
    }
  }
  println!(
    "cut-over: inline up to {MEASURED_CUTOVER} entries (recorded in dir.rs from this probe)"
  );
  MEASURED_CUTOVER
}

fn namespace_rows(tree: &mut Tree) {
  let leaf = tree.leaves[tree.leaves.len() / 2];
  let files = tree.files;
  let (store, vol) = (&mut tree.store, &mut tree.vol);
  report(
    &format!("create and unlink in a {files}-file tree"),
    &measure(
      || {
        if vol.create_file(store, leaf, "probe", 0o644).is_ok() {
          let _ = vol.unlink(store, leaf, "probe");
        }
      },
      BUDGET,
    ),
  );
  let name = file_name(7);
  report(
    &format!("lookup one name in a {FILES_PER_DIR}-entry dir of a {files}-file tree"),
    &measure(
      || {
        std::hint::black_box(vol.lookup(store, leaf, &name).ok());
      },
      BUDGET,
    ),
  );
  let path = format!(
    "/g{}/d{}/{}",
    (tree.leaves.len() / 2) % GROUPS.min(tree.leaves.len()),
    tree.leaves.len() / 2,
    name
  );
  report(
    &format!("resolve a three-component path in a {files}-file tree"),
    &measure(
      || {
        std::hint::black_box(vol.resolve(store, &path).ok());
      },
      BUDGET,
    ),
  );
  report(
    &format!("readdir of a {FILES_PER_DIR}-entry dir"),
    &measure(
      || {
        std::hint::black_box(vol.readdir(store, leaf).ok());
      },
      BUDGET,
    ),
  );
  let (from, to) = (file_name(3), "renamed".to_owned());
  report(
    "rename within a dir (there and back)",
    &measure(
      || {
        if vol.rename(store, leaf, &from, leaf, &to).is_ok() {
          let _ = vol.rename(store, leaf, &to, leaf, &from);
        }
      },
      BUDGET,
    ),
  );
}

fn content_rows(tree: &mut Tree) -> Result<(), Box<dyn Error>> {
  let leaf = tree.leaves[0];
  let (store, vol) = (&mut tree.store, &mut tree.vol);
  let f = vol.create_file(store, leaf, "content", 0o644)?;
  let window = vec![5u8; store.content.chunk_bytes()];
  vol.write(store, f, 0, &window)?;
  let page = vec![6u8; PAGE];
  report(
    "write 4 KiB in place (same epoch)",
    &measure(
      || {
        let _ = vol.write(store, f, 0, &page);
      },
      BUDGET,
    ),
  );
  let mut out = vec![0u8; PAGE];
  report(
    "read 4 KiB",
    &measure(
      || {
        std::hint::black_box(vol.read(store, f, 0, &mut out).ok());
      },
      BUDGET,
    ),
  );
  let far = u64::try_from(store.content.chunk_bytes())? * 3;
  report(
    "write 4 KiB into a fresh window then truncate it away",
    &measure(
      || {
        if vol.write(store, f, far, &page).is_ok() {
          let _ = vol.truncate(store, f, far);
        }
      },
      BUDGET,
    ),
  );
  Ok(())
}

/// Snapshot and clone cost at one tree size, returned for the AC-1.3 verdict.
fn snapshot_rows(tree: &mut Tree) -> Result<(Measurement, Measurement), Box<dyn Error>> {
  let files = tree.files;
  let (store, vol) = (&mut tree.store, &mut tree.vol);
  let snapshot = measure(
    || {
      if let Ok(s) = vol.snapshot(store) {
        let _ = vol.destroy_snapshot(store, s);
      }
    },
    BUDGET,
  );
  report(
    &format!("snapshot and destroy it at {files} files"),
    &snapshot,
  );
  let s = vol.snapshot(store)?;
  let clone = measure(
    || {
      if let Ok(mut c) = Volume::clone_of(store, vol, s, config(2)) {
        let _ = c.destroy(store);
        while let Ok(DestroyProgress::Released(_)) = c.destroy_step(store, u64::MAX) {}
        let _ = vol.unpin(s);
      }
    },
    BUDGET,
  );
  report(
    &format!("clone and destroy the clone at {files} files"),
    &clone,
  );
  vol.destroy_snapshot(store, s)?;
  Ok((snapshot, clone))
}

/// AC-1.3: the cost at the largest size is indistinguishable from the cost at the smallest.
/// A per-file term over three decades would show as at least a thousand timer reads; O(1)
/// noise is tens of nanoseconds. The allowance is the two measurements' own bootstrap-interval
/// widths plus one timer read, so a genuine per-file term fails while measurement jitter (a
/// lucky-fast small-size sample under a loaded machine) does not; the intervals overlapping is
/// independence too.
fn independent_of_size(what: &str, rows: &[(usize, Measurement)]) -> bool {
  let (Some((small, a)), Some((large, b))) = (rows.first(), rows.last()) else {
    return true;
  };
  let resolution = slates_machine::bench::timer_overhead_ns().max(1);
  let a_spread = a.interval.upper.saturating_sub(a.interval.lower);
  let b_spread = b.interval.upper.saturating_sub(b.interval.lower);
  let allowance = resolution.saturating_add(a_spread).saturating_add(b_spread);
  let growth = b.median_ns().saturating_sub(a.median_ns());
  let overlaps = b.interval.lower <= a.interval.upper;
  let ok = growth <= allowance || overlaps;
  println!(
    "ac-1.3 {what}: {} ns at {small} files, {} ns at {large} files, growth {growth} ns against the {allowance} ns noise allowance: {}",
    a.median_ns(),
    b.median_ns(),
    if ok {
      "independent of size"
    } else {
      "FAIL: grows with size"
    }
  );
  ok
}

/// Derived: the overshoot a slice may show past its budget: the volume checks its clock every
/// sixteen release units, so a slice can run on for sixteen units of the dearest kind after
/// the deadline; the allowance is sixteen times the dearest unit cost this run measured.
const CLOCK_CHECK_UNITS: u64 = 16;

/// Shape: how long the scheduling-jitter probe watches the clock, long enough to meet the
/// OS's timer tick a few times over.
const JITTER_PROBE: Duration = Duration::from_millis(100);

/// Measured in place: the longest gap between two consecutive clock reads over the probe
/// window, with no work between them. Anything that gap long can happen to any slice on this
/// machine and is not the volume's doing (the thread is not pinned here; the Phase 0 rt bench
/// measured timer lateness p99 at 108 µs on this machine).
fn scheduling_jitter_ns() -> u64 {
  let started = Instant::now();
  let mut last = started;
  let mut longest = 0u64;
  while started.elapsed() < JITTER_PROBE {
    let now = Instant::now();
    longest = longest.max(u64::try_from((now - last).as_nanos()).unwrap_or(u64::MAX));
    last = now;
  }
  longest
}

/// The process's involuntary context switches so far (`getrusage`): a slice during which the
/// count moved was preempted by the scheduler, and its length is not the volume's doing.
#[cfg(unix)]
fn involuntary_switches() -> u64 {
  let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
  // SAFETY: `getrusage` fills the `rusage` the pointer names and returns zero on success;
  // `RUSAGE_SELF` is a valid selector; the buffer is ours and fully sized.
  let usage = unsafe {
    if libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) != 0 {
      return 0;
    }
    usage.assume_init()
  };
  u64::try_from(usage.ru_nivcsw).unwrap_or(0)
}

/// Without the counter every slice counts.
#[cfg(not(unix))]
fn involuntary_switches() -> u64 {
  0
}

/// AC-1.8: destroy in slices under the shard's step budget, sliced by the volume's own clock;
/// no slice may run past the budget by more than the clock-check allowance plus the machine's
/// measured scheduling jitter, unless the scheduler preempted it (the process's involuntary
/// context-switch count moved during the slice). The time inside the allocator's `dealloc` is
/// attributed per slice too, so a stall from the allocator returning memory is told apart from
/// the volume's own work.
fn destroy_rows(mut tree: Tree, step_budget_ns: u64) -> Result<bool, Box<dyn Error>> {
  let files = tree.files;
  let jitter = scheduling_jitter_ns();
  let (store, vol) = (&mut tree.store, &mut tree.vol);
  vol.destroy(store)?;
  let mut slices: Vec<(u64, u64, usize)> = Vec::new();
  let mut preempted: Vec<u64> = Vec::new();
  let mut released = 0usize;
  let total = Instant::now();
  loop {
    let dealloc_before = DEALLOC_NS.load(Ordering::Relaxed);
    let switches_before = involuntary_switches();
    let started = Instant::now();
    let progress = vol.destroy_step(store, step_budget_ns)?;
    let took = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let in_dealloc = DEALLOC_NS.load(Ordering::Relaxed) - dealloc_before;
    let was_preempted = involuntary_switches() > switches_before;
    match progress {
      DestroyProgress::Released(n) => {
        released += n;
        if was_preempted {
          preempted.push(took);
        } else {
          slices.push((took, in_dealloc, n));
        }
      }
      DestroyProgress::Done => break,
    }
  }
  // The per-unit cost is the volume's own: the slices' time, not the wall time around them
  // (which holds the bench's bookkeeping, two `getrusage` calls per slice).
  let elapsed: u64 = slices.iter().map(|s| s.0).sum::<u64>() + preempted.iter().sum::<u64>();
  let _ = total;
  slices.sort_unstable();
  let at = |q: usize| slices.get(slices.len() * q / 100).map_or(0, |s| s.0);
  let (longest, longest_dealloc, longest_units) = slices.last().copied().unwrap_or((0, 0, 0));
  let dearest_unit = slices
    .iter()
    .map(|(took, _, n)| took / u64::try_from(*n).unwrap_or(1).max(1))
    .max()
    .unwrap_or(0);
  let allowance = CLOCK_CHECK_UNITS * dearest_unit;
  let over = slices
    .iter()
    .filter(|s| s.0 > step_budget_ns + allowance + jitter)
    .count();
  // Per unit under slicing: follows the profile's step budget (more slices, more clock reads
  // at their starts), so it is printed, not gated; the one-slice row below is the gate.
  report_info_value(
    &format!("destroy per unit at {files} files"),
    "ns",
    elapsed / u64::try_from(released).unwrap_or(1).max(1),
  );
  report_info_value(&format!("destroy slice p99 at {files} files"), "ns", at(99));
  report_info_value(
    &format!("destroy longest slice at {files} files"),
    "ns",
    longest,
  );
  let ok = over == 0;
  println!(
    "ac-1.8: {released} units in {} slices under a {step_budget_ns} ns budget; slice p50 {} ns, p99 {} ns, longest {longest} ns ({longest_units} units, {longest_dealloc} ns inside dealloc); dearest unit {dearest_unit} ns so the clock-check allowance is {allowance} ns; scheduling jitter measured {jitter} ns; {} slices preempted by the scheduler (longest {} ns) and not judged; {over} slices past budget, allowance and jitter: {}",
    slices.len() + preempted.len(),
    at(50),
    at(99),
    preempted.len(),
    preempted.iter().max().copied().unwrap_or(0),
    if ok {
      "within budget"
    } else {
      "FAIL: over budget"
    }
  );
  Ok(ok)
}

/// The pure per-unit cost of a destroy: a fresh 10^6-file tree released in one slice (an
/// unbounded budget), so no slicing overhead is in the number; the gated row.
fn one_slice_destroy_row(cutover: usize) -> Result<(), Box<dyn Error>> {
  let mut tree = build(SIZES[SIZES.len() - 1], cutover)?;
  let files = tree.files;
  let (store, vol) = (&mut tree.store, &mut tree.vol);
  vol.destroy(store)?;
  let started = Instant::now();
  let mut released = 0usize;
  while let DestroyProgress::Released(n) = vol.destroy_step(store, u64::MAX)? {
    released += n;
  }
  let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
  report_value(
    &format!("destroy per unit in one slice at {files} files"),
    "ns",
    elapsed / u64::try_from(released).unwrap_or(1).max(1),
  );
  Ok(())
}

/// T-1.7: the 190k-file create burst, best of N with all N shown, and its heap per file.
fn burst_rows(cutover: usize) -> Result<(), Box<dyn Error>> {
  let mut runs = Vec::with_capacity(BURST_RUNS);
  let mut heap_per_file = 0u64;
  for _ in 0..BURST_RUNS {
    let started = Instant::now();
    let tree = build(BURST_FILES, cutover)?;
    let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    runs.push(elapsed / u64::try_from(BURST_FILES).unwrap_or(1));
    heap_per_file = u64::try_from(tree.heap / BURST_FILES).unwrap_or(u64::MAX);
    drop(tree);
  }
  report_runs(
    &format!("create burst of {BURST_FILES} files per file"),
    "ns per file",
    runs,
  );
  report_value(
    &format!("create burst of {BURST_FILES} files heap per file"),
    "bytes",
    heap_per_file,
  );
  Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
  let profile = slates_machine::MachineProfile::measure(slates_machine::ProfileOptions {
    budget_per_probe: Duration::from_millis(50),
    codecs: false,
    core_matrix: false,
  });
  let step_budget_ns = profile.derived().task_step_budget_ns.get();
  println!("task step budget: {step_budget_ns} ns (from the machine profile)");
  println!(
    "sizes: Inode {} B, DirNode {} B, DirBlock {} B, TrieNode {} B",
    size_of::<Inode>(),
    size_of::<DirNode>(),
    size_of::<DirBlock>(),
    size_of::<TrieNode>()
  );

  let cutover = cutover_probe();

  let mut snapshots = Vec::new();
  let mut clones = Vec::new();
  let mut memory_ok = true;
  let mut last: Option<Tree> = None;
  for &files in &SIZES {
    let started = Instant::now();
    let mut tree = build(files, cutover)?;
    println!(
      "built {files} files in {} ms, {} heap bytes",
      started.elapsed().as_millis(),
      tree.heap
    );
    let per_file = u64::try_from(tree.heap / files).unwrap_or(u64::MAX);
    report_value(
      &format!("heap bytes per file at {files} files"),
      "bytes",
      per_file,
    );
    let budget = heap_budget(files, tree.dirs, PAGE, JOURNAL_BYTES);
    let ok = tree.heap <= budget.get();
    memory_ok &= ok;
    println!(
      "ac-1.5 at {files} files and {} dirs: {} bytes against the derived budget {} ({} per file; {}): {}",
      tree.dirs,
      tree.heap,
      budget.get(),
      budget.get() / files,
      budget.formula,
      if ok {
        "within budget"
      } else {
        "FAIL: over budget"
      }
    );
    if files == SIZES[0] {
      namespace_rows(&mut tree);
      content_rows(&mut tree)?;
    }
    if files == SIZES[1] {
      namespace_rows(&mut tree);
    }
    let (s, c) = snapshot_rows(&mut tree)?;
    snapshots.push((files, s));
    clones.push((files, c));
    last = Some(tree);
  }
  let snapshot_ok = independent_of_size("snapshot", &snapshots);
  let clone_ok = independent_of_size("clone", &clones);
  let destroy_ok = match last {
    Some(tree) => destroy_rows(tree, step_budget_ns)?,
    None => true,
  };
  one_slice_destroy_row(cutover)?;
  burst_rows(cutover)?;
  if snapshot_ok && clone_ok && destroy_ok && memory_ok {
    Ok(())
  } else {
    Err("an acceptance criterion failed; see the ac- lines above".into())
  }
}
