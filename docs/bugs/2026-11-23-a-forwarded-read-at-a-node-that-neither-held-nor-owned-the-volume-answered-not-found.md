# A forwarded read at a node that neither held nor owned the volume answered `NotFound` (2026-11-23, §4.8 "Lookup", A-394)

Found by use: `a_seeded_nemesis_of_single_faults_keeps_every_register_invariant` (`crates/cli/tests/live_chaos.rs`) on
adacachy2, run after the A-354 fix (`docs/bugs/2026-11-23-a-held-write-given-up-at-the-failover-slo-was-recorded-done.md`).
It is a safety violation: ReadSafety.

## Description

ReadSafety says a latest-state read sent after a write's acknowledgement answers that write's bytes or a refusal, never
other bytes and never missing. At seed 8173581894251929445 the first fault deafens the volume's owner from 203 ms for
1.5 s. Five reads of the newest acknowledged writes, sent through a holder of the volume's live log, answered
`NotFound` ("missing"). Every one of those writes was served by the last owner at the end, so NoLoss held: the writes
were durable, and the reads claimed they did not exist.

## Measured

- 1 of 5 runs of the nemesis on adacachy2 failed ReadSafety this way (`chaos-137 read through chaos-nemesis-a at
  992.010336ms: missing, though acknowledged at Some(224.32838ms)`, and four more). Replaying the seed
  (`SLATES_CHAOS_SEED=8173581894251929445`) failed it in 2 of 4 runs, and in 1 of 4 runs of a diagnostic build.
- The diagnostic build logged both ends of each such read:
  - The origin: "a forward was answered NotFound: … sent to Some(HostId(12386175492892640848)) (known
    Some(HostId(12386175492892640848)), cached None) … owner-by-routing Some(HostId(12386175492892640848)) configuration
    7".
  - The node it reached: "forwarded read answered NotFound here: … held false owner-by-routing None this host
    HostId(12386175492892640848) unrecovered false configuration 8".
- A status monitor of every node, every 100 ms, showed the reading node holding the live log at head 153 (past the
  five writes) with no catalog record of the volume.

## Root cause

A verb forwarded to a node runs there as on the volume's owner. A-394 (2026-11-18) guarded one case: a receiver whose
own routing names itself the owner but whose catalog does not hold the volume yet is refused `Overloaded`
(`forwarded_before_made`). A receiver whose routing names another member, or no owner, was not guarded. The origin
routed by configuration 7, which named this node the successor. The node had installed configuration 8, which routed
the volume to no owner. So the guard let the read through, `dispatch` found no volume, and answered `NotFound`.

Only the volume's owner may say that a volume does not exist (§4.8 "Lookup": "The answer comes from the current owner
and is authoritative"). A node without the record cannot tell an absent volume from one another member owns, or one
not made here yet.

## Fix

`crates/server/src/verbs.rs`:
- `refusal_of_unheld_forward` replaces `forwarded_before_made` and states the rule by authority. Where the catalog holds
  no record of the volume and the shard does not record it unrecovered, a forwarded verb is answered there only where
  this node is the volume's owner by the rule a local client's verb is routed by (`owned_by_another_host`): the
  volume's creator, which no routing on it sends elsewhere and which did not lose it to a successor. There `dispatch`
  answers a volume never made or destroyed `NotFound`. Anywhere else the verb is refused `Overloaded`, retryable and
  recorded nowhere:
  - a successor before it has made the volume, counted `fleet.forward.refused.before_made` (A-394, unchanged);
  - a node whose routing names another member, or no owner while it is not the creator, counted
    `fleet.forward.refused.not_owner_here` (new).
  
  The origin drops its cached route at any refusal (`route_after`), so the resend locates the owner again.
- `forwarded_to`: where no shard of the node holds the partition the id names, `NotFound` only from the volume's creator,
  and `Overloaded`, counted `fleet.forward.refused.not_owner_here`, from any other node.
- The test `a_forwarded_read_at_a_node_neither_holding_nor_owning_its_volume_is_refused_retryably` (beside
  A-394's). Red on main: `Refused { refusal: NotFound }` for the volume routed to no owner. Green with the fix: both
  cases are refused `Overloaded`.

Docs: §4.8 "Lookup"'s status (the rule), A-394's amendment, and GAPS (A-394's entry).

## Impact

During any configuration change that the origin and the receiver of a forward had not both installed, a read through
a non-owner could answer "missing" for acknowledged writes. A forwarded write met the same `NotFound`, recorded as its
completion, so its resend under its id met it for good, and its client was told the volume did not exist. Changes of
this kind are a takeover, a retirement undone by the host's refutation, a move, or a re-admission.

## Siblings (every forwarded entry point that can answer "absent")

| Entry point | Before | Now |
|---|---|---|
| Forwarded reads (`serve_forwarded_read`): Status, Drifted, Conflicts, CheckMerge, ReadMergeCheck, Versions, MergeRecord, ChangedSince, ReadRange, ReadWindow, ReadDir, StageBegin, StagePut, MoveWorkFloor | `NotFound` from a non-owner without the record | refused `Overloaded`, retryable |
| Forwarded writes (`forwarded_write`): every volume mutation, `Attach { form: Root }`, `DetachOf` | `NotFound`, recorded as the completion | refused `Overloaded`, nothing recorded |
| A forwarded `CreateWork`'s green check (`forward_placement`) | dispatched | refused `Overloaded` |
| `forwarded_to` with no shard for the id's partition | `NotFound` from any node | `NotFound` from the creator only, else `Overloaded` |
| A peer's reap of its dead client (`remote_attach::serve_unforwardable`) | `Detached` with nothing ended, never "absent" | unchanged |
| `named_if_unshared` | rewrites `Forbidden` only | unchanged |
| Attach forms other than the record form (mounts, exports, devices, streams): never forwarded | refused by kind, `AttachmentUnsupported { reason: OwnedByAnotherHost }` (`remote_attach::foreign_attach`, decided by creator and routing) | unchanged: already by authority |
| `volume list` | this node's catalog only, no verdict on a given volume | unchanged |
| Landing verbs (`Present`, `Land`): never forwarded | `find` answers `NotFound` on a node with no record and no register copy, even where another host owns the volume | not changed: no refusal fits (`HomedElsewhere` names another region), so it is reported to main as owed |
