//! A region backed by a shared memory object (§4.7, §4.8): the arena addresses bytes over it
//! exactly as over a private mapping, and — the property that matters for recovery — content placed
//! in it survives the mapping that wrote it being dropped, which is what a daemon restart is at the
//! memory level. This is the first slice of the anchor-owned volume storage the recovery gap
//! (BUG-11 / GAP-A9-6, §4.8) needs: the store's content arena must live in memory like this so an
//! agent's writes are still there after a crash.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use slates_mem::SharedObject;
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_mem::{ExclusiveObject, Words};

/// Shape: a small shared object and page for the test.
const PAGE: usize = 4096;
const LEN: usize = 64 * PAGE;

/// A per-process object name within macOS's 31-character `shm_open` limit.
fn object_name(tag: &str) -> String {
  format!("slates-{tag}-{}", std::process::id())
}

/// A `ChunkArena` over a shared-object-backed region allocates and serves bytes exactly like one
/// over a private mapping — the arena is agnostic to the backing, which is what lets the store's
/// content live in anchor-owned RAM without changing the volume core.
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `region.rs`
fn an_arena_over_a_shared_region_allocates_and_serves_bytes() {
  let object = SharedObject::create(&object_name("arena"), LEN, Words::new()).unwrap();
  let mut arena = ChunkArena::new(PAGE);
  // SAFETY: this test is the object's only mapper, and the object declares no words.
  let object = unsafe { ExclusiveObject::new(object) };
  arena.add_region(Region::shared(object, PAGE)).unwrap();

  let extent = arena.alloc(PAGE).unwrap();
  let payload = b"content in a shared region";
  arena.bytes_mut(extent).unwrap()[..payload.len()].copy_from_slice(payload);
  assert_eq!(
    &arena.bytes(extent).unwrap()[..payload.len()],
    payload,
    "the arena serves back the bytes it stored in the shared region"
  );

  arena.free(extent).unwrap();
}

/// The recovery-foundation property: bytes written through a shared-object-backed region are still
/// there when read through a *second* mapping of the same object after the first mapping is dropped
/// — the memory-level shape of "an agent's writes survive a daemon restart" (§4.8). A private
/// mapping (`Region::map`) would lose them; this is why the store's content must be anchor-owned.
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `region.rs`
fn content_in_a_shared_region_survives_the_writing_mapping_being_dropped() {
  let object = SharedObject::create(&object_name("survive"), LEN, Words::new()).unwrap();
  // The handoff a restarted daemon would use to re-map the same object.
  let handoff = object.handoff().unwrap();

  // The "running daemon" places content in the object through its region.
  // SAFETY: this region is the object's only accessor until it is dropped below, and the object declares
  // no words.
  let mut region = Region::shared(unsafe { ExclusiveObject::new(object) }, PAGE);
  let offset = 8 * PAGE;
  let content = b"volume bytes an agent wrote before the crash";
  region.bytes_mut()[offset..offset + content.len()].copy_from_slice(content);

  // The "restarted daemon" re-maps the same object from the handoff, then the old mapping goes away
  // (the crashed process exits). The content must still be readable through the new mapping.
  // The re-map comes first (on macOS the name lives only while its creator's mapping does); only the
  // mapping is taken, no byte is touched, until the old region is gone.
  let reattached = SharedObject::open(&handoff, LEN, Words::new()).unwrap();
  drop(region);
  // SAFETY: the writing region is dropped; this is the object's only accessor now.
  let recovered = Region::shared(unsafe { ExclusiveObject::new(reattached) }, PAGE);

  assert_eq!(
    &recovered.bytes()[offset..offset + content.len()],
    content,
    "content in a shared region survives the writing mapping being dropped (a restart)"
  );
}

