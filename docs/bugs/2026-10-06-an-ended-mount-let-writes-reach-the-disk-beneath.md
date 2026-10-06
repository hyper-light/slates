# An ended mount let writes reach the disk beneath

**Found:** 2026-10-06, by an adversarial battery under the hermeticity trace on a real Linux 6.12 FUSE mount: a
volume destroyed while mounted and written to.

## Description

After `slates volume destroy` of a mounted volume, the mount went away. A later `echo x > mnt/new` by the mount's
user succeeded and wrote `new` to the directory beneath the mount point, on disk. A process working in the volume (an
agent, a build) that kept writing by path after the volume ended had its writes land on the host disk, silently: an
escape for conditions 3 and 4. Slates itself wrote nothing: the trace held, and it was the kernel resolving the path
to the bare directory once the mount was gone.

## Root cause

Every way a FUSE mount's serving ended ran `fusermount3 -u -z`, a lazy unmount:
- the volume gone (destroyed);
- a serve failure;
- a device that could not be adopted after a restart;
- a consumer's revocation (through `end_attachment`);
- the daemon's stop (`EndMounts`, AUD-29-64);
- the user's `detach`.

Only the last is a request to remove the mount.

## Fix (A-102)

Those ends drop the device and release the anchor's copy (`ended`) without unmounting. The kernel then aborts the
connection and answers every call `ENOTCONN`, FUSE's own rule for a server that has gone, until the user unmounts. A
revoked consumer's mount stays too, its requests refused under the revoked registry attachment.
`end_attachment` takes an `Ending`: only `Detached` unmounts. A refused attach still unmounts, since its user never
had the mount.

## Tests

`a_destroyed_volumes_mount_refuses_writes_and_never_lets_them_reach_the_disk_beneath` (`crates/cli/tests/cli.rs`,
Linux): mount, write, destroy, then a create at the mount path. The mount stays in the kernel's table, the create is
refused, and after the user's `fusermount3 -u` the directory beneath is empty. It failed before the fix ("the
destroyed volume's mount stays until its user unmounts it"). After it, all 10 CLI mount tests pass on Linux (the
explicit unmount and detach flows among them), as do the 22 real-kernel FUSE suites and the daemon suite's 19
revocation tests.

## Sibling sweep

- **Startup's stale-mount sweep (`unmount_stale`, AUD-29-64)** unmounts the dead mounts a killed daemon left, the same
  fall-through for any process still inside one. It is a separate audit decision, left as is and opened in GAPS.
- **NFS mounts are never unmounted by the daemon:** a destroyed volume answers `NFS3ERR_STALE` or `NFS4ERR_STALE` in
  place.
- **virtio-fs guests:** the device's revocation ends the guest's view (AUD-29-68). No host path is exposed.

## Follow-up the same day: the daemon's stop

The stop unmounted every FUSE mount too (`EndMounts`, a tested AUD-29-64 decision). It now ends them the same way
(`end_all`; a fenced shard's `drop_devices` writes no record). The three tests that asserted "the daemon's stop
unmounted" now assert the A-102 contract: the mount stays, refuses a write, and after its user's `fusermount3 -u` the
directory beneath is empty. On Linux the server's FUSE mount suite (3), the bridge's real-kernel suites and the CLI's
10 mount tests all pass.

