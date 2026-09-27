# A late copy of a served request forgot the reply in flight

Date: 2026-09-27. Contracts: §4.10a §8 (request/reply over the session plane, `Endpoint::serve_once`), §4.8
(every fleet RPC rides it). Found by the congestion bake-off (`crates/transport/examples/congestion_bakeoff.rs`):
a 64 kbit/s, 20 ms, 5 %-loss run never finished.

## Symptom

The ping exchange deadlocked. Tracing every packet of both ends showed:

1. The client's request on stream 7169 went out and was lost.
2. The client's probe copy (pn 60) reached the server, 226 ms later on the thin link. The server served
   it, set its floor to 7170, and sent the reply in pn 71. The reply was lost too.
3. A second probe copy of the request (pn 61) reached the server.
4. The server logged its reply "complete" at the same instant, with no acknowledgement of pn 71 ever
   received.
5. The client waited for the reply forever. Both ends then idled; virtual time ran to 152,000 seconds.

## Root cause

A request and its reply share one stream id. On each turn `serve_once` calls `discard_streams_below(floor)`
to drop late copies of already-served requests. That called `Connection::forget_stream(id)`, which forgets
**both** halves of the id, including the reply this end is still sending. The reply's frames left tracking
unacknowledged, `send_complete` became true, and nothing ever retransmitted it.

The bug predates the constrained-link work. It needs a request copy to arrive after the server has served
it, which on a lossless or fast path almost never happens.

## Fix

`Connection::forget_recv_stream` forgets only the receive half (its reassembler and its per-stream receive
accounting). `discard_streams_below` uses it. The whole-stream `forget_stream` stays where an exchange has
truly finished.

In the same change: a stream frame the peer sends in breach of flow control or a final size used to be
ignored (`let _ = offer(...)`). It is now counted (`Connection::protocol_violations`), as the
swallowed-error rule requires.

## Evidence

- `crates/transport/src/connection.rs` `discarding_a_late_request_copy_keeps_the_reply_in_flight`: it
  failed first ("the reply is still owed"). It passes now, and the probe timeout delivers the reply.
- The bake-off scenario finishes in 0.43 s for all five laws and three seeds.