/// A-64 (a shard's arena range of the content object). Do: create a sparse object, as the anchor creates the
/// content object; write a header into its first range by copy; map a later range alone, exclusively, put an
/// arena's block there and write into it; drop that mapping and map the range again. Expect: the arena's
/// bytes are there through the new mapping, at the offset the block names, and the header beside the range
/// is still what the copy wrote.
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `region.rs`
fn a_range_of_a_sparse_object_mapped_alone_keeps_its_bytes_across_a_remap() {
  let granule = slates_mem::mapping_granule().unwrap();
  let range = 4 * granule;
  let object =
    slates_mem::SparseObject::create(&object_name("range"), range * 3, Words::new()).unwrap();
  let handoff = object.handoff().unwrap();
  let mut object = object;
  let header = b"the write log and image slots are reached by copy";
  object.write(0, header).unwrap();

  let at = 2 * range;
  // SAFETY: nothing else reaches `[at, at + range)`: the object's own view is used only for its first range.
  let mapped = unsafe { ExclusiveObject::open_range(&handoff, at, range) }.unwrap();
  let mut arena = ChunkArena::new(PAGE);
  arena.add_region(Region::shared(mapped, PAGE)).unwrap();
  let block = arena.alloc(PAGE).unwrap();
  let content = b"a chunk written once, in anchor RAM";
  arena.bytes_mut(block).unwrap()[..content.len()].copy_from_slice(content);
  let offset = block.offset();
  drop(arena);

  // SAFETY: the mapping that wrote the range is dropped; this is its only accessor now.
  let remapped = unsafe { ExclusiveObject::open_range(&handoff, at, range) }.unwrap();
  let region = Region::shared(remapped, PAGE);
  assert_eq!(
    &region.bytes()[offset..offset + content.len()],
    content,
    "the range keeps the arena's bytes across a remap"
  );
  let mut kept = vec![0u8; header.len()];
  object.read(0, &mut kept).unwrap();
  assert_eq!(kept, header, "the copied range beside it is untouched");
}

/// A-64. Do: map a range whose offset is not on the mapping granule. Expect: refused `OutOfRange` on every
/// platform, never a mapping of other bytes (Unix's map would round the offset down).
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `region.rs`
fn a_range_off_the_mapping_granule_is_refused() {
  let granule = slates_mem::mapping_granule().unwrap();
  let object =
    slates_mem::SparseObject::create(&object_name("offgran"), granule * 4, Words::new()).unwrap();
  let handoff = object.handoff().unwrap();
  // SAFETY: nothing reaches the object's bytes; the mapping is refused before any exists.
  let refused = unsafe { ExclusiveObject::open_range(&handoff, granule / 2, granule) };
  assert!(
    matches!(refused, Err(slates_mem::MemError::OutOfRange { .. })),
    "{refused:?}"
  );
}

/// The virtio-fs DAX window's foundation (§4.6): a block in a shared-object-backed region exposes its backing
/// descriptor and byte offset, so the device can map the block's pages into a guest's page table through a VMM's
/// cache region. A private (anonymous) region exposes no descriptor — there is none to hand a VMM — so DAX is
/// confined to anchor-backed volumes. The offset and length are the block's own, and the fd is the region's object
/// (the very descriptor whose bytes the arena already serves, proven by the first test above).
#[cfg(unix)]
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `region.rs`
fn block_backing_exposes_a_shared_regions_fd_and_none_for_a_private_one() {
  use std::os::fd::AsRawFd;
  let object = SharedObject::create(&object_name("daxfd"), LEN, Words::new()).unwrap();
  let mut arena = ChunkArena::new(PAGE);
  // SAFETY: this test is the object's only mapper, and the object declares no words.
  arena
    .add_region(Region::shared(
      unsafe { ExclusiveObject::new(object) },
      PAGE,
    ))
    .unwrap();
  let extent = arena.alloc(PAGE).unwrap();

  let (fd, off, len) = arena
    .block_backing(extent)
    .expect("a shared-object region exposes its backing fd for the DAX window");
  assert_eq!(
    off,
    extent.offset(),
    "the offset is the block's offset within the object"
  );
  assert_eq!(len, extent.len(), "the length is the block's length");
  assert!(fd.as_raw_fd() >= 0, "a live descriptor is handed back");
  arena.free(extent).unwrap();

  // A private (anonymous) region has no descriptor, so no DAX fd.
  let mut private = ChunkArena::new(PAGE);
  private
    .add_region(Region::map(LEN, PAGE, false).unwrap())
    .unwrap();
  let anon = private.alloc(PAGE).unwrap();
  assert!(
    private.block_backing(anon).is_none(),
    "a private (anonymous) region exposes no DAX fd"
  );
}

