# A rename replaced a base directory it had not listed

**Date:** 2026-09-30. **Area:** `slates-vfs` (overlay, §4.5), `slates-bridge-core`, `slates-land`,
`slates-server` (landing). **Audit:** AUD-29-16, and four sibling defects found while fixing it.

## Description

Renaming a directory over a base directory whose children the mount had never read replaced it.
`Volume::rename` asked the overlay node alone (`DirNode::is_empty`). An unread base directory has no
overlay entries, so it looked empty, and a directory with a file on disk was replaced.

Red test, `crates/vfs/tests/base.rs`: `a_directory_never_replaces_a_lazy_base_directory_with_children`
got `Ok(())` where it expected `Err(NotEmpty)`.

## Root cause

rmdir and rename used two different emptiness checks:

- `Overlay::rmdir` loaded the listing and asked `empty_for_rmdir`.
- `Overlay::rename` loaded nothing, and `Volume::rename` asked `is_empty`.

Beneath both, `empty_for_rmdir` answered "empty" for a merged directory whose listing was not loaded. The
target lookups swallowed every error with `.ok()` / `let _`, so a host refusal meant "no target".

## Fix

- **One emptiness check (`vfs`).** `Volume::rename` uses `empty_for_rmdir`, the check rmdir uses.
  `empty_for_rmdir` refuses `BaseUnavailable(0)` for a merged directory whose listing is not loaded.
  `Overlay::rename` loads a merged target's listing first. Both target lookups treat only `NotFound` as
  "no target".
- **Hostless verbs (`bridge-core`).** `host_for` refuses every verb on an overlay served without its host.
  The volume layer cannot see base names it has not materialized, so a hostless rename creates a
  directory that shadows the disk's. All 19 host-dispatching verbs ask it.

## Siblings found on the way

Each came with a failing test first.

1. **Stale listings (the listing racy rule).** An outsider adding a file to a base directory in the same
   timestamp tick as the listing left the directory's fingerprint unchanged (the sim's traced fingerprint
   was identical: size 0, mtime 0). The stale listing was trusted and the rename replaced a directory that
   had a file on disk.
   - Fix: `load_listing` trusts an unchanged fingerprint only when the listing was read more than the
     host's granularity after the directory's last change (mtime or ctime), the same rule as a file's
     witness. Otherwise it re-lists.
   - Test: `a_rename_over_a_base_directory_sees_outsider_children_and_refuses_on_a_host_fault`.
   - Cost: a relist inside the window costs O(n log m), because `listed_entry` binary-searches the
     `(hash, fold)`-sorted listing. The linear search had made `rm -r` of 40,000 base files quadratic
     (the base suite ran over 10 minutes; `sample` showed `load_listing → prune_unloaded`).
2. **Descriptors across a relist.** In the racy regime, unlink-while-open lost its bytes: the read
   answered `NotFound`. `refresh_unloaded` closed every untouched entry's descriptor on each relist,
   including the one the open took. After the unlink, nothing could reopen it by name.
   - Fix: keep a held descriptor while the listed name still names the same `(dev, ino)`.
   - Test: `unlink_of_an_open_base_file_keeps_its_bytes_across_a_relisting_directory`.
   - Any outsider change to the directory triggered the same relist before the racy rule.
3. **A landed scratch volume was served hostless.** `land_advance` makes a scratch volume an overlay over
   its target, named by the landing host's handles. The server dropped that host (`run_landing`). The
   landed files, now base entries, were unreadable, and names the overlay had not materialized were
   invisible. The new bridge guard turned this into a refused MNT (`snapshot_landing`).
   - Fix: `OsLand::into_host`; the slot keeps it.
   - Test: `snapshot_landing` now reads the landed file back through the mount.
4. **Crash resume took its own put-back for an outsider's edit.** With a simulated clock that moves, as
   real clocks do, `t_1_15_*without_exchange*` resumed `Partial`. The sweep's put-back rename moved the
   file's ctime (traced: same inode, size, mtime, mode and bytes; ctime 0 → 1,000,000,000). The verdict
   called that `ModifyModify`, and the swap's guard called it `TargetInUse`.
   - A first fix, judging by content (same bytes and mode means unchanged), was wrong and was reverted.
     The engine deliberately treats a ctime-only change as an outsider's metadata edit: owner and xattrs
     are not in the fingerprint (`engine.rs`: "Only our exchange may explain a changed ctime").
   - Fix: the sweep records the fingerprint its own put-back left (`Landing::restored`), and the entry is
     judged against the witness with that ctime (`witness_of`). A later change still conflicts.

## Test fixtures

The sim's clock only moves when a test moves it, so a tree built and mounted in one instant is racy
forever. Fixtures now model a disk tree that predates its mount (`advance_ns` past the granularity):
`crates/vfs/tests/base.rs`, `crates/bridge-core/tests/{base_overlay,invalidation}.rs`, and the five land
test helpers (`common::SETTLED_NS`). The racy regime keeps its own tests (`Fixture::racy`).

## Open, reported

- **A restart loses a landed volume's base.** The durable record still says `Scratch`, so a restart
  rebuilds the volume without it.
- **Two hosts' handles are mixed when an overlay is landed.** The engine's base operations
  (`land_advance`, `read_overlay_bytes`) run through the writer's `OsLand` host, whose handles are not the
  slot's.
- **No open pins.** A file replaced on disk while open is served the new file after a relist; the open
  should keep the old inode.
