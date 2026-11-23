# A shard kept a 1 GiB extent for 10 MiB of headroom after its burst's volume was gone, and the burst test measured the process, not the pool (2026-11-22)

## Description
`crates/server/tests/recovery.rs`, `a_bursts_extents_go_back_to_the_system_once_its_volume_is_gone`, failed since its
own commit, 362bd8c39 (2026-11-20; main's bisect on the big Studio: 56 MiB before, 1,358 MiB after, the same as at
main 7240125cf). On the MacBook: "the burst's extents went back to the system: 63 MiB before, 1365 MiB after, for a
256 MiB burst". The content object held 2 MiB; the 1.3 GiB was elsewhere.

## Root cause
`vmmap -summary` of the test process at the test's three points, and a print of `Store::release_idle`'s inputs, named
two causes.

1. **The pool kept the extent.** The burst's volume reserves 512 MiB, so the shard's arena grew by the pool part that
   covers its need (A-848's best fit): one 1 GiB extent. After the destroy, `Store::release_idle` keeps the shard's
   demand: 0 committed, 10 MiB of operation headroom, 0 hold. The arena's only region was that extent, wholly free.
   `ChunkArena::shrink` skips a region whose return would leave capacity under what it keeps: 1,024 − 1,024 = 0 MiB <
   10 MiB. So the shard held the whole 1 GiB extent for 10 MiB of headroom. vmmap showed its 19 mappings still there,
   264.8 MiB resident but only 7.5 MiB dirty: the idle purge's `MADV_FREE_REUSABLE`, which (362bd8c39's probe) leaves a
   shared object's pages resident.
2. **The measure counted the process.** `pool_resident` added the content object's residency to the process's RSS. In
   this test the client and the daemon are one process, so RSS also counted the test's own 256 MiB payload and the
   client's and the write path's copies of it: 1.0 GiB in 9 `MALLOC_LARGE (empty)` regions, freed large blocks macOS's
   allocator keeps dirty. No give-back of the pool could return those.

## Impact
- Every shard whose burst grew its arena by a large extent kept that extent after the work was gone, until another
  shard's want made it give back (A-853) or the daemon stopped. On the Studio, whose parts are larger, that was a 1 GiB
  extent per burst. The design's rule ("learned, given back") did not hold for the last extent of an arena.
- The test could never pass while it read process RSS in-process.
- Not in this change: a separate daemon's heap. main's r41 found a real daemon (LargeMapped as its allocator) holding
  about 1.3 GB of heap resident against 31 to 60 MB live. That is its own piece, (b), the idle heap give-back.

## Edits
- `crates/mem/src/arena.rs`: `ChunkArena::shrink_trading`: after `shrink`, each wholly free region kept only for
  `keep_bytes` is traded for the smallest extent the source holds that covers the shortfall. The small extent is
  claimed first and the large one returned after, so capacity never falls under what is kept. A smaller extent that
  does not cover the shortfall is given back. `make_room`'s rollback still uses plain `shrink`, which never claims.
  Test: `an_idle_arena_trades_a_large_free_region_for_the_smallest_that_keeps_its_headroom` (a best-fit `FittingPool`
  of a 64-granule and a 4-granule region).
- `crates/vfs/src/volume.rs`: `Store::release_idle` gives back through `shrink_trading`.
- `crates/server/tests/recovery.rs`: `pool_resident` measures the pool by its own pages (main's ruling (a)): the content
  object's residency plus the bytes of the extents the arenas hold, from the daemon's memory report (`arenas`). A
  released extent is unmapped and unlinked, so a held one bounds its pages.

## Evidence
- Before, the MacBook, main 646de255c: with the new measure and plain `shrink`, "0 MiB before, 1026 MiB with it, 1026
  MiB after its volume was destroyed", red.
- After: "0 MiB before, 1026 MiB with it, 18 MiB after" (16 MiB held: the trade's small extent), green.
  `slates-mem --lib arena` 18/18; `slates-server --lib content_pool` 3/3; `slates-server --test recovery` as recorded in
  the landing.