/// §4.6 L1: the arena's shared object is backed by **huge pages**, so moving a block moves physical rows.
///
/// `Volume::dax_migrate` relocates a writable block to a different *virtual* block; if the backing is 4 KiB
/// pages that moves no physical rows and the moving target is, in the design's own words, security theatre. A
/// private `Region` has asked for huge pages since it was written; a shared object did not until 2026-10-19,
/// and the DAX arena is a shared object.
///
/// Do: make a shared object several huge pages long, touch every byte, and read the kernel's own account of
/// the mapping. Expect: `ShmemPmdMapped` covers it. The advice alone proves nothing — measured on Linux 7.2, a
/// 64 KiB and a 1 MiB mapping both take `MADV_HUGEPAGE` with `OK` and get **zero** huge pages — which is why
/// this asserts the kernel's number and not the call's return.
///
/// Linux-only, and skips loudly where the host will not give shmem huge pages (a container kernel with
/// `shmem_enabled: never`, for instance): the property is the kernel's to grant.
#[cfg(target_os = "linux")]
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `region.rs`
fn a_shared_objects_mapping_is_backed_by_huge_pages_so_a_shift_moves_physical_rows() {
  let shmem = std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/shmem_enabled")
    .unwrap_or_default();
  if !(shmem.contains("[always]") || shmem.contains("[advise]") || shmem.contains("[within_size]"))
  {
    eprintln!("skipping the huge-page backing proof: this host's shmem_enabled is {shmem:?}");
    return;
  }
  // Several huge pages, so the kernel has something it can honour: below one huge page the advice is a no-op.
  let span = 16 * 1024 * 1024;
  let object = SharedObject::create(&object_name("huge"), span, Words::new()).unwrap();
  // SAFETY: this test is the object's only accessor, which is what `ExclusiveObject` promises.
  let mut region = Region::shared(unsafe { ExclusiveObject::new(object) }, PAGE);
  region.bytes_mut().fill(0x7A);

  // Asked of the kernel, not of `madvise`'s return: the advice succeeds on mappings that get nothing.
  let backed = region
    .huge_backed_bytes()
    .expect("Linux answers how much of a mapping is huge-page backed");
  assert!(
    backed > 0,
    "the shared object's mapping is huge-page backed: {backed} bytes of {span}"
  );
}

/// §4.6 L1 — an object long enough to hold a huge page is mapped where one can form behind it, and that span
/// collapses to a huge page even where the host has shmem huge pages switched off.
///
/// The sibling proof above asks only for the *advice*, which `shmem_enabled` governs, so it skips wherever
/// shmem huge pages are off — and `never` is the kernel's default, so that is most hosts, including every one
/// this project can reach. That left the premise under §4.6's sandbox asserted nowhere that runs.
///
/// Two things decide it, and this drives both. A huge page behind a memory object is addressed twice over, by
/// where it sits in the process and by where it sits in the object, and the kernel forms one only where the
/// two agree — so the mapping is *placed*, and that placement is asserted here rather than hoped for. Then the
/// sandbox does not stop at the advice: it asks the kernel to compact the span now (`MADV_COLLAPSE`, Linux
/// 6.1, shmem support 6.2), an explicit request rather than a preference.
///
/// Measured 2026-10-20 on Linux 6.12.76 (aarch64, `shmem_enabled` `never`): the advice delivered 0 bytes and
/// the collapse delivered a full 2 MiB. Before the placement, 13 of 16 objects began off a huge boundary and
/// the collapse was refused `EINVAL` on every one of them, whichever end was aligned — which is why the DAX
/// sandbox was reserved by address luck
/// (`docs/bugs/2026-10-20-the-dax-sandbox-was-reserved-by-mmap-address-luck.md`).
#[cfg(target_os = "linux")]
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `region.rs`
fn an_object_that_can_hold_a_huge_page_is_mapped_where_one_forms_and_collapses_to_it() {
  let Some(huge) =
    slates_machine::facts::huge_page_bytes().and_then(|bytes| usize::try_from(bytes).ok())
  else {
    println!(
      "SKIP an_object_that_can_hold_a_huge_page_is_mapped_where_one_forms_and_collapses_to_it: this host has \
       no huge page to ask for"
    );
    return;
  };
  // Derived: four huge pages, comfortably more than the one the placement must make room for.
  let span = huge.saturating_mul(4);
  let object = SharedObject::create(&object_name("placed"), span, Words::new()).unwrap();
  // SAFETY: this test is the object's only accessor, which is what `ExclusiveObject` promises.
  let mut region = Region::shared(unsafe { ExclusiveObject::new(object) }, PAGE);
  let base = region.base() as usize;
  assert_eq!(
    base.checked_rem(huge),
    Some(0),
    "an object long enough to hold a huge page is mapped on a huge-page boundary, so its address and its \
     offsets agree and the kernel can form one: base {base:#x}, huge page {huge}"
  );
  region.bytes_mut().fill(0x7A);

  let shmem = std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/shmem_enabled")
    .unwrap_or_default();
  // Asked of the kernel both times, never of `madvise`'s return: the advice succeeds on mappings that get
  // nothing, which is the trap `huge_backed_bytes` exists to avoid, and the sandbox reads the measurement too.
  let advised = region
    .huge_backed_bytes()
    .expect("Linux answers how much of a mapping is huge-page backed");
  let asked = region.collapse_huge(0, huge);
  let collapsed = region
    .huge_backed_bytes()
    .expect("Linux answers how much of a mapping is huge-page backed");
  eprintln!(
    "shmem_enabled {shmem:?}: base {base:#x}; advice gave {advised} bytes, collapse returned {asked} and left \
     {collapsed} of {span}"
  );
  assert!(
    collapsed >= u64::try_from(huge).unwrap_or(u64::MAX),
    "the placed span is huge-page backed after the collapse the sandbox asks for: {collapsed} bytes, advice \
     alone gave {advised}, collapse returned {asked}, shmem_enabled {shmem:?}"
  );
}

