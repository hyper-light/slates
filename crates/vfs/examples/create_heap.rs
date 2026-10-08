//! What one file costs the heap, phase by phase, as the daemon makes it through the macOS mount (§4.2 memory per
//! file): create it in a directory of 1,000, write its 100 bytes, set its 60-byte provenance attribute (every file
//! macOS creates carries `com.apple.provenance`), then publish as the daemon's barrier does — a delta, or a checkpoint
//! streamed into its slot when the journal asks for one. Every allocation, reallocation and live byte is
//! counted by the allocator and charged to the phase that made it, so each byte a file keeps and each allocation a
//! create makes has an owner. The daemon measured 139 MB after 50,000 such files where the volume alone needs 47
//! (2026-10-06, `mem-50k.sh`).
//!
//! `cargo run --release -p slates-vfs --example create_heap [FILES]` prints, per phase: live bytes kept per file,
//! allocations and reallocations per file. With `CREATE_HEAP_SIZES=PATH` (one file size per line, e.g. a real tree's
//! `find -type f -printf '%s\n'`) it makes one file per line instead, writes each in NFS-sized pieces
//! ([`WRITE_PIECE`]) and sets no attribute, so the heap a real tree's content costs is charged to the write phase.

// A benchmark harness: an unwrap here is a failed run, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::checkpoint_log::{Journal, ShardDelta};
use slates_vfs::clock::StepClock;
use slates_vfs::delta::VolumeRecord;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::recover::{ImageOut, ShardImage};
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};
use slates_vfs::xattr::XattrSet;

/// Shape: the files made by default (the daemon measurement's count).
const FILES: usize = 50_000;
/// Shape: files per directory, as the measured run made them.
const PER_DIR: usize = 1_000;
/// Shape: each file's bytes, as the measured run wrote them.
const FILE_BYTES: usize = 100;
/// Shape: the provenance attribute's value bytes (what macOS writes is about this size).
const ATTRIBUTE_BYTES: usize = 60;
/// Format: the attribute every file macOS creates carries.
const PROVENANCE: &[u8] = b"com.apple.provenance";
/// Format: the piece a sized file is written in: the Linux NFS client's write size against this server
/// (`procedures::MAX_TRANSFER`, 256 KiB).
const WRITE_PIECE: usize = 256 * 1024;
/// Format: the page size (the store's granule).
const PAGE: usize = 4096;
/// Shape: the RAM region the store maps (address space; only what is written is touched).
const REGION_BYTES: usize = 1 << 31;
/// Shape: slab caps above the run's objects.
const CAP: usize = 1 << 19;
/// Shape: the volume's quota.
const QUOTA: u64 = 1 << 31;
/// Shape: the op log's byte budget: a 1 GiB volume's share in the daemon (`JOURNAL_SHARE_PERMILLE` 10).
const JOURNAL_BYTES: usize = 10 << 20;
/// Format: the cache line.
const CACHE_LINE: usize = 64;
/// Shape: the small-directory cut-over, the store's measured default range.
const DIR_CUTOVER: usize = 32;
/// Shape: the checkpoint memory and the delta log (address space for the run's largest image and its deltas).
const SLOTS_BYTES: usize = 1 << 27;
/// See [`SLOTS_BYTES`].
const LOG_BYTES: usize = 1 << 26;

/// Live heap bytes, allocations and reallocations (statistics, `Relaxed`, in a bench binary).
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// See [`LIVE`].
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
/// See [`LIVE`].
static REALLOCS: AtomicUsize = AtomicUsize::new(0);

