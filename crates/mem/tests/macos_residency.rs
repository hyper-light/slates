//! What macOS reports of a content arena's shared region as it is written, freed and purged (goal 93 P7, main's ruling
//! of 2026-11-25: `mem.gauge.content.object_resident_bytes` equalled `arena_mapped_bytes` to the byte in every settled
//! reading, and the process's resident set did not fall with the purge). Three readings at each step: the arena's own
//! (`mincore`, what the gauge reads), and the process's `resident_size` and `phys_footprint` (`proc_pid_rusage`). Its
//! own binary, so the process's figures are this test's alone. `#[ignore]`d: a measurement, run by name.
#![cfg(target_os = "macos")]
#![allow(
  clippy::unwrap_used,
  clippy::print_stdout,
  clippy::cast_possible_truncation
)]

use slates_mem::arena::{ChunkArena, Owner};
use slates_mem::region::Region;
use slates_mem::{ExclusiveObject, SharedObject, Words};

/// Shape: the region, as a content extent might be.
const LEN: usize = 64 << 20;
/// Shape: the bytes written, freed and purged: half the region.
const WRITTEN: usize = 32 << 20;
/// Shape: a block written.
const BLOCK: usize = 1 << 20;

/// The process's resident size and physical footprint (`proc_pid_rusage`, `RUSAGE_INFO_V0`).
fn rusage() -> (u64, u64) {
  let mut info = std::mem::MaybeUninit::<libc::rusage_info_v0>::zeroed();
  // SAFETY: `info` is a writable rusage_info_v0, the flavor asked for; the pid is this process's.
  let rc = unsafe {
    libc::proc_pid_rusage(
      libc::getpid(),
      libc::RUSAGE_INFO_V0,
      info.as_mut_ptr().cast(),
    )
  };
  assert_eq!(rc, 0, "proc_pid_rusage");
  // SAFETY: the call succeeded, so it filled the record.
  let info = unsafe { info.assume_init() };
  (info.ri_resident_size, info.ri_phys_footprint)
}

/// Prints the arena's residency and the process's figures at `when`.
fn reading(when: &str, arena: &ChunkArena) {
  let (resident, footprint) = rusage();
  println!(
    "macos_residency: {when}: arena mincore resident {} MiB of {} MiB mapped; process resident_size {} MiB, phys_footprint {} MiB",
    arena.residency().resident >> 20,
    arena.capacity() >> 20,
    resident >> 20,
    footprint >> 20
  );
}

/// Do: on a shared object of [`LEN`] bytes, an arena over one region of it, discarding from a page; read; write
/// [`WRITTEN`] bytes in blocks; read; free them and purge to the end; read. Prints the three readings at each step.
#[test]
#[ignore = "a measurement of macOS residency: run by name"]
fn what_macos_reports_of_a_shared_regions_pages_written_freed_and_purged() {
  let page = 16 << 10;
  let name = format!("/slates-residency-{}", std::process::id());
  let object = SharedObject::create(&name, LEN, Words::new()).unwrap();
  // SAFETY: the test alone maps the object, which declares no words.
  let object = unsafe { ExclusiveObject::new(object) };
  let mut arena = ChunkArena::new(page).discarding_from(page);
  arena.add_region(Region::shared(object, page)).unwrap();
  reading("fresh", &arena);
  let blocks: Vec<_> = (0..WRITTEN / BLOCK)
    .map(|_| {
      let block = arena.alloc(BLOCK, Owner::Volume).unwrap();
      arena.bytes_mut(block).unwrap().fill(0xAB);
      block
    })
    .collect();
  reading("written", &arena);
  for block in blocks {
    arena.free(block).unwrap();
  }
  let mut given = 0usize;
  loop {
    let purged = arena.purge(usize::MAX);
    given += purged.bytes;
    if !purged.more {
      break;
    }
  }
  println!("macos_residency: purged {} MiB", given >> 20);
  reading("freed and purged", &arena);
}

/// Goal 93 P7 (main's ruling of 2026-11-25: `object_resident_bytes` must tell what a purge gave back on macOS, where a
/// discarded page is marked reusable and `mincore` counts it until the kernel takes it). Do: as the measurement above,
/// write [`WRITTEN`] bytes into a shared region's arena, free and purge them. Expect the arena's residency after the
/// purge to have fallen by what it gave back, to within a block: the pages marked reusable read out of it, as the
/// process's footprint drops. Red before: the residency unchanged by the purge (32 MiB read before and after).
#[test]
fn a_purged_shared_region_reads_its_purged_pages_out_of_its_residency() {
  let page = 16 << 10;
  let name = format!("/slates-purged-{}", std::process::id());
  let object = SharedObject::create(&name, LEN, Words::new()).unwrap();
  // SAFETY: the test alone maps the object, which declares no words.
  let object = unsafe { ExclusiveObject::new(object) };
  let mut arena = ChunkArena::new(page).discarding_from(page);
  arena.add_region(Region::shared(object, page)).unwrap();
  let blocks: Vec<_> = (0..WRITTEN / BLOCK)
    .map(|_| {
      let block = arena.alloc(BLOCK, Owner::Volume).unwrap();
      arena.bytes_mut(block).unwrap().fill(0xAB);
      block
    })
    .collect();
  let written = arena.residency().resident;
  for block in blocks {
    arena.free(block).unwrap();
  }
  let mut given = 0usize;
  loop {
    let purged = arena.purge(usize::MAX);
    given += purged.bytes;
    if !purged.more {
      break;
    }
  }
  let after = arena.residency().resident;
  println!(
    "macos_residency: written {} MiB resident, {} MiB given back, {} MiB resident after",
    written >> 20,
    given >> 20,
    after >> 20
  );
  assert!(
    given >= WRITTEN - BLOCK,
    "the purge gave back what was freed: {given}"
  );
  assert!(
    after <= written.saturating_sub(given as u64) + BLOCK as u64,
    "the residency {after} after giving back {given} of {written}"
  );
}
