# The pressure hold was split per shard while the content pool is shared

**Found:** 2026-10-09, reading the content pool (A-98) beside the pressure hold, while fixing
`docs/bugs/2026-10-09-the-pressure-hold-counted-other-processes-swings-on-a-roomy-host.md` (its sibling note).
**Status: fixed** 2026-10-10.

## Description

On a daemon of two shards with no volumes, the sampler was shown a host whose free memory fell by 73 pool units
(19.1 MB). Each shard's room, the `available` a refused oversized create names, fell by 9.6 MB, half the hold. With a
volume already on each shard, an 11.4 MB hold shrank the room of the shard that still had room by 5.7 MB, again half.

`a_pressured_daemons_hold_is_withheld_whole_from_a_volume_on_any_one_shard` and
`a_hold_the_pool_cannot_cover_is_taken_from_the_free_room_the_shards_hold` (`crates/server/tests/recovery.rs`)
reproduce both. Before the fix each failed on its first run with exactly half the hold.

## Root cause

`refresh_pressure_hold` (`crates/server/src/daemon.rs`) divided the daemon's hold evenly among the shards, as each
shard's ledger hold. A shard backed its share only when it admitted something: the deficit it claims for counts its
hold.

That matched the design before A-98, when each shard could fill only its own slice. Since the pool, one volume on one
shard can reach every free extent. A shard admitting a large volume held back only its own share; the other shards'
shares sat on shards that claimed nothing and so withheld nothing.

The invariant the hold keeps (§4.2, amended 2026-10-09) is daemon-wide: what the daemon could still fill must fit in
what the host has free. The split kept it only when every shard admitted at once.

## Impact

- **Under real host memory pressure, one busy volume escaped most of the hold:** all but 1/N of it on an N-shard
  daemon. That is four fifths on this machine's 5-shard default, and 17/18 on an 18-core host.
- **The workload it hits is the common one:** an agent's install or build in one volume while the host is short. In a
  container with a memory limit and no swap (the KIND lane's shape), the daemon could promise memory the cgroup did not
  have, so the OOM killer, not admission, decided.
- **Takeovers were not affected:** a takeover's recovery runs past the hold.

## Exact edits

- `crates/server/src/content_pool.rs`: the hold's part withheld from the pool.
  - `POOL_HOLD_OWNER` (`u16::MAX`, never a partition) and `PoolGeometry` (every extent with its length).
  - `hold_toward` claims free extents under that owner, greedily, largest first, never past the target; the part
    lengths are divisible, so greedy is optimal. It releases what no longer fits.
  - `hold_bytes` reads what the hold owns.
  - `ContentPool` hands out the hold's extents only while open (`ExtentSource::open_hold`, `claim_from_hold`), and
    counts them as available only then.
- `crates/mem/src/budget.rs` `Pressure`, the debt: the part of the hold that neither the pool's whole extents nor any
  shard's free room covered, owed by whichever shard next grows from the pool.
  - `ShardBudget::debt`, `absorb_debt`, `mark_room`, `set_pressure`; `pressure` and `restore_pressure` save and restore
    the state.
  - Growth since the sampler read a shard's room takes on the debt the refresh sends back, so a refresh racing a growth
    never drops it.
  - Unit tests: `a_debt_becomes_hold_when_the_shard_grows_from_the_pool`,
    `capacity_grown_since_the_samplers_read_takes_on_its_debt`.
- `crates/mem/src/arena.rs` `make_room` grows by the deficit and the debt together, all or nothing, then absorbs the
  debt; `open_hold` passes through. Unit test: `making_room_takes_on_the_pressure_debt`.
- `crates/vfs/src/volume.rs`:
  - `Store::admittable` counts what the pool could add, less the debt;
  - `release_idle` keeps the hold, so room taken from a shard is not handed back to the pool for others to claim.
- `crates/server/src/daemon.rs` `refresh_pressure_hold`, now async on the control shard:
  - it samples the daemon-wide hold (`sample_pressure_hold`) and withholds it from the pool first (`hold_toward`);
  - what remains, it takes from the shards' free room in proportion to it (`free_room_of` asks each shard,
    `apportion` divides it exactly), and leaves the rest as debt;
  - it sends each shard's hold and debt only when they change.
  - Unit tests of `apportion`: the parts sum exactly and never exceed a room.
  - Observations: `Daemon::pressure_pooled`, `Daemon::pressure_withheld`.
- `crates/server/src/fleet.rs` `past_the_pressure_hold`: a takeover's recovery also opens the pool's hold, and saves and
  restores the debt, keeping the 2026-10-07 fix.
- `crates/server/tests/recovery.rs`:
  - the two tests above;
  - `a_hold_finer_than_the_pools_free_extents_is_owed_by_the_next_shard_to_grow`: slices of a 128-unit part and a
    1-unit part, and a 64-unit hold. Each shard's room falls by exactly 64 units, and the shard that grows takes the
    62-unit debt as hold. One more extent would have held 128 units; leaving the remainder out would have held 2.
  - The three sampler tests now check the whole hold withheld (`pressure_withheld`), not the first shard's share. Their
    "nothing wrongly held" checks read only shard 0's ledger, which a hold in pool extents never moves, so they could not
    have caught this.

## Why a debt, not one more extent

Under a derived layout a slice cuts into only a few parts: this machine's 9.2 GB slice is 8 GiB, 512 MiB, 32 MiB and
2 MiB. A 2.3 GB hold on a daemon holding nothing can be withheld in whole parts only as 1.1 GB, leaving 1.2 GB.
- Claiming the next extent up would hold back 8 GiB for it.
- Leaving it out would let the first shard to grow take it.
- As a debt, it is held by whichever shard grows into those extents, at the growth itself.

Every shard is told the same debt. So two shards growing within one refresh both hold it: conservative, never under,
and recomputed exactly at the next refresh.

## Siblings

- The sibling note in the 2026-10-09 roomy-host bug doc is this bug; marked fixed there.
- `Daemon::pressure_hold` (shard 0's ledger hold) still answers for the nfs_mount test that injects holds by hand; the
  sampler's tests read the whole hold.
