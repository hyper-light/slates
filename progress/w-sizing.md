# w-sizing -- end gross over-allocation, slates-wide (brief: briefs/w-sizing.md; supervisor "main")
# Machine: Adas-Mac-Studio-4.local (14 cores, 36 GiB), local builds, target ~/.cache/w-sizing-local-target.noindex
# Clone ~/slates-work/w-sizing-main (no checkout); worktree ~/slates-work/w-sizing-seg (branch agents/w-sizing)
# Logs/patches ~/.cache/w-sizing-logs

## Piece 1: the segment's prefault reserve -- LANDED (A-765)
- LANDED main 6337049f6 (piece 6ea16f4c5), confirmed merge-base. Worktree ~/slates-work/w-sizing-seg.
- Cause (base 159f1c873): the idle reserve (A-214) = min(last burst, reserve_per_shard, arena free), no decay; the sweep
  settled with it held, so an idle shard held its largest burst forever. reserve_per_shard = memory / shards / 3.
- Fix: daemon::decayed_reserve (burst whole one sweep interval, then halved per interval); the sweep stays armed while a
  reserve is held; below one discard it is zero.
- Numbers (macOS arm64, Studio-4, local): 64 MiB burst, 20 s idle: reserve 67,108,864 B before -> 0 after.
  Tests: lib decayed_reserve 1/1; recovery an_idle_shards_reserve_decays + a_deleted_files_memory_goes_back 2/2
  (also on the merged tree).
- Finding (probe ~/.cache/w-sizing-probe/shm_probe.c, 64 MiB shm_open object): MADV_FREE_REUSABLE drops the footprint
  64 -> 0 MiB but RSS and mincore stay 64; MADV_FREE/DONTNEED do nothing; MADV_PAGEOUT refused. So on macOS RSS/vmmap
  "resident" overstates given-back shared memory; facts::resident_now (pressure hold's own growth) reads RSS.
- Finding: content.purged_bytes counts discarded ranges, not resident pages (10.3 GB "given back" for a 64 MiB burst).
- Finding: content_range.arena.resident read 0 with 64 MiB allocated in the red run (a stale sweep).

## Queue
2a. (main's input, g226) address space: ~220 GB mapped on the 96 GB Studio. By reading: each shard's content slice =
   write log R/4 + images 2R + delta log R + arena R (R = reserve_per_shard = memory/shards/3) ~ 1.42 x memory, plus
   rings/snapshot slots. Size it from the work (the arena already grows from the pool's extents, A-98). Use g226's
   in-process region report in `slates status`, never vmmap on an anchored daemon.
2. Sweep of every sizing site -> docs/wip/GAPS.md entry; convert machine-fraction ones, largest first.
3. Device pressure seam (crate::pressure / slates_machine::memory_pressure exist: A-570) consulted by sizing sites.
4. Zero-alloc on data paths. Seen by reading: the idle purge allocates per pass (prefault_reserve -> Vec,
   arena.order.clone(), pieces_against -> Vec per run).

## For main to delete
- (none)
