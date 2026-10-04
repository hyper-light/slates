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
