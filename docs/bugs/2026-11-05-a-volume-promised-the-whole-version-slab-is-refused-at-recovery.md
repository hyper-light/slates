# A volume promised the whole version slab is refused at recovery

Found 2026-11-05 by the load test's chaos runs. **Fixed 2026-11-18 (A-395, goal g232)**: see "Fix (made)" below.
Design: §4.2 (accounting through recovery), §4.8 (a recovered snapshot), D-18.

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

## Measured again at the kernel's scale (2026-11-18)

The measurer's primary kill over the whole kernel (`crates/cli/tests/load_scale.rs`, `SLATES_LOADTEST_KILL_NODE=0`, Mac
Studio, main 7d760c4ea, twice, the same numbers): the restarted primary refused the green's base volume, "recovered
inode allowance exceeds the version slab: budget exceeded: 648393 bytes requested, 642045 available", and fenced the
green over it (`OriginUnavailable`). The shortfall, 6,348 slots, is linux-v7.4-rc1's directory count: `find -type d`
over the dataset (`~/.cache/slates-agents-scratch/loadtest-scale/dataset/linux-v7.4-rc1` on the Studio) counts 6,348
directories, 97,254 files and 106 links. The base volume is snapshotted and never written after, so the live volume
retained nothing; recovery retained one slot a directory.

## Fix (made)

A-395 (`crates/vfs/src/recover.rs`): a snapshot's directory whose inode handle is the head's is captured as shared
with the head, by number, as an unchanged file is (image version 31), and recovery maps it to the head's node, the
snapshot's root included. The node and every block of its entries are stamped at the directory inode's own birth
epoch, so the head copies it before changing it, as live copy-on-write did; a private directory naming a shared one
checks the shared node's parent and name and never writes them; the global deadlist pass claims everything the head
reaches. Recovery charges exactly what the live volume held, without any reservation around it.

Tests: `a_snapshots_unchanged_directories_take_no_version_slot_after_a_restart` (`crates/server/tests/recovery.rs`: an
overlay over `f` and `d1/d2/g`, pinned, snapshotted, restarted; every shard's committed and retained version slots
equal before and after the restart) and `a_head_change_after_recovery_copies_a_directory_the_snapshot_shares`
(`crates/vfs/tests/recover.rs`). Kiln r279 (2026-11-18, Mac Studio, debug): green, (0, 0) and (454, 0) by shard before
and after; red with the vfs change reverted, (457, 3) after: the snapshot's three directories, three slots.
