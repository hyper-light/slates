# A volume promised the whole version slab is refused at recovery

Found 2026-11-05 by the load test's chaos runs. Open: root cause established, fix not made (see Ownership). Design:
§4.2 (accounting through recovery), §4.8 (a recovered snapshot), D-18.

## Description

In every one of 26 daemon kills under the anchor, the overlay the load test seeds its green from (created
`Dynamic { max: u64::MAX >> 1 }` over the kernel's `kernel/`, pinned whole, snapshotted) was not rebuilt: "recovered
0 volumes from their images (1 refused)". Reduced (`cargo test -p slates-server --test recovery` with a one-file
overlay or a scratch volume of the same size class, snapshotted, the daemon restarted over its anchor): "recovered
inode allowance exceeds the version slab: budget exceeded: 4855948 bytes requested, 4855947 available". With
`Dynamic { max: 1 << 30 }` the same volumes recover.

## Root cause

A dynamic maximum past the shard makes the volume's inode allowance the most the version slab can back one volume
(`inode_allowance` in `crates/server/src/verbs.rs`: capacity less headroom), reserved at create. Live, its snapshot
holds no retained version while the head has not diverged. At recovery, `rebuild_snapshot_tree`
(`crates/vfs/src/recover.rs`) shares a snapshot's unchanged files and symlinks with the head but rebuilds its
directories privately (GAPS already records this as "freed on drop, so RAM only"), and each private directory is an
inode version on the snapshot's deadlist, re-charged to the slab as retention. Measured through the daemon status: a
bounded overlay committed 465,831 version slots after create, pin and snapshot, and 465,832 after the restart. With the
allowance already the whole slab, that one slot (one per directory of the snapshot) cannot be had, so the volume is
refused. Reserving the allowance before the rebuild (the live order) only moves the refusal to the rebuild's own
retention charge (`ENOSPC`).

## Impact

- A volume whose allowance is the whole slab never survives a daemon restart once it has a snapshot.
- On a shard whose slab is fully promised, the last volumes recovered fail the same way, by the number of directories
  their snapshots hold.
- Recovery takes more slab than the live daemon held, so a recovered shard admits less than it did before the restart.
- The green itself is unaffected (its origin is recorded in the partition), so the load test's oracle still passes.

## Fix (owed)

A recovered snapshot shares every directory the head holds unchanged, as it shares files, so recovery takes exactly the
slab the live volume held. A check that would have caught this: committed version slots after a restart equal those
before it, for a volume with snapshots and directories.

## Ownership

`crates/vfs/src/recover.rs` sits beside the directory storage the slabs agent owns; raised with the supervisor before
either agent changes it. `crates/server/tests/recovery.rs` `a_created_and_pinned_overlay_survives_a_restart` (a bounded
created overlay, pinned and snapshotted) passes and stays as coverage of the path that works.
