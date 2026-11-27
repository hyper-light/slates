# The ssh patience test judged a socket's timeout by another clock

- **Date:** found 2026-11-27, on diag CI 9's Windows x86_64 lane (job 140570009931, at 56b609053), and fixed the same
  day.
- **Status:** fixed in the test. No product code changes.
- **Subsystem:** the vcs_ssh tests,
  `vcs_ssh::tests::a_connection_closed_past_the_clients_patience_is_seen_ended` in
  `crates/server/src/vcs_ssh/tests/serve.rs`.
- **Severity:** a red lane on Windows x86_64. The product is not affected.

## Symptom

```
assertion `left == right` failed: a fixed patience judged the end too early; the end itself is seen
  left: (true, true, [120])
 right: (false, true, [120])
```

## Root cause

The test's control reads a connection whose peer writes one byte and closes it three read timeouts later. The
control is the seat tests' old client: it judges the connection ended if its wait for the end took less than one
timeout (`TEST_LOOK`, a tenth of a liveness window, 100 ms).

The wait was timed by `Instant`, while the socket's timeout is timed by the socket's own timer. The close comes no
sooner than three timeouts after the byte, by `Instant`'s own count. So a read that ended under one timeout by `Instant`
cannot have seen the close: it answered its timeout, and the socket's timer judged that timeout shorter than `Instant`
did. Then `elapsed < TEST_LOOK` read "ended".

This is the clock family of section 3 of `docs/bugs/2026-11-27-four-windows-reds-of-diag-ci-2.md`: two clocks
compared where one answer was already in hand.

## Exact edits

The control judges the connection by the read's own answer. A close (`Ok(0)`) within the timeout means ended; the
timeout, or any error, means not ended. No second clock is read.

## Verification

- **The red:** diag CI 9's Windows x86_64 lane, above. The Mac's socket timer and `Instant` agree, so the test was
  never red here.
- **On the MacBook (debug):** `cargo test -p slates-server --lib -- vcs_ssh::tests::a_connection_closed_past_the_clients_patience_is_seen_ended`.
  Passed, 1 of 1 (2026-11-27).
- **Owed (main):** the next diag CI's Windows x86_64 lane.
