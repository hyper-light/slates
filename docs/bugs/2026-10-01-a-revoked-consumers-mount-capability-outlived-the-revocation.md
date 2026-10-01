# A revoked consumer's mount capability outlived the revocation (AUD-29-84)

**Date:** 2026-10-01. **Audit:** AUD-29-84 (P1). **Design:** §4.13 ("every later effect from a channel bound to it
refuses `ConsumerRevoked`"), A-28.

## Description

`revoke_on_channel` recorded `ConsumerRevoked` and marked every shard's client slots bound to the consumer, so its
channels refused every later verb. But a consumer that had attached a host mount held a mount capability (the
attachment's token) that the NFS edge checks only against the attachment record (`authorized_rights`), and nothing
ended that record: the consumer's mounted source kept reading and writing after the human was told `Revoked`. The
same held for a consumer's FUSE mount and for a container binding held to the attachment.

## Root cause

Revocation was modelled as a channel property; capabilities that outlive a channel (mount attachments) were never
part of it.

## Exact edits

- `crates/db/src/partition.rs`: `attachments_held_by_consumer(consumer)`.
- `crates/server/src/verbs.rs`: `mark_revoked` (run on every shard before the acknowledgement) ends each such
  attachment through `end_attachment` (recorded; unmounts a FUSE mount; revokes the mount's registry attachment) and
  returns the refusal of one that cannot be ended; `revoke_everywhere` refuses the revocation on it.
- `crates/server/tests/common/nfs.rs`: `mount_status`.

## Proof

`crates/server/tests/daemon.rs` `a_revoked_consumers_mount_capability_reaches_nothing` (macOS, where the host mount is
offered): before the fix the consumer's handle read `0` after `Revoked`; now `NFS3ERR_ACCES`, the old capability
mounts nothing, and the account's own capability still reads.

## Carried

Guest devices admitted for a consumer are revoked by the device fan-out of AUD-29-73. Linux offers no host NFS mount;
a consumer's FUSE mount there ends through the same `end_attachment` that `tests/fuse_mount.rs` drives by `detach`.
