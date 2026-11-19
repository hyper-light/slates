# The deadlines tests wrote the failover delay that the beat overwrites (A-660)

## Description

`deadlines::tests::a_status_capture_is_held_the_failover_delay_from_its_capture_not_its_request` failed on every
machine, at `crates/server/src/deadlines.rs:830`: "the capture was let go within half a window of its capture". The
look gave up because the shard never let the capture go within half a liveness window. On the Mac Studio it failed 5
of 5 alone on g229's A-648 branch and 3 of 3 on a clean clone of main at 3239c88. Every agent running the server's lib
tests saw it (A-648's and earlier entries of the amendment log record it as failing alike on main).

## Root cause

Since A-574 the control shard's beat runs `fleet::publish_failover` on every beat:
`state.failover_ns = state.injected_failover_ns.unwrap_or_else(|| failover_in_periods(estimate))`. The test wrote
`state.failover_ns = DELAY_NS` inside its first observe and then, two delays later, captured the status report in
another observe. By then the beat had published the node's estimate over the test's value, so the capture's hold was
the estimate (`verbs.rs` reads `failover_ns` when the capture is made), well past the half window the test looks for.

`a_lapsed_cli_attachment_ends_at_its_term_without_a_tick` wrote the delay the same way. It passed only because the
attachment's term is read in the same observe that wrote it, before any beat could run.
`a_lapsed_write_lease_is_released_at_its_term_without_a_tick` records its lease's expiry directly and reads no
failover delay.

The product's own seam for a test's delay is `Daemon::inject_failover`. It sets `injected_failover_ns` on every shard,
and every beat then publishes it in place of the estimate.

## Impact

Tests only. The beat's rule is the product's (A-574); the tests went around its seam. While the server lib had a
failing test on main, every server agent's green run carried one failure that needed explaining.

## Exact edits

- `crates/server/src/deadlines.rs`: `a_status_capture_is_held_the_failover_delay_from_its_capture_not_its_request` and
  `a_lapsed_cli_attachment_ends_at_its_term_without_a_tick` call `daemon.inject_failover(Some(..))` before their first
  observe, and no longer write `state.failover_ns`; their docs say why.
- The same file's `look_until`, which every keeper test looks with, now gives up past half a liveness window widened by
  the descheduling the shard measured meanwhile (`slates_rt::futures::scheduler_overrun_ns`), the shard's own measure
  of the time the host kept it off its CPUs. A miss now reports how long it looked and what the shard measured. On a
  quiet host the bound is the half window, as before.

## Evidence

Mac Studio M3 Ultra, kiln, debug, 2026-11-19 (the test binary run directly):
- Red, main's `deadlines.rs`: the status capture's test failed 10 of 10 alone; the CLI attachment's passed 10 of 10,
  since it reads its delay before any beat.
- Green: each of the three tests 10 of 10 alone; the deadlines module 15 of 15 while two whole suites ran beside it in
  other processes.
- Under load, the whole suite at 32 test threads: one run of eleven with the failover fix missed the bare half window
  in the CLI attachment's and the write lease's tests (load average 14.7, just after a burst of two suites). With the
  look allowed ten seconds, every release in the whole suite came 101 to 166 ms after its 100 ms term. Twelve suites
  since, two at once at load averages 20 to 25, missed nothing, with or without the widened look, so that miss was not
  reproduced. The widening is the house's gate that measures its own noise floor (`destroy_rows`, `vfs_bench`), and a
  miss now carries the measurement.
- A correction: an interim series that seemed to show the status capture's test failing 5 of 5 in the whole suite with
  the fix ran the red build. The red step's rebuild was the newest binary, and the series picked binaries by time. Its
  numbers are not evidence; the green above was rebuilt from the fixed source.

## Siblings

The six other test sites that write `failover_ns` (`moves.rs`, `nfs.rs`, `telemetry.rs`, two in `verbs.rs`,
`work_home.rs`) each write and read it inside one `audit_on_shard` closure. No beat runs between the write and the read,
so none is affected.
