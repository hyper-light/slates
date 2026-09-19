# The mount capability's attachment dies with the client that made it, and with the daemon

**Date:** 2026-09-19. **Found by:** `cargo test -p slates-server --test recovery
a_crash_at_every_durable_step_recovers_and_the_resume_reaches_the_reference` after AUD-01 bound every NFS
request to an attachment capability (the file handle carries `(attachment, token)`, validated on the
owner shard). Crash point 6 of 15 (step 3, before-publish) failed at `common/nfs.rs:131` ("WRITE
succeeded"): a WRITE through a handle minted before the restart was refused after it.

## Description

`slates mount ID PATH` (and the test support `Daemon::mount_capability`) attach and then mount the volume
under the capability the attach returned. Every later request from the kernel carries that capability in
its file handle and is authorized only if the attachment record still exists with the same token. Two
existing lifetimes end the record while the kernel mount is still live:

1. **The client's death.** The attach records `consumer: Consumer::Sdk { client }` (the CLI's ring
   client). `slates mount` exits right after `mount_nfs`; the reap loop finds the client gone within two
   liveness budgets (`LIVENESS_BUDGET_NS` = 1 s each) and `reap_client` removes every attachment of that
   client (`crates/server/src/verbs.rs`, `attachments_of_client`). The kernel mount then holds handles
   whose capability names no record: every request is refused.
2. **The daemon's restart.** `reconcile_lost` (`crates/server/src/verbs.rs`) removes every attachment of
   every rebuilt volume at recovery ("their clients attach again"). The anchor keeps the NFS listener
   across a restart precisely so the kernel mount survives it (§4.6); the mount survived, its authority
   did not.

## Root cause (confirmed by logging)

Temporary logging in `Daemon::mount_capability` and `authorized_rights`, one crash point:

```
DBG mint partition 1 attachment 1000000000001 mutate Ok(1) commit Ok(Some(1)) next_seq 2   # first daemon
DBG authorize partition 1 attachment 1000000000001 record Some((true, true, ..)) next_seq 2   # MNT, CREATE on the first
DBG authorize partition 1 attachment 1000000000001 record None next_seq 3                    # WRITE on the second
```

The record was durable (the transaction committed at sequence 1) and was replayed; sequence 2 is the
`AttachmentRemoved` that `reconcile_lost` wrote at the second daemon's boot. The catalog's consumer
taxonomy already distinguishes the consumer that owns an attachment — `Consumer::Sdk { client }`,
`Consumer::Bridge` (the OS filesystem bridge, §4.6), `Consumer::Launcher` (`crates/db/src/catalog.rs`) —
but `Bridge` had no writer: every attachment, the kernel mount's included, was recorded as the ring
client's, so it inherited the ring client's lifetime.

## Impact

With AUD-01's gate in place and no fix, a `slates mount` would work for about two seconds and every
kernel mount would go dark on a daemon restart. Before AUD-01 the NFS edge checked no attachment, so the
same records were removed with no visible effect (and the record form's lifetime was never exercised by
a mount).

## Fix

- The wire's attach form gains `AttachRequest::HostMount` (append-only): a host kernel mount the daemon's
  bridge serves. The attach records it with `consumer: Consumer::Bridge` — the attachment is the
  mount's, not the requesting process's — and returns the capability token. The SDK's record form stays
  `Root` with `Consumer::Sdk { client }`.
- `reap_client` already removes only `Sdk` attachments; `reconcile_lost` now removes only `Sdk`
  attachments at recovery and keeps a bridge's, since nothing can attach again for a kernel that holds
  handles.
- The mount's attachment ends the way the kernel ends the mount: a `MOUNT` `UMNT` of `/<name>@<capability>`
  detaches it on the owner shard (the capability validates it; the same core as the `detach` verb: the
  record removed, a green pin dropped, the holder's last write attachment releasing the lease). A `UMNT`
  of the capability-scoped root (`/@<capability>`) detaches nothing: the root is a browse over the
  capability, not the attachment's mount.
- An attachment's recorded rights are bounded by its intent: an attach-for-read records `write: false`,
  so a read attachment's capability cannot mount a writable view (the edge maps the record's rights).
- `slates mount [--read-only] ID PATH`: a write mount takes the write lease as any write attachment does
  (D-16; refused `LeaseHeld` while another principal holds it unexpired), and the read-only mount is the
  remedy — `Intent::Read`, a read-only record, `mount_nfs -o rdonly`.
- `Daemon::mount_capability` (test support) mints a bridge attachment.

## Validation (this box, 18 cores, 2026-09-19)

- The recovery crash sweep is the restart proof: `cargo test -p slates-server --test recovery` —
  **4 passed, 5.30 s**, 15/15 crash points, a handle minted before the crash resolving after it. It
  failed at crash point 6/15 before the fix (`record None`, the log above); after the consumer change
  the recovery banner reads "reconciled out … 0 attachments" where it read "1 attachments".
- `crates/server/tests/nfs_mount.rs` (**5 passed, 1.19 s**): a `UMNT` of the volume's mount path ends
  the attachment — the handle answers `NFS3ERR_ACCES`, the volume reports zero attachments and its
  lease released — while a `UMNT` of the capability-scoped root ends nothing. The reaper cannot be
  driven in-process on macOS (it asks whether the client's *process* is gone, `crate::peer::peer_gone`),
  so "the mount outlives its client" is proven across real processes:
- `SLATES_TEST_CLI=1 cargo test -p slates-cli --test cli slates_mount_establishes…` (**1 passed,
  2.75 s**, a real `mount_nfs` kernel mount): `status ID` reports one attachment while mounted; the
  daemon's `clients_reaped` moves (the `slates mount` process exited) and the file still reads back
  through the mount with the attachment kept; after `slates unmount` the kernel's own `UMNT` leaves
  zero attachments.

## Siblings

- `Consumer::Launcher` (the OCI launcher) has no writer either; the OCI bind form still records the ring
  client as its consumer, so a container bind's record ends with the client that requested it — the
  bind itself is the runtime's and outlives nothing of ours, so no capability is affected. Noted, not
  changed here.
- The `next_attachment` counter restarts at 1 on every boot (`crates/server/src/daemon.rs`), so a
  restarted daemon's next attach mints an id an attachment kept across the restart may already hold;
  the guard refuses it typed (`AlreadyExists`) instead of minting past it. Fixed here: the counter is
  seeded past every recovered attachment of the partition at boot.
