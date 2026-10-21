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

/// One row of `/proc/self/maps`: where it starts and ends, whether it is a mapping of a memory object whose
/// name carries `prefix`, and whether it is an anonymous `PROT_NONE` reservation (which is what a placement's
/// slack looks like once the window has been mapped inside it).
#[cfg(target_os = "linux")]
struct MapRow {
  start: usize,
  end: usize,
  ours: bool,
  reserved: bool,
  line: String,
}

/// The address space this process holds for the memory objects named with `prefix`: every mapping of one, plus
/// the anonymous `PROT_NONE` slack abutting it that the placement reserved around it. Returns the total and the
/// rows it counted, so a failure says exactly what it saw instead of only a number.
///
/// This is read per object rather than process-wide, and that is the whole point of it. The first version of
/// this test took the difference of `/proc/self/statm` across the allocation, which is every byte the *process*
/// gained — so it measured the seven sibling tests allocating 8 and 16 MiB objects beside it (153 MB where the
/// same run alone read 4.5 MB), and it was answered with a child process of its own. That was treating the
/// symptom. Under ThreadSanitizer the child is not quiet either: the instrumentation's own bookkeeping grows
/// with the memory the test touches, and the lane failed on 2026-10-20 reading three pages of slack per object
/// where the placement reserves one (run 39527867650). A figure that is attributable to the objects needs no
/// quiet process and cannot be moved by anything that is not one of them.
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)] // reading this process's own address space through /proc, read-only
fn address_space_held(prefix: &str) -> (usize, Vec<String>) {
  let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else {
    return (0, Vec::new());
  };
  let rows: Vec<MapRow> = maps
    .lines()
    .filter_map(|line| parse_map_row(line, prefix))
    .collect();
  // Every row to charge, by index, so slack lying between two of our own windows is counted once.
  let mut charged: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
  for at in rows
    .iter()
    .enumerate()
    .filter(|(_, row)| row.ours)
    .map(|(at, _)| at)
  {
    charged.insert(at);
    charge_abutting_slack(&rows, at, &mut charged);
  }
  let mut held = 0usize;
  let mut counted = Vec::new();
  for row in charged.iter().filter_map(|at| rows.get(*at)) {
    held = held.saturating_add(row.end.saturating_sub(row.start));
    counted.push(row.line.clone());
  }
  (held, counted)
}

/// Charges the run of anonymous `PROT_NONE` reservations abutting the window at `rows[at]`, on both sides of
/// it. A placement whose congruent address falls inside its reservation leaves slack before *and* after the
/// window, so both directions are walked, and each stops at the first row that is not an abutting reservation.
#[cfg(target_os = "linux")]
fn charge_abutting_slack(
  rows: &[MapRow],
  at: usize,
  charged: &mut std::collections::BTreeSet<usize>,
) {
  for before in (0..at).rev() {
    if !abuts_before(rows, before) {
      break;
    }
    charged.insert(before);
  }
  for after in at.saturating_add(1)..rows.len() {
    if !abuts_after(rows, after) {
      break;
    }
    charged.insert(after);
  }
}

/// Whether `rows[before]` is a reservation that ends exactly where the row after it begins.
#[cfg(target_os = "linux")]
fn abuts_before(rows: &[MapRow], before: usize) -> bool {
  match (rows.get(before), rows.get(before.saturating_add(1))) {
    (Some(row), Some(next)) => row.reserved && row.end == next.start,
    _ => false,
  }
}

/// Whether `rows[after]` is a reservation that begins exactly where the row before it ends.
#[cfg(target_os = "linux")]
fn abuts_after(rows: &[MapRow], after: usize) -> bool {
  match (rows.get(after.saturating_sub(1)), rows.get(after)) {
    (Some(previous), Some(row)) => row.reserved && previous.end == row.start,
    _ => false,
  }
}

