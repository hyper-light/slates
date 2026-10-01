# An awaited reply could be evicted from the client's reply buffer

**Date:** 2026-10-01. **Area:** `slates-client` (`Client::begin`, `buffer`, `take_ready`). **Audit:** AUD-29-22
(P1). **Design:** §4.7 async path, §4.9 bounds, banned item 8 (bounds by refusal, never by dropping owed work).

## Description

The async path drains every reply on the completion ring into a buffer keyed by request id. The buffer was
bounded by evicting its oldest entry past twice the command ring's slots, on the premise that no more
requests can be in flight than the ring holds. That premise is false for a caller that keeps old ids while it
begins and drains new ones: a completed reply leaves the ring. Such a caller could push an awaited reply out of
the buffer, and its later `poll_reply` returned nothing, forever.

## Root cause

The client had no record of which operations a caller owned. Boundedness came from dropping data instead of
refusing admission, so it could not tell a protocol-only acknowledgement's reply (safe to drop) from an awaited
one.

## Fix (`crates/client/src/client.rs`)

- **Ownership.** `begin` records each caller-owned operation in `awaited`. Its reply is kept until
  `poll_reply` takes it, or until the caller calls `Client::abandon`, the release a cancelled or terminally
  settled call uses.
- **Admission.** `begin` refuses `ClientError::TooManyOutstanding { limit }` once the ring's slots are
  outstanding (the client's in-flight bound, the same one the retryable set keeps), before sending anything.
- **The buffer.** It keeps only awaited replies, so it never holds more than `awaited` and evicts nothing. A
  reply nothing awaits — the periodic acknowledgement's, now sent unawaited, or an abandoned call's — is
  dropped on arrival and counted (`unawaited_dropped`).
- `outstanding()` lists what a binding must settle when its channel ends (AUD-29-20).

## Tests

`an_awaited_reply_survives_any_drain_and_admission_refuses_at_the_bound` (`crates/client/tests/async_core.rs`,
a real daemon):
- an early reply is held while three bounds' worth of calls are begun and drained, with acknowledgements
  falling due, and is still taken afterwards;
- admission refuses at the bound with the early call holding one slot;
- an abandoned call's buffered reply is gone, and a late reply to an abandoned call is dropped and counted;
- nothing is left outstanding.

On the old code `begin` never refused: it spun on the full ring until the client called it stalled.

## Follow-ups

The SDKs surface `TooManyOutstanding` as an error. Queuing on it, cancellation through `abandon`, and terminal
settlement are AUD-29-19 and AUD-29-20.
