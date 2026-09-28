# The client tests judged a live daemon by a shorter clock, and counted a spurious wake as a broken rule

Date: 2026-09-28. Design: §4.7 ("Wake strategy"), §4.9 (the client's deadlines, `Deadlines::derive`), R3.

## Two failures

1. **CI run 36404234656** (`42ebbbb`, ubuntu-latest, io_uring): the test
   `a_session_outlives_a_daemon_restart_and_its_retry_meets_the_completion_record` panicked at
   `crates/client/tests/client.rs:328`. The error was `Stalled { after_ns: 200000000 }`, on the test's first
   `create` after the daemon started. This test failed once in the last 60 CI runs.
2. **Local reproduction** (six concurrent copies of the client test binary, 900 runs, macOS): once, in run
   133 of copy 5, `the_typed_verbs_drive_the_lifecycle_and_refusals_are_typed` failed with
   `parks never exceed replies: 18/17`. The `Stalled` failure did not reproduce in those 900 runs.

## What is established

- **`Stalled` means the daemon had not replied.** At the deadline, `ClientEnd::wait` checks the ring once more
  before it returns `DeadlineExceeded`. A reply that arrived but whose wake was lost is still taken there. So
  `Stalled` is not a lost client wake: no reply had been written 200 ms after the request.
- **The 200 ms deadline was the test's own.** `client.rs` and `server/tests/recovery.rs` hand-picked a
  200 ms reply deadline (a "Shape" constant, R3). The product derives the reply deadline from the anchor's
  liveness budget, 1 s (`Deadlines::derive`, used by the CLI and by `reap.rs`), so the product would still
  have counted this daemon alive. The failing run's binary had five tests starting daemons concurrently on
  one runner.
- **The park rule was wrong.** One `wait` can park more than once. The OS wait can return without the wake
  word moving (a spurious return), and a previous reply's wake can land after that reply was taken and wake
  the next park. Either way nothing is in the ring, so the client parks again. "parks ≤ replies" is not a
  rule the design states or needs. The rule that does hold: every park ends in a reply, a wake with no reply,
  or the deadline.

## What is not established

Why the first `create` took more than 200 ms on the CI runner. There are no daemon-side logs of that run,
and it did not reproduce locally. The deadline fix cannot hide a daemon that never answers: the derived
deadline still fails such a daemon, and a lost client wake still returns the reply at the deadline, as it
always did. It only stops a reply slower than 200 ms but inside the product's 1 s contract from being called
a stall. If the stall recurs at the derived deadline, it is a real daemon bug and gets the logging-first
protocol.

## Fix

- **The tests use the product's deadlines.** `client.rs` and `recovery.rs` now take
  `Deadlines::derive(LIVENESS_BUDGET_NS, RECOVERY_BUDGET_NS)`, as `reap.rs` and the CLI do. The hand-picked
  constants are gone.
- **Unanswered wakes are counted.** `ClientEnd` counts a wake that finds no reply
  (`unanswered_wakes`, exposed on `Client`). The typed-verbs test asserts
  `parks <= replies + unanswered_wakes`.
- **A deterministic test gives the counter a non-vacuity check.**
  `a_wake_without_a_reply_is_counted_and_the_client_parks_again` (`crates/ipc/tests/rings.rs`): the peer
  wakes a parked client with an empty ring and waits until the client has parked again, then replies.
  Expected result: exactly one unanswered wake, and the reply delivered. It passed 300 of 300 runs.

## The sweep, done twice

The first sweep for siblings grepped for one spelling of the constructor, found only
`server/tests/recovery.rs`, and missed four. CI run 36413408328 (`0fe5160`, macos-latest) then failed the
same way, `Stalled { after_ns: 200000000 }`, in `verbs::harness_tests::run_spawns_the_workload_as_an_ephemeral_consumer_and_revokes_it_after`.

The second sweep listed every `Deadlines { .. }` and `Deadlines::derive(..)` in the tree. It moved the
remaining hand-picked deadlines to the derivation:

- `cli/src/verbs.rs` harness tests (200 ms);
- `client/tests/consumer.rs` (200 ms);
- `client/tests/async_core.rs` (200 ms; its poll bound now reads the derived reply deadline);
- `mcp/tests/mcp.rs` (5 s);
- `server/tests/daemon.rs` (5 s).

No Rust client in the tree now builds its deadlines by hand. The Python and Node SDK tests pass 1 s and
2 s, which equal the derived values. Six concurrent copies of the CLI binary's own tests ran 360 times
after the fix with no failures.

## The open question, investigated

The investigation, recorded in `2026-09-28-a-client-request-waited-for-a-timer-after-a-lost-doorbell.md`:

- **What it found:** a real lost-wake defect in the request doorbell, proven by loom and fixed. It was
  the sibling the 2026-09-13 record had left open.
- **What it did not find:** a local reproduction of the CI stall, even under x86 ordering (Rosetta) and
  six-copy load. The largest first-`create` latency seen was 30 ms.
- **Attribution:** the stall is attributed, unconfirmed, to the CI runner's resource limits (Ada,
  2026-09-28). The derived 1 s deadline absorbs that. A stall at that deadline would be a daemon bug.
