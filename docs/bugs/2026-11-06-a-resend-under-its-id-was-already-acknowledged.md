# A resend under its own id was already acknowledged (2026-11-06, §4.9 "Exactly-once", §4.16 "Commit", AUD-11)

Found by agents/fixtures on its 1,000-client `f = 1` Linux rung of agents/loadtest-scale-1 at 12596890, merged with
its own 89347125 (A-235). The defect was introduced by 12596890, which made the harness resend refusals retryable
under their own id (`Client::begin_again`). That commit was never on the landing list.

## Description

Some refusals are retryable under the request's own id. For a forwarded submit at `f > 0`, the verb ran on the owner,
and its acceptance waits for the version's merge record to commit at the quorum. Past the owner's liveness budget
the origin answers `Overloaded` (`await_deferred_completion`). The completion is recorded when the version commits,
and a resend under the same id meets it.

The client's acknowledgement watermark stopped below an `Unpublished` id awaiting its retry, and below every request
still awaited. Nothing else held it. Once the `Overloaded` reply was taken it counted as received. The next periodic
`Acknowledge` covered the id, the owner pruned up to it, and the resend under the id met `Seen::Acknowledged`:
refused `DuplicateRequest` for a submit whose version had committed.

The harness also resent `ContentUnavailable` under its id. That refusal is recorded as the verb's completion (a fenced
green, unreadable content), so each resend only met it again until the budget ran out.

## Measured

- fixtures' rung (Linux, `f = 1`, 1,000 clients, log at adacachy
  `~/.cache/slates-agents-scratch/fixtures/rungs2/1k-clean.log`):
  - The run's wall time was 264 s, against 17.6 s without the resends.
  - Refusals: 447 `SubmitAccepted DuplicateRequest`, 251 `ReadHead DuplicateRequest`, 125 `SubmitAccepted NotFound`,
    64 `Edit NotFound`, about 60 "still refused retryable after 120 s".
  - The harness made 7,906 resends.
- The failing test, `a_forwarded_submit_refused_overloaded_while_it_commits_is_answered_under_its_own_id` in
  `crates/server/tests/fleet.rs`, on this Mac (loaded arm, debug), two nodes, `f = 1`, the holder withholding records:
  - The setup: the forwarded submit is answered `Overloaded`, the client acknowledges, and the version commits.
  - Before the fix, the resend under its id was refused `DuplicateRequest`.
  - After the fix, it is answered `Submitted { version: Some(1) }`.

## Root cause

The rule that keeps the watermark below an id the caller will resend (§4.9) covered only `Unpublished`.
`Overloaded` and `OwnerUnreachable` are retryable under the same id, and the verb may have run.

## Fix

- **The client** (`crates/client/src/client.rs`).
  - On the async path (`poll_reply_word`, and a reply drained into `pending`), an `Overloaded` or `OwnerUnreachable`
    refusal holds the watermark below its id (`retry_held`).
  - The hold lasts within the reconnect budget from the refusal, the window in which a resend under the id is made,
    and each refusal renews it.
  - The hold is bounded by the ring's slots, with the oldest released and counted (`retry_released`).
  - `release_retryable(word)` releases an id the caller gives up on. The sync `call` path never sees its id, so it
    holds only `Unpublished`, as before.
- **The harness** (`crates/client/examples/load_bench.rs`).
  - `ContentUnavailable` is no longer resent under its id: it is reported.
  - A client that gives up a retryable id releases it.

## Impact

Every forwarded submit whose commit outlived the owner's budget was answered `DuplicateRequest` on its resend. The
oracle then counted those submits as failures, and the resends loaded the client node and the primary. The
`NotFound` answers are the same race seen later (the completion pruned, the work consumed).

## Siblings, reported, not fixed here

- `Overloaded` raised inside a verb (`fs_verbs::admit`, `image::defer` at a full task arena, `direct_map`) is
  recorded as the verb's completion. A resend under its id therefore meets the recorded refusal forever, though the
  protocol calls it retryable ("the request was not started"). The protocol should either not record those, or
  answer them as a refusal to resend as new.
- The synchronous `Client::call` gives its caller no id, so it cannot resend under one. A sync caller answered
  `Overloaded` for a deferred acceptance can only submit again as new: the server's idempotent accept answers that
  only if the increment identity matches.
