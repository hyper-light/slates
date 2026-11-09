# A revoked consumer's Linux FUSE mount went on serving

- **Date:** 2026-11-09. **Status:** fixed. **Audit:** AUD-29-84's owed Linux proof. **Design:** §4.13 ("every later
  effect"), A-28, A-102.
- **Found by:** the bug sweep, writing the Linux proof GAPS owed for AUD-29-84 ("Linux's consumer FUSE mount ends through
  the same `end_attachment` the detach test proves").

## Description

A consumer's revocation, acknowledged to the human, must leave every capability the consumer held reaching nothing. Its
per-shard step (`verbs::mark_revoked`) ends each attachment the consumer holds as a recorded operation through
`end_attachment(Ending::Otherwise)`. For a host NFS mount that revokes the mount's registry attachment, so its next
request is refused (`a_revoked_consumers_mount_capability_reaches_nothing`, macOS). A Linux FUSE mount keeps its
registry attachment in the mount itself (`ShardState::fuse_mounts`), which `end_attachment` never touched; an end other
than a detach leaves the mount in place, and nothing refused its requests. The revoked consumer went on reading.

## Evidence

`a_revoked_consumers_fuse_mount_reaches_nothing` (`crates/server/tests/fuse_mount.rs`, Linux): the account mounts a
volume, writes `f` and shares the volume with an enrolled consumer; a workload bound to the consumer mounts it for
reading and reads `f`; the human revokes the consumer; `f` is opened afresh through the consumer's mount (a fresh open
asks the daemon) and through the account's. Before: the consumer's fresh open read "shared bytes" after the revocation,
3 of 3 on adacachy2 (Linux 7.3.3). After: refused, 3 of 3, the account's mount still reading.

## Root cause

`end_attachment` revoked the registry attachment of host mounts (`mount_attachments`) and unmounted a FUSE mount only on
a detach. Its own comment promised that any other end leaves the mount "refused under the revoked registry attachment",
which held for host mounts and not for FUSE mounts.

## Fix

`fuse::fence_if_mounted`, called by `end_attachment` however the attachment ends, revokes a FUSE mount's registry
attachment. The mount's next request then fails `begin`, the turn ends the mount, its device is dropped, and the kernel
answers `ENOTCONN` until its user unmounts it (A-102), as at a daemon's stop.

## Impact

Confidentiality and integrity of a shared volume after a consumer's revocation, Linux only: a revoked consumer's mount
read (and, mounted for writing, wrote) the volume until it was unmounted. macOS's NFS mounts were fenced.

## Residual

A process holding a descriptor opened before the revocation can still read pages the kernel cached for it; the kernel
serves those without asking the daemon. The fence stops every request that reaches the daemon.
