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

## Residual, closed the same day

A process holding a descriptor opened before the revocation went on reading pages the kernel had cached for it: the
kernel serves a cached page without asking the daemon, so the fence never saw the read. The page need not be one the
process read. The kernel's readahead fetches past a read, so the process could read bytes it never asked for before its
revocation.

**Evidence.** `a_revoked_consumers_open_descriptor_reads_nothing_it_had_not_read` (`crates/server/tests/fuse_mount.rs`,
Linux). The consumer opens a 1 MiB file through its mount and reads its first page, the human revokes the consumer,
and the consumer reads the next page through the same descriptor. Before: `Ok`, 3 of 3 on adacachy2 (Linux 7.3.3).
Reading at 32 KiB instead reached the fenced mount uncached and was refused, so the test reads inside the first
readahead window.

**Fix.**
- `fuse::fence_if_mounted` wakes the mount's task through its change signal.
- The mount's next turn finds its attachment refusing and starts the revoked round (`slates_bridge_fuse::channel`
  `revoked_turn`). It sends an inode invalidation of all data for the root and for every node its kernel references
  (`KernelHolds::nodes_after`). The nodes go in slices under the shard's budget (at least one node a slice), and every
  slice is written on the mount's notify thread. Dropping a page waits for any `READ` that holds it, and the serving
  thread is what answers that `READ` (the 2026-10-17 deadlock).
- While the round runs the mount reads every request and answers it `ENOTCONN` without reaching the volume
  (`refuse_revoked`). A `FORGET` is taken off what the kernel holds, so the round skips the node. The lease fence is
  skipped for such a mount, since it answers nothing from the volume and must go on reading.
- The mount ends only once no round is out, so dropping it never joins a notify thread still waiting in the kernel. A
  round the device refuses ends it at once.
- The revocation awaits every fenced mount's end on its shard (`fuse::revoked_mounts_ended`, parked with the volume's
  told waiters, which a mount's end wakes) before it acknowledges. The wait is bounded by `told::TOLD_WAIT_NS`; past
  that bound the revocation is acknowledged anyway and counted (`fuse.revoked_overdue`), since every request is already
  refused.
- Counted: `fuse.revoked_invalidated` (non-vacuity), `fuse.revoked_refused` and `fuse.revoked_unwoken`.

**After.** Refused 5 of 5 on adacachy2, the drop counted and nothing overdue. A buffered read whose page fetch fails
comes back `EIO` from the kernel's page-cache path, so the test accepts `EIO` or `ENOTCONN`. Red-checks, each on
adacachy2:
- with the invalidations withheld, the unread page read `Ok`, 3 of 3;
- without the acknowledgement's wait, it read `Ok` 5 of 5, since the read always beat the drop;
- without the wake, it read `Ok` 3 of 3, after the revocation waited out its bound.

`a_revoked_mount_drops_every_node_its_kernel_holds_in_slices_before_it_ends` (`crates/bridge-fuse/tests/revoked_round.rs`)
drives the round over an in-memory device. It looks up 40 files, revokes, and turns the mount with a zero budget. It
expects the root and the first file in the first slice and one node a slice after. A `READ` mid-round is answered
`ENOTCONN`, a `FORGET` mid-round is answered by nothing and its node skipped, each node is invalidated once, and the
mount ends with all of them written. It was red with the slicing removed and red with the refusal unanswered.

**Cost.** One message per node the kernel references, at most the volume's inode allowance, sent in slices under the
shard's budget. The fuse_mount revocation tests ran in the same 0.15 to 0.17 s as before.
