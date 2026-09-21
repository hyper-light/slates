# The host digest test assumes an earlier read could not be cached

Date: 2026-09-20. Design: §4.15, T-1.21; the real-host watcher oracle.

## Reproduction and cause

The Linux CI command `SLATES_TEST_RAMDIR=/dev/shm cargo test -p slates-base
--test host -- --nocapture` fails in 0.01 s: the final digest reuse count is two,
while the test expects one. Log: `/private/tmp/slates-linux-gates-remaining.log`.

The test replaces a file, hashes it, waits for the timestamp tick to close, hashes
again, injects an unrelated directory change, and finally checks reuse. The first
replacement hash can already occur outside the racy timestamp window. In that case
it is cached correctly, and the read after the wait is an additional valid reuse.
The test's absolute final count assumes the replacement hash was never cached.

Targeted counter logs confirm both histories. Without a forced pause, this sample
computes three digests and reuses one; pausing by the fixture's existing `TICK` before
the replacement hash computes two and reuses two. Both return the correct replacement
BLAKE3. The latter fails deterministically with the old assertion. Logs:
`/private/tmp/slates-host-digest-red.log` and
`/private/tmp/slates-host-digest-closed-red.log`. No production cache change is needed.

## Fix and verification

Measure the final read against the counters immediately before it: exactly one new
reuse and no new hash. Assert its identity and size against the replacement bytes.
This proves the fast path executes without imposing a scheduler-dependent count on
earlier reads. Remove the temporary logs and forced pause.

Sibling sweep: the absolute counts in `crates/vfs/tests/base.rs` use `SimHost` and
explicit clock advancement, so their racy-window histories are controlled. Keep them.
The real-host test retains its timestamp-wait fixture; this change does not claim a
new timing or performance bound.

The corrected complete Linux host suite passes all six tests in 0.01 s. Log:
`/private/tmp/slates-linux-gates-landing-green.log`.
