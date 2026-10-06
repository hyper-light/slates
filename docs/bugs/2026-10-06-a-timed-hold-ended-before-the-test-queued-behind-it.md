# A timed hold ended before the test queued behind it

**Found:** 2026-10-06, CI's TSan lane (run 37448849001; red on every run read that day):
`a_request_drained_during_shutdown_is_terminated_on_its_receipt` (`crates/rt/tests/admission.rs`) failed
`refused_at_shutdown` 0, where 1 was due. Its receipt said `Terminated`, as required.

## Description and root cause

The test holds the shard inside one poll, then queues a shutdown and a request behind the hold, so the shard drains
the request after its shutdown began and refuses it (counted). The hold was a fixed 300 ms spin. Under TSan the test
thread runs several times slower, so the spin ended first. The shard took the shutdown, emptied, and exited before
the request was queued. The request was then dropped undrained, and its receipt answered `Terminated` by itself
(`crates/rt/src/task.rs`), uncounted, which is that path's documented behaviour. The runtime was right. The test's
ordering was a timing guess.

## Fix

`hold` returns a release, a sender. The held task spins until the test sends on it, or drops it, so a failed test
never wedges the shard. Both tests that hold now release after queueing, so the order they state holds on any
machine. The fixed `HOLD_NS` is gone. 200 of 200 runs pass locally, the suite in 0.01 s, against at least 0.3 s
before.

## Sibling sweep

- `hold` had two callers in `admission.rs`, both converted.
- `crates/rt/tests/burst.rs` had the same timed 300 ms hold with a burst queued behind it. There a hold that ended
  early would not fail: the burst would drain as it was sent, and the test would pass without ever testing a drain
  past one batch. It is converted the same way, and now asserts that nothing of the burst ran before the release.
- `wake_estimate.rs` spins too, but its spin is the CPU time it measures, and its other loop stops on a flag: no
  ordering rests on either.

**Mutation check.** With the drain's re-arm removed (the 2026-09-17 bug), the burst test fails "8 of 32", as it
should.

**Reported, not fixed here:** after that failure, the test binary hangs in the runtime's drop, which waits on a
shutdown the broken drain never delivers. That regression would show on CI as a timeout, not a red test.
