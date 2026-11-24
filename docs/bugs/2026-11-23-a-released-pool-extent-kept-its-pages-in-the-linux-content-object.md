# A released pool extent kept its pages in the anchor's content object on Linux (2026-11-23)

## Description
`crates/server/tests/recovery.rs`, `a_bursts_extents_go_back_to_the_system_once_its_volume_is_gone`, failed on Linux.
CI's ThreadSanitizer lane read "0 MiB before, 68 MiB after, for a 64 MiB burst" (run 45235122249, main 62d22aefe), and a
plain debug build on adacachy2 read the same shape at main db3ddff02: "the pool held 8 MiB in RAM before, 330 MiB with
it, 73 MiB after its volume was destroyed". The daemon's arenas gave their extents back (256 MiB held to 4 MiB), but
the anchor's content object stayed at 69 MiB in RAM for the test's whole 20 s wait. The test was written and made
green on macOS (A-789, `docs/bugs/2026-11-22-a-shard-kept-a-1-gib-extent-for-10-mib-of-headroom.md`). No Linux lane
reached it before now: CI's `gates` job stopped at fmt or clippy in every main run checked back to 11-22 14:31, so the
TSan lane was the only Linux lane running the recovery suite.

## Root cause
On Linux a pool extent is a range of the anchor's one content object, a memfd (`ContentPool::map`). A memfd's pages
belong to the object, not to a mapping, so they outlive every mapping of them. `ContentPool::release` dropped the
extent's mapping and called `give_back`, which acts only where an extent is an object of its own (macOS: its name is
removed) or a section the anchor holds (Windows: the section is closed). On Linux it did nothing, so the destroyed
burst's pages stayed in the object. The design assumed the opposite ("Linux keeps one memfd with holes punched",
A-789's status), but the only hole punch was the idle purge's, which gives back the free runs of the regions an arena
still holds. A publication's `Store::release_idle` returns a wholly free extent at once, before any idle purge reaches
its runs, and from then on no purge walks it.

## Impact
- Every Linux daemon kept the RAM of each extent it gave back to the pool: a destroyed volume's burst, an extent a
  shard traded for a smaller one (`ChunkArena::shrink_trading`), and every extent a shard returned to answer another's
  want (A-853). The pages stayed until another claimant of the same extent overwrote them. MEMORY SIZING's rule, learned
  and given back, did not hold for released extents on Linux.
- The bytes were zeros (a freed block is scrubbed, A-99), so no data crossed volumes. The cost was RAM only.

## Edits
- `crates/server/src/content_pool.rs`: `ContentPool::release` punches the extent's hole before it drops the mapping
  (`ContentPool::punch`). On Linux, where the extent is a range of the content object, it calls `Region::discard` over
  the whole extent. That is `MADV_REMOVE`, which on shmem punches the object's hole: madvise(2) says it frees the range
  and its backing store, the same as `fallocate(FALLOC_FL_PUNCH_HOLE)`. The idle purge already gives a free run back
  with this call. The pages leave every mapping of the object, the anchor's included. A refusal is counted in the
  pool's refusals (`content.pool_refused`), as a refused give-back is on macOS. Elsewhere `punch` does nothing, since
  `give_back` frees the extent whole.
- Chosen over `fallocate(PUNCH_HOLE | KEEP_SIZE)` on the memfd after the unmap. Both end in the same shmem hole punch.
  `MADV_REMOVE` needs only the mapping the release already holds and the reviewed `Region::discard` path, while the
  descriptor call would add a new file syscall to `slates-mem`. g226's probe of both (BENCHMARKS, "Content give-back
  per platform") found that either frees the pages in every mapping at once.

## Evidence
- Red, adacachy2 (CachyOS, 32 CPUs, plain debug build, load 13-16), main db3ddff02: "8 MiB before, 330 MiB with it,
  73 MiB after" (the content object 69 MiB, the arenas' extents 4 MiB).
- Green with the punch, the same machine and build at load 54: "8 MiB before, 330 MiB with it, 12 MiB after". Punching
  the burst's 256 MiB extent with 68,042,752 bytes resident took 170,559 ns, measured around the call in that run.
- macOS unchanged (`punch` does nothing there): the test passes as before.

## Siblings found, not changed here
- Windows with no anchor holding the extents' sections (`content_extents::installed()` false): an extent is a range of
  the content object's section and a release only unmaps it. `Region::discard` there is `DiscardVirtualMemory`, which
  does not return a section's commit (g226: decommitting a view is refused, error 87). Its give-back needs jupiter or CI.
- `ChunkArena::remove_region` takes a region out without lowering `resident_free` by the region's recorded free runs.
  The gauge (`mem.gauge.content.arena_resident_free_bytes`) overcounts until the next complete purge recounts it.
