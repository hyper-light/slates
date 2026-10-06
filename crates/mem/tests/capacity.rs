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
  assert_eq!(extent.len(), capacity);
  arena.free(extent).unwrap();
  budget.release(reservation);
}

/// The kernel's count of this process's locked memory (`/proc/self/status` `VmLck`), in bytes.
#[cfg(target_os = "linux")]
fn locked_by_the_kernel() -> usize {
  std::fs::read_to_string("/proc/self/status")
    .unwrap()
    .lines()
    .find_map(|line| line.strip_prefix("VmLck:"))
    .and_then(|rest| rest.split_whitespace().next())
    .and_then(|kib| kib.parse::<usize>().ok())
    .unwrap()
    * 1024
}

/// A block of `len` bytes allocated locked, or `None` (a loud skip) where the environment refuses mlock, having
/// checked the refusal left nothing locked.
fn locked_or_skipped(
  arena: &mut ChunkArena,
  len: usize,
) -> Result<Option<slates_mem::Extent>, MemError> {
  match arena.alloc_locked(len) {
    Ok(extent) => Ok(Some(extent)),
    Err(MemError::LockRefused { .. }) => {
      assert_eq!(
        arena.locked_bytes(),
        0,
        "a refused lock leaves nothing locked"
      );
      eprintln!(
        "skipped the_arena_locks_the_blocks_asked: the environment refuses mlock (no memlock capacity)"
      );
      Ok(None)
    }
    Err(e) => Err(e),
  }
}

/// The kernel's locked count where it reports one (Linux), else none.
fn kernel_locked() -> Option<usize> {
  #[cfg(target_os = "linux")]
  return Some(locked_by_the_kernel());
  #[cfg(not(target_os = "linux"))]
  None
}

/// Where the kernel reports its locked count, it has moved by exactly `expected` bytes since `before`.
fn assert_kernel_moved(before: Option<usize>, expected: usize, what: &str) {
  if let (Some(before), Some(now)) = (before, kernel_locked()) {
    assert_eq!(now - before, expected, "{what}");
  }
}

/// §4.2 (BUG-1, D-12): the arena locks exactly the blocks asked to be locked (a strict volume's content), never the
/// address space it maps and never another volume's blocks beside them. Do: over a 16-page region, allocate a plain
/// 2-page block and a locked 4-page block; then lock the plain one; free the locked one. Expect: locked bytes are 4,
/// then 6, then 2 pages, never the region; the plain block is unlocked until asked; on Linux the kernel's own count
/// moves by exactly as much. mlock capacity is environment-dependent, so a refusal is a loud skip that leaves
/// nothing locked.
#[test]
fn the_arena_locks_the_blocks_asked_never_the_region_or_their_neighbours() {
  let region = 16 * PAGE;
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(region, PAGE, false).unwrap())
    .unwrap();
  let plain = arena.alloc(2 * PAGE).unwrap();
  let kernel_before = kernel_locked();
  let Some(strict) = locked_or_skipped(&mut arena, 4 * PAGE).unwrap() else {
    return;
  };
  assert_eq!(
    arena.locked_bytes(),
    4 * PAGE,
    "only the block allocated locked is locked"
  );
  assert!(!arena.is_locked(plain), "the plain block beside it is not");
  assert!(arena.is_locked(strict));
  assert_kernel_moved(
    kernel_before,
    4 * PAGE,
    "the kernel counts exactly the locked block",
  );
  arena.lock_extent(plain).unwrap();
  arena.lock_extent(plain).unwrap();
  assert_eq!(
    arena.locked_bytes(),
    6 * PAGE,
    "locking an existing block locks it, once"
  );
  arena.free(strict).unwrap();
  assert_eq!(arena.locked_bytes(), 2 * PAGE, "a freed block is unlocked");
  assert_kernel_moved(
    kernel_before,
    2 * PAGE,
    "the kernel's count falls with the free",
  );
  assert!(
    arena.locked_bytes() < region,
    "the region's other pages are never locked"
  );
  let again = arena.alloc(4 * PAGE).unwrap();
  assert!(
    !arena.is_locked(again),
    "a block reusing a freed locked block's space starts unlocked"
  );
  arena.free(plain).unwrap();
  arena.free(again).unwrap();
  assert_eq!(arena.locked_bytes(), 0);
}
