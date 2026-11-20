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

## Piece 1b (main's 4 items after A-765), in flight
- w-hg's split (release 1d4909821, after hg history push): RSS 8,520 MiB; prefault_reserve 4,931.4 MiB = whole burst;
  20 s idle -> 12,055 MiB (+3.5 GB refill). Content retained 4,495.5 MiB is w-hg's.
- Item 4 DONE (probes ~/.cache/w-sizing-probe): REUSABLE on shm: footprint 513->1 MiB, RSS 513 stays, NOT reclaimed in
  10 s under pressure (reusable_reclaimed +0); frees only when the object is destroyed. Purgeable Mach memory entry +
  VM_PURGABLE_EMPTY: RSS 513 -> 1 MiB at once with two mappings, reads zero after NONVOLATILE, whole-entry granularity.
  => true give-back on macOS needs arena extents as purgeable entries shared with the anchor by port (design piece).
- Item 1 A/B: kiln run r1 (queued): ab/run.sh in ~/slates-work/w-sizing-seg (untracked ab/): A = main 6337049f6, B =
  ab/no-prefault.patch (reserve 0); provision_bench + nfs_large_write, release, interleaved x3, /usr/bin/time -l.
- Sweep list LANDED main 79e0a4723 (GAPS, MEMORY SIZING entry: every site, today -> target).
- A/B r1 lost its placement before starting; resubmitted as r2, started on d1323898.
- LANDED main 2dc414a84: idle purge walk allocation-free (for_each_piece, no order clone; oracle test). mem arena:: 19/19.
- A/B: r2 (sync not answered by d1323898), r4 (d1323898 silent 15 s), r3 stopped (no room at mem=24g); r5 = no-sync
  remote clone of origin/main cbf7c84c3 + inline patch, needs cpus=4,mem=8g: QUEUED (macOS fleet full). Local Studio-4 is
  at load 26 (A/B here would be noise).
- Correction (probe): on macOS RSS is the honest holding; footprint UNDERstates (given-back shared pages stay physical,
  not reclaimed in 10 s under pressure). resident_now (RSS) stays; GAPS line "(open)" to be corrected.
- By reading: reserve_per_shard's tables (inodes, dirs, chunks, tags, clients) are segmented slabs grown on demand,
  bounds only; its RAM cost is the prefault cap + admission; the rest is address space.
- True macOS give-back design (to scope with main): pool extents as purgeable Mach memory entries (VM_PURGABLE_EMPTY frees
  while the anchor still maps it), handed to the anchor by port; replaces shm_open's one content object on macOS.
- No-prefault removal written, tested, NOT landed: branch agents/w-sizing-noprefault (1fe113d5b + dbf369922, base
  5518fb2bc): -907 lines (reserve, refill, burst fields, status row, arena prefault/upcoming/KeptRun, Buddy::upcoming);
  red/bound test a_burst_leaves_nothing_resident_behind_it_but_its_data (Linux). Local: mem 36/36, server idle_sweep 2/2,
  recovery deleted-file 1/1. To land: fresh worktree from origin/main, squash, docs (A-214 status, GAPS, BENCHMARKS).
- A/B now branch-based (A = branch~2, B = branch tip), no sync: r6 macOS, r7 Linux (both queued). r5 was stopped by me
  just as it started on d1323898 (mistake).
- A/B DONE (r6, big Studio, release, interleaved x3): provisioning 1-client p99 A 226.7/197.4/203.5 vs B
  232.5/200.6/182.3 us (both fail 50 us at load 9-31 alike); NFS best equal; kept rounds ~5% to A; NFS peak RSS A
  1,137-1,139 vs B 625-627 MiB. => removal.
- A-777 LANDED main 21a306a04 (reserve removed, -907 lines; BENCHMARKS entry with all N; design A-214 status + A-777
  section; GAPS). Worktree ~/slates-work/w-sizing-land. Branch agents/w-sizing-noprefault kept until r7 (Linux A/B) runs.
- r12 (kiln, d1323898, its hg source ~/.cache/w-hg-mem/hg 84 MB): hg history push before (47dc24b72) vs after
  (21a306a04), interleaved BAAB: rss, top MEM, system free/reusable, status content ranges after push and after 20 s.
- Proposal to main: inbox/main-from-w-sizing-content-extents.md (content as extents made on demand; needs ruling).
- Then item 2 (if prefault pays: EWMA of pages turns touch) or remove; item 3 reserve_per_shard work-derived + pressure.
- Disk here 14 GiB free (others' caches; mine 2.5 GB).

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
