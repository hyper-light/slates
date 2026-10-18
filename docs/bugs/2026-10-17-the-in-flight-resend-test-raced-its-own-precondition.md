# The in-flight resend test raced its own precondition

- **Date:** 2026-10-17
- **Area:** `crates/cli/tests/cli.rs` (the A-61 resend test), §4.6 Linux / A-61 `FUSE_NOTIFY_RESEND`
- **Status:** fixed

## Description

`a_write_in_flight_at_the_daemons_kill_is_resent_and_every_acknowledged_write_survives` failed the
ubuntu lane of CI run 39090330413 at `crates/cli/tests/cli.rs:4644`:

```
3 cycles, 0 resent, 0 reported lost writes
a kill caught a request in flight in 3 attempts
```

The product was correct throughout: nothing was lost and no `fsync` reported a loss. The test simply
never produced the state it tests. It had passed the identical code on run 39081321612, and the same
job at 02:35 shows the test "ok" — that earlier line is the loud skip, so each job runs it for real
once and the outcome was a coin toss.

Not a slow test and not a product regression.

## Root cause

The test needs the `SIGKILL` to land while the daemon holds a request it has read and not answered —
only those are resent (`fuse_resend` splices `fpq->processing`; a request still pending is served
normally by the successor, and one being copied sits on `fpq->io` and is spliced by neither). The
daemon is in that state for the slice of its serve loop between `dispatch_ready`'s read returning and
`reply`'s write, and out of it for the rest of the turn — `futures::yield_now`, the delegation drain,
`hand_over_session`, and `turn`'s own prologue before it reads again (`crates/server/src/fuse.rs`).

So the catch probability is the serve loop's duty cycle, and with a 4 KiB write the serving part does
not dominate. Measured on the cachyos box (32 cores, kernel 7.2.9-2-cachyos, the test pinned to 4 of
them), binaries differing only in the page constant and run alternately — kill cycles that caught a
request in flight:

| page | cycles | caught | per cycle |
|---|---|---|---|
| 4 KiB | 58 | 33 | 0.57 |
| 16 KiB | 14 | 12 | 0.86 |
| 64 KiB | 13 | 12 | 0.92 |
| 256 KiB | 37 | 37 | 1.00 |

At 4 KiB with `IN_FLIGHT_ATTEMPTS = 3` the whole test failed 9 of 107 runs across two hosts, which is
the CI red.

Two things the test believed were not true. `IN_FLIGHT_WRITERS`' doc claimed that with several writers
queued "the daemon always holds one it read and has not answered"; queue depth does not give that,
because one serve loop holds one request at a time whatever is queued behind it. And waiting for a
writer to be inside `write(2)` — added the day before — is necessary but not sufficient: on fedora
`in_write` was 4 both at the observation and after it, so the writers were saturated the whole time
and the kill still missed.

A second cause sat behind the first: `kill_the_daemon` resolves the pid with a `slates status` round
trip and shells out to `kill(1)`, 27.3–46.0 ms between observing the window and the signal landing
(fedora, 2 cores). The window being waited for lasts microseconds, so the wait informed nothing.

## Impact

CI only. A lane red about once in twelve runs on a test whose subject — a request resent across a
daemon's death (A-61) and the write log that replays what the dead daemon acknowledged (A-63) — was
working correctly every time. The risk of leaving it is the usual one: a real A-61 regression would
have been read as the known flake.

## Fix

- The pid is resolved before the writers start and the kill is one `rustix::process::kill_process`
  syscall (`signal_kill`): 7.2–15.9 µs from the observed window to the signal, about 3400× tighter.
- A page is `slates_bridge_fuse::init::MAX_WRITE`, so the daemon's service of one write is the longest
  a single write can hold it. The write-through mount keeps the bytes in the daemon before `write(2)`
  returns (D-18; writeback cache is never asked for, `init.rs`), so a writer inside `write(2)` really
  does have a request on the device.
- `WRITES_BEFORE`/`WRITES_AFTER` become one derived `WRITES_PER_PHASE`, taken from a writer budget of
  half the volume's bound rather than from a chosen number.

No attempt count or timeout was raised.

## Verification

Interleaved on the cachyos box, 30 runs per arm, 2026-10-17: 256 KiB caught 30 of 32 cycles (0.94)
with 0 test failures; 4 KiB in the same series caught 30 of 48 (0.63). On fedora (kernel 6.19.10,
pinned to 2 cores — harsher) the current tree is 10 runs of 10 green, 10 of 11 cycles caught (0.91);
4 KiB with the fast kill caught 2 of 15 (0.13), so the fast kill alone is necessary and nowhere near
sufficient. At 0.91 per cycle and three attempts a miss is about 1 in 1300 runs.

## Found on the way, not fixed here

- **The writers' budget had to be halved, and the reason is not established.** With the whole bound,
  42 MiB of planned pages overflowed a 64 MiB volume *with the fast kill in place* (2 runs of 30 on
  cachyos, 2 of 8 on fedora), so the volume charges a write more than half again its length. The
  chunk-window charge, the inode allowance (A-109) and the pages a writer adds between the kill and
  the restart are all candidates; none was isolated. Half the bound is 0 of 30 and 0 of 10. The two
  symptoms are coupled, which is why an idle host can miss the overflow entirely: a run that catches
  on its first cycle writes the least and never approaches the bound, so `ENOSPC` only appears on the
  runs that need a second or third cycle — the same runs a slower or more contended host produces.
- **`max_write` is advertised at 256 KiB but the kernel caps a request below it.** `wanted()` in
  `crates/bridge-fuse/src/init.rs` does not ask for `FUSE_MAX_PAGES` and the INIT reply writes
  `max_pages = 0`, so the kernel keeps its own default pages-per-request ceiling
  (`FUSE_DEFAULT_MAX_PAGES_PER_REQ`, 32 pages = 128 KiB on a 4 KiB-page host) and a 256 KiB write
  arrives as two requests. Found independently by two readings of the source; not measured, and no
  `strace` of the daemon's reads was taken to confirm it. It does not affect the measurements above,
  which counted catches and not requests.
