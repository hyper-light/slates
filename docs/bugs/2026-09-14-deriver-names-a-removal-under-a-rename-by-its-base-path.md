# The deriver named a removal beneath a renamed directory by its base path, which no applier step can reach

Date: 2026-09-14
Area: `crates/vfs/src/derive.rs` (the ops-document deriver, §4.16 "Composition at seal", Phase 1 task 14)
and its reference applier in `crates/vfs/tests/derive.rs`
Severity: correctness of the increment's ops document — an unlink inside a directory renamed in the
same increment was lost on application, so the merged post-state kept a file the work had deleted.

## Symptom

`crates/vfs/tests/derive.rs::net_apply_equals_raw_replay` (T-1.18/AC-1.15, the generative oracle:
300 cases per run) failed intermittently — once in the base-fuse integration run on main, once for the
base-fuse agent at `4f5deae`, and in a hunt every ~100 runs (run 92 of 600; run 401 of 2,000 in a
private target directory). Its shrunk input:

```
prefix = [Mkdir([], "A"), Create(["A"], "e")]
suffix = [Unlink(["A"], "e"), Rename([], "A", [], "a")]
files: applied ["/a/e"] head []
doc = OpsDocument { dirs_renamed: [("/A", "/a")], removed: ["/A/e"], .. }
```

The rate: 1 failure in roughly 30,000 generated histories, because the case needs a removal inside
a directory the same increment renames, and the six-name, two-deep generator rarely produces both.

## Root cause

Two halves, one rule.

1. The deriver classified every journaled path under its **base** name (`classify_path` on the
   touched set), so a removal beneath a renamed directory was emitted as `removed: ["/A/e"]`. The
   post-processing in `derive` already mapped `dirs_removed` against rename **sources** but never
   rewrote a removal *beneath* a source, and `walk_subtree` over the renamed directory reads the
   head's subtree, where the removed entry no longer exists — so nothing emitted `/a/e`.
2. The document's stated order ("renamed directories are detached, then removals apply, then the
   subtrees attach") cannot apply a removal inside a renamed directory at all: at the removal step
   the subtree is detached, so neither `/A/e` nor `/a/e` exists. The reference applier followed that
   order and silently removed nothing; the re-attached subtree brought `/a/e` back.

## Fix

The one order, restated in the deriver's module doc and realized in the reference applier: renamed
subtrees detach (deepest source first); removals **outside** every rename target apply (a rename over
a removed directory); the subtrees attach (shallowest target first); removals **beneath** a rename
target apply, named by their **post-rename** paths — the only paths that exist at that step. The
deriver rewrites every `removed` and `dirs_removed` path beneath a renamed source through the renames
it lies under (`post_rename_path`: the deepest matching source, repeated for a renamed ancestor of
that target's source, bounded by the rename count), so the emitted document is applicable. The
nested-rename shape (`/A/b` → `/A/C` inside `/A` → `/a`, whose inner target is in post-rename
coordinates) needs the deepest-first detach and shallowest-first attach, or the outer subtree carries
a stale copy of the inner one.

Failing tests first, `crates/vfs/tests/derive.rs`:
- `a_removal_beneath_a_renamed_directory_is_named_by_its_post_rename_path` (the shrunk case) —
  before: `files: applied ["/a/e"] head []`; after: ok.
- `removals_beneath_renamed_directories_follow_every_rename_shape` (a removed subdirectory and a
  removed symlink beneath a renamed parent; a removal beneath a directory renamed inside a renamed
  parent) — before: `dirs_removed: ["/A/b"], removed: ["/A/d"]` applied nothing; after the deriver
  fix alone the nested shape still applied `["/a/b/e"]`; after the applier order: ok.

The golden identity (`a_fixed_history_has_a_golden_identity`) is unchanged: that history has no
removal beneath a rename, so its document bytes are identical. The generative oracle: 0 failures in
12,000 cases on the fix (40 runs), plus the long run recorded in the commit.

## The second shape, found by the long run on the first fix (49,500 cases)

```
prefix = [Mkdir([], "e")]
suffix = [Rename([], "e", [], "a"), Mkdir(["a"], "a"), Rename([], "a", [], "e")]
directories: applied ["", "/e"] head ["", "/e", "/e/a"]   doc = OpsDocument { (empty) }
```

A directory renamed away and back within one increment journals a child created meanwhile under
the transient name `/a/a`, which resolves to nothing in the base or the head, while the child's real
head path `/e/a` is never journaled and the parent classifies as unchanged (the same inode on both
sides, so no subtree walk reaches it). The document was empty. Root cause: `fold` collected the
journaled **names**; only content-written inodes were also touched at every path they have.

Fix: every inode a record names is touched at its head and base paths — a directory through the
node's parent chain (`Volume::path_of_dir`, the new head twin of `path_of_dir_in`; `path_of_inode`
names files only, so the directory form was needed), a file or symlink through its home and links —
so a transient journaled name can never hide a change. Failing test first:
`a_directory_renamed_away_and_back_with_a_child_created_meanwhile_reads_right` — before: the empty
document above; after the `fold` change alone still empty (no head-side directory accessor existed);
after `path_of_dir`: ok. Oracle on the completed fix: 0 failures in 36,000 cases (two 60-run
batches), plus the long run recorded in the commit.

