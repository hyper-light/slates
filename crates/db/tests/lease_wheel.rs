//! A partition's lease-expiry wheel sized by its leases, not its bound (main 01a20182's (b2) of 2026-11-22): its own
//! binary, so the slates binary's allocator ([`LargeMapped`]) is the global one and its figures read what a partition
//! maps.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_db::partition::{Partition, PartitionCaps};
use slates_mem::large::{LargeMapped, stats};

#[global_allocator]
static ALLOCATOR: LargeMapped = LargeMapped;

/// Shape: the lease timers the partition may hold: a bound of the order a laptop daemon derives (its volume bound), far
/// past what an empty partition holds.
const TIMERS: usize = 1 << 22;

/// Shape: the bytes a lease timer's slot takes at least (its deadline and its waker word).
const SLOT_BYTES_AT_LEAST: usize = 16;

/// Main 01a20182's (b2) (an idle two-shard daemon's 12 large blocks, 1,298,595,840 bytes mapped and 30 MB written: two
/// of them, 595 MiB each, the partitions' lease-expiry wheels, each a slot for every volume it could hold). Do: make an
/// empty partition whose lease timers are bounded at [`TIMERS`]. Expect: the large path's mapped bytes grown by less than
/// a tenth of the bound's slots. Red before: by the whole bound, [`TIMERS`] slots of at least [`SLOT_BYTES_AT_LEAST`]
/// bytes.
#[test]
fn an_empty_partition_maps_no_lease_timer_slots_for_its_bound() {
  let caps = PartitionCaps {
    volumes: TIMERS,
    snapshots: TIMERS,
    attachments: 1 << 12,
    segment_slots: 64,
    timers: TIMERS,
    tick_ns: 1_000,
    green_chain_bytes: 1 << 20,
    audit_records: 1 << 12,
  };
  let before = stats().mapped_bytes;
  let partition = Partition::new(caps, 0);
  let after = stats().mapped_bytes;
  let bound = u64::try_from(TIMERS * SLOT_BYTES_AT_LEAST).unwrap();
  let grown = after.saturating_sub(before);
  eprintln!(
    "an empty partition bounded at {TIMERS} lease timers: the large path mapped {grown} bytes more (bound's slots \
     {bound})"
  );
  assert!(
    grown < bound / 10,
    "an empty partition maps {grown} bytes for its lease timers' bound of {bound}"
  );
  drop(partition);
}
