# A request's ring span outlasted its client's wait (2026-11-22)

## Description

The edit-and-merge bench's attribution (`crates/client/examples/edit_merge_bench.rs`, `attrib` lines) subtracts a
sample's `ring.request` spans from the client's own time and calls the rest "outside the ring". Under CI's load the
ring exceeded the client's whole total in the median band, so "outside the ring" read 0. That 0 was the subtraction's
floor, not a measurement:

- arm H, run 45108777338, arm64: ring 466 µs against a client total of 387 µs;
- G+E, run 45104143760, arm64: 827 against 710;
- main itself (1ce8a383a) on x86_64 in run 45108777338: 252 against 104.

Quiet runs did not show it (ring 95 against client 121 µs, x86_64 main). Both w-perf's and main's readings of G+E
leaned on the split before this was seen.

## Root cause

The span is the slot read to the reply written (§4.14 "Span roster"). Both of its ends were read at the wrong time:

- **Start.** `serve_client` opened the span at `now`, the round's one clock read, taken before any slot. The comment
  called it "a small, conservative over-estimate within one batch". But a shard can lose its CPU inside a round: at
  the wake of the reply before, a system call a loaded host deschedules at. A request its client published meanwhile
  was then taken in the same round under the old reading. Its span started before the client had sent it.
- **End.** `send_reply` returned before the end was read, and it returns only after `DaemonEnd::reply` has woken the
  parked client and nudged its completion descriptor. Both are system calls. So the end could fall after the client
  had already seen its reply.

## Impact

- The bench's ring and "outside the ring" are wrong under load, by as much as the shard spends off its CPU. Every
  reading of an A/B's attribution split under load before this fix is suspect: the G+E and H rows in BENCHMARKS say
  so.
- The shard's telemetry export (`Telemetry`, `slates status`) carried the same spans.
- No served result was affected.

## The failing test, then the fix

`a_requests_ring_span_lies_within_its_clients_own_time` (`crates/server/tests/daemon.rs`) runs a one-shard daemon
under the raw ring client:
1. It publishes a 2 MiB edit and, once the shard has taken it, a second request.
2. It takes both replies and drains the shard's spans.
3. It holds each request to one ring span that starts no earlier than the client's publication and ends no later than
   the client's sight of the reply.

Before the fix it failed on an idle MacBook: the edit's span started 165 µs before the client published it. The shard
had read the round's clock, lost its CPU at the wake of the drain's reply before it, and took the edit in the same
round.

Exact edits:
- `crates/server/src/verbs.rs`:
  - `serve_client` reads the clock at each slot read and opens the span there. `now` still marks the round for the
    reap's silence clock.
  - `send_reply` returns `Sent` (`Written(published_ns)`, `Dropped`, `Full`). The synchronous and the deferred paths
    end the span at the publication; a reply dropped for a gone client ends it at the shard's clock then.
- `crates/ipc/src/endpoint.rs`: `DaemonEnd::reply` and `reply_message` return the clock read just before the reply's
  push. The slot's stamp carries the same reading (`slot::stamp_at`, beside `stamp_now`).

After the fix the test passes. Both bounds now hold by order: the shard reads its clock after the take, which follows
the publication, and the reply's reading precedes its push, which precedes the client's sight of it.

Siblings checked: a forwarded request's span ends through `deliver` and `retry_deferred`, now at the publication too.
`refuse_unsent_forward` goes through the same deferred path.
