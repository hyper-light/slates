//! §4.2 admission over usable capacity, not mapping length (BUG-2, GAP-A9-1). A buddy arena over a
//! region can hand out only its largest power-of-two number of granules, which is below the mapping
//! length whenever the mapping is not itself a power-of-two of granules. A budget must be sized to
//! that usable capacity, or it admits a reservation the arena then cannot back — an over-promise a
//! write discovers as arena exhaustion despite admission having "succeeded". This drives the arena
//! and the budget together and asserts the observable outcome (an allocation), not a constant.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use slates_mem::MemError;
use slates_mem::arena::ChunkArena;
use slates_mem::budget::ShardBudget;
use slates_mem::region::Region;

/// Shape: the granule (base page) the arena and budget share.
const PAGE: usize = 4096;

/// A region of three pages: three granules, whose largest power-of-two is two, so the arena's
/// usable capacity (two pages) is below its three-page mapping.
#[test]
fn a_budget_over_usable_capacity_never_over_promises_what_the_arena_can_back() {
  let mapping = 3 * PAGE;
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(mapping, PAGE, false).unwrap())
    .unwrap();

  let capacity = arena.capacity();
  assert_eq!(
    capacity,
    2 * PAGE,
    "the buddy hands out the largest power-of-two of granules, not the whole mapping"
  );
  assert!(
    capacity < mapping,
    "usable capacity {capacity} is below the {mapping}-byte mapping"
  );

  // The bug: a budget sized to the mapping length admits three pages, but the arena cannot allocate
  // them — admission over-promised.
  let mut over = ShardBudget::new(u64::try_from(mapping).unwrap(), 0);
  let _admitted = over
    .reserve(u64::try_from(mapping).unwrap())
    .expect("a mapping-sized budget wrongly admits the whole mapping");
  assert!(
    arena.alloc(mapping).is_err(),
    "the arena cannot back what the mapping-sized budget admitted (the over-promise)"
  );

  // The fix: a budget sized to the usable capacity refuses the over-large reservation, and every
  // reservation it does admit the arena can actually allocate.
  let mut budget = ShardBudget::new(u64::try_from(capacity).unwrap(), 0);
  assert!(
    matches!(
      budget.reserve(u64::try_from(mapping).unwrap()),
      Err(MemError::BudgetExceeded { .. })
    ),
    "a capacity-sized budget refuses beyond what the arena can back"
  );
  let reservation = budget
    .reserve(u64::try_from(capacity).unwrap())
    .expect("the usable capacity is admissible");
  let extent = arena
    .alloc(capacity)
    .expect("what the budget admitted, the arena backs");
  assert_eq!(extent.len, capacity);
  arena.free(extent).unwrap();
  budget.release(reservation);
}
