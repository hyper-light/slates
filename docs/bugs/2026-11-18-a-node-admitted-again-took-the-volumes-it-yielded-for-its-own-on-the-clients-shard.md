# A node admitted again took the volumes it yielded for its own on the client's shard

**Date:** 2026-11-18. **Goal:** g229 (chaos across real processes). **Amends:** A-334 (its two owed siblings).
**Fixed in:** `crates/server/src/remote_attach.rs`, `crates/server/src/verbs.rs`.

## Description

A host the council retired while it was alive refutes its death and is admitted again under its own id. A-334 made each
of its shards yield the volumes it catalogued then (`ShardState::yielded`), so a verb on one goes to its successor. Two
of A-222's attach rules still took such a volume for this node's own:

- **A mount of a yielded volume attached the stale copy.** A-222 refuses every attach form but the record form for
  another host's volume, by its kind (`AttachmentUnsupported`, `OwnedByAnotherHost`), since such a form hands the client
  something of this host. The refusal is decided on the client's shard (`remote_attach::foreign_attach`) by the id's
  creator and this node's routing. The volume was created on this node, so the rule took it for this node's own. The
  attach then ran on the volume's shard and attached the copy whose writes the successor had passed. With the NFS form,
  the old owner answered `Attached` with the record form's capability.
- **A forwarded record-form attach of a yielded volume was never reaped.** A record-form attach of a yielded volume is
  sent to its successor (`verbs::owned_by_another_host`), which records it under the old owner's client. A-222's reap
  ends such an attachment when its client dies, but only for volumes the client's shard remembered before sending the
  attach (`remote_attach::remember`). The same creator rule kept this one out, and the reap's own filter
  (`reap_at_owners`) dropped volumes this node created as well. The attachment stayed on the successor for good.

## Root cause

The client's shard cannot see the loss. A node admitted again lost every volume it created, but the yield and the fence
live on the volume's shard (`verbs::lost_to_a_successor`), and the client's shard knows only the id's creator and its
own routing. The routing forgot the host's own entries at the re-admission, so it names no owner, and the creator names
this node.

## Impact

No register invariant: the mount gates (A-346's, joined to A-334) answer such a mount `NFS3ERR_STALE` or end it as
revoked, so nothing stale is read through it. The attach itself succeeded where it should have been refused, and a dead
client's forwarded attachment of a yielded volume stayed recorded on the successor. The successor kept counting it, and
it held the volume's attachment bound, though the write lease ended at its term.

## Exact edits

- `remote_attach::lost_attach`, asked on the volume's shard in `verbs::serve_here` and in `send_forward`'s owner task
  before `owned_by_another_host`. A non-record attach of a volume this node lost to a successor is refused by its kind,
  as `foreign_attach` refuses another host's volume, and its completion is recorded. Counted
  `attach.foreign_refused.lost_to_successor`.
- `remote_attach::lost_record_attach`, `remember_volume` and `remember_volume_at`. A record-form attach of a lost volume
  is remembered with its client before it is sent, decided on the volume's shard:
  - directly, when the client's shard is the volume's;
  - otherwise by a cross-shard call to the client's shard (`verbs::remembered_or_refusal`), which refuses
    `RemoteAttachmentsFull` past the client's bound and answers `Overloaded`, not started and not recorded, when the
    client's shard does not answer within a liveness budget. The attach is never sent unremembered.
- `remote_attach::reap_at_owners` keeps every remembered volume this node's routing does not name it the owner of. The
  creator rule is dropped there: it took a volume this node created and lost for this node's own.

## Tests (red first)

Both are in `crates/server/tests/fleet.rs`, on A-334's setup (`yielded_by_an_owner_admitted_again`).

- `an_owner_admitted_again_refuses_a_mount_of_the_volume_it_yielded_by_kind`. Red: an NFS host mount through the old
  owner answered `Attached { attachment: 2, … }`. Green: refused `AttachmentUnsupported { NfsLoopback,
  OwnedByAnotherHost }`.
- `a_holder_of_a_volume_its_node_yielded_is_reaped_at_the_successor`, with a re-invoked holder process
  (`yielded_holder_role`). Red: the successor held the attachment, and after the holder was killed it was never reaped
  (`left Some(1)`, `remote_attach.ended` 0, after the whole period budget, 414 s). Green: none left once the old
  owner's reap ran.
