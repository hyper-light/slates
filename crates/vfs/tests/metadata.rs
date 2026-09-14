//! §4.2 all-cost charging, the metadata dimension (GAP-A9-1): a shard's metadata class is laid out
//! before it serves — every slab's maximum footprint comes off the top, at the true cost of a slot,
//! and the remainder is the ledger each volume's records (its journal budget, its object, its
//! snapshot slab's first segment) are reserved from. So the heap the shard's metadata may take is
//! bounded by the class, not by the machine: "an uncharged heap allocation cannot sit outside the
//! bound". These tests drive the layout and the reservation and assert what is admitted.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{PAGE, store};
use slates_mem::MemError;
use slates_vfs::volume::Volume;

/// A class the slabs alone would exceed is refused whole (a boot learns its layout is wrong before
/// serving); a class above them leaves exactly the remainder as the records' ledger, and a volume's
/// records reserve from it and return on release.
#[test]
fn the_metadata_class_takes_the_slabs_off_the_top_and_offers_the_rest_to_volume_records() {
  let mut store = store();
  let slabs = store.slab_footprint_bytes();
  assert!(slabs > 0, "the fixture's slabs have a footprint");
  assert_eq!(
    store.set_metadata_class(slabs),
    Err(MemError::BudgetExceeded {
      requested: slabs,
      available: slabs
    }),
    "a class the slabs fill entirely leaves no room for a single record"
  );
  let journal: usize = 1 << 16;
  let footprint = Volume::metadata_footprint(journal, PAGE);
  assert!(
    footprint >= u64::try_from(journal).unwrap(),
    "a volume's records cost at least its journal budget"
  );
  let class = slabs + 2 * footprint;
  assert_eq!(
    store.set_metadata_class(class).unwrap(),
    2 * footprint,
    "the ledger is the class less the slabs"
  );
  let first = store.metadata.reserve(footprint).unwrap();
  let second = store.metadata.reserve(footprint).unwrap();
  assert_eq!(
    store.metadata.reserve(footprint),
    Err(MemError::BudgetExceeded {
      requested: footprint,
      available: 0
    }),
    "a third volume's records do not fit the class"
  );
  store.metadata.release(second);
  assert_eq!(
    store.metadata.admittable(),
    footprint,
    "a released record's bytes back another volume"
  );
  store.metadata.release(first);
  assert_eq!(store.metadata.committed(), 0);
}
