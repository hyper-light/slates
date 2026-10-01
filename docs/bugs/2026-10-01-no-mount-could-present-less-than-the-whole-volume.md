# No mount could present less than the whole volume (AUD-29-76, the subtree half)

**Date:** 2026-10-01. **Audit:** AUD-29-76 (P2: "carry the authorized scope and version through the existing
attachment/view mechanism"). **Design:** §4.4 `attach`, §4.6 scoped exports.

## Description

Every host mount presented the volume whole: the macOS NFS export, the Linux FUSE mount and the container bind
built on them. Handing a tenant one directory, such as a build sandbox's `src/` or a sidecar's `out/`, gave it the
whole tree. Before this change, the only way to narrow a mount was a path check before a bind. A component swapped
for a symbolic link after that check would have let the runtime bind whatever the path then named.

## Root cause

The attachment record carried a volume and a version but no scope. The `Bridge` seam that every transport serves
through had no notion of one, so a handle naming any inode of the volume was answered.

## Impact

Only least-privilege sharing was affected: nothing beyond the attachment's own volume was ever reachable. No data
was corrupted.

## Exact edits

- `crates/vfs/src/volume.rs`: `Volume::within(store, no, scope)` decides whether inode `no` lies beneath
  directory `scope` in the head:
  - for a directory, by climbing its parents;
  - for any other node, by its home directory.
  - The climb is bounded by the volume's live inodes. A node with no home is outside.
- `crates/bridge-core`:
  - `Bridge::within`. The default admits only the scope itself; `VolumeBridge` answers it from the volume. An
    AppleDouble number is judged by its owner, and an inode that no longer exists is not within.
  - `scoped::ScopedBridge`, which wraps any bridge so that:
    - the scope is the root;
    - every object a request names is checked first, and one outside is answered `NotFound`;
    - lookup results and listing entries are held to the same rule;
    - `..` in a listing of the scope names the scope itself.
- `crates/ipc/src/protocol.rs`: `AttachRequest::ScopedHostMount{subtree}` (the NFS host mount) and
  `ScopedFuseMount{mount_point, subtree}` (the Linux FUSE mount).
- `crates/db/src/catalog.rs` (append-only):
  - the forms `AttachForm::ScopedMount{scope, mount_point}` and `ScopedFuseMount{path, scope}`;
  - helpers `scope()` and `fuse_mount_point()`.
- `crates/db/src/partition.rs`: binding a mount keeps its scope, and a FUSE form keeps its own mount point.
- `crates/server/src/verbs.rs`:
  - `scope_of` resolves the subtree in the head before any effect. It is refused `NotDirectory` or `NotFound`.
  - The scope is recorded by inode, so a later rename of the directory does not change what the mount presents.
  - A snapshot of a subtree is refused `SnapshotNotPresentedByHostMount`.
  - The FUSE paths (deferred establishment, the mount-point check, recovery ending dead FUSE mounts) accept the
    scoped form.
- `crates/server/src/nfs.rs` (`with_export`, both NFSv3 and the v4 front end) and `crates/server/src/fuse.rs`
  (each serve turn and the reclaim of an undelivered reply) serve a scoped record through `ScopedBridge`.
- `crates/client`: `attach_scoped_mount` and `attach_scoped_fuse`. `crates/cli`: `slates mount ID DIR --subtree
  DIR` on macOS (NFS) and Linux (FUSE).

## Proof

Each test failed under a mutation of the fix:

- `crates/vfs/tests/scope.rs` `a_subtree_contains_exactly_what_hangs_beneath_its_root`.
- `crates/bridge-core/tests/volume_bridge.rs` `a_scoped_bridge_reaches_nothing_outside_its_directory`. It failed
  with the admission check removed.
- `crates/server/tests/attach_forms.rs` `a_scoped_host_mount_reaches_nothing_outside_its_directory`, over the real
  NFS wire:
  - The mount lists `g` alone, reads it, and a write lands in `shared`.
  - Handles to `private`, `private/secret` and the volume's root, forged with the scoped capability, are refused
    by GETATTR, LOOKUP and READ.
  - A file or a missing path as the subtree is refused, with nothing attached.
  - With the scope dropped at the export, the mount listed `shared` and `private` and the test failed.
  - Green on macOS and on Linux arm64 (Docker, rust:1.98.0, non-root).
- `crates/server/tests/fuse_mount.rs` `a_scoped_fuse_mount_presents_one_directory_and_follows_it`, through the
  Linux kernel:
  - It lists `g`; a write lands in `shared`.
  - After `shared` is renamed into `private` through the whole mount, the scoped mount still presents the same
    directory.
  - A file subtree is refused `NotDirectory`.
  - With the scope dropped in the serve turn, it listed `private` and `shared` and failed.
- `crates/cli/tests/cli.rs` `slates_mount_subtree_presents_one_directory_of_the_volume` drives the real binary and
  a real anchor-supervised daemon through the kernel mount. It passed on macOS (NFS) and on Linux arm64 (FUSE),
  2026-10-01.

## Not done here

- An `advance` that re-pins a mounted snapshot view.
- On Windows, no attach-driven mount form exists for WinFsp to carry a scope. `ScopedBridge` sits on the shared
  seam and applies to it unchanged once one does.
- The cost of the scope check per named object (a climb of the directory's depth) is unmeasured.
