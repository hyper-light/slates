# A client that never acknowledged grew its completion records without bound

**Found:** 2026-10-07, running a new fleet test against the old routing rule. The fleet harness's raw client asked a
status about once a second for 24 minutes without acknowledging. The daemon then stopped answering within the
client's 5 s deadline (`client Status: deadline exceeded`, 1,425 s into the run). A stack sample of the test
process put the shard's time in `ClientCompletions` encoding.

## Description

Every verb records a completion, keyed by its request id, so a retry is answered from the record (§4.9
"Exactly-once"; RIFL, Lee et al., SOSP 2015). A record is released only when its client acknowledges it.
`slates_client` acknowledges every half ring of replies, but nothing on the daemon enforced it. A client that did
not acknowledge grew the shard's records without end: a third-party ring client, a buggy SDK, or a test harness.

Every partition snapshot encodes every retained record (`Partition::to_snapshot`), and snapshots fall due by log
bytes. The cost of each snapshot therefore grew with the client's whole history, and the total cost
quadratically. One client could slow a shard until it missed every client's deadlines. That is banned item 8, an
unbounded structure, and a denial of service by one unprivileged client.

The design already required the bound: "Completion records and retained response windows have bounds and
acknowledgements; exhaustion refuses admission rather than forgetting a live exactly-once obligation" (§4.9). It
was never built.

## Fix

- **The bound.** `verbs::completion_bound` is `2 × slots` per client per shard. A conforming client has at most its
  ring's slots in flight, owes an acknowledgement every half ring, and may lose one acknowledgement sent unawaited.
- **The refusal.** `verbs::acknowledgement_owed`, on the shard that would record the verb (`serve_here` and
  `run_forwarded`, after the retry lookup), refuses a new request `AcknowledgementOwed { retained, bound }`. The
  request is not run and nothing is recorded, and the refusal is counted (`acknowledgement_owed`).
  - An `Acknowledge` is never refused.
  - A retry of an answered id is answered from its record.
  - The refusal is appended to `Refusal` (append-only evolution).
- **Its inputs.** `ClientWindow::retained` and `Partition::retained_completions`.
- **Forwarded verbs.** Their records at the owner are bounded the same way; the origin relays its client's
  watermark with each forward.
- **The fleet harness.** Its raw client now acknowledges as `slates_client` does: every half ring, before the next
  new request.

## Tests

- `slates-server` `tests/daemon.rs`
  `a_client_that_never_acknowledges_is_refused_at_its_bound_and_served_once_it_acknowledges`.
  - Before the fix: 192 unacknowledged statuses, no refusal (the test failed).
  - After: refused at the bound, a retry answered from its record, the acknowledgement served, then the refused id
    and a new request served.
- The fleet suite passes 72/72 with the acknowledging harness. Under the old harness its polls met the bound and
  stalled, which is the refusal working.
