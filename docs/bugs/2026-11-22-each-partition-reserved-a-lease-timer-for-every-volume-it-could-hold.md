# Each partition reserved a lease timer for every volume it could hold: 595 MiB of address space a shard at idle (2026-11-22)

## Description
After (b1) split the memory report, an idle two-shard daemon's `large` row read 12 mappings, 1,298,595,840 bytes
reserved and 30.4 MB resident (the MacBook, a debug build of main b79b54350). main's r74 (the big Studio, an svn import
idle) read the same shape: 15 mappings, 972 MB reserved, 24 MB resident. Main's ruling (b2): size each block from the
work, not the machine, and hold the reserved bytes. On Windows, which charges commit for reserved memory, a thirteen-
daemon CI run already met the commit limit.

## Root cause
A temporary probe printed a backtrace for each block the large path mapped. Of the 12 live blocks:
- 2 of 595 MiB: `slates_db::partition::Partition::new` → `slates_rt::timer::Wheel::new` → `Slab::preallocated`, the
  partition's lease-expiry wheel. Its bound is `PartitionCaps::timers`, which the server sets to the partition's volume
  bound (`crates/server/src/config.rs`, `timers: volumes.get()`). `Wheel::new` reserves its whole bound at once (so the
  runtime's own wheel never allocates on insertion; the 2026-09-19 note in `Wheel::new`), so every partition held a slot
  of 48 bytes for every volume it could ever hold, leases or none.
- 2 of 2 MiB: `ShardContext::build` → `Wheel::new`, the runtime's own timer wheel (hot path, preallocated by design).
- 5 of 1 MiB: `SparseObject::create`/`open` → `UnitSet::whole` (a shared object's page marks).
- 2 of 1 MiB: `slates_rt::registry::register_slot` → a control `sync_channel`.
- 1 not logged (it was mapped while the probe's own capture held its reentrancy guard).
The partition's other tables (volumes, snapshots, attachments) already grow by segments up to their bounds
(`Slab::new(caps.segment_slots, bound)`); the lease wheel was the one reserved whole.

## Impact
About 595 MiB of address space reserved per shard, at idle, on every platform: 96% of the large path's reservation
here. On macOS and Linux it cost almost no RAM (pages are touched only as slots fill), but it broke "size from the
work", inflated every report of the daemon's reservation, and on Windows it counts against the commit limit.

## Edits
- `crates/rt/src/timer.rs`: `Wheel::growing(tick_ns, segment_slots, max_timers, now_ns)`: a wheel whose slab grows
  `segment_slots` at a time up to the bound, for a wheel off the hot path. `Wheel::new` is unchanged and still serves
  the runtime's wheel.
- `crates/db/src/partition.rs`: the lease-expiry wheel is `Wheel::growing` over the partition's own segment size, as
  its tables beside it are.
- `crates/db/tests/lease_wheel.rs` (new; `LargeMapped` as its global allocator):
  `an_empty_partition_maps_no_lease_timer_slots_for_its_bound`.

## Evidence
- Before: the new test, with the partition on `Wheel::new`, at a bound of 4,194,304 timers: "the large path mapped
  201326592 bytes more", red.
- After: "mapped 0 bytes more", green. `slates-rt --lib timer` 6/6; `slates-db --test model` 27/27 (leases armed and
  lapsed through `expired_leases`); `--test snapshot_cost` 3/3; `slates-server --lib lease` 36/36. An idle two-shard
  debug daemon's `large` row: 12 mappings, 1,298,595,840 bytes reserved before; 7 mappings, 12,353,536 bytes after.
- Not in this piece: the other blocks (the runtime's wheels and the 1 MiB mark sets and channels, about 10 MiB), each
  a fixed or deliberately preallocated size.
