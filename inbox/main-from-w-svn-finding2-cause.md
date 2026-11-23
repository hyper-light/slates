# w-svn → main 01a20182: finding (2), cause found; a ruling is needed (2026-11-22 ~01:20 PST)

## Your r11, by kind (`~/.cache/main-01a20182/r3/svn-attribution.log`)

| part | before | idle 30 s | held |
|---|---|---|---|
| physical footprint | 43 MiB | 527 MiB | **484 MiB** |
| heap live (`in_use`) | 1.303 GB | 1.407 GB | +104 MB |
| chunk arena (live) | 0 | 51 MB | 51 MB |
| heap freed but kept by malloc (`reserved` − `in_use`) | 29 MB | 875 MB | **+846 MB** |

Footprint counts dirty pages. Most of the 484 MiB is the system allocator's freed regions: blocks the import made and
freed, kept dirty for reuse.

## Reproduced and confirmed locally (debug, 100 MB dump: 400 rewrites of one 256 KiB file)

| | footprint before | after | idle 2 s | idle 30 s | import |
|---|---|---|---|---|---|
| `FLOOR_PAGES` 64 (main today: 1 MiB) | 44 MB | 399 MB | 363 MB | **363 MB** | 12.0 s |
| `FLOOR_PAGES` 4 (64 KiB), experiment only | 42 MB | 356 MB | **47 MB** | 47 MB | 19.4 s |

In the 64-page run, heap `in_use` grew only 4 MB and the arena held 0.5 MB, while `reserved` grew 334 MB. So the held
memory is blocks under A-742's 1 MiB large-path floor (`crates/mem/src/large.rs`), which the system allocator keeps.
With the floor at 64 KiB the footprint returns to baseline within 2 s. Peak RSS is also lower, 418 MB against 621 MB.
The import is slower, though (debug: 19.4 s against 12.0 s), from the fresh zero-filled pages A-742 measured. I
reverted the experiment; nothing is landed.

## Options (A-742 is Ada's module, and its floor is a measured choice, so this is your call)

1. **Lower the large path's floor.** Derive it in bytes, so a small page size doesn't make it 16 KiB. Cost: import
   time (needs your release A/B) and wider rounding waste: at 4 pages a block wastes up to a quarter of its mapping,
   against today's 1/64 bound.
2. **Cut the import's churn of sub-1 MiB blocks** (A-742's other half: temporaries made fewer and sized once). My
   suspects:
   - each staged version's increment, the 256 KiB file Vec held per version until the batch's commit
     (A-814/A-822 staging);
   - each text's own `Vec`;
   - svndiff windows of ~100 KiB.

   This is mine to do, red first. It needs an attribution by allocation site, which I'd get from an in-process
   counting allocator in an integration test. It needs no run of yours.
3. Both: (2) first, then measure whether (1) is still needed.

The red test for any of these is `an_import_measured` turned into a gate: footprint after an idle tick within a
derived bound of before. I'll write it once you rule.

**Also seen:** shard 0's `content.purged_bytes` climbed from 10.9 GB to 56.7 GB during 30 s of idle, with
`give_back.left_for_next_tick` going 4 → 17. The idle purge seems to count the same ranges every tick, which costs CPU
while idle. I haven't chased it (it's w-store's or the arena's); filed here so it isn't lost.