## The third shape (202,200 cases on the second fix), and the rule that closes all three

```
prefix = [Mkdir([], "d"), Mkdir(["d"], "C")]
suffix = [Rename([], "d", [], "a"), Mkdir([], "d"), Symlink(["d"], "C")]
doc = { dirs_renamed: [("/d","/a")], dirs_created: ["/d"], dirs_removed: ["/a/C"], symlinks: ["/d/C"] }
directories: applied ["", "/a", "/d"] head ["", "/a", "/a/C", "/d"]
```

A renamed directory's old name re-created as a fresh directory holding a new entry at a path the
base held a subdirectory at. The symlink at `/d/C` classified as base=Dir, head=Symlink ("a directory
gave way to a symlink") and emitted `dirs_removed: ["/d/C"]`; the first fix's post-hoc rewrite then
mapped it to `/a/C` because `/d` is a rename source — but this `/d/C` is the *new* `/d`'s child, not
the renamed subtree's, and `/a/C` still exists in the head. Nothing should be removed.

The rule that closes all three shapes, replacing the post-hoc rewrite: **a path's base side is read
through the renames, never by its head name.** The deriver collects the increment's directory renames
(`find_renames`: a head directory whose inode the base held at another path) before any path is
classified. Beneath a renamed *target*, a head path's base side is the entry at the source-relative
base path; beneath a renamed *source*, the base subtree was carried away, so a head entry there is
new and nothing is removed (`base_path_of`). Removals inside a renamed subtree come from the one
place that can see them — the base subtree under the source compared with the head subtree under
the target (`walk_subtree`'s new pass over `readdir_in`), named by their post-rename paths. The
applier's two-phase removal order (removals beneath a target apply after the subtree attaches) is
unchanged and is what makes those removals applicable.

Failing test first: `a_renamed_directorys_old_name_recreated_with_a_new_entry_reads_right` — before:
the document above; after: ok. The three earlier regression tests still pass under the new rule
(the first shape now emits its removal from the subtree pass rather than the rewrite).

## Impact

Any increment that deleted an entry inside a directory it also renamed produced a document whose
application kept the deleted entry. The merge engine's own deriver (`crates/merge`, `Increment`)
consumes the journal directly and was not affected; the vfs `OpsDocument` reaches the wire through
`derive` and is what a holder or a later reader applies, so the correction is to the document's
contract, not only to the test.

## Sibling sweep

- `dirs_removed` entries that are themselves a rename source are already dropped (the rename carries
  the move); unchanged.
- A file *renamed* (not removed) inside a renamed directory is carried by the parent's rename and
  skipped by `walk_subtree` ("what moved unchanged with a renamed parent is skipped"); unchanged.
- `symlinks` and `files` of the post-state are already named by their head paths (they come from
  the head walk); only the removal classes were named by base paths.
