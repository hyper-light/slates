# 2026-09-10 — An abandoned content exchange leaves its stream open on the reused session, and the server folds it into the next request

## Description

While root-causing the in-process content-placement stall (see
`2026-09-10-busy-shard-never-harvests-io.md`, the primary cause), the diagnostics showed a second,
independent defect on the record plane's content path. Once a content put timed out at its round
deadline, **every later exchange on that holder's session was corrupt**: the server reported a request
of 664 bytes (a 93-byte offer concatenated with the 571-byte put that had timed out), the handler could
not decode it, and it replied empty — so the object stayed `NotPlaced` on every retry thereafter, a
permanent wedge once a single put was ever late.

## Root cause

Two halves, symmetric to `2026-09-10-abandoned-request-retransmit-lockstep.md` (which fixed the same
class for a `request`'s *own* stream, but whose "siblings swept" wrongly concluded the content exchanges
are never abandoned at a deadline):

1. **The abandoning caller left the stream open.** `cluster::request_within` races `Endpoint::request`
   against the round deadline and, on a timeout, drops the request future but **keeps the endpoint** for
   reuse (a warm session, continuous packet numbers). The abandoned exchange's stream stayed open on that
   endpoint with its data still in the send window. The content plane runs offer (stream 4) then put
   (stream 5) then a fresh offer (stream 4) each round; when the put on stream 5 was abandoned, the next
   round's offer on stream 4 reused the same endpoint, and stream 5's stale bytes rode that flush
   alongside the offer.

2. **The server folded unrelated streams into one request.** `Endpoint::serve_once` drained *every*
   receive stream into a single request buffer and served the first to complete. With the leaked stream 5
   arriving beside the offer on stream 4, the handler received their concatenation — a corrupt request
   for whichever stream completed first.

## Impact

- One late content put (which the I/O-starvation bug above made routine in-process, but which any
  momentarily slow holder causes) permanently wedged that object's placement: the reused session served
  corrupt requests from then on, so no retry could place. A takeover successor serving a dead owner's
  content over NFS could never complete its placement.
- Latent for any deadline-raced exchange on a reused fleet session (records and content both ride
  `request_within`), on laptop and fleet alike.

## Exact edits

- `crates/transport/src/endpoint.rs`: new `Endpoint::forget_stream(stream_id)` (drops an abandoned
  exchange's in-flight frames and receive state on a reused session). `serve_once` now buffers received
  bytes **per stream** (`BTreeMap<stream_id, Vec<u8>>`) and serves only the completed stream's own bytes,
  so a stray or leaked stream can never corrupt an unrelated request.
- `crates/cluster/src/lib.rs` `request_within`: on a deadline timeout, calls
  `endpoint.forget_stream(stream_id)` before handing the session back, so the abandoned stream's stale
  frames never ride the next flush.

## Validation

Both content-placement fleet tests 5/5 (`ClusterError::Uncertain`/`NotPlaced` gone; the put now places at
`f + 1`), the full in-process fleet suite 12/12, transport 85 + integration tests green.

## Siblings swept

- This corrects the "Siblings swept" note in `2026-09-10-abandoned-request-retransmit-lockstep.md`, which
  stated the content exchanges are never abandoned at a deadline: `request_within` abandons them exactly,
  and the leaked stream is the result. Both fixes (the caller forgetting the stream, and the server never
  folding unrelated streams) are kept, so neither end can re-introduce the corruption.
- `recv_stream`/`send_stream` are driven to completion inside the record plane's own session-dropping
  deadline (the whole session is dropped, not the stream), so they leak nothing; `request` already forgets
  its own stream on entry and on completion.