/// The environment variable that turns [`a_quiet_process_reports_what_a_placed_mapping_costs`] into the
/// measuring child. Unset, that test does nothing, which is how it behaves inside the ordinary suite run.
#[cfg(target_os = "linux")]
const SLACK_CHILD: &str = "SLATES_TEST_SLACK_CHILD";

/// The process's virtual size in bytes, from the first field of `statm` (pages).
#[cfg(target_os = "linux")]
fn virtual_bytes() -> usize {
  std::fs::read_to_string("/proc/self/statm")
    .ok()
    .and_then(|statm| {
      statm
        .split_whitespace()
        .next()
        .and_then(|pages| pages.parse::<usize>().ok())
    })
    .map_or(0, |pages| pages.saturating_mul(PAGE))
}

/// The measuring half of [`a_placed_mapping_costs_a_page_of_slack_and_returns_every_byte_of_it`], run in a
/// process of its own so that nothing else is allocating while it reads a process-wide figure. Does nothing
/// unless the parent selected it, so an ordinary suite run passes straight over it.
#[cfg(target_os = "linux")]
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `region.rs`
fn a_quiet_process_reports_what_a_placed_mapping_costs() {
  if std::env::var(SLACK_CHILD).is_err() {
    return;
  }
  // Derived: a client's ring is far smaller than a huge page, which is the case the bound is about; sixty-four
  // of them so the per-object slack stands well above the allocator's own movement.
  const OBJECTS: usize = 64;
  let each = PAGE.saturating_mul(16);
  let before = virtual_bytes();
  let mut held = Vec::new();
  for sample in 0..OBJECTS {
    let Ok(object) =
      SharedObject::create(&object_name(&format!("slack{sample}")), each, Words::new())
    else {
      println!("SLACK refused");
      return;
    };
    // SAFETY: this test is the object's only accessor, which is what `ExclusiveObject` promises.
    held.push(Region::shared(
      unsafe { ExclusiveObject::new(object) },
      PAGE,
    ));
  }
  let mapped = virtual_bytes().saturating_sub(before);
  drop(held);
  let kept = virtual_bytes().saturating_sub(before);
  println!(
    "SLACK {mapped} {kept} {} {OBJECTS}",
    each.saturating_mul(OBJECTS)
  );
}

