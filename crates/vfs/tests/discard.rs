//! Discarding a freshly-created volume returns its owned slab slots (§4.2 create-failure residue,
//! docs/bugs/2026-09-06-partial-volume-slab-leak-on-create-failure.md). The test drives create then
//! discard far more times than the small inode-version slab could hold if a fresh volume's slots
//! (its root inode, trie nodes and root dir) leaked on each discard — so every create must succeed.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::StepClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// The page the test store uses.
const PAGE: usize = 4096;

/// A store whose inode-version, trie and directory slabs hold only `max_inodes` slots each, so a
/// leaked fresh volume shows up within a couple of iterations rather than after tens of thousands.
fn small_store(max_inodes: usize) -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * 256, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 64,
      max_dirs: max_inodes,
      max_inodes,
      max_chunks: 1 << 12,
      max_dir_blocks: max_inodes,
      dir_cutover: 4,
    },
    arena,
    0,
  )
}

fn config() -> VolumeConfig {
  VolumeConfig {
    prefix: 1,
    names: NameEquivalence::Exact,
    quota: Quota::Bounded { limit: 1 << 20 },
    journal_bytes: 1 << 16,
    clock: Box::new(StepClock::new(0, 1)),
  }
}

/// T-A9: a never-served volume, discarded, returns every slab slot it took, so create-then-discard
/// runs unboundedly on a tiny slab. Non-vacuous — dropping the volume instead of discarding it
/// exhausts a 32-slot slab within a couple of iterations (a fresh volume takes a dozen-odd slots).
#[test]
fn discarding_a_fresh_volume_returns_its_slab_slots() {
  let mut store = small_store(32);
  for i in 0..200 {
    let volume = Volume::create(&mut store, config()).unwrap_or_else(|e| {
      panic!("iteration {i}: create failed — a discard leaked slab slots: {e:?}")
    });
    volume.discard_partial(&mut store).unwrap();
  }
}
