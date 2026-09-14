# A local client's acknowledgement was keyed under the ephemeral member id, not the completion record's stable anchor

Date: 2026-09-14. Status: fixed on `agent/recovery-restart`. Design: §4.9 "Exactly-once", §4.8
task #22 (the two-id model: a stable cert-anchor vs. a per-boot ephemeral member id); AC-2.3. Found
while running the recovery charter's own crate tests (`slates-server --test daemon`).

## Description

`crates/server/tests/daemon.rs`
`the_daemon_serves_the_lifecycle_verbs_exactly_once_with_leases_and_typed_refusals` failed
deterministically at the RIFL step: after creating a volume under a request id, acknowledging up to
that id, and re-issuing the id, the daemon returned the retained `Created` reply instead of
`Refused(DuplicateRequest)` (3/3 runs failed before the fix; `test result: ok` after, 13.95 s).

## Root cause

`serve` and `record_completion` key a local client's completion record under this node's **stable
cert-anchor** (`state.origin_anchor.0`) — the id that does not change across a daemon restart, so a
retry meets its record (§4.8 task #22). But `acknowledge` (`crates/server/src/verbs.rs`) keyed the
`Op::CompletionsAcknowledged` under `state.fleet.host().0`, the **ephemeral member id**
(`member_id(anchor, generation)`), and `retry_deferred` recorded a deferred local reply's completion
under the same ephemeral id. Before the ephemeral-id split those two ids were equal (both the machine
hash), so it worked; the split made them differ, so the acknowledgement pruned nothing (the records
live under the anchor) and a post-ack retry still found the record and returned its stored reply.

Two consequences, both real: completion records are never released by acknowledgement, so they grow
unbounded (violating the bounded-everything rule and §4.9's exactly-once pruning), and a retry after
an acknowledgement returns the stale reply instead of the typed `DuplicateRequest`.

## Impact

Every deployment. A well-behaved client's periodic acknowledgement (the client sends one every
`ack_every` calls, `crates/client/src/client.rs`) failed to release the daemon's retained completion
records, so a shard's completion table grew without bound over a session; and the post-acknowledgement
duplicate contract (§4.9, the test's last assertion) was wrong. Recovery-adjacent: the same records
are what a retry meets across a daemon restart (keyed under the anchor), so keeping the ack under a
different key also left the restart-survival path's records unreleasable.

## Exact edits (`crates/server/src/verbs.rs`)

- `acknowledge`: `let origin = state.fleet.host().0;` → `state.origin_anchor.0`, matching
  `serve`/`record_completion`.
- `retry_deferred`: same change for the deferred local reply's completion key.

Both now use the one key every local-completion site uses (`origin_anchor.0`, also at lines ~556,
~633, ~945).

## Sibling sweep

Every `state.fleet.host().0` use in `verbs.rs` audited. The only remaining two are **not** completion
keys and are correct: `fresh_volume_id` (the volume id's creator-host bits — fleet routing identity)
and `FleetReport.host` (the node's member id in its status report). Every completion-keying site now
uses `origin_anchor.0`.

## Scope note

This is in the §4.9/exactly-once path, adjacent to the GAP-A9-6 recovery charter rather than inside
it — surfaced by the charter's mandated crate tests, confirmed not caused by the recovery changes
(disabling the recovery publish barrier left it failing), and fixed at Ada's instruction.
