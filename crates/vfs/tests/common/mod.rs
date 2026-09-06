//! Shared fixtures of the volume-core tests: a store over one RAM region and volumes with the
//! policies the tests need. Every test crate includes this module with `mod common;`.

#![allow(dead_code)]

pub(crate) mod drive;
pub(crate) mod steps;

use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::StepClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::{Ceiling, Quota};
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Format: the page size the tests use everywhere (the store's granule).
pub(crate) const PAGE: usize = 4096;

/// Shape: the number of pages the test region holds (16 MiB of content).
pub(crate) const REGION_PAGES: usize = 4096;

/// A store with the default caps: 65,536 directories, inodes and chunks, cut-over at 4 entries
/// so both directory representations are exercised by small trees.
pub(crate) fn store() -> Store {
  store_with(1 << 16, 4)
}

/// A store with a chosen directory-slab cap and cut-over.
pub(crate) fn store_with(max_dirs: usize, dir_cutover: usize) -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page: PAGE,
      cache_line: 64,
      max_dirs,
      max_inodes: 1 << 16,
      max_chunks: 1 << 16,
      max_dir_blocks: 1 << 16,
      dir_cutover,
    },
    arena,
    0, // no operation headroom in the fixtures, so a volume's quota is the whole region
  )
}

/// A bounded, name-folding volume with prefix 7 and a stepping clock.
pub(crate) fn volume(store: &mut Store, quota: u64) -> Volume {
  volume_with(
    store,
    Quota::Bounded { limit: quota },
    NameEquivalence::Fold,
  )
}

/// A dynamic volume whose pressure source grants up to `source_limit` bytes.
pub(crate) fn dynamic_volume(store: &mut Store, max: u64, source_limit: u64) -> Volume {
  volume_with(
    store,
    Quota::Dynamic {
      max,
      source: Box::new(Ceiling {
        granted: 0,
        limit: source_limit,
      }),
      granted: 0,
      denied: 0,
    },
    NameEquivalence::Fold,
  )
}

/// A volume with the given quota and policy.
pub(crate) fn volume_with(store: &mut Store, quota: Quota, names: NameEquivalence) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 7,
      names,
      quota,
      journal_bytes: 1 << 20,
      clock: Box::new(StepClock::new(0, 1_000)),
    },
  )
  .unwrap()
}

/// A clone's configuration: folding names, a 16 MiB bounded quota, the given prefix.
pub(crate) fn clone_config(prefix: u16) -> VolumeConfig {
  VolumeConfig {
    prefix,
    names: NameEquivalence::Fold,
    quota: Quota::Bounded { limit: 1 << 24 },
    journal_bytes: 1 << 16,
    clock: Box::new(StepClock::new(0, 1)),
  }
}
