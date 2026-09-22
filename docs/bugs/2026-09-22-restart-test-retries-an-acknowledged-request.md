# Restart regression retried a request whose completion had been acknowledged

## Evidence

CI run 35615970514, macOS workspace job 106386649830, fails the client restart
test with `DuplicateRequest`. On 2026-09-22 the unchanged history passes with
eight ring slots (0.96 s) but fails with four (0.97 s). A temporary diagnostic
before retrying create sequence 1 reports acknowledgment through sequence 4.
The profile derives ring size from measured service and wake costs, so this
was a machine-dependent assumption about retained records.

```sh
cargo test --offline -p slates-client --test client \
  a_session_outlives_a_daemon_restart_and_its_retry_meets_the_completion_record \
  -- --exact --nocapture
```

The red fixture pins four supported slots; log:
`/private/tmp/slates-ci-35615970514-replay-small-ring-red.log`.

## Contract and correction

Design §4.7, Protocol, retains a completion **until acknowledged** and requires
a retry after a **lost reply** to return the original result. The old test
consumed the create reply, made more synchronous calls that could acknowledge
it, and then required its released record to survive. This contradicts the
record-retention contract; increasing the ring or disabling automatic
acknowledgment would merely conceal that assumption.

Keep the small ring and the real client/daemon restart. Explicitly acknowledge
the completed work and require its later retry to refuse `DuplicateRequest`.
Then send another create and discard its transport reply without acknowledgment. Observe its effect
through a separate client, restart, and retry that unacknowledged id over the new channel before
sending new work. Require the exact original volume id, one reconnect under
the same session, the preserved snapshot, and no duplicate volume. This tests
the lost-reply behavior independently of automatic-ack frequency.

## Sibling audit

`Client::ack_watermark` currently uses the highest sent sequence, constrained
only by `Unpublished` replies. Mixed asynchronous/synchronous callers may
acknowledge unconsumed requests. That is a separate suspected implementation
defect requiring a regression; it is not excused by this fixture correction.

## Validation

All 5 client lifecycle tests pass in 1.05 s, including the strict small-ring
restart history. No changed production retry semantics.
