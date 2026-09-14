# Acknowledgements and deferred completions keyed on the ephemeral member id: a retry after `acknowledge` re-serves the retained reply (§4.9 exactly-once, task #22)

- **Date:** 2026-09-14
- **Subsystem:** the server's RIFL completion window (`crates/server/src/verbs.rs`: `acknowledge`, `retry_deferred`; the partition's `completions` table keyed `(origin, client)`).
- **Severity:** correctness of the exactly-once contract (§4.9, AC-2.3). An acknowledged request's retry is served from its retained record instead of being refused `DuplicateRequest`; the acknowledgement never prunes the window it was meant to (unbounded retention of acknowledged completions, banned item 8, until a restart); a retry of a request whose reply was deferred (the client's ring full) finds no record and runs the verb a second time.
- **Found by:** `crates/server/tests/daemon.rs::the_daemon_serves_the_lifecycle_verbs_exactly_once_with_leases_and_typed_refusals` (`rifl_scenario`, line 282), which fails deterministically — 3 of 3 runs on the branch and 1 of 1 on the unmodified base commit `c80b6f9` (the base tree exported with `git archive` and run with the same command), each in ≈1.8 s, so it is not a timing flake. Found while running the digest work's daemon scenario (GAP-A9-13), 2026-09-13/14.

## Symptom

`rifl_scenario`: create under request id `(client, seq)`, retry the same id (the retained `Created` comes back, one volume — correct), `Acknowledge { up_to: seq }` → `Acknowledged`, then retry the same id again: expected `Refused(DuplicateRequest)`, got the retained `Created` reply again.

## Root cause

Commit `cefb159` ("server: make the member id ephemeral per boot, RIFL origin on the stable anchor", task #22) moved the completion **record** key to the node's stable cert-anchor: `serve` records and checks a local client's completions under `state.origin_anchor.0` (`verbs.rs` `serve`, the same-node cross-shard `run_forwarded` site, and the relayed watermark read in `send_forward`). Two **writers** were left on the ephemeral member id `state.fleet.host().0`:

- `acknowledge` wrote `CompletionsAcknowledged { origin: fleet.host().0, .. }`, so the acknowledgement landed in a `(member_id, client)` window that nothing reads — `serve`'s check under `(anchor, client)` still saw `Seen::Completed` and returned the retained reply; the anchor window's records were never pruned.
- `retry_deferred` recorded a deferred reply's completion under `fleet.host().0`, so a retry of that request, checked under the anchor, was `Seen::New` and ran the verb again.

`daemon.rs` derives the member id as `member_id(origin_anchor, generation)` with a per-boot generation, so the two ids differ on every boot, including a laptop's (R8: the same code path at f = 0). An earlier fix of the same class (`e18619a`, "ack keyed on the ephemeral member id fixed", GAP-A9-12) predates `cefb159`, which re-introduced it for these two sites; the sweep in `cefb159` covered the readers and one writer, not all writers.

## Fix

`crates/server/src/verbs.rs`: `acknowledge` and `retry_deferred` key on `state.origin_anchor.0`. Every completion-window reader and writer for a local client now uses one id — the stable cert-anchor — and a cross-node forwarded verb uses the peer's anchor (`fleet.rs` passes `peer_anchor` to `serve_forward`; `prune_forwarded` and `run_forwarded` key on it), which was already consistent.

Evidence: the daemon scenario fails 3/3 before and passes after; the exact command is in the commit and in `docs/wip/clean-digest.md`.

## Sibling sweep

Every use of `fleet.host().0` as a completion origin in `crates/server/src` was listed (`grep -n 'fleet\.host()\.0\|origin_anchor'`): the remaining `fleet.host().0` sites are `ObjectId::creator` (`verbs.rs:97`) and the status report's host (`:1267`), both of which the two-id model says must stay ephemeral. No other writer of `CompletionsAcknowledged` or `CompletionRecorded` exists outside `record_completion`, `acknowledge`, `prune_forwarded` and `retry_deferred`.

Not covered by a dedicated by-use test in this change: the deferred-reply path (it needs a client whose ring is full when the reply is ready); its fix is the same one-line key change and is reported here rather than proven.
