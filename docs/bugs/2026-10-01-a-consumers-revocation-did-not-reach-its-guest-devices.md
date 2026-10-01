# A consumer's revocation did not reach its guest devices (AUD-29-73)

**Date:** 2026-10-01. **Audit:** AUD-29-73 (P1). **Design:** §4.13 ("every later effect from a channel bound to it
refuses `ConsumerRevoked`"), §4.6 (virtio-fs).

## Description

A guest device admitted for an enrolled consumer captured that consumer's rights at admission and served under its
registry attachment from then on. The consumer's revocation marked its client channels (and, since AUD-29-84, ended
its catalog attachments), but nothing reached its guest devices: an acknowledged revocation left the guest reading
and writing.

## Root cause

Guest devices were not linked to their consumer anywhere the revocation could find them.

## Exact edits

- `crates/bridge-virtiofs/src/admission.rs`: the admitted device keeps the consumer the seam authenticated
  (`AdmittedDevice::consumer`).
- `crates/server/src/state.rs`, `crates/server/src/daemon.rs`: `ShardState::guest_devices` (device id and consumer,
  bounded by the shard's device limit).
- `crates/server/src/virtiofs.rs`: a loop is entered in the table while it runs; `revoke_consumer_devices` asks each
  of a consumer's device loops to revoke.
- `crates/server/src/verbs.rs`: `mark_revoked` calls it on every shard before the revocation is acknowledged.

## Proof

`crates/server/tests/virtiofs.rs` `a_consumers_revocation_stops_its_guest_device`: a guest authenticated as the
consumer reads; after `Revoked` a read and a create go unanswered for five heartbeats and the loop ends `Revoked`
with its references swept. With the hook removed the later request was answered.

## Carried

An access-list reduction's effect on an admitted device (its rights were captured at admission).
