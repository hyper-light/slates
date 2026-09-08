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
  let object = SharedObject::create(&object_name("arena"), LEN).unwrap();
  let mut arena = ChunkArena::new(PAGE);
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
  let object = SharedObject::create(&object_name("survive"), LEN).unwrap();
  // The handoff a restarted daemon would use to re-map the same object.
  let handoff = object.handoff().unwrap();

  // The "running daemon" places content in the object through its region.
  let mut region = Region::shared(object, PAGE);
  let offset = 8 * PAGE;
  let content = b"volume bytes an agent wrote before the crash";
  region.bytes_mut()[offset..offset + content.len()].copy_from_slice(content);

  // The "restarted daemon" re-maps the same object from the handoff, then the old mapping goes away
  // (the crashed process exits). The content must still be readable through the new mapping.
  let reattached = SharedObject::open(&handoff, LEN).unwrap();
  let recovered = Region::shared(reattached, PAGE);
  drop(region);

  assert_eq!(
    &recovered.bytes()[offset..offset + content.len()],
    content,
    "content in a shared region survives the writing mapping being dropped (a restart)"
  );
}

/// The whole memory-level recovery scenario end to end (§4.8), the shape the daemon integration will
/// take: an arena over a shared object allocates content (the extents a snapshot's metadata would
/// name), the writing mapping is dropped and the object re-attached, a *fresh* arena is built over
/// it and **re-seeded by reserving those extents** (as recovery replays them), the content reads
/// back byte-identical through the fresh arena, and a new allocation never overlaps the recovered
/// content — proving the re-seed stops the fresh allocator from handing out live bytes.
#[test]
fn a_reseeded_arena_recovers_content_and_never_reallocates_over_it() {
  let object = SharedObject::create(&object_name("reseed"), LEN).unwrap();
  let handoff = object.handoff().unwrap();

  // The running daemon writes two chunks and records their extents (its metadata).
  let mut arena = ChunkArena::new(PAGE);
  arena.add_region(Region::shared(object, PAGE)).unwrap();
  let first = arena.alloc(2 * PAGE).unwrap();
  let second = arena.alloc(PAGE).unwrap();
  let first_bytes = b"the first recovered chunk's bytes";
  let second_bytes = b"the second chunk";
  arena.bytes_mut(first).unwrap()[..first_bytes.len()].copy_from_slice(first_bytes);
  arena.bytes_mut(second).unwrap()[..second_bytes.len()].copy_from_slice(second_bytes);

  // The restarted daemon re-attaches the object and builds a fresh arena over it, then re-seeds the
  // allocator from the recovered extents before serving anything.
  let reattached = SharedObject::open(&handoff, LEN).unwrap();
  drop(arena);
  let mut recovered = ChunkArena::new(PAGE);
  recovered
    .add_region(Region::shared(reattached, PAGE))
    .unwrap();
  recovered.reserve(first).unwrap();
  recovered.reserve(second).unwrap();

  // The content is intact through the fresh arena.
  assert_eq!(
    &recovered.bytes(first).unwrap()[..first_bytes.len()],
    first_bytes,
    "the first chunk recovered"
  );
  assert_eq!(
    &recovered.bytes(second).unwrap()[..second_bytes.len()],
    second_bytes,
    "the second chunk recovered"
  );

  // A new allocation is disjoint from the recovered extents, so it cannot overwrite live content.
  let fresh = recovered.alloc(PAGE).unwrap();
  let disjoint = |a: slates_mem::arena::Extent, b: slates_mem::arena::Extent| {
    a.offset + a.len <= b.offset || b.offset + b.len <= a.offset
  };
  assert!(
    disjoint(fresh, first),
    "a fresh alloc avoids the first recovered chunk"
  );
  assert!(
    disjoint(fresh, second),
    "a fresh alloc avoids the second recovered chunk"
  );
}
