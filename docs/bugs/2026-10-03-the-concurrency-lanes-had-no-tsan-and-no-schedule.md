# ci: the design's "TSan nightly" lane did not exist, and no lane ran nightly

**Date:** 2026-10-03. **Audit:** AUD-29-32 (last open part). **Design:** Part 6 "Concurrency" row (CI: loom
and Miri; nightly: shuttle and TSan); `docs/wip/concurrency.md`.

## Description

Part 6 promises "no data race" from TSan, run nightly. The workflow had no TSan job and no `schedule`
trigger. The lanes called "nightly" ran on pushes to main only, and a day without a push ran nothing. That
included the two lanes on the moving nightly toolchain (Miri, and now TSan).

## Root cause

Both pieces need non-Rust CI cadence or tooling, which CLAUDE.md banned item 13 reserves for Ada's
per-item authorization. They were recorded as owed (2026-10-01) until Ada authorized them on 2026-10-03.

## Impact

No race in the threaded crates had been checked by a race detector. loom covers the modelled cores within a
bound. Miri checks one interleaving at a time and does not run the IPC or client tests at all. The first
instrumented run found no race (below), so nothing shipped is known to be affected.

## Exact edits

- `xtask/src/tsan.rs` (new) and `xtask/src/main.rs`: `cargo xtask tsan`. It runs the canary, requires the
  race report, then runs the suites instrumented; a skip list holds two tests, each with its measured reason.
- `crates/mem/tests/race_canary.rs` (new): the deliberate race, `#[ignore]`d.
- `.github/workflows/ci.yml`:
  - a `schedule` trigger (daily at 03:17 UTC);
  - the push-only lanes now run on `!= 'pull_request'`, so they also run on the schedule, and are renamed;
  - the `tsan-nightly` job.
- `docs/wip/concurrency.md` §4 records the lane, its measurement and its skips; §5 lists what is owed.
  The design status and the GAPS row are updated.

## Proof

- **The lane:** `cargo xtask tsan` in an aarch64 Linux container (rust:1.98.0 plus nightly 2026-10-02 with
  `rust-src`), exit 0. The canary was reported (`WARNING: ThreadSanitizer: data race`, non-zero exit). Then
  220 tests across mem (68), rt (100), ipc (38) and client (14) ran with no report.
- **Red-check of the canary gate:** run uninstrumented, the canary test passes with exit 0 and no report.
  The task's condition (report present and non-zero exit) therefore fails it, so a dropped instrumentation
  cannot pass vacuously.
- **The two skips, measured:**
  - mlock: TSan's own verbosity-1 line says it ignores mlock. The test passes uninstrumented in the same
    container.
  - the zero-timeout driver test: running the whole library, it failed 3 of 12 instrumented runs and 0 of
    10 plain runs. Run alone, it passed 20 of 20 either way.

## Sibling sweep

The other Part 6 "nightly" rows still have no lane: xfstests/LTP, linearizability/Elle, change-point
latency and the extended seed budget. Each is its own design item with its own gap, and none is changed
here. TSan over the server and fleet suites is listed as owed in `docs/wip/concurrency.md` §5.
