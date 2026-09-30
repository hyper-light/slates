//! AUD-29-10 (§4.2): an arena frees only the live extents it issued. A caller cannot construct an extent
//! (its fields are private: the `compile_fail` examples on `Extent` and `buddy::Block`), so what a caller
//! can still attempt is a copy: kept after its free and the reuse of its place, freed twice, or handed to
//! another arena. Each is refused by name with the arena's totals unchanged, and the arena keeps serving.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_mem::arena::{ChunkArena, Extent};
use slates_mem::region::Region;
use slates_mem::{ExtentRefusal, MemError};

/// Shape: the granule (a common base page; the arithmetic under test needs no real page size).
const PAGE: usize = 4096;
/// Shape: one region of two pages, so a reuse lands on the freed place.
const REGION: usize = 2 * PAGE;

fn arena() -> ChunkArena {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(REGION, PAGE, false).unwrap())
    .unwrap();
  arena
}

/// The reason `arena` refuses `extent`, which must leave its allocated and free bytes as they were.
fn refusal(arena: &mut ChunkArena, extent: Extent) -> ExtentRefusal {
  let (allocated, free) = (arena.allocated_bytes(), arena.free_bytes());
  let reason = match arena.free(extent) {
    Err(MemError::ForeignExtent { reason, .. }) => reason,
    other => panic!("{extent:?} was not refused: {other:?}"),
  };
  assert_eq!(
    (arena.allocated_bytes(), arena.free_bytes()),
    (allocated, free),
    "{reason:?} changed the totals"
  );
  reason
}

/// AUD-29-10. Do: allocate the whole region, free it, allocate it again (the same place), then free the
/// first copy. Expect: `Stale`; the new owner's extent is untouched (its bytes keep what it wrote) and
/// frees; a second free of it is `NotAllocated`; the region then allocates whole.
#[test]
fn a_copy_kept_past_its_free_and_the_reuse_is_refused_and_the_new_owner_keeps_its_block() {
  let mut arena = arena();
  let first = arena.alloc(REGION).unwrap();
  arena.free(first).unwrap();
  let second = arena.alloc(REGION).unwrap();
  assert_eq!(second.offset(), first.offset(), "the place was reused");
  arena.bytes_mut(second).unwrap()[0] = 7;
  assert_eq!(refusal(&mut arena, first), ExtentRefusal::Stale);
  assert_eq!(
    arena.bytes(second).unwrap()[0],
    7,
    "the new owner's bytes stand"
  );
  arena.free(second).unwrap();
  assert_eq!(refusal(&mut arena, second), ExtentRefusal::NotAllocated);
  assert_eq!(arena.alloc(REGION).unwrap().len(), REGION);
}

/// AUD-29-10. Do: allocate from one arena and free the extent into a second arena of identical geometry
/// holding a live block at the same place. Expect: `OtherArena`; the second arena's block stays live and
/// frees normally; the first arena frees its own extent.
#[test]
fn an_extent_freed_into_another_arena_is_refused() {
  let mut issuing = arena();
  let mut other = arena();
  let theirs = issuing.alloc(PAGE).unwrap();
  let ours = other.alloc(PAGE).unwrap();
  assert_eq!(
    (theirs.region(), theirs.offset()),
    (ours.region(), ours.offset())
  );
  assert_eq!(refusal(&mut other, theirs), ExtentRefusal::OtherArena);
  other.free(ours).unwrap();
  issuing.free(theirs).unwrap();
  assert_eq!(issuing.allocated_bytes() + other.allocated_bytes(), 0);
}