/// §4.6 L1 — placing a mapping costs one grain of address space and gives every byte of it back.
///
/// A placed mapping reserves more address space than it maps, and two things about that have to be true or
/// the placement is worse than the bug it fixed. It must give the reservation back when the object goes: a
/// daemon opens an object per client session (`ipc/src/region.rs`), so a reservation outliving its owner
/// would bleed address space for as long as the process ran. And the slack must be one *page* for an object
/// too small to hold a huge page, not one huge page — i686 is a tested target with about 3 GB of address
/// space to spend, and a huge page per session would spend it.
///
/// The figure is process-wide, so it is read in a **child process of its own**: measured first in-process,
/// sixty-four ring-sized objects read as 153 MB because seven sibling tests were allocating 8 and 16 MiB
/// objects at the same time, and the same measurement run alone read 4.5 MB. A test that only holds when it
/// runs alone is not a test, so the parent re-invokes this binary for the one case, as `anchor.rs` and
/// `reap.rs` do, and reads a tagged line out of it.
#[cfg(target_os = "linux")]
#[test]
#[cfg_attr(miri, ignore)] // spawning the test binary is not modelled by Miri
fn a_placed_mapping_costs_a_page_of_slack_and_returns_every_byte_of_it() {
  let Ok(binary) = std::env::current_exe() else {
    println!(
      "SKIP a_placed_mapping_costs_a_page_of_slack_and_returns_every_byte_of_it: no path to re-invoke"
    );
    return;
  };
  let run = std::process::Command::new(binary)
    .args([
      "--exact",
      "a_quiet_process_reports_what_a_placed_mapping_costs",
      "--nocapture",
    ])
    .env(SLACK_CHILD, "1")
    .output();
  let Ok(run) = run else {
    println!(
      "SKIP a_placed_mapping_costs_a_page_of_slack_and_returns_every_byte_of_it: the child did not run"
    );
    return;
  };
  let printed = String::from_utf8_lossy(&run.stdout);
  // Tagged, because libtest prints its own banner on the child's stdout before the role runs.
  let Some(line) = printed.lines().find_map(|line| line.strip_prefix("SLACK ")) else {
    panic!("the child reports what it measured; it printed {printed:?}");
  };
  let figures: Vec<usize> = line
    .split_whitespace()
    .filter_map(|word| word.parse().ok())
    .collect();
  let (Some(mapped), Some(kept), Some(asked), Some(objects)) = (
    figures.first().copied(),
    figures.get(1).copied(),
    figures.get(2).copied(),
    figures.get(3).copied(),
  ) else {
    println!("SKIP a_placed_mapping_costs_a_page_of_slack_and_returns_every_byte_of_it: {line:?}");
    return;
  };
  let huge = slates_machine::facts::huge_page_bytes()
    .and_then(|bytes| usize::try_from(bytes).ok())
    .unwrap_or(0);
  // The placement reserves one grain beyond the object, and the grain for an object this small is the page.
  let allowed = asked.saturating_add(PAGE.saturating_mul(objects).saturating_mul(2));
  assert!(
    mapped >= asked,
    "the objects are mapped at all: {mapped} bytes for {asked} asked (non-vacuity — a reading that saw \
     nothing would pass the bound below for the wrong reason)"
  );
  assert!(
    mapped <= allowed,
    "a small object reserves a page of slack, not a huge page: {mapped} bytes of address space for {asked} \
     asked over {objects} objects, which allows {allowed}; a huge-page grain would have cost about {}",
    asked.saturating_add(huge.saturating_mul(objects))
  );
  // Derived: the allocator's own bookkeeping moves the figure a little either way, so the test is that the
  // objects' address space came back, not that the number is identical to the byte.
  let noise = asked.checked_div(8).unwrap_or(0);
  assert!(
    kept <= noise,
    "dropping the objects gives their address space back: {kept} bytes still held of the {mapped} the \
     mappings took, which is more than the {noise} the allocator's own movement allows"
  );
}

/// §4.6 L1 — an absurd length is refused with a typed error rather than a panic, and the mapper still works.
///
/// `ExclusiveObject::open_range` maps a range of an object a handoff names, so a caller's length reaches the
/// mapping path without an `ftruncate` to refuse it on the way. Expect a typed error, no panic, and the next
/// ordinary open to map and read normally, so a refusal leaves nothing broken behind it.
///
/// What refuses it, exactly, because the difference matters and the first version of this test claimed the
/// wrong one: `Words::new().layout(len)` caps the length before the mapper is reached. Red-checked 2026-10-20
/// by replacing the reservation's own `len.checked_add(grain)` with a wrapping add — this test still passed,
/// so it does **not** exercise that arithmetic. That `checked_add` is defence in depth for a future caller
/// that reaches the mapper without the layout cap, kept because no production path may panic, and it is
/// honestly unreachable from any public API today.
///
/// Also measured while writing this, so the bound is not mistaken for something tighter: a span of 64 TiB
/// (`1 << 46`) is *not* hostile here — a memory object is sparse and the address space is 256 TiB, so it
/// reserves and maps perfectly well.
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `region.rs`
fn an_absurd_length_is_refused_with_a_typed_error_and_the_mapper_still_works() {
  let span = PAGE.saturating_mul(4);
  let object = SharedObject::create(&object_name("overflow"), span, Words::new()).unwrap();
  let handoff = object.handoff().unwrap();
  // SAFETY: the handoff names this test's own object, which nothing truncates while the view lives; the call
  // is expected to refuse, and on refusal it maps nothing at all.
  let refused = unsafe { ExclusiveObject::open_range(&handoff, 0, usize::MAX) };
  assert!(
    refused.is_err(),
    "a length of usize::MAX is refused with a typed error rather than panicked on"
  );

  // The refusal left nothing behind: the same object still opens and reads.
  // SAFETY: as above, and this range is the object's own.
  let mut mapped = unsafe { ExclusiveObject::open_range(&handoff, 0, span) }.unwrap();
  mapped.bytes_mut().fill(0x6B);
  assert!(
    mapped.bytes().iter().all(|byte| *byte == 0x6B),
    "a range opened after the refusal maps and reads back whole"
  );
}
