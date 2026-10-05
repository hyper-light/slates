//! Heap bytes per entry of real trees in a volume (§4.2 memory per file, AC-1.5's real-world counterpart): the paths an
//! `npm install express lodash typescript` and a pip venv of requests and flask leave (captured 2026-10-05 in
//! node:20-alpine and python:3.12-alpine), and this workspace's `target/release` after a release build (a cargo
//! tree: mostly 4-entry fingerprint directories and a few large ones, `deps` 1,049), in `data/*-tree.txt`: `d PATH` for a directory, `f PATH` for anything else),
//! built into a fresh volume through `mkdir_no` and `create_file_no`, with every heap byte counted by the allocator.
//! The bench's own tree has 36-entry directories of 49-byte names; real directories are mostly 3 to 56 entries with a
//! median of 4 to 6, which is where a directory block's fixed size shows.
//!
//! `cargo run --release -p slates-vfs --example tree_heap`

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::error::Error;
use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};

use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::dir::{DirNode, MEASURED_CUTOVER};
use slates_vfs::dirtree::DirBlock;
use slates_vfs::ids::InodeNo;
use slates_vfs::inode::Inode;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::trie::TrieNode;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Shape: the base page the store is cut in.
const PAGE: usize = 4096;
/// Shape: the cache line the store's inline threshold follows.
const CACHE_LINE: usize = 128;
/// Shape: the content region's pages (the trees here hold no content).
const REGION_PAGES: usize = 64;
/// Shape: the op log's byte budget, as the bench gives.
const JOURNAL_BYTES: usize = 1 << 16;
/// Shape: copies of each tree built side by side, so the slabs' segment slack is amortized as on a host serving many
/// checkouts (one tree alone is a few hundred blocks, under three of the block slab's 64-page segments).
const COPIES: usize = 64;
/// Shape: slab caps far above the trees' sizes.
const CAP: usize = 1 << 20;
/// Shape: the permission bits every created entry gets.
const MODE: u32 = 0o755;

/// Live heap bytes (a statistic, `Relaxed`, in a bench binary).
static LIVE_HEAP: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every call forwards to the system allocator unchanged; the counter does not affect what is returned.
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    LIVE_HEAP.fetch_add(layout.size(), Ordering::Relaxed);
    // SAFETY: the layout is the caller's, forwarded as is.
    unsafe { System.alloc(layout) }
  }

  unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
    LIVE_HEAP.fetch_sub(layout.size(), Ordering::Relaxed);
    // SAFETY: `ptr` came from `alloc` with this layout.
    unsafe { System.dealloc(ptr, layout) }
  }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Builds the tree listed in `listing` and prints its heap per entry.
fn measure(label: &str, listing: &str) -> Result<(), Box<dyn Error>> {
  let mut arena = ChunkArena::new(PAGE);
  arena.add_region(Region::map(PAGE * REGION_PAGES, PAGE, false)?)?;
  let mut store = Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: CACHE_LINE,
      max_dirs: CAP,
      max_inodes: CAP,
      max_chunks: REGION_PAGES,
      max_dir_blocks: CAP,
      dir_cutover: MEASURED_CUTOVER,
    },
    arena,
    0,
  );
  let mut vol = Volume::create(
    &mut store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Fold,
      quota: Quota::Bounded { limit: 1 << 40 },
      journal_bytes: JOURNAL_BYTES,
      clock: Box::new(HostClock::default()),
    },
  )?;
  let root = vol.root_inode(&store)?;
  let before = LIVE_HEAP.load(Ordering::Relaxed);
  let mut dirs: BTreeMap<String, InodeNo> = BTreeMap::new();
  let (mut dir_count, mut file_count, mut name_bytes) = (0usize, 0usize, 0usize);
  for copy in 0..COPIES {
    let top = vol.mkdir_no(&mut store, root, &format!("copy{copy}"), MODE)?;
    dirs.clear();
    build_one(
      &mut vol,
      &mut store,
      top,
      listing,
      &mut dirs,
      (&mut dir_count, &mut file_count, &mut name_bytes),
    )?;
  }
  report(label, before, (dir_count, file_count, name_bytes), dirs)
}

/// One copy of the tree under `root`.
fn build_one(
  vol: &mut Volume,
  store: &mut Store,
  root: InodeNo,
  listing: &str,
  dirs: &mut BTreeMap<String, InodeNo>,
  (dir_count, file_count, name_bytes): (&mut usize, &mut usize, &mut usize),
) -> Result<(), Box<dyn Error>> {
  for line in listing.lines() {
    let Some((kind, path)) = line.split_once(' ') else {
      continue;
    };
    let (parent, name) = match path.rsplit_once('/') {
      Some((parent, name)) => (
        dirs
          .get(parent)
          .copied()
          .ok_or("a parent before its child")?,
        name,
      ),
      None => (root, path),
    };
    *name_bytes += name.len();
    if kind == "d" {
      let no = vol.mkdir_no(store, parent, name, MODE)?;
      dirs.insert(path.to_owned(), no);
      *dir_count += 1;
    } else {
      vol.create_file_no(store, parent, name, MODE)?;
      *file_count += 1;
    }
  }
  Ok(())
}

/// Prints the heap per entry the trees took since `before`.
fn report(
  label: &str,
  before: usize,
  (dir_count, file_count, name_bytes): (usize, usize, usize),
  dirs: BTreeMap<String, InodeNo>,
) -> Result<(), Box<dyn Error>> {
  // The listing's own map is the harness's, not the volume's: measured after it is dropped.
  let entries = dir_count + file_count;
  drop(dirs);
  let heap = LIVE_HEAP.load(Ordering::Relaxed).saturating_sub(before);
  println!(
    "{label} ×{COPIES}: {entries} entries ({dir_count} dirs, {file_count} files, mean name {} B): {heap} heap bytes, {} per entry \
     (sizes: Inode {} B, DirNode {} B, DirBlock {} B, TrieNode {} B)",
    name_bytes / entries.max(1),
    heap / entries.max(1),
    size_of::<Inode>(),
    size_of::<DirNode>(),
    size_of::<DirBlock>(),
    size_of::<TrieNode>()
  );
  Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
  measure("npm", include_str!("data/npm-tree.txt"))?;
  measure("pip", include_str!("data/pip-tree.txt"))?;
  measure("cargo", include_str!("data/cargo-tree.txt"))?;
  Ok(())
}