std::thread_local! {
  /// Whether allocations are being traced now (one create of a traced run), and whether a trace is being taken (the
  /// trace's own allocations are not traced).
  static TRACING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
  static IN_TRACE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
  static TRACES: std::cell::RefCell<Vec<(&'static str, usize, String)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Records where an allocation of `size` bytes came from, when tracing: the slates frames of a backtrace.
fn trace(kind: &'static str, size: usize) {
  if !TRACING.with(std::cell::Cell::get) || IN_TRACE.with(std::cell::Cell::get) {
    return;
  }
  IN_TRACE.with(|flag| flag.set(true));
  let backtrace = std::backtrace::Backtrace::force_capture().to_string();
  let frames: Vec<String> = backtrace
    .lines()
    .filter(|line| line.contains("slates_vfs::") && !line.contains("create_heap"))
    .map(|line| {
      line
        .trim()
        .trim_start_matches(|c: char| c.is_ascii_digit() || c == ':')
        .trim()
        .to_owned()
    })
    .take(4)
    .collect();
  TRACES.with(|traces| traces.borrow_mut().push((kind, size, frames.join(" <- "))));
  IN_TRACE.with(|flag| flag.set(false));
}

struct Counting;

// SAFETY: every call forwards to the system allocator unchanged; the counters do not affect what is returned.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    LIVE.fetch_add(layout.size(), Ordering::Relaxed);
    ALLOCS.fetch_add(1, Ordering::Relaxed);
    trace("alloc", layout.size());
    // SAFETY: the layout is the caller's, forwarded as is.
    unsafe { System.alloc(layout) }
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    // SAFETY: `ptr` came from `alloc` or `realloc` with this layout.
    unsafe { System.dealloc(ptr, layout) }
  }

  unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
    LIVE.fetch_add(new_size, Ordering::Relaxed);
    LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    REALLOCS.fetch_add(1, Ordering::Relaxed);
    trace("realloc", new_size);
    // SAFETY: `ptr` came from this allocator with `layout`, forwarded as is.
    unsafe { System.realloc(ptr, layout, new_size) }
  }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// One phase's running totals: live bytes it left (signed), allocations and reallocations it made.
#[derive(Default, Clone, Copy)]
struct Phase {
  live: isize,
  allocs: usize,
  reallocs: usize,
}

/// Runs `f`, charging what it allocated to `phase`.
fn charge<R>(phase: &mut Phase, f: impl FnOnce() -> R) -> R {
  let (live, allocs, reallocs) = (
    LIVE.load(Ordering::Relaxed),
    ALLOCS.load(Ordering::Relaxed),
    REALLOCS.load(Ordering::Relaxed),
  );
  let out = f();
  phase.live += LIVE.load(Ordering::Relaxed).cast_signed() - live.cast_signed();
  phase.allocs += ALLOCS.load(Ordering::Relaxed) - allocs;
  phase.reallocs += REALLOCS.load(Ordering::Relaxed) - reallocs;
  out
}

