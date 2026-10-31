# The byte states panicked on an empty range (2026-10-31)

**Status:** fixed before it shipped, failing test first, in A-192's change.

## Description

A-192 feeds an NFS connection's READs and WRITEs into A-140's byte states (`crates/bridge-fuse/src/coherence.rs`
`PageStates`), which until then only a FUSE mount's rounds fed. The A/B's hostile case
(`crates/server/tests/nfs_writeback.rs` `hostile_writes_are_answered_and_the_export_goes_on`) sends a WRITE of no bytes
and WRITEs at the last offsets of the range. On arm B's first run the shard serving them panicked, and the case's next
call found no answer:

```
range start is greater than range end in BTreeMap
   2: <alloc::collections::btree::node::NodeRef<..., u64, (u64, u64, slates_bridge_fuse::coherence::Byte), ...>>::search_tree_for_bifurcation::<u64, core::ops::range::Range<u64>>
```

## Root cause

`FileStates::clipped`, the copy of a file's states over one range that `stale_after` judges, searched the map from just
past the range's start to its end. For an empty range, `start..start` (a WRITE of no bytes, or one whose end saturates
at `u64::MAX`), that search starts one past where it ends, and `BTreeMap::range` refuses such a range with a panic.
`merge_around` did the same for a range ending before it starts. No FUSE kernel sends an empty write or one at the end
of the offset range, so the code had never met either; an NFS client may send both.

## Impact

None shipped: the path became reachable only with A-192, and the panic was found before A-192 landed. Had it shipped,
one NFS WRITE of no bytes would have ended the daemon in a release build (`panic = "abort"`), and taken the shard's
thread in a debug one.

## Reproduction

`crates/bridge-fuse/src/coherence.rs` `empty_ranges_and_ranges_at_the_end_are_judged_without_a_panic`: on a tracked
file, judge, fetch and change the empty ranges `0..0`, `5..5` and `u64::MAX..u64::MAX`, then change and judge the
range's last byte. Before the fix it failed at the first judgment with the panic above (macOS, 1 run of 1:
`test result: FAILED. 0 passed; 1 failed`).

## Exact edits

- `FileStates::clipped`: an empty range (`start >= end`) holds no bytes and returns an empty copy, keeping the file's
  lost mark.
- `FileStates::merge_around`: a range ending before it starts has nothing around it to merge, and returns.
- The unit test above.

## Siblings swept

The six `BTreeMap::range` searches in `coherence.rs`: two take `..x` or `..=x`, which cannot be inverted; `update`'s
`start..end` returns first on `start >= end`; `merge_around`'s `first..=end` starts at or before `range.start`, now
never past `end`; and `clipped`'s two are the ones fixed. The NFS side builds every range with `saturating_add`
(`nfs_writeback::judge`, `nfs::read_range`), so a range there is empty or ordered, never inverted. The hostile case's
four WRITEs and its READ past the end are answered, and the export goes on serving.
