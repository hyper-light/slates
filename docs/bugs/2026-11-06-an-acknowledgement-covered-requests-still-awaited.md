# An acknowledgement covered requests still awaited (2026-11-06, §4.9 "Exactly-once", AC-2.3)

Found by agents/loadtest-scale while building the 10,000 to 100,000 client harness, which multiplexes many logical
clients over one connection, so one connection has many requests in flight at once.

## Description

`slates_client::Client::ack_watermark` returned the last sequence the client had sent, held back only below an id
answered `Unpublished`. `begin_ack_if_due`, which the async path calls after taking a reply, therefore acknowledged
every request sent so far, including requests whose replies had not come back. The daemon releases a client's
completion records up to the watermark (`Op::CompletionsAcknowledged`), and a record at or below the watermark is
refused (`Op::CompletionRecorded` is checked `StaleCompletion`, which maps to `DuplicateRequest`).

## Reproduction (the failing test)

`crates/client/tests/acknowledge.rs`, `an_acknowledgement_never_covers_a_request_still_awaited`. The setup is a
two-shard daemon under a played anchor, ring of 4 slots, so the acknowledgement is due after two replies. The other
shard is held for one liveness budget, and a create it owns is begun (its reply waits for the hold). Two creates the
client's own shard owns are begun, their replies taken, and `begin_ack_if_due` sends what it finds due. The hold
ends, and another client sees the held create applied. The daemon is restarted over the same anchor, the client
reconnects under its id, and it resends the held create under its own id.

On 00e5269c with agents/loadtest-net merged: `acknowledged up to 4, the awaited create's resend answered
Err(Refused(DuplicateRequest))`, while the volume it made was listed. With the fix: `acknowledged up to 0`, and the
resend is answered `Created` with the listed volume's id.

## Root cause

The watermark ignored the outstanding set. A sync caller has one request in flight and acknowledges inline before
the next, so it never acknowledged past an unanswered request. An async caller with several in flight did.

## Impact

- **Every async caller with more than one request in flight.** That covers the SDK cores built on
  `begin`/`begin_ack_if_due`, and the many-client harness once it multiplexes. Across a daemon restart, an applied
  verb whose reply was lost with the channel is answered `DuplicateRequest` instead of its result.
- **On the fleet.** A forwarded verb's completion is recorded at its owner, and the origin relays the client's
  watermark with each later forward (`verbs::forwarded_request`, `prune_forwarded`). So a logical client's deferred
  submit could have its record pruned by another logical client's forward before its commit recorded it. The
  acceptance would then be lost to the submitter (`await_deferred_completion` sees `Acknowledged`, answers
  `Overloaded`, and the retry meets `DuplicateRequest`). Today the forwards to one owner go one at a time (GAPS, the
  network load test's fault one), which happens to order them. Fixing that fault would have exposed this one.

## Exact edits

- `crates/client/src/client.rs` `ack_watermark`: the minimum of the old watermark and one below the lowest
  sequence among the awaited requests whose replies have not been drained into `pending`. A drained reply is
  received, and its own poll answers from the buffer first.
- `docs/wip/SLATES_DESIGN.md` §4.9 "Exactly-once": status blockquote.
- `docs/wip/GAPS.md`: the scale load test's entry.

## Consequence for callers

A connection that keeps completing requests past an old one still awaited meets `AcknowledgementOwed` at the
`2 × slots` bound, as backpressure, until that request answers. The refusal is never run or recorded, so it is
sent again as new. The load harness counts these sends (`backpressure`).

## Siblings swept

- `Client::acknowledge(up_to)` and `acknowledge_all` go through `ack_watermark`, so they are fixed with it.
- The sync `call` acknowledges inline with one request in flight, so it was unaffected.
- Chain steps of a large async edit are begun through `begin`, so they are in the awaited set and covered.
- The attest in flight after a reconnect is outside the awaited set. Acknowledging it is safe, since a lost attest
  is made again on the next send.
