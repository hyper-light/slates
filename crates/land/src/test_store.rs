//! The store and the volume configuration this crate's unit tests land over (`captured_tests`, `chmod_tests`): one place
//! for the fixture's sizes, each a shape of the tests rather than a tuning (R3), so the suites build the same fixture and
//! a size is reviewed once; and the host refusals of every kind the seam names, which those suites arm.

use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::StepClock;
use slates_vfs::host::{HostError, OsCode};
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, VolumeConfig};

/// Shape: the test store's page.
const PAGE: usize = 4096;
/// Shape: pages of content the test store maps (4 MiB).
const REGION_PAGES: usize = 1024;
/// Shape: slots per slab in the test store, far above what any test makes.
const CAP: usize = 1 << 14;
/// Shape: the line the store pads its shared words to: 64 bytes, the cache line of every machine the crate builds for.
const CACHE_LINE: usize = 64;
/// Shape: the entries a directory holds before it becomes a tree of blocks, small so a test's directories cross it.
const DIR_CUTOVER: usize = 4;
/// Shape: the volume's quota (16 MiB), above anything a test writes.
const QUOTA_BYTES: u64 = 1 << 24;
/// Shape: the volume's journal (64 KiB).
const JOURNAL_BYTES: usize = 1 << 16;
/// Shape: the volume clock's first reading, in nanoseconds, so a test reads exact times.
const CLOCK_START_NS: u64 = 1_000;
/// Shape: the volume clock's step a read, in nanoseconds.
const CLOCK_STEP_NS: u64 = 1;

/// Shape: a host refusal of each kind the seam names (A-539), by the number its platform gives it: a name that exists
/// (`EEXIST`, 17), no room (`ENOSPC`, 28), a verb the filesystem lacks (macOS's `EOPNOTSUPP`, 102, which its `open`
/// answers for a socket), anything else (`EIO`, 5), and a Win32 code (`ERROR_ALREADY_EXISTS`, 183) beside them. Where the
/// landing tells none of these apart, it must read each alike; it never compares the number.
pub(crate) const REFUSALS_OF_EVERY_KIND: [HostError; 5] = [
  HostError::Exists(OsCode::Errno(17)),
  HostError::NoSpace(OsCode::Errno(28)),
  HostError::Unsupported(OsCode::Errno(102)),
  HostError::Unavailable(OsCode::Errno(5)),
  HostError::Exists(OsCode::Win32(183)),
];

/// A store over one mapped region of `REGION_PAGES` pages.
pub(crate) fn store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
    .unwrap();
  Store::new(
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
  )
}

/// A volume's configuration under the inode-number `prefix`.
pub(crate) fn config(prefix: u16) -> VolumeConfig {
  VolumeConfig {
    prefix,
    names: NameEquivalence::Exact,
    quota: Quota::Bounded { limit: QUOTA_BYTES },
    journal_bytes: JOURNAL_BYTES,
    clock: Box::new(StepClock::new(CLOCK_START_NS, CLOCK_STEP_NS)),
  }
}
