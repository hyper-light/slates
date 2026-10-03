# vfs: the reverse name lookup missed every directory entry that opens a leaf, so a snapshot diff omitted changed files

**Date:** 2026-10-03. **Audit:** AUD-29-76 (its follow-up measurement of the snapshot diff on a large span).
**Design:** §4.4 `advance` of an immutable reader ("invalidates old caches"); §4.5 directory trees.
**Found by:** `crates/vfs/examples/scope_diff_bench.rs`'s own check. 10,000 files, one byte written to each between
two snapshots: `paths_changed_between` named 9,925.

## Description

`Tree::name_of(hash, wanted)` (`crates/vfs/src/dirtree.rs`) answers the name of the entry with a given hash whose
child is a given inode. It is the reverse lookup every "path of this inode" query uses through the inode's home
(its parent and its name's hash). It returned `None` for 75 of 10,000 files: in each 256-entry directory, the
entries that open a leaf of the directory's block tree.

## Root cause

The lookup descended the tree by `(hash, "")`. An index block's key for a child is that child's first key, and every
real name sorts after the empty one. So for an entry that opens its leaf, the descent took the child before it,
searched that leaf, and stopped. The doc comment acknowledged only "a run of one hash that spans two leaves is not
followed; the caller falls back to a walk". But the miss needs no hash collision, and two of its callers have no
fallback (`path_of_inode`, `path_of_inode_in`). `iter_from_hash` (the cookie resume, AUD-29-86) descends the same
way and is correct, because its in-order walk steps into the next leaf.

## Impact

- **`Volume::paths_changed_between`** (`advance` of a snapshot mount): a changed file opening a leaf was not named,
  so a reader's cache could keep that file's old bytes. Proven by the test below.
- **`Volume::path_of_inode`** (head): the base plane's Witness and Drift journal records were given an empty
  path, and its drift report named `inode N` instead of a path, for such a file. This is found by inspection of
  the callers (`base.rs` `copy_up`, `record_drift`, the drift listing); no test reproduces it.
- **`derive.rs`:** `head_paths` and `base_paths` fell back to a whole-tree walk on `None`. So they were correct,
  and slower for such a file.
- **Checked and not affected in this shape:** the copy-up read path (`base.rs` `home_of`). A by-use test read
  every file of a 2,000-file base directory through the overlay, and all 2,000 reads succeeded before the fix. The
  test could not tell the two versions apart, so it was not kept.

## Exact edits

- `crates/vfs/src/dirtree.rs` `Tree::name_of`: the entries from `iter_from_hash(hash)` while their hash equals
  `hash`, the first whose child is wanted. One descent, then the tested in-order walk. It covers an entry opening a
  leaf and a run of one hash spanning leaves.

## Proof

- `crates/vfs/tests/snapshot_diff.rs` `a_write_to_every_file_of_a_wide_volume_names_every_file`: failed before the
  fix with "9925 named of 10000; first missing ["/dir0/f147", "/dir0/f86", …]"; passes after.
- The tree oracle `the_tree_equals_the_ordered_map_across_splits_merges_and_epochs` (`assert_equal_to_model`) now
  asks `name_of` for every entry, after the 3,000 inserts and after the removals with merges. Red-checked:
  - with the pre-fix `name_of` restored, it fails ("name_of by hash 329084778800567608", left `None`);
  - with the fix, it passes, as do all 3 dirtree tests and all 3 snapshot-diff tests.

## Sibling sweep

Every descent by `(hash, "")` in `dirtree.rs` was checked: `name_of` (fixed) and `iter_from_hash` (correct, by its
walk). Every other descent routes with the real name. Every caller of `name_of` goes through the fixed function:
`dir.rs`, `volume.rs` (twice) and `base.rs`.
