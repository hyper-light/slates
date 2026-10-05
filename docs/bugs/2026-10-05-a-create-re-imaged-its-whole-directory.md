# A create re-imaged its whole directory at every barrier

Date: 2026-10-05. Scope: the barrier's delta publication (A-68, `crates/vfs/src/delta.rs`) and its replay at
recovery.

## Symptom

With Nagle fixed, the hot-directory storm through Linux's own NFS client (`docs/wip/bench/hotdir/native.sh 1 8000`)
still showed an OPEN-with-create round trip of 0.33 ms against WRITE's 34 µs. The daemon's local p99 was 0.59 ms
against a p50 of 1.8 µs (`nfs.local_p*_ns`).

## Root cause

A `perf record -a -g` during the run showed the machine 93.5% idle. A fifth of the serving shard's samples were in
the sort inside `Volume::dir_entries`, reached through `serve_local` → `verbs::publish_shard` → `Volume::delta` →
`image_of_inode`. A delta carries a changed directory's entries by name, so `delta` cleared the entries the full
inode image had built. But building them had already enumerated the directory, cloned every name and sorted them,
so each create paid its parent's size. Replay had the same shape: `VolumeImage::apply` cloned a changed directory's
whole entry list and scanned it, folding every name, for each changed entry.

## Impact

Every barrier after a create in a large directory: every NFS create, `fsync` and `close` flush. A directory of
n entries filled one file at a time cost O(n² log n) in total. Recovery replay of such deltas cost O(n²)
allocations.

## Fix

- Failing test first: `one_creates_delta_costs_the_same_in_a_large_directory_as_in_a_small_one`
  (`crates/vfs/tests/delta_cost.rs`) counts allocator calls for one create's publication and replay in a 64-entry
  and a 4,096-entry directory. Before the fix: (71, 68) against (4104, 4100).
- `image_of_inode_without_entries` images a directory without enumerating it, and `Volume::delta` uses it.
- `VolumeImage::apply` moves the held entries instead of cloning them, and finds each change by binary search. Image
  entries are ordered by folded name under the volume's policy (`entry_order`; unchanged under `Exact`), which is
  what makes the search correct under `Fold`. `IMAGE_VERSION` is 14 (A-89).
- Re-measured: OPEN 0.33 → 0.041 ms; create+write+close p99 0.86 → 0.38 ms (one writer) and 13.3 → 4.2 ms (16).

## Sibling sweep

- Snapshot images still call the full `image_of_inode`, correctly: a full image needs the entries.
- A volume with snapshots, a clone origin or a base plane still publishes in full at every barrier (GAPS, A-68
  owed), so it remains linear in its size.
- Readdir of a large directory is slow (p99 7.9 ms at 8,000 entries against tmpfs's 0.9 ms) and is next.
