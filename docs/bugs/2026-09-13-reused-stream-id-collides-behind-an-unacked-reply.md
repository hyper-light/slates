# A request reusing a stream id collides behind an abandoned exchange's unacknowledged reply

Date: 2026-09-13
Area: `crates/transport/src/endpoint.rs` (the request/reply exchange), `crates/transport/src/connection.rs`
(the receive side), `crates/cluster/src/swim.rs` (the SWIM probe wait), `crates/cluster/src/detector.rs`
(the suspicion ageing) — with the caller `crates/cluster/src/lib.rs::request_within`.
Severity: fleet correctness under load — a request the peer never serves and a reply read for the wrong
exchange; the mechanism behind the false death of a CPU-starved live peer that
`docs/bugs/2026-09-13-swim-fixed-probe-deadline-kills-a-starved-live-peer.md` closed with a re-send at the
SWIM layer, now replaced at the transport where the defect is.

## Description

Every exchange of a request *kind* reused one stream id (`PROBE_STREAM = 1`, `RECORD_STREAM`,
`CONFIG_STREAM`, …) because the server dispatched on the id. RFC 9000 §2.1 forbids reusing a stream id
within a connection, and the reason showed as soon as a caller abandoned an exchange at its deadline and
sent the next one on the same id. Reproduced on the simulated fabric (`crates/transport/tests/session.rs`,
`a_request_behind_an_abandoned_exchanges_unacknowledged_reply_is_served`): the client flushes request A
(one poll of `request`), abandons it, lets the server receive A and put its reply on the wire, then sends B
on the same id. Traced on the unchanged transport:

```
INGEST pn=3 stream=1 "request A"      → SERVE serving "request A"   (reply in flight, never acknowledged)
INGEST pn=4 stream=1 "request B"        existed=Some((9, true))      → deduplicated against A's completed stream
REQUEST got reply="ylx|lz{'H"           ('A' + 7 — the client read A's late reply as B's)
```

Recorded failure: `the server served request B behind A's unacknowledged reply: served [[warm-up],
[request A]]` — B never served; and B's "reply" was A's. Both halves of the collision in one run.

## Root cause

Three transport rules were missing and one detector rule, each shown by the traces:

1. **Stream ids were reused per kind.** B's offset-0 frame on id 1 was a duplicate of A's completed
   stream at the server (`StreamAssembler` deduplicates below the read cursor), so B was never assembled
   as a request; and A's reply, arriving on id 1, was the "reply" the client's next `request` read.
2. **The server was stuck in phase two on the abandoned exchange.** `serve_once` phase two loops until
   the reply is acknowledged; a client that abandoned the exchange never acknowledges, so the server
   retransmitted A's reply into a client that discards it while the live request B waited behind it
   (traced: `TRACE-PHASE2 stream=257 awaiting ack … newer_complete=None`, sixteen turns).
3. **Partial request bytes were call-local.** `serve_once` kept its `pending` map on the stack, so bytes
   of a request that arrived while the previous reply was being acknowledged were lost with the call.
4. **An answered probe still aged its suspect to death that same period.** With fresh streams the SWIM
   re-send loop is gone (a nonce mismatch on our own stream is a failure again), and the fleet starvation
   test then showed: five backed-off misses aged the suspicion to five, the sixth probe was **acknowledged**
   (`ACK nonce=26 rtt=43 ms`), and the tick that credited it also aged the suspicion to six — dead — before
   the refutation (which rides the reply to the *next* ping) could arrive. In the previous tree the re-send
   of probe 25's nonce reached the peer one probe earlier, masking it.

