//! THROWAWAY PROBE, never committed: what a whole-base pin over the real host holds per file.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing, missing_docs)]
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use slates_base::OsHost;
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::base::BaseConfig;
use slates_vfs::clock::StepClock;
use slates_vfs::host::HostFs;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

struct Counting;
static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicIsize = AtomicIsize::new(0);
fn note(d: isize) { let v = LIVE.fetch_add(d, Ordering::Relaxed) + d; PEAK.fetch_max(v, Ordering::Relaxed); }
unsafe impl GlobalAlloc for Counting {
  unsafe fn alloc(&self, l: Layout) -> *mut u8 { note(l.size() as isize); unsafe { System.alloc(l) } }
  unsafe fn dealloc(&self, p: *mut u8, l: Layout) { note(-(l.size() as isize)); unsafe { System.dealloc(p, l) } }
  unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 { note(n as isize - l.size() as isize); unsafe { System.realloc(p, l, n) } }
}
#[global_allocator]
static A: Counting = Counting;

#[test]
fn probe() {
  let tree = std::env::var("PIN_PROBE_TREE").unwrap();
  let page = 4096;
  let mut arena = ChunkArena::new(page);
  arena.add_region(Region::map(page * 65536 * 4, page, false).unwrap()).unwrap();
  let mut store = Store::new(&StoreConfig { page, cache_line: 64, max_dirs: 1 << 18, max_inodes: 1 << 19, max_chunks: 1 << 19, max_dir_blocks: 1 << 18, dir_cutover: 4 }, arena, 0);
  let (mut host, root) = OsHost::open_root(std::path::Path::new(&tree)).unwrap();
  let facts = host.facts(root).unwrap();
  let mut vol = Volume::create_overlay(&mut store, VolumeConfig { prefix: 7, names: NameEquivalence::Exact, quota: Quota::Bounded { limit: 1 << 32 }, journal_bytes: 1 << 20, clock: Box::new(StepClock::new(1_000_000, 1_000)) }, BaseConfig { root, facts, large_class_bytes: 65_536 }).unwrap();
  let before = LIVE.load(Ordering::Relaxed);
  PEAK.store(before, Ordering::Relaxed);
  let t = std::time::Instant::now();
  let pinned = vol.with_host(&mut host).pin(&mut store, None).unwrap();
  let took = t.elapsed();
  let after = LIVE.load(Ordering::Relaxed);
  println!("PROBE tree {tree} pinned {pinned} took {took:?} ({:?}/file) heap_delta {} ({} B/file) heap_peak_delta {} open_handles {} inodes_used {} content_alloc {}", took / pinned.max(1) as u32, after - before, (after - before) / pinned.max(1) as isize, PEAK.load(Ordering::Relaxed) - before, host.open_handles(), store.inodes.capacity() - store.inodes.room(), store.content.allocated_bytes());
}
