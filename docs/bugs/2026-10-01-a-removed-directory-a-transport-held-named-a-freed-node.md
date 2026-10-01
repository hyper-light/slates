# A removed directory a transport still held named a freed node (every transport, every lane)

**Date:** 2026-10-01. **Area:** `slates-vfs` (`Volume::rmdir`, `rename` over an empty directory, the overlay's
base removal, `release_body`). **Found by:** the OCI container workloads lane (AUD-29-78). Ada's direction: a bug
found this way is fixed cross-platform and cross-architecture. **Design:** §4.5 (inode lifetime), §4.6, §4.8.

## Description

`rm -r` through the daemon's Linux FUSE mount answered `EIO` on the final `rmdir`:
- every unlink succeeded and the directory listed empty;
- the RMDIR itself succeeded in the volume, but its barrier was refused (`FUSEDBG RmDir error 0 refuse
  Some(5)`, temporary logging);
- the shard's publication had skipped the volume: "a volume was not imaged, skipped: ENOMEM (stale handle: slot 2
  generation 0)".

From then on every mutation on that volume was refused durability (`EIO` over FUSE, the same refusal over NFS
and virtio-fs), and a restart would refuse to rebuild the volume.

## Root cause

Reproduced purely, through the FUSE dispatch and through the volume alone.

A kernel keeps a lookup reference on every directory it created until its FORGET. `rmdir` freed the directory's
node at once (`release_dir_node`) and then dropped the inode's links. A still-referenced inode survives as an
orphan, so the orphan's body named a freed node. Every image walk then met `StaleHandle`, and so would any
`getattr` or `readdir` through an open handle on the removed directory. Rename over an empty directory and the
overlay's base removal had the same order.

## Exact edits

- `crates/vfs/src/volume.rs`:
  - `release_body` releases a `Body::Directory` node, so a directory's node leaves with its inode: at once when
    nothing holds it, or at the orphan's last unreference.
  - `rmdir` and the rename-replacement no longer free it early.
  - `entry_dir` is new: a directory removed while held refuses a new entry `NotFound` (POSIX `ENOENT`). It is
    used by `create_file_no`, `mkdir_no`, `mknod_no`, `symlink_no`, `link_no` and the target of `rename_no`.
    It still lists, empty, and stats.
- `crates/vfs/src/base.rs`: the overlay's base removal follows the same order; its entry-creating `_no`
  operations use `entry_dir`.

## Proof

- `crates/vfs/tests/lifetime.rs`, both red before the change and green after:
  - `a_directory_removed_while_referenced_stays_valid_and_empty_until_its_last_reference`: images, lists empty,
    refuses a create or mkdir inside, and is reclaimed at the last reference;
  - `a_directory_replaced_by_a_rename_while_referenced_stays_valid_until_its_last_reference`.
- `crates/server/tests/fuse_mount.rs`: a directory of 100 files removed recursively through the real kernel
  mount, as an ordinary user in a container. It answered `EIO` before and passes now.
- The suites pass on macOS arm64, Linux arm64 (`rust:1.98.0` container, 71 suites) and the Linux kernel FUSE
  mount: vfs, bridge-core, bridge-fuse, bridge-nfs, bridge-virtiofs. CI runs x86_64 Linux, i686 and Windows.

## The finding that led here: hard links through Docker Desktop's share (not a slates defect, measured)

git in a container over the OCI bind failed with "cannot update ref … with nonexistent object".
- Narrowed by experiment: link a file, remove its first name, open the second. 99 of 100 failed at once through
  Desktop's share over the slates mount; 0 of 10 failed after two seconds.
- Controls: in a Desktop-shared APFS directory 0 of 100 failed; on the host's own mount 0 of 200.
- The macOS NFS client answers `fsgetpath` by file id `ENOTSUP` where APFS answers the current name. So Desktop
  resolves such a file by the first path it saw. That is consistent with what was measured; Desktop itself was
  not traced.
- `slates` serves the second name at once on every transport. This is proven by
  `a_hard_link_outlives_the_removal_of_its_first_name` (the bridge, every lane), the FUSE kernel test, and 100
  rounds through the macOS kernel mount in the live-mount flow.
- The tested profile now states the rule (`HardLinkRule::OtherNamesStaleAfterTheFirstIsRemoved`), and
  `slates oci-runtime` prints it. The container workloads lane runs git with `core.createObject=rename` on both
  sides, as the rule asks, and sets aside, and counts, the NFS client's `.nfs.*` silly-renames of names removed
  while the share held them open (the transport's declared `SillyRenamed` rule). git, cargo and python are now
  identical.

## Siblings reported, not changed here

- `Daemon::fleet_refusals` reads the control shard only, so a volume on another shard counted
  `fuse.barrier_refused` invisibly (the test printed `{}`).
- `PUBLISH_SKIPPED` logs the first skipped volume per process without naming it.