/// Parses one `/proc/self/maps` row (`start-end perms offset dev inode path`). `None` for a row that does not
/// parse, which is never one of ours.
#[cfg(target_os = "linux")]
fn parse_map_row(line: &str, prefix: &str) -> Option<MapRow> {
  let mut fields = line.split_whitespace();
  let range = fields.next()?;
  let perms = fields.next()?;
  let (start, end) = range.split_once('-')?;
  let start = usize::from_str_radix(start, 16).ok()?;
  let end = usize::from_str_radix(end, 16).ok()?;
  // Fields three to five are the offset, the device and the inode; the path, if any, is the rest.
  let path = fields.nth(3).unwrap_or("");
  Some(MapRow {
    start,
    end,
    ours: path.contains(prefix),
    reserved: path.is_empty() && perms.starts_with("---"),
    line: line.to_owned(),
  })
}

/// §4.6 L1 — placing a mapping costs one page of address space per object and gives every byte of it back.
///
/// A placed mapping reserves more address space than it maps, and two things about that have to be true or
/// the placement is worse than the bug it fixed. It must give the reservation back when the object goes: a
/// daemon opens an object per client session (`ipc/src/region.rs`), so a reservation outliving its owner
/// would bleed address space for as long as the process ran. And the slack must be one *page* for an object
/// too small to hold a huge page, not one huge page — i686 is a tested target with about 3 GB of address
/// space to spend, and a huge page per session would spend it.
///
/// Do: map sixty-four ring-sized objects, read the address space charged to those objects out of
/// `/proc/self/maps`, drop them, and read it again. Expect: every byte of every window, no more than one page
/// of slack each, and nothing at all left afterwards.
#[cfg(target_os = "linux")]
#[test]
#[cfg_attr(miri, ignore)] // a shared memory object is not modelled by Miri, as at `region.rs`
fn a_placed_mapping_costs_a_page_of_slack_and_returns_every_byte_of_it() {
  // Derived: a client's ring is far smaller than a huge page, which is the case the bound is about; sixty-four
  // of them so a per-object page of slack is well clear of a single row's rounding.
  const OBJECTS: usize = 64;
  let each = PAGE.saturating_mul(16);
  let tag = format!("slack{}", std::process::id());
  let (before, _) = address_space_held(&tag);
  assert_eq!(
    before, 0,
    "nothing holds this tag's address space before the objects exist"
  );

  let mut held = Vec::new();
  for sample in 0..OBJECTS {
    let Ok(object) =
      SharedObject::create(&object_name(&format!("{tag}-{sample}")), each, Words::new())
    else {
      println!(
        "SKIP a_placed_mapping_costs_a_page_of_slack_and_returns_every_byte_of_it: the host refused a shared object"
      );
      return;
    };
    // SAFETY: this test is the object's only accessor, which is what `ExclusiveObject` promises.
    held.push(Region::shared(
      unsafe { ExclusiveObject::new(object) },
      PAGE,
    ));
  }

  let (mapped, counted) = address_space_held(&tag);
  let asked = each.saturating_mul(OBJECTS);
  let allowed = asked.saturating_add(PAGE.saturating_mul(OBJECTS));
  let huge = slates_machine::facts::huge_page_bytes()
    .and_then(|bytes| usize::try_from(bytes).ok())
    .unwrap_or(0);
  assert!(
    mapped >= asked,
    "every byte of every window is mapped: {mapped} bytes charged to {OBJECTS} objects of {each}, which is \
     under the {asked} they asked for (non-vacuity — a reading that saw nothing would pass the bound below \
     for the wrong reason)"
  );
  assert!(
    mapped <= allowed,
    "a small object reserves a page of slack, not a huge page: {mapped} bytes of address space for {asked} \
     asked over {OBJECTS} objects, which allows {allowed}; a huge-page grain would have cost about {}. The \
     rows charged were:\n{}",
    asked.saturating_add(huge.saturating_mul(OBJECTS)),
    counted.join("\n")
  );

  drop(held);
  let (kept, left) = address_space_held(&tag);
  assert_eq!(
    kept,
    0,
    "dropping the objects gives their address space back whole; {kept} bytes are still held:\n{}",
    left.join("\n")
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
