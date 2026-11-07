# A held request kept every later completion of its client (2026-11-07, §4.9 "Exactly-once", A-245)

Found by agents/fixtures on its `f = 1` rungs of 7bc80672 merged with main 7ce90ebb, with perf reading the counters,
and seen first as the backpressure column of agents/loadtest-scale's quiet-arm 10,000-client rung. Introduced as a
steady state by 9530659d, the retry hold (`2026-11-06-a-resend-under-its-id-was-already-acknowledged.md`), on a
watermark that any slow request already pinned.

## Description

A client acknowledged its completions with one watermark, `Acknowledge { up_to }`, and every partition released the
client's records at or below it. The watermark stopped below the lowest id the client still owed:
- one answered `Unpublished` and not yet retried;
- a request still awaited;
- since 9530659d, an id answered `Overloaded` or `OwnerUnreachable` and held for a resend under it.

A watermark is contiguous, so one owed id kept every later completion of its client. The owner pruned none of them,
and once they reached its bound, `2 × slots` a client, it refused the client's next requests `AcknowledgementOwed`,
unserved.

At `f = 1` a submit's acceptance waits for its commit at the quorum, past the owner's budget under load, so nearly
every submit was answered `Overloaded` and held. The load harness multiplexes 32 logical clients over one connection,
so one held submit stopped the other 31 too.

## Measured

- fixtures' rungs (adacachy, three servers at `f = 1` and one client node, kernel/, 32 a connection, think time 0,
  load 35 to 85; logs at `~/.cache/slates-agents-scratch/fixtures/rungs4/`):
  - 1,000 clients: owner shard 1 counted `acknowledgement_owed` 906,317 against 59,646 served. The harness's
    backpressure column read 905,950; submits took 9.2 s at the median and up to 120 s, and the run was ended at its
    bound.
  - 300 clients: backpressure 848,772.
  - Before the hold (bbbaddd9), at 1,000 clients: 10,772 to 27,510 a rung over six A/B runs.
- The quiet arm's 10,000-client rung at accac5c5 (one node, the Studio): 2,463,549 requests refused
  `AcknowledgementOwed` and sent again.
- `crates/server/tests/fleet.rs`, `a_client_holding_a_submit_for_its_resend_is_served_past_its_completion_bound`:
  a two-node `f = 1` fleet whose holder withholds merge records, a forwarded submit held for its resend, then four
  rings of edits from the same client, the submit resent every half ring as the harness does.
  - Red on both Linux runners, three of three (CI run 42396554623): the 64th edit refused
    `AcknowledgementOwed { retained: 64, bound: 64 }`, with the hold alive through four resends.
  - Green with the fix on both runners, five of five across runs 42401021306, 42401558923 and 42402705671: all 128
    edits served, and the last resend answered `Submitted { version: Some(1) }`.
- fixtures' 3,000-client `f = 1` rung after the fix (adacachy, e2882c6c merged with fixtures' 5cd6db04 and 8be0e745,
  load 20 to 40): the owner's `acknowledgement_owed` was 15, against 513k on 5cd6db04 alone; 83.8 s, 9,000 accepted,
  oracle 0.
- `crates/db/tests/model.rs`, `no_owed_id_is_released_whatever_order_its_acknowledgements_arrive_in` (raft's check):
  green on both runners (runs 42401558923, 42402705671), with the db, client and server suites, the fleet's forward
  and resend tests, the CLI and clippy (run 42402705671).
- `crates/server/tests/recovery.rs`, `an_unpublished_verb_stays_retryable_across_the_clients_acknowledgement`,
  asserted the old mechanism, that the watermark stops below an `Unpublished` id (red under the fix, run 42402232126).
  It now asserts the acknowledgement covers the id, names it owed, and the retry still re-executes; and that, once
  retried and acknowledged, the id is refused `DuplicateRequest` like any other.

## Root cause

The acknowledgement could say only "everything up to here", so the ids a client still owed held back every id after
them. RIFL's watermark (Lee et al., SOSP 2015) assumes the first incomplete request completes soon; a request held
for a resend, or a commit waiting on a quorum, does not.

## Fix

- An acknowledgement covers every id the client issued and names the ones at or below it still owed:
  `Acknowledge { up_to, owed }`. The owed ids are the awaited, the held within the reconnect budget and the
  `Unpublished`: at most the ring's slots and the hold's cap (`slates_client` `Client::acknowledgement`). It is due
  every half ring of requests, as before.
- Every partition releases the rest and keeps the owed ids open: an open id is answered from its record or run, and
  its completion is recorded (`slates_wire::request::ClientWindow::acknowledge`).
- An acknowledgement closes only what it speaks for (raft's rule): an open id at or below its `up_to` that it does not
  name is closed, an open id above its `up_to` is left open, and a closed id is never opened again. An
  acknowledgement that arrives late, by the scatter to the other shards or the relay with a forwarded write, can close
  only ids it covers and reopen none, so a released id is never executed twice. The watermark never moves back.
- A malformed acknowledgement, one naming an owed id above its own `up_to` or more owed ids than the client's
  completion bound, is refused whole, `BadRequest`, counted (`ack.malformed`), and releases nothing; a relayed one is
  dropped and counted. Never cut: cutting would release ids the client still owes (raft). The window ignores one
  whole if it were ever logged, so replay stays deterministic (`verbs::acknowledgement_owing`).
- The durable form: `Op::CompletionsAcknowledged` and the snapshot's `ClientCompletions` carry `owed`, changed in
  place: a log of the old form is refused by its schema hash at replay (`record.rs`), never misread, as an appended op
  would be (raft). The model's generated histories acknowledge with owed ids, the recovered partition compared whole,
  and `no_owed_id_is_released_whatever_order_its_acknowledgements_arrive_in` delivers a client's acknowledgements in
  any order, duplicated and late, with crashes between: no owed id is released, an owed id's record is kept, and every
  recovery equals the live partition. A takeover's successor serves the owner partition's records from its recovered
  database, so the open ids ride in the snapshot.
- The origin relays its window's owed ids beside the watermark with a forwarded write (`ForwardedRequest::ack_owed`),
  so the owner keeps a held submit's record too. The owner applies a relayed acknowledgement when the watermark
  advances or an id it holds open is no longer owed.

## Impact

At `f = 1` most requests were refused unserved and sent again, the owner shard's time went to refusing them, and a
held submit's own client could not proceed past a ring of requests. The oracle was unaffected: nothing ran twice or was
lost. The harness counted the refusals as backpressure, which is how the 10k rung's 2.46 million went unexplained.

## Siblings, reported, not fixed here

- The `slates_client` sync `call` path holds only `Unpublished` ids, since it never sees an id it could resend.
- A relayed acknowledgement rides only a forwarded write; a forwarded client that stops writing leaves its owner's
  records until the next one (bounded by `2 × slots`, as before).
