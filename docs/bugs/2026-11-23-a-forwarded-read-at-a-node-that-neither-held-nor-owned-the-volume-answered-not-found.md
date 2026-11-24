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
  answers a volume never made or destroyed `NotFound`. Anywhere else the verb is refused, retryable and recorded
  nowhere:
  - a successor before it has made the volume: `Overloaded`, counted `fleet.forward.refused.before_made` (A-394,
    unchanged: it owns the volume and will serve it);
  - a node whose routing names another member, or no owner while it is not the creator: `Refusal::NotOwnerHere {
    owner }`, `owner` the owner its routing names, counted `fleet.forward.refused.not_owner_here`.
- `forwarded_to`: where no shard of the node holds the partition the id names, `NotFound` only from the volume's creator,
  and `NotOwnerHere` from any other node.
- `absent`: a verb that is not forwarded (a landing's `Present` or `Land`) on a node that holds no record or register
  copy of a volume another host owns as far as it can tell (`remote_attach::elsewhere`, now crate-visible), or one it
  lost to a successor, meets `NotOwnerHere` instead of `NotFound`.
- `route_after`: a `NotOwnerHere` naming an owner keeps that owner as the client's route, counted
  `fleet.owner_location.route_hinted`, and any other refusal drops the route, so the resend locates the owner again.

The refusal is its own kind by main's ruling: a first cut (6b2952f4d) answered `Overloaded`, which says a shard is
busy. The wire is `Refusal::NotOwnerHere { owner: Option<u64> }`, appended at tag 84 (`crates/ipc/tests/wire_order.txt`).
The API has `RetryCause::NotOwnerHere` (contract 60). `slates_client` resends a read so within its reconnect budget
(`call_through_transient`) and holds the id for an async caller (`note_reply`). The CLI, MCP and both SDKs render a
refusal by its `Debug` form, so they carry it by name with no change.

Tests:
- `a_node_neither_holding_nor_owning_a_volume_answers_not_owner_here_and_its_creator_not_found` (`crates/server/src/verbs.rs`):
  a forwarded read routed to no owner and then to the creator, the forwarded write's gate, a landing's `Present` and
  `Land`, the creator's own volume mid-destroy (its `Destroying`) and an id it never made (`NotFound`). Red on
  6b2952f4d's code with the variant in place: both reads and the write gate `Overloaded`, `Present` and `Land`
  `NotFound`. Green with the fix.
- `a_node_that_is_not_the_volumes_owner_is_a_retry_naming_the_owner_it_knows` (`crates/api/src/error.rs`).

Docs: §4.8 "Lookup"'s status (the rule), A-394's two amendments, and GAPS (A-394's entry).

## Impact

During any configuration change that the origin and the receiver of a forward had not both installed, a read through
a non-owner could answer "missing" for acknowledged writes. A forwarded write met the same `NotFound`, recorded as its
completion, so its resend under its id met it for good, and its client was told the volume did not exist. Changes of
this kind are a takeover, a retirement undone by the host's refutation, a move, or a re-admission. A landing sent to a
node that is not where the volume is was told the volume did not exist.

## Regression found later (2026-11-23)

The guard this fix added in `forwarded_write` ran before a request's recorded completion, and it left out A-255's
authority over a work no member owns. So four takeover-resend fleet tests met `NotOwnerHere` for good. They were fixed
in the same change as the forwarded-write fix; see
docs/bugs/2026-11-23-a-takeover-resend-met-the-unheld-forward-guard-before-its-completion.md, which names the suites
a change here must run.

## Found later: the write policy's verbs (2026-11-23, main's ruling)

`landing::policy_record` answered a volume with no record `NotFound` itself, so the policy's read and setting at a node
that knows the volume is elsewhere said it did not exist, while a landing's verbs at that node answered `NotOwnerHere`.
It now meets `verbs::absent`, one rule for every slotless verb. `crates/cli/tests/fs_server.rs`'s ask landing expects
`NotOwnerHere` for both. Red on main: the policy was answered `NotFound` in 97 µs. Green: both are `NotOwnerHere` in
240 µs and 156 µs, so the caller gets the refusal at once and no retry budget is spent.

## Siblings (every entry point that can answer "absent" from a node that is not the owner)

| Entry point | Before | Now |
|---|---|---|
| Forwarded reads (`serve_forwarded_read`): Status, Drifted, Conflicts, CheckMerge, ReadMergeCheck, Versions, MergeRecord, ChangedSince, ReadRange, ReadWindow, ReadDir, StageBegin, StagePut, MoveWorkFloor | `NotFound` from a non-owner without the record | `NotOwnerHere`, retryable |
| Forwarded writes (`forwarded_write`): every volume mutation, `Attach { form: Root }`, `DetachOf` | `NotFound`, recorded as the completion | `NotOwnerHere`, nothing recorded |
| A forwarded `CreateWork`'s green check (`forward_placement`) | dispatched | `NotOwnerHere` |
| `forwarded_to` with no shard for the id's partition | `NotFound` from any node | `NotFound` from the creator only, else `NotOwnerHere` |
| Landing verbs (`Present`, `Land`): never forwarded | `NotFound` from `find` on a node with no record and no register copy | `NotOwnerHere` |
| A peer's reap of its dead client (`remote_attach::serve_unforwardable`) | `Detached` with nothing ended, never "absent" | unchanged |
| `named_if_unshared` | rewrites `Forbidden` only | unchanged |
| Attach forms other than the record form (mounts, exports, devices, streams): never forwarded | refused by kind, `AttachmentUnsupported { reason: OwnedByAnotherHost }` (`remote_attach::foreign_attach`, by creator and routing) | unchanged: already by authority |
| `volume list` | this node's catalog only, no verdict on a given volume | unchanged |
| A held copy not yet materialized (`absent`'s register copy) | `Overloaded`, the takeover settle window | unchanged |
