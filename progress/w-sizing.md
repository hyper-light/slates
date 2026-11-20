# w-sizing -- end gross over-allocation, slates-wide (brief: briefs/w-sizing.md; supervisor "main")
# Machine: Adas-Mac-Studio-4.local (14 cores, 36 GiB), local builds, target ~/.cache/w-sizing-local-target.noindex
# Clone ~/slates-work/w-sizing-main (no checkout); worktree ~/slates-work/w-sizing-seg (branch agents/w-sizing)
# Logs/patches ~/.cache/w-sizing-logs

## Piece 1: the segment's prefault reserve (in flight)
- Found (base 159f1c873): the idle shard keeps `prefault_reserve_bytes` = min(last write burst, reserve_per_shard,
  arena free) pre-faulted (A-214), with NO decay: it re-faults and holds its largest burst for as long as it stays
  idle; the sweep then settles and never wakes again. reserve_per_shard = effective memory / shards / 3 (machine
  fraction: ~1 GiB a shard on the 96 GB Studio at 32 shards). The content arena lives in the anchor's shared object,
  so this shows as shared memory, not heap.
- Fix: reserve = burst whole for one sweep interval after the pass that closed it, then halved each interval
  (`daemon::decayed_reserve`); the sweep stays armed while a reserve is held; under one discard size it is zero.
- Red: crates/server/tests/recovery.rs `an_idle_shards_reserve_decays_after_its_burst_and_gives_its_pages_back`
  (one shard, N-byte burst, 20 s idle: reserve < N/4, arena resident <= allocated + N). Running.
- Asked w-hg for r2/r9 numbers (inbox/w-hg-from-w-sizing.md); none on the branch yet.

## Queue
2. Sweep of every sizing site -> docs/wip/GAPS.md entry; convert machine-fraction ones, largest first.
3. Device pressure seam (crate::pressure / slates_machine::memory_pressure exist: A-570) consulted by sizing sites.
4. Zero-alloc on data paths. Seen by reading: the idle purge allocates per pass (prefault_reserve -> Vec,
   arena.order.clone(), pieces_against -> Vec per run).

## For main to delete
- (none)
