# The watchdog's end line named no instant for its verdict

- **Date:** found 2026-11-27 (the TSan lane, diag CI 7, run 46004209050 at 87e628d04); fixed the same day.
- **Subsystem:** the lone daemon's watchdog end (A-232, §4.14): `crates/server/src/watchdog.rs` (`end_process`) and
  `crates/client/tests/lone_daemon.rs`.

## Symptom

`a_lone_daemons_wedged_shard_ends_it_with_its_verdict_while_the_log_is_held`: "W = 1s, B = 100ms, L = 12.261ms, so W +
B + L = 1.112261s; the verdict's line read 1.112891441s after t0": 0.63 ms past the rule. It passed in diag CI 1 and 2.

## Cause

The rule bounds the verdict: the watchdog judges a wedged shard within its window, a beat and the lateness it measures
(`L`, `verdict_lateness_ns`, taken at the look that judged it). The test held the instant it read the end line to that
rule. But the line is formatted and written after that look, within a beat (`write_end_line`), and then crosses the
stderr pipe to the test's reader thread, which TSan slows. The line named no instant, so the test could not tell the
verdict from the line's way to it.

## Fix

- `end_process` names the look that judged the shard, on the host's monotonic clock: "…, judged at monotonic N ns",
  beside the lateness it measured there. Parsers read only the numbers after their own words, so nothing else changes.
- The test judges the verdict by that instant (`judged − t0 ≤ W + B + L`), and the line's read by the product's own
  promise that the end is written within a beat of the verdict (`read − judged ≤ B`). No slack is added to either.
- On the MacBook (debug): wedged-log-held judged 1.270 s after t0 against 1.286 s, the line read 181 µs after; wedged
  judged 1.342 s against 1.350 s, read 218 µs after. The watchdog's unit tests pass, 29 of 29.
