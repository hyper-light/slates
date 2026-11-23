# Extent claims used mapped length for headroom and growth

On 2026-11-22 at `e7269dae4`, four focused tests failed against the public `ChunkArena` pool
operations on macOS arm64. A host page was 16,384 bytes. The tests use the same best-fit rule
as the server and give the pool mappings of different lengths.

| Operation | Before the fix | After the fix |
| --- | --- | --- |
| Keep three pages of headroom in an idle 16-page arena, with three- and four-page mappings available | Returned zero and kept 16 pages | Returns 12 pages and serves the allocation from four |
| Grow by three pages from that pair of mappings | Claimed both and added six usable pages | Adds four usable pages and leaves the three-page mapping available |
| Grow twice by one byte, with a two-page granule and mappings of one and two pages | First call added zero and lost the one-page mapping. Second call added two pages | First call adds two pages. Second call returns zero and leaves the one-page mapping available |
| Grow an arena configured with an invalid three-page granule | Added zero and lost the four-page mapping | Returns zero with all four pages still in the pool |

## Cause and fix

A buddy region can use only the largest power-of-two number of whole granules its mapping holds.
In `crates/mem/src/arena.rs`, `ChunkArena::claim_smaller` asked for the raw shortfall. The pool
chose the three-page mapping first. Its buddy supplied two pages, so the trade gave it back and
kept the original extent. `ChunkArena::grow` also asked for the raw shortfall and consumed
a mapping that its buddy could refuse.

Both operations now use `ChunkArena::block_len` to ask the pool for a mapping large enough to
supply the missing usable capacity in one region. Growth can still combine smaller regions.
It returns an offer below one granule and checks the arena's granule before claiming.
An idle trade acquires its replacement before giving up the held region.

## Verification

`cargo test -p slates-mem --lib a_pool_claim_for_` exercises the four rows. Each failed before
the fix and passed after it. The daemon already cuts its pool into power-of-two parts in
`crates/server/src/content_pool.rs` (`arena_parts`). These measurements describe the allocator
contract for mixed-size sources. Daemon RSS is measured separately by the recovery test.

Four neighboring tests also passed individually on macOS arm64. The existing idle trade kept
its headroom, growth combined two smaller extents, an admission race returned its partial
claims, and the capacity integration test admitted only what the arena could allocate.
