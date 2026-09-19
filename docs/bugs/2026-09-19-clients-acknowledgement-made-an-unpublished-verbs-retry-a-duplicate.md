# The client's acknowledgement made an unpublished verb's retry a duplicate

**Date:** 2026-09-19. **Found by:** the CI Linux test lane on `40aa3de`
(`crates/server/tests/recovery.rs::an_unpublished_verb_is_refused_typed_a_retry_re_executes_and_a_restart_agrees`
failed at the retry with `Refused(DuplicateRequest)`); green on this box and in a Linux container under
both epoll and io_uring (three runs each), so not a driver or platform matter.

## Description

AUD-06's contract (AC-2.3): a verb whose record cannot be published is refused `Unpublished`, rolled
back with its completion record, and **retried under the same id**, which re-executes it. The client
also acknowledges the replies it has received every `ack_every` requests (`slots / 2`, §4.9), so the
daemon releases the completion records at or below the acknowledged sequence; a later request at or
below that watermark is, by RIFL's rule, a stale duplicate (`Seen::Acknowledged` →
`DuplicateRequest`).

The two rules met. The client acknowledged `up_to = sequence` — everything it had received, the
`Unpublished` refusal included. Whether an acknowledgement fell between the refusal and the retry
depended on `ack_every`, derived from the ring's slots, which the machine's measured profile sets:
large here (no acknowledgement in the window), small on the runner (the test's next verb, a `list`,
acknowledged first). The retry then met a released sequence and was refused as a duplicate — the
contract lost to the bookkeeping, deterministically per machine.

## Root cause

The client's acknowledgement watermark did not know that an `Unpublished` reply is not an answer but a
request to try again: the sequence must stay unacknowledged until the retry is answered otherwise.

## Fix

`crates/client/src/client.rs`: the client keeps the sequences answered `Unpublished` and not yet
retried (`unpublished`, bounded by the ring's slots; past that the oldest is forgotten and counted,
`unpublished_forgotten`), and every acknowledgement — the automatic one in `call` and
`begin_ack_if_due`, `acknowledge_all`, and an explicit `acknowledge(up_to)` — stops below the lowest
of them (`ack_watermark`). A retry answered anything else releases the id. Both the sync and the async
reply paths note replies (`note_reply`).

## Failing test first

`crates/server/tests/recovery.rs::an_unpublished_verb_stays_retryable_across_the_clients_acknowledgement`:
after the refused create, the client acknowledges everything; the watermark must stop below the
unpublished id, the retry must re-execute, and once retried the id is acknowledged like any other.
Before the fix: the watermark passed the id (`3 < 2` failed) and the retry was refused
`DuplicateRequest` — the CI failure, reproduced on any machine.

## Validation (this box, 2026-09-19)

- `cargo test -p slates-server --test recovery`: **6 passed, 4.24 s**.
- The client, CLI, daemon and observe suites: see the commit message.
- `cargo clippy -p slates-client -p slates-server --all-targets -- -D warnings`, `cargo fmt --check`:
  clean.

## Siblings

- The daemon's side is right as it stands: a released sequence is a duplicate, and a rolled-back verb
  leaves no record — the two are only consistent if the client withholds the acknowledgement, which is
  the client's knowledge alone.
- The SDKs (Python, Node) acknowledge through the same client, so they inherit the rule.
