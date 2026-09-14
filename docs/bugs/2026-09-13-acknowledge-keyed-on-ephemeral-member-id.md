# 2026-09-13 — An acknowledgement never released a completion: keyed on the ephemeral member id

## Description

`slates-server` keeps each request's completion (RIFL, §4.9 "Exactly-once") in a window keyed by
`(origin, client)`. Since `cefb159` ("make the member id ephemeral per boot, RIFL origin on the
stable anchor", task #22, 2026-09-12) a local request's window is keyed on the node's **stable
cert-anchor** (`state.origin_anchor`) — `serve` records and looks up under it (`verbs.rs:635`,
`send_forward` `:941`) — but two sites were left on the **ephemeral member id**
(`state.fleet.host()`): the `Acknowledge` verb (`acknowledge`, `verbs.rs:3447`) and the completion a
deferred reply records (`retry_deferred`, `:3669`). `deploy::member_id(anchor, generation)` is
`BLAKE3(anchor ‖ generation)`, so the two keys never agree, at any generation.

Consequences:

1. An acknowledgement created or advanced a window under `(member_id, client)` that no record was
   ever written to, and left the real window untouched — so **no acknowledgement ever released a
   completion** and a retry of an acknowledged request was still answered from its record instead of
   refused `DuplicateRequest`. Retained completions therefore only ever grew (the design's "retained
   response windows have bounds and acknowledgements" was not in effect; ban 8).
2. A reply that came back through `deliver` (a scatter's — `List`, `DaemonStatus`, `Acknowledge` — or
   a same-node forward's) had its completion recorded under the member id while a retry of it is looked
   up under the anchor, so such a retry re-executed instead of returning the retained reply.

## Root cause

Two call sites not updated when the RIFL origin moved to the stable anchor (`cefb159` changed
`serve`/`send_forward`; `f43130a` had introduced `fleet.host().0` at all sites the day before).

## Evidence

`crates/server/tests/daemon.rs`, `rifl_scenario` (part of the serial test
`the_daemon_serves_the_lifecycle_verbs_exactly_once_with_leases_and_typed_refusals`), whose last
step retries an acknowledged request:

- Before (2026-09-13, this branch at `c80b6f9` plus the §4.14 work, which does not touch these paths):
  `cargo test -p slates-server --test daemon the_daemon_serves_the_lifecycle_verbs_exactly_once_with_leases_and_typed_refusals -- --exact`
  → `FAILED` at `daemon.rs:283`: "a retry of an acknowledged request is a stale duplicate, got
  `Created { id: [148, 198, 63, 101, 199, 78, 65, 102, …] }` (the original volume was
  `VolumeId { bytes: [148, 198, 63, 101, 199, 78, 65, 102, …] })`" — the **retained** reply: the
  acknowledgement had released nothing. Deterministic across three runs.
- After the fix: the same command → `ok` (15.59 s, the whole serial lifecycle test).

## Impact

Every daemon since `cefb159`: unbounded completion retention per client (memory growth in the
partition until restart), and a retry of a scattered or same-node-forwarded verb re-executing. The
CLI's `status` flow test did not catch it because it never retries after an acknowledgement; the
server's `rifl_scenario` did, on the first run of the serial test on this branch.

## Fix (minimal; sibling sweep in the same change)

`crates/server/src/verbs.rs`: `acknowledge` and `retry_deferred` key on `state.origin_anchor.0`
(with the comment naming this record). Sweep of every completion-window key site in the crate
(`completion(`, `acknowledged_up_to(`, `CompletionsAcknowledged`, `record_completion(`,
`prune_forwarded(`): `serve` (`:635`/`:639`), `send_forward` (`:941`), the forwarded client's
watermark relay (`:558`) and `prune_forwarded` (the peer's authenticated origin) were already on the
anchor; the two remaining `fleet.host().0` uses (the volume id's creator field, `:97`; the fleet
report's `host`, `:1268`) are not RIFL keys. No other instance.

Test message improved to print both ids (`daemon.rs`), so a recurrence names the retained reply.