fn main() {
  let sizes: Option<Vec<usize>> = std::env::var("CREATE_HEAP_SIZES").ok().map(|path| {
    std::fs::read_to_string(path)
      .unwrap()
      .lines()
      .filter_map(|line| line.trim().parse().ok())
      .collect()
  });
  let files: usize = sizes.as_ref().map_or_else(
    || {
      std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(FILES)
    },
    Vec::len,
  );
  let piece = vec![b'x'; WRITE_PIECE];
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(REGION_BYTES, PAGE, false).unwrap())
    .unwrap();
  let mut store = Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: CACHE_LINE,
      max_dirs: CAP,
      max_inodes: CAP,
      max_chunks: CAP,
      max_dir_blocks: CAP,
      dir_cutover: DIR_CUTOVER,
    },
    arena,
    0,
  );
  let mut vol = Volume::create(
    &mut store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Fold,
      quota: Quota::Bounded { limit: QUOTA },
      journal_bytes: JOURNAL_BYTES,
      clock: Box::new(StepClock::new(0, 1)),
    },
  )
  .unwrap();
  let root = vol.root_inode(&store).unwrap();
  let mut journal = Journal::default();
  let mut slots = vec![0u8; SLOTS_BYTES];
  let mut log = vec![0u8; LOG_BYTES];
  let mut scratch: Vec<u8> = Vec::new();
  let mut buffer: Vec<u8> = Vec::new();
  let data = vec![b'x'; FILE_BYTES];
  let attribute = vec![7u8; ATTRIBUTE_BYTES];
  let names: Vec<String> = (0..PER_DIR).map(|f| format!("f{f:04}")).collect();
  let mut marked = Phase::default();
  let retained_from = LIVE.load(Ordering::Relaxed);
  let (mut mkdir, mut create, mut write, mut xattr, mut delta, mut checkpoint) = (
    Phase::default(),
    Phase::default(),
    Phase::default(),
    Phase::default(),
    Phase::default(),
    Phase::default(),
  );
  let mut checkpoints = 0usize;
  let mut dir = root;
  for file in 0..files {
    if file % PER_DIR == 0 {
      let name = format!("d{:04}", file / PER_DIR);
      dir = charge(&mut mkdir, || {
        vol.mkdir_no(&mut store, root, &name, 0o755).unwrap()
      });
    }
    let name = &names[file % PER_DIR];
    let traced = std::env::var("CREATE_HEAP_TRACE")
      .ok()
      .filter(|_| file == PER_DIR + 500);
    if traced.is_some() {
      TRACING.with(|flag| flag.set(true));
    }
    let no = charge(&mut create, || {
      vol.create_file_no(&mut store, dir, name, 0o644).unwrap()
    });
    match sizes.as_ref().and_then(|sizes| sizes.get(file)) {
      Some(&size) => charge(&mut write, || {
        let mut offset = 0;
        while offset < size {
          let len = (size - offset).min(WRITE_PIECE);
          vol
            .write(&mut store, no, offset as u64, &piece[..len])
            .unwrap();
          offset += len;
        }
      }),
      None => {
        charge(&mut write, || vol.write(&mut store, no, 0, &data).unwrap());
        charge(&mut xattr, || {
          vol
            .xattr_set(&mut store, no, PROVENANCE, &attribute, XattrSet::Either)
            .unwrap()
        });
      }
    }
    // The daemon's delta: each volume's publication streamed into the kept scratch, then logged; a full record means
    // the volume asks for a checkpoint instead (the probe's one volume publishes whole only when the journal does).
    let appended = !journal.wants_checkpoint()
      && charge(&mut delta, || {
        scratch.clear();
        let at = ShardDelta::encode_start(&mut scratch);
        scratch.extend_from_slice(&[1; 16]);
        let tag_at = scratch.len();
        vol.encode_publication(&store, None, &mut scratch).unwrap();
        let whole =
          scratch.get(tag_at..tag_at + 4) == Some(&VolumeRecord::WIRE_TAG_FULL.to_le_bytes()[..]);
        ShardDelta::encode_finish(&mut scratch, at, 1, &mut [], None, None).unwrap();
        !whole && journal.append_encoded(&mut log, &scratch).is_ok()
      });
    if !appended {
      checkpoints += 1;
      charge(&mut checkpoint, || {
        let mut stream = journal.begin_checkpoint(&mut slots, &mut buffer).unwrap();
        ShardImage::encode_start(stream.buf(), 1);
        stream.buf().extend_from_slice(&[1; 16]);
        vol.encode_image_into(&store, None, &mut stream).unwrap();
        ShardImage::encode_finish(stream.buf(), &[], &mut Vec::new());
        journal.finish_checkpoint(stream).unwrap();
      });
    }
    charge(&mut marked, || vol.mark_published(&store));
    if traced.is_some() {
      TRACING.with(|flag| flag.set(false));
    }
  }
  TRACES.with(|traces| {
    for (kind, size, frames) in traces.borrow().iter() {
      println!("{kind} {size}: {frames}");
    }
  });
  let per = |phase: Phase| {
    (
      phase.live as f64 / files as f64,
      phase.allocs as f64 / files as f64,
      phase.reallocs as f64 / files as f64,
    )
  };
  println!("{files} files, {checkpoints} checkpoints");
  println!("phase,live_bytes_per_file,allocs_per_file,reallocs_per_file");
  let mut total = Phase::default();
  for (label, phase) in [
    ("mkdir (1 per 1000 files)", mkdir),
    ("create", create),
    ("write 100 B", write),
    ("xattr provenance", xattr),
    ("publish delta", delta),
    ("checkpoint (streamed)", checkpoint),
    ("mark published", marked),
  ] {
    let (live, allocs, reallocs) = per(phase);
    println!("{label},{live:.1},{allocs:.2},{reallocs:.3}");
    total.live += phase.live;
    total.allocs += phase.allocs;
    total.reallocs += phase.reallocs;
  }
  let (live, allocs, reallocs) = per(total);
  println!("total,{live:.1},{allocs:.2},{reallocs:.3}");
  println!(
    "retained heap per file at the end (the volume, its op log, the publish stage): {:.1} bytes",
    (LIVE.load(Ordering::Relaxed) - retained_from) as f64 / files as f64
  );
}
