# A driver test treated a valid wake as an early timeout

Date: 2026-09-19. Area: `crates/rt/src/driver.rs` test, AC-0.6.

## Local CI failure

The full macOS `cargo test --workspace` run reported
`the_os_driver_wakes_on_a_kick_and_delivers_a_nop` failed, then the remaining
registry contention test did not finish. The run was stopped at that failure.
Twenty subsequent isolated parallel runtime-unit runs passed.

The driver test registered a real shard slot. The concurrent registry stress
test deliberately sends wakes to neighbouring live slots, including that slot.
The driver test asserted that one timed wait lasted at least 1 ms, even though
the driver contract permits a kick or interrupt to end the wait earlier.
An assertion panic also skipped its explicit `unregister`, leaving an abandoned
live ring. A neighbouring sender could fill that ring and wait for a consumer
that would never run.

## Controlled reproduction

Place a kick just before the timed wait, then run:

```text
cargo test -p slates-rt --lib \
  driver::tests::the_os_driver_wakes_on_a_kick_and_delivers_a_nop -- --nocapture
assertion failed: driver.now_ns() - before >= 1_000_000
```

This reproduces the invalid assertion directly. The full-run panic text was
still captured by libtest when its hanging sibling was stopped; the injected
case establishes the test defect without claiming that missing diagnostic.

## Fix

Retain the injected kick. Observe the no-op under one absolute deadline and
continue timed waits toward their original deadline after an early wake.
The test does not require a quiet global registry. Own the registration in a
drop guard declared before the driver, so both ordinary return and unwinding
retire it after the driver closes. The kick thread is scoped and joined on unwinding too.
No production driver timing is changed.

A subsequent parallel run exposed the broader interference: the registry stress fixture
could fill another unit test's ring before that fixture's own initial sends. Both tests
then waited for a consumer that had not reached its drain. The stress history now runs in
its own integration-test process, with the same sixteen concurrent workers and 200 cycles
each. It still races registration, sends and retirement against other workers, but cannot
send into unrelated fixtures. Its retirement check uses the public holder identity, not a
private generation field. A unit test likewise checks that the old holder ended rather
than assuming no parallel test has reused its slot.

Validation on 2026-09-19: `cargo test -p slates-rt --lib --test registry_contention --
--nocapture` passes all 18 unit tests and the separate contention history. The subsequent
macOS `cargo test --workspace` passes: 1,490 passed, 14 ignored, zero failed, including all
48 fleet tests in 333.81 s. Environment-gated tests retain their declared skips; this command
does not replace the separate mounted Linux conformance run.
