# Past the stream limit, data was dropped but acknowledged

Date: 2026-09-28. Contracts: §4.10a (the session plane's streams), RFC 9000 §4.6 (stream concurrency) and
§19.11 (`MAX_STREAMS`). Found while writing the concurrent-exchange tests that Ada asked to probe
deadlocks and leaks; confirmed by a run before it was fixed.

## Symptom

A receiver holds reassembly state for a bounded number of the peer's streams: the receive ceiling over the
frame cap. When a frame for a new stream arrived past that bound, the receiver dropped the frame and counted
a violation. It still acknowledged the packet the frame came in, as it must for the packet's other frames.
The sender then treated the stream data as delivered, never retransmitted it, and waited for a reply that
could never come. The sender had no way to know the limit, so nothing stopped it opening those streams.

## Evidence

With the credit check switched off (the sender free to open past the receiver's limit, as before this
change), `concurrent_exchanges_past_the_stream_limit_survive_every_hostile_path_and_leak_nothing` fails on
its first scenario:

```
5% random loss, seed 1: the client failed: ... "stalled in the client's work: no progress by 120 virtual seconds"
```

With the credit check in place it passes, for every hostile path and seed, with no protocol violations on
either end. The test drives the credit to exhaustion: its non-vacuity assertion is that the typed backlog
refusal was met.

## Root cause

The stream limit existed only at the receiver. RFC 9000 enforces concurrency at the sender instead: the
receiver advertises how many streams the peer may open (`MAX_STREAMS`), and the sender never opens past it.
A receiver that drops data inside an acknowledged packet breaks the reliability contract.

## Fix

`crates/transport/src/streams.rs` (`StreamSpace`) is the session's stream-id space:

- **Ids.** Each end allocates its own sequences in order, and the initiator is a bit in the id, so both ends
  can open exchanges without collisions.
- **Credit.** A receiver extends credit (the limit plus the number of the peer's streams it has closed) in a
  `MaxStreams` frame (kind 7). A sender's stream past the credit waits unsent.
- **Refusal.** When a limit's worth of streams is already waiting, `Endpoint::begin` refuses with the typed
  `StreamRefusal::Backlogged`.
- **Closing.** A stream returns credit only when both halves are done. A request still awaiting its reply
  keeps its credit spent, so a slow server holds back its client.

## Sibling found in the same change

Closed receive streams were remembered in a tombstone set pruned after three probe timeouts. A late copy
arriving after the prune reopened the stream as a partial one that was never freed. Found by reading, not
reproduced. The stream-id space replaces the set: every sequence below the peer's highest seen is either
open, awaited, or closed, with no per-stream memory. So a late copy is recognised as closed forever.
`the_space_equals_a_model_that_remembers_everything` is the oracle.
