# A snapshot view could not move, and a container or guest could not be scoped (AUD-29-76, the rest)

**Date:** 2026-10-01. **Audit:** AUD-29-76 ("neither the OCI form nor the guest admission request carries an authorized
subtree or a pinned snapshot view… carry the authorized scope and version through the existing attachment/view
mechanism, including open-handle and advance lifetimes"). **Design:** §4.4 attachment lifecycle (`Bound → Advancing →
Bound`), §4.6.

## Description

After the snapshot and subtree mounts landed (`9fe9dbb`, `d04f89f`), three things remained:

- **A snapshot mount could not move.** `advance` on its attachment answered `NotGreen`. The only way to present a
  later snapshot was to detach and mount again, which invalidates every open handle and names no changed path.
- **A guest device presented the volume's head, whole.** `Daemon::attach_guest_device` took no view, so a microVM could
  be given neither one directory nor an immutable snapshot.
- **The container bind's subtree path was unproven.** A bind of a directory inside a mount is refused
  `NotAMountPoint` (correctly), and no test showed that binding a scoped mount gives the container only that directory.

## Root cause

- `advance` was written for green attachments only (§4.16), and nothing could name the paths that differ between two
  snapshots of a plain volume.
- The guest seam's bridge (`ShardBridge`) built a `VolumeBridge` over the head on every pass, with no scope and no view.

## Impact

None of these gaps presented wrong data: each refused, or presented the whole head as asked. Readers that needed a
moving immutable view or a scoped guest had no supported path.

## Exact edits

- `crates/vfs/src/trie.rs` `changed`: a parallel walk of two inode tables that skips every node they share. Path
  copying by birth epoch makes shared nodes the same handle, so the walk is proportional to what the span copied.
- `crates/vfs/src/volume.rs` `paths_changed_between(from, to)`: the paths that changed, each named itself:
  - an inode whose record differs is named by its path in each snapshot (a rename gives its old and new path);
  - a moved directory also names every path beneath it in both snapshots, since the records inside did not change;
  - a changed hard-linked node is named by all its names, found by one walk of each tree, taken only in that case.
- `crates/db`: `Op::AttachmentRepinned{id, snapshot}` (appended). Its check refuses an attachment that presents no
  snapshot. Applying it moves the mount and every container bind borrowing it together.
- `crates/server/src/snapshot_view.rs` `advance`:
  - opens the new view first, pinning its snapshot, so a refusal changes nothing;
  - computes the invalidated paths;
  - records the re-pin as one operation;
  - then swaps the views and closes the old one, unpinning its snapshot.

  A request is served whole within one shard turn, so none spans the swap. With no version, it moves to the volume's
  newest snapshot. `merge_service::advance` dispatches a snapshot mount's attachment here.
- `crates/server/src/virtiofs.rs`:
  - `GuestView{subtree, snapshot}`, passed to `attach_guest_device`;
  - a subtree is served through `ScopedBridge`;
  - a snapshot is served through a read-only view kept in the shard's `guest_views`, admitted with no write and
    closed at the device's end;
  - a subtree of a snapshot is refused (`ViewRefused`), as for a host mount.

  `snapshot_view::end_all_of` closes guest views before a destroy too.
- `crates/server/src/verbs.rs`: `resolve_scope`, shared by the mount forms and the guest view.

## Proof

Each test failed under a mutation of its fix:

- `crates/vfs/tests/snapshot_diff.rs`:
  - The proptest, 256 generated history pairs: every path whose file bytes, link count, symlink target, xattrs or
    directory listing differ is named by its own path, and nothing is named that neither snapshot holds. With
    in-place record changes dropped from the diff, it failed at once (`changed but not named: ["/"]`).
  - The exact case: a write names `/work/b` alone. A directory rename names `/`, `/moved`, `/moved/b`, `/work` and
    `/work/b`. A write through a hard link names both names.
- `crates/server/tests/attach_forms.rs` `a_snapshot_mount_advances_to_a_later_snapshot_and_names_what_changed`:
  - A missing snapshot is refused with nothing changed.
  - The advance names `/f` alone, and the same capability then reads `middle`, never the head's `after!`.
  - The old snapshot is unpinned and the new one pinned.
  - A restarted daemon still presents the new snapshot. With the durable record skipped, the restart's mount was
    refused (`MNT` 2).
- `crates/server/tests/virtiofs.rs` `a_guest_device_presents_a_subtree_or_a_snapshot_and_nothing_else`:
  - The scoped device finds `g` and answers `ENOENT` for `private`, both by name and by its inode named directly.
  - The snapshot device sees the snapshot's 6 bytes, not the head's 7, and its CREATE is `EPERM`.
  - A subtree of a snapshot is refused.
  - Every view closed with its device, so the snapshot is destroyable afterwards.
  - With the scope dropped: `(-2, 0, 0)` against `(0, -2, -2)`. With the view dropped: size 7.
- `crates/cli/tests/cli.rs` `a_container_bound_to_a_subtree_mount_sees_only_that_directory`, through Docker Desktop
  29.3.1: the container sees `g` alone, and its write lands in `shared`. With the scope dropped, the container saw the
  volume's root (`cat: can't open '/work/g'`).

Green on macOS and on Linux arm64 (Docker, rust:1.98.0, non-root, FUSE), including Linux clippy.

## Not done here

- A guest's view is chosen when the harness attaches it and cannot be advanced. Guest devices have no durable
  attachment record yet; that is AUD-29-68's durable-record leg.
- The cost of `paths_changed_between` on a large span is unmeasured. It visits only copied trie nodes, plus moved
  subtrees, plus whole-tree walks when a hard-linked node changed.