Also found on the way (recorded as a sibling below): the server drops the client's **first** 1-RTT datagram
after the handshake by design (`confirm_as_server`: "the client's exchange retransmits it"), which holds for
an awaited exchange and not for an abandoned one — the first collision harness had A dropped there and
never received (the trace showed the server's first ingest was `pn=1` = B).

## Fix

- `endpoint.rs`: a stream id is `exchange_stream_id(kind, sequence)` — the kind in the low
  `STREAM_KIND_BITS = 8` bits, a per-connection monotonic sequence (from `FIRST_EXCHANGE = 1`) above.
  `Endpoint::request(kind, bytes)` allocates the fresh id (`next_exchange`, `open_exchange`) and abandons a
  still-open exchange first; `Endpoint::abandon_exchange()` replaces the public `forget_stream`
  (`request_within` calls it on its deadline). `serve_once`/`serve_once_async` hand the handler
  `stream_kind(id)` and reply on the full id — no dispatch site changed. The server keeps
  `pending_requests` across calls, checks for a complete request **before** awaiting a datagram, serves the
  **newest** complete one (the peer runs one exchange at a time, so older complete ones were abandoned),
  raises `serve_floor` past it, and — in phase two — drains arriving bytes and, when a newer request
  completes while the reply awaits acknowledgement, drops the reply and returns so the serve loop serves the
  newer one at once.
- `connection.rs`: `discard_streams_below(floor)` drains (reads, so every byte is credited back through the
  flow-control cursors) and forgets the late replies of abandoned exchanges, the stragglers still arriving
  capped at `LATE_REPLY_STREAMS = REORDER_THRESHOLD + 1`; `forget_stream` refunds the forgotten in-flight
  bytes from `connection_sent` (the sibling the charter named — small and exact: those bytes were fresh
  sends that will never be delivered; acknowledged bytes stay counted).
- `swim.rs`: `probe_once` is one `request` raced against its deadline, abandoning the exchange when the
  deadline wins; `exchange_until_echoed`/`Echoed` removed (replace, not layer).
- `detector.rs`: `tick` passes the member whose probe was answered this period to `age_suspicions`, which
  does not age it — the suspicion stands until refuted (SWIM §4.2), but a period that heard from a member
  cannot declare it dead.

## Validation (2026-09-13, this worktree at f3ced8b + the change; box shared, load 7–8)

- Transport: `cargo test -p slates-transport --test session
  a_request_behind_an_abandoned_exchanges_unacknowledged_reply_is_served` — FAILED before (above), ok after
  (0.01 s); the crate 87 unit + 5 + 1 + 8 live, all green.
- Cluster: `cargo test -p slates-cluster` — 116 unit (incl. the new
  `an_answered_probe_does_not_age_a_suspect_to_death_that_period`) and all twelve live suites green; the
  SWIM live test rewritten as `a_late_targets_next_probe_is_acknowledged_on_its_own_stream` (target sleeps
  past the first deadline, then serves both pings; the first probe times out, the second is acknowledged
  with the served gossip, the target is alive) — failed on the transport before the phase-two yield (the
  second probe timed out behind the abandoned reply), passes after; the stale-nonce rejection test
  unchanged and green.
- Server: `cargo test -p slates-server --lib` 20/20; by use
  `cargo test -p slates-server --test fleet a_starved_but_live_peer_is_not_retired -- --exact` — with the
  SWIM re-send removed and only the transport change: ok 8.98 s; with the phase-two yield added it FAILED
  at "A kept B a member" (the detector ageing above, traced); with the detector rule: ok 9.13 s.
- Gates: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo xtask check`
  clean. Not run (the integrator's, on a quiet box): the whole fleet suite, the CLI process tests.

## Siblings

- `Endpoint::confirm_as_server` drops the client's first 1-RTT datagram after the handshake, relying on
  the client's exchange to retransmit it. An exchange abandoned before the server leaves confirmation is
  therefore lost silently and never reaches the server at all (the first probe of a freshly established
  session under a short deadline). Reported, not changed here: the fix is to ingest that datagram rather
  than drop it, which needs the serve loop to accept a request that arrived during confirmation.
- The kind constants keep their values; a handler that dispatched on the raw stream id would now see the
  kind (`stream_kind`), which every existing dispatch site already matched on — none changed.
- The MTU budget (piece 2 of the charter) is not started: several frames per packet and DPLPMTUD remain
  owed (`docs/wip/fleet-transport.md`).
