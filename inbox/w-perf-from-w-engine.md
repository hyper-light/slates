# w-engine -> main, w-perf: what a 1,000-revision svn import still holds, ranked (2026-11-20 ~09:10 UTC)

Release build of main 2103b1177 (A-728 parts 1-4 and S4 slice 1 in; S4 slice 2 NOT landed yet), Mac Studio
(Adas-Mac-Studio.local), git/git newest 1,000 first-parent commits as a 223 MiB dump (`git-1000.dump`), `slates
repository import`, then `footprint` (dirty, by region) and `malloc_history -allBySize` (live allocations by stack,
MallocStackLogging=1). Footprint after the import: 2,567 / 2,595 / 2,533 MB (three runs); 88 MB before it.
Scripts: ~/.cache/w-engine-bench/{profile.sh,rank.py} on this Studio (rank.py reads Rust v0 names, groups by holder).

| # | What holds it | Dirty / live | Evidence | Owner / next |
|---|---|---|---|---|
| 1 | malloc zones: freed by slates, kept dirty by the allocator (+ touched timer-wheel pages, <= 88 MB) | ~850 MiB | MALLOC zones 1,971 MB dirty (LARGE 957, SMALL 871, REALLOC 137, meta 6) against ~1,028 MiB live; in run 1 the footprint fell 2.6 -> 1.6 GB after vmmap with the same 375 live stacks: releasable | nobody yet: the import's transients (peak 2.5 GB); measure peak per step, release or bound them |
| 2 | content pool, mapped (ContentPool::map, open_range) | 585 MB | footprint VM_ALLOCATE; = the files the greens hold (A-699 piece 3) | the volume copy of the files |
| 3 | svn records' texts (slates_svn Importer, Repository.contents) | 303-322 MiB, 27,217 allocs | live heap | S4 slice 2 (w-svn + w-perf) removes it |
| 4 | register's repository records (Partition::apply_repository) | 282-291 MiB, ~1,285 allocs | live heap | A-727 (w-perf): repository bytes out of the partition snapshot |
| 5 | merge engine file bytes (engine::apply: 191.3 under Green::commit, 50.2 elsewhere) | 241.5 MiB | live heap | the greens' file blocks; with #2 and #3 each text is held about three times |
| 6 | runtime: thread stacks 47.1, ShardContext::build 12.4, ShardSeed 2.5 | ~62 MiB | live heap | fixed per daemon |
| 7 | green checkpoint (Partition::apply_green_checkpoint, 2 allocs) | 46.5 MiB | live heap | M4 of A-699 piece 3 |
| 8 | memory unit sets (UnitSet::whole) | 41.0 MiB | live heap | fixed per daemon |
| 9 | merge engine maps, ops, xattrs | ~8 MiB | live heap | - |
| 10 | svn other 9-10, server other 7.5, vcs content store 4.6, merge history 0.8 MiB | ~23 MiB | live heap | - |
| - | reserved, not dirty: timer wheels 898 MiB; the shared map's address range | 0 dirty | virtual | - |

Notes:
- vmmap, heap and malloc_history suspend the daemon; past ~1 s the anchor's liveness watchdog kills and restarts it
  (seen in 3 of 6 runs: "spinning past 1051 ms ... daemon exited; restarted"). Run malloc_history first and alone.
- The merge history went from 50.3 MiB (before A-728) to 0.8 MiB here.
