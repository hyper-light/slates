# A refused admission claimed the whole content pool and kept it

**Found:** 2026-10-09, reading the content pool (A-98) beside the pressure hold. **Status: fixed** (same day).

## Description

On a daemon of two shards with a 32 MiB slice each, a client asked shard 1 for a bounded volume larger than the whole
pool (96 MiB). That create was refused `BudgetExceeded`, as it should be. A create of 8 MiB on shard 0 straight after
was then refused too, `BudgetExceeded { available: 0 }`, with the pool's other slice wholly free a moment before.

`a_refused_admission_leaves_the_pool_to_the_other_shards` (`crates/server/tests/recovery.rs`) reproduces it: before the
fix it failed on its first run with exactly that refusal.

## Root cause

Every admission makes room first (`ChunkArena::make_room`, `crates/mem/src/arena.rs`). The arena claims extents from the
pool until it has added the deficit, then the budget admits or refuses. `make_room` claimed whatever the pool had, even
when the pool could not cover the deficit. The admission it was claiming for was then refused anyway, and the claimed
extents stayed in the refusing shard's arena.

A shard gives wholly free extents back only after a committed publication (`Store::release_idle`). A refused create
publishes nothing. So an idle shard that refused one oversized request held the whole pool until something else
mutated on it, and every other shard's admissions were refused meanwhile.

The budget's own contract (`crates/mem/src/budget.rs`, §4.2) is "refused whole, with nothing changed". The arena's
growth broke it from A-98 on.

## Impact

- **One request starves every shard.** Any client could block volume creates, dynamic growth, retention charges, replica
  holds and takeover restores on every other shard of the daemon by asking one idle shard for more than the pool holds.
  The starvation lasted until that shard happened to publish again.
- **Every admission path, not only creates:** all ten callers of `make_room` had the same claim-then-refuse shape:
  - a bounded reservation and a dynamic growth (`Store::reserve`, `Store::grow`);
  - a dynamic volume's write, allocation and retention (`crates/vfs/src/volume.rs`, `crates/vfs/src/base.rs`);
  - a replica hold's charge (`crates/cluster/src/content.rs`);
  - a takeover's restore (`crates/server/src/verbs.rs`).
- **A race between shards:** two shards making room at once could each take part of what both needed, both refuse, and
  both keep their part.

## Exact edits

- `crates/mem/src/arena.rs` `ChunkArena::make_room`: all of the deficit or nothing.
  - It claims nothing when the pool's free extents (`claimable`) cannot cover the deficit.
  - When another arena claims first, between the look and the claims, it returns what it took (`shrink` down to the
    capacity it had before; fresh regions hold no block and are returned newest first).
  - Unit tests, failing first: `making_room_the_pool_cannot_cover_claims_nothing` and
    `room_another_arena_took_first_returns_what_was_claimed`. The second uses a pool whose look counts one granule
    more than it can hand out.
- `crates/vfs/src/volume.rs` `Store::reserve` and `Store::grow`: a refusal names `Store::admittable` (held capacity plus
  the pool's free extents, less commitments, headroom and hold). With nothing claimed, the budget alone would name only
  what the shard already holds, often 0.
- `crates/server/src/verbs.rs`: a takeover restore refused over budget names the same.
- `crates/server/tests/recovery.rs` `a_refused_admission_leaves_the_pool_to_the_other_shards`:
  - the oversized create is refused, naming at least a slice available;
  - the modest create on the other shard is admitted.

## Siblings

- Swept every `make_room` caller (ten sites, listed under Impact). All are fixed by the one change in `make_room`, since
  none relied on a partial claim. A dynamic quota's growth needs its whole increment (`Quota::admit` asks
  `may_grow(needed)`), so a partial claim never let an admission through.
- Open, separate: the pressure hold is divided evenly among the shards, while the pool lets one shard reach the whole
  capacity (GAPS, 2026-10-09).
