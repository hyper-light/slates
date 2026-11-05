//! Windows gives freed content pages back (A-205; A-105, A-117): by use, through the process's own memory counter. This is its
//! own test binary, with one test in it, because the working set is the whole process's: a test running beside it would
//! move the reading by more than the margin it holds.
// Test harness code: an unwrap here is a failed test.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::panic_in_result_fn,
  clippy::unwrap_in_result,
  clippy::indexing_slicing
)]
#![cfg(windows)]

use slates_machine::facts::Facts;
use slates_mem::{ChunkArena, ExclusiveObject, Extent, Region, SharedObject, SparseObject, Words};

/// Shape: the bytes each step frees: far above what the test binary itself moves between two readings.
const FREED: usize = 64 << 20;
/// Shape: one content chunk, the block a file's bytes are written in, sixteen 4 KiB pages.
const CHUNK: usize = 64 << 10;
/// Shape: the share of the freed bytes that must leave the working set, in percent (VFS plan step 12's done-when).
const AT_LEAST_PERCENT: u64 = 90;

fn working_set() -> u64 {
  Facts::resident_now().expect("Windows reports the working set")
}

fn page() -> usize {
  usize::try_from(Facts::query().page.base).unwrap()
}

/// Holds that the working set fell by at least the done-when's share of [`FREED`], printing the reading whatever it is.
fn assert_most_of_it_left(what: &str, before: u64, after: u64) {
  let freed = u64::try_from(FREED).unwrap();
  let left = before.saturating_sub(after);
  eprintln!(
    "{what}: working set {before} -> {after}, {left} of {freed} bytes freed left it ({}%)",
    left * 100 / freed
  );
  assert!(
    left * 100 >= freed * AT_LEAST_PERCENT,
    "{what}: {left} of {freed} bytes left the working set ({before} -> {after})"
  );
}

/// Purges until the arena says it is done: the bytes it gave back.
fn purge_all(arena: &mut ChunkArena) -> usize {
  let mut bytes = 0;
  loop {
    let purged = arena.purge(usize::MAX);
    bytes += purged.bytes;
    if !purged.more {
      return bytes;
    }
  }
}

/// One arena over `region`. Do: allocate [`FREED`] bytes as chunks beside one live chunk, write every byte, free them
/// and purge; allocate them again, read every byte of them, write them and read them back. Expect: the purge counts the
/// whole [`FREED`] given back and the working set falls by at least the done-when's share of it; the allocation clears
/// every page it reuses (the counter moves by the bytes, so a dead clearing path cannot pass); every reused byte reads
/// zero; the reused pages count again when written; and the live chunk keeps its bytes throughout, so clearing reaches
/// nothing outside the blocks it hands out.
fn a_purged_arena_leaves_the_working_set_and_reads_zeros_on_reuse(what: &str, region: Region) {
  let page = page();
  let mut arena = ChunkArena::new(page).discarding_from(page);
  arena.add_region(region).unwrap();
  let live = arena.alloc(CHUNK).unwrap();
  arena.bytes_mut(live).unwrap().fill(0x5A);
  let chunks: Vec<Extent> = (0..FREED / CHUNK)
    .map(|_| arena.alloc(CHUNK).unwrap())
    .collect();
  for chunk in &chunks {
    arena.bytes_mut(*chunk).unwrap().fill(0xAB);
  }
  for chunk in &chunks {
    arena.free(*chunk).unwrap();
  }
  let written = working_set();
  let given_back = purge_all(&mut arena);
  let purged = working_set();
  assert_most_of_it_left(what, written, purged);
  assert_eq!(given_back, FREED, "{what}: the purge's own count");
  assert!(
    arena.bytes(live).unwrap().iter().all(|byte| *byte == 0x5A),
    "{what}: the live chunk kept its bytes through the purge"
  );
  reuse_reads_zeros_and_keeps_what_is_written(what, &mut arena, purged);
  assert!(
    arena.bytes(live).unwrap().iter().all(|byte| *byte == 0x5A),
    "{what}: the live chunk kept its bytes through the reuse"
  );
}

