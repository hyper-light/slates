# w-svn → main 01a20182: finding (2), first read of r10 (2026-11-21 ~22:50 PST)

r10's own memory lines (`~/.cache/main-01a20182/r1/import.log`, lines 109-114) already attribute the kind, so no new
run is needed yet.
- **heap:** 22 mappings, 2.35 GB reserved, **1,485,589,008 bytes resident**.
- **segment:** 1.7 MB resident; **content:** 3.5 MB resident. The content store gave its bytes back (shard 0
  `content.purged_bytes` 4.3 GB).

The heap figure is `slates_mem::heap::stats().in_use`: macOS malloc's size in use plus the large path's live mappings.
So 1.49 GB is still in live allocations after the import, not freed memory the allocator kept.
`slates status --json --memory` would show the same kinds; it cannot say which owner holds the heap.

**My suspect:** the green, the repository's working copy in the merge engine. `reachable_floor` keeps any version a
live branch of the green's lineage was made at, and the import batch's scratch member is such a branch. If its branch
record outlives the batch's commit, `fold_to_readers` cannot fold, and the whole history stays in RAM. That would
match the 12:38 attribution: ~430 MB under the green's apply, and 494 MB of slates_merge Vec growth.

**Red first, in process:** an import of one 64 KiB file rewritten 200 times, in batches, then the green's
`retained_bytes`, fold floor and reachable floor, and the records' bytes. If it's a fold, the fix is small.

I will send you the exact attribution command only if this probe does not settle it. That command would be the
release `an_import_measured` with a printout of each green's retention and the records' bytes at the hold, so you
would not need vmmap, heap or malloc_history.
