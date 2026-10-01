# Loop-held bounds ignored the machine's own scheduling noise

**Date:** 2026-10-01. **Area:** the async-call acceptance tests: `crates/client/tests/driver.rs` and
`crates/sdk-node/tests/sdk_async.test.mjs`. **Related:** AUD-29-19, AUD-29-20 (A-56).

## Description

Two CI runs failed on a loop-held bound, each once:
- the Rust driver test: "the longest loop step was 245.5685ms, bound 100ms" (macOS runner, run 36855724203);
- the Node SDK test: "worst lateness 106 ms, bound 100 ms" (macOS runner, run 36856449049).

The bound is a tenth of the reply deadline, the longest pause the reconnect pacing allows. Neither failure
said which call held the loop, and neither could tell a blocking call in the SDK from the runner keeping the
whole process off its cores.

## Root cause

The tests judged wall-clock steps against a bound derived from the product alone. The machine's own
scheduling noise is part of every wall-clock step on a loaded runner, and it was not measured, unlike the
vfs bench's `destroy_rows`, which measures its noise floor before judging.

## Fix

- **A sampler measures the noise in the same window.** It sleeps a millisecond at a time and records how
  much longer each sleep took. In Rust it is a thread; in Node it is a worker thread, its own isolate, so
  nothing the main loop does can hold it. A step is judged against the bound plus the longest overshoot the
  sampler saw.
- **A real block still fails.** A call that blocks the loop leaves the sampler's sleeps on time, so it is
  still caught.
- **The Rust ticker labels every timed step** with the call it made (`pump`, `tick`,
  `Connecting::poll`, `Client::begin_connect`, the whole step), and a failure names it.
- **The Node sampler is stopped in `finally` too,** so a failed assertion never leaves a worker holding
  the process.
- **The Python test is unchanged.** A Python sampler thread would contend for the GIL, so a blocking
  extension call would stall it as well and wrongly excuse itself. That test has not failed.

## Tests

- Both tests pass locally:
  - `cargo test -p slates-client --test driver`;
  - the Node async suite, 3 of 3, against a built addon and daemon.
- A recurrence on CI now prints the call and the noise next to the bound.