/// The second half of the step above: `arena` has [`FREED`] bytes purged, and the working set read `purged` after.
fn reuse_reads_zeros_and_keeps_what_is_written(what: &str, arena: &mut ChunkArena, purged: u64) {
  let reused_before = arena.reused_bytes();
  let again: Vec<Extent> = (0..FREED / CHUNK)
    .map(|_| arena.alloc(CHUNK).unwrap())
    .collect();
  assert!(
    arena.reused_bytes() - reused_before >= u64::try_from(FREED).unwrap(),
    "{what}: every page handed out again was cleared"
  );
  for chunk in &again {
    assert!(
      arena.bytes(*chunk).unwrap().iter().all(|byte| *byte == 0),
      "{what}: a reused chunk reads zeros, every byte of it"
    );
    arena.bytes_mut(*chunk).unwrap().fill(0xCD);
  }
  let rewritten = working_set();
  assert!(
    rewritten.saturating_sub(purged) * 100 >= u64::try_from(FREED).unwrap() * AT_LEAST_PERCENT,
    "{what}: the rewritten pages count again: {purged} -> {rewritten}"
  );
  for chunk in &again {
    assert!(
      arena
        .bytes(*chunk)
        .unwrap()
        .iter()
        .all(|byte| *byte == 0xCD),
      "{what}: a reused chunk keeps what was written"
    );
  }
}

/// A-117 on Windows. Do: fill [`FREED`] bytes of a sparse object past a page that stays, discard them, write a few bytes
/// inside the discarded range through a mapping opened afterwards (a restarted daemon over the anchor's object), and
/// discard a far range nothing ever committed. Expect: the discard gives back the whole range and the working set falls
/// by at least the done-when's share; the small write keeps its bytes and clears no byte around it; and the uncommitted
/// range gives nothing back, unrefused.
fn a_discarded_object_range_leaves_the_working_set_and_a_later_write_keeps_its_page() {
  /// Shape: a layout far larger than what the step touches, as the content object is.
  const LAYOUT: usize = 4 << 30;
  /// Shape: where the discarded range starts: past the first allocation granule, which holds the page that stays.
  const FROM: usize = 64 << 10;
  /// Shape: bytes a later mapping writes inside the discarded range.
  const NOTE: &[u8] = b"written after the discard";
  let name = format!("slates-mem-give-back-{}", std::process::id());
  let mut object = SparseObject::create(&name, LAYOUT, Words::new()).unwrap();
  object.write(0, &[0x5A; 4096]).unwrap();
  let piece = vec![0xABu8; 1 << 20];
  for at in (FROM..FROM + FREED).step_by(piece.len()) {
    object.write(at, &piece).unwrap();
  }
  let written = working_set();
  let given_back = object.discard(FROM, FREED).unwrap();
  assert_most_of_it_left("the sparse object", written, working_set());
  assert_eq!(given_back, FREED, "the sparse object's own count");

  let mut later = SparseObject::open(&object.handoff().unwrap(), LAYOUT, Words::new()).unwrap();
  later.write(FROM + 100, NOTE).unwrap();
  let mut back = vec![0u8; NOTE.len()];
  object.read(FROM + 100, &mut back).unwrap();
  assert_eq!(back, NOTE);
  let mut kept = [0u8; 4096];
  later.read(0, &mut kept).unwrap();
  assert!(
    kept.iter().all(|byte| *byte == 0x5A),
    "the page outside the discard kept its bytes"
  );

  assert_eq!(
    object.discard(LAYOUT - FREED, FREED).unwrap(),
    0,
    "a range this process never committed has no page to give back"
  );
}

/// VFS plan step 12, at the crate. See the two steps; they run in turn because they read one process-wide counter.
#[test]
fn freed_pages_leave_the_working_set_and_read_zeros_when_reused() {
  let page = page();
  let private = Region::map(FREED * 2, page, false).unwrap();
  a_purged_arena_leaves_the_working_set_and_reads_zeros_on_reuse("a private region", private);

  let object = SharedObject::create(
    &format!("slates-mem-give-back-arena-{}", std::process::id()),
    FREED * 2,
    Words::new(),
  )
  .unwrap();
  // SAFETY: this test holds the only mapping of the object, and it declares no words.
  let exclusive = unsafe { ExclusiveObject::new(object) };
  a_purged_arena_leaves_the_working_set_and_reads_zeros_on_reuse(
    "a shared region",
    Region::shared(exclusive, page),
  );

  a_discarded_object_range_leaves_the_working_set_and_a_later_write_keeps_its_page();
}
