# 2026-09-13 — The trace identity was the request word, and no span named its cause

## Description

§4.14's three-id law says a span carries a request identity (for replay), a trace and span identity
(connecting work across bridges, rings, shards and holders) and a caused-by identity, and §4.9
"Trace context" says these "have different lifetimes and cannot substitute for one another" (A-9:
trace fields never authorize). The emitted spans satisfied this only in type: `emit_span` built every
span with `trace: TraceId(u128::from(id.word()))` — the request word widened — and `caused_by: None`.
So (1) the trace identity was derivable from, and equal in value to, the request identity (a retry of
a request re-used "its" trace; a bridge call, which has no client request id, got trace 0 for every
call); (2) no span named the span that caused it, so a request's `ring.request`, `shard.op` and
`log.append` could not be connected except by the request id, and a forwarded verb's spans on the
owner shard were roots with no link to the origin's ring read; (3) a `Span` could be built from any
three numbers (`pub` fields), so nothing enforced the law.

## Root cause

The emission foundation landed (2026-09-09) with a placeholder trace ("seeded from the request word
until propagation is wired") and no cause, and the status record said so; the placeholder was never
replaced.

## Impact

Observation only (trace fields never authorized anything), but every exported trace would have been
the request word and no causal chain was recoverable — the second half of GAP-A9-12.

## Fix

`crates/wire/src/observe.rs`: `Tracer` mints trace ids (node ‖ partition ‖ counter) and span ids
(partition ‖ counter); `SpanContext` is private, built only by `open_root` (`Cause::Root`),
`open_within(&cause)` (same request and trace, `Cause::Span(parent)`) and `open_unlinked`
(`Cause::Missing`, the explicit missing link); a `Span` exists only by ending an `OpenSpan`.
`crates/server`: the ring read opens the root, `run_recorded` opens `shard.op` within the current
span and `log.append` within `shard.op`, `merge.verdict` and `land.entry` within the verb, the
same-node forward carries the ring context to the owner, `deliver` ends the origin's ring span, and
the NFS bridge root carries the RPC xid as its replay identity.

## Evidence

- `crates/wire/src/observe.rs`: `a_child_span_shares_its_causes_request_and_trace_and_names_it`,
  `ids_are_distinct_across_shards_and_a_retry_opens_a_new_trace`,
  `an_unlinked_span_declares_its_missing_cause` — `cargo test -p slates-wire --lib` 29 passed.
- `crates/server/tests/daemon.rs` `telemetry_scenario`: one `Create` followed through both shards'
  drains — `ring.request` `Root`, `shard.op` `Span(ring)` on the other shard, `log.append`
  `Span(shard.op)` within it, one trace ≠ the request word, three distinct span ids, `missing_links`
  0 — `cargo test -p slates-server --test daemon the_daemon_serves_the_lifecycle_verbs_exactly_once_with_leases_and_typed_refusals -- --exact`
  ok, 15.24 s (2026-09-13).
