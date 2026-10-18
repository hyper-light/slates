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
- `WRITES_BEFORE`/`WRITES_AFTER` become one `WRITES_PER_PHASE`.
- A writer counts only its before-plus-middle allowance and appends to a throwaway file after that,
  truncated back every phase's worth. Without that cap the loop is unbounded: the successor daemon
  answers as soon as it is serving, but the test only learns of it at the next 20 ms `await_daemon`
  poll, and four writers fill that interval at full speed with pages no phase counts. The cap is for
  boundedness alone and costs no catch rate (measured below).
- `IN_FLIGHT_ATTEMPTS` rises from 3 to a derived 11, and the volume's bound follows from it rather
  than the other way round: `WRITER_BUDGET_BYTES` is now what the writers can append in the worst
  case where every cycle misses, and `HELD_VOLUME_BYTES` is twice that (99 MiB and 198 MiB). This is
  the part that makes the test green, and it is why the fix is not just the page size. A bound is a
  quota, not an allocation, and attempts are spent only when a cycle misses, so the usual run is
  unchanged: 139 of 200 runs took one cycle, at a mean of 0.49 s.

An attempt count was raised, deliberately. An earlier draft of this record said none was, which was
wrong: at `IN_FLIGHT_ATTEMPTS = 3` and the measured per-cycle catch of about 0.7, a run misses every
attempt roughly once in 37, which is a flake whatever the page size. Three was never derived from
anything; eleven is derived from the measured catch rate and a stated target.

## Verification

All on the cachyos box (32 cores, kernel 7.2.9-2-cachyos), the test pinned to 4 of them, 2026-10-17,
with the box under another session's load (recorded per series).

The page size, interleaved, 30 runs per arm: 256 KiB caught 30 of 32 cycles (0.94) with 0 test
failures; 4 KiB in the same series caught 30 of 48 (0.63). On fedora (kernel 6.19.10, pinned to 2
cores) 4 KiB with the fast kill caught 2 of 15 (0.13), so the fast kill alone is necessary and
nowhere near sufficient.

The cap, four arms interleaved, 20 runs each at load 11 — allowance 1x, 4x, 16x and uncapped, all on
a volume large enough that none could fill: 0.833, 0.909, 0.833 and 0.870 caught per cycle, 0
failures anywhere. The cap does not change the catch rate; it only bounds what a cycle writes.

The final shape, 360 runs at load 13-16: **0 failures, 0 `ENOSPC`** — 100 runs of the binary as it
ships (p = 0.709, 141 cycles, no run over 3 cycles) and 260 of the same design before the busy file
went from `set_len` to a reopen (p = 0.705, 369 cycles). The tail is geometric, which is what licenses the attempt count: the share of runs
still going after k cycles fell 1 → 0.305 → 0.100 → 0.035 over the 200-run series, a ratio of 0.33
to 0.35 at each step, and no run in it exceeded 4 cycles of the 11 allowed. Forcing every cycle to
run, a worst-case run finishes in 2.1 s with no crash-loop backoff and no `ENOSPC` (5 runs).

## Measured and rejected

- **Pausing at the allowance instead of writing somewhere cheap.** The first cap simply stopped a
  writer once it had spent its allowance. It bounds the volume, and it breaks the test outright: the
  kill waits for a request to be in flight, a paused writer has none, and no run completed at all
  (40 attempted, none reached a kill; before the change a run took about 0.5 s). The writers have to
  keep the daemon busy without growing the file, which is what the throwaway file is for.
- **Rewriting one page in place rather than appending to a throwaway file.** Also bounded, and the
  same to within noise: 0.725 against 0.750 caught per cycle, 40 runs each. The append is kept
  because it is the same request the counted pages make, not because it measured better.
- **Blaming the cap for a lower catch rate.** One interleaved A/B at load 20-23 put the capped arm at
  0.651 against 0.903 uncapped, which read as the cap costing catch probability. It did not survive
  a larger sweep at load 11 (four arms above, all 0.83-0.91). Two mechanisms were proposed for it and
  both were measured false: append throughput is flat in file size (950 MiB/s from 8 MiB to 192 MiB)
  and flat as a volume fills to its bound (870 MiB/s to the last byte of 64 MiB), so neither a
  growing file nor a near-full volume was making the uncapped daemon slower and easier to catch.

## Found on the way, not fixed here

- **The volume does not overcharge — an earlier draft of this record said it did, and that was
  wrong.** Filling one file by `dd` until `ENOSPC` reached 67108864 B of a 64 MiB bound to the byte at
  both 4 KiB and 256 KiB pages (fedora, 2026-10-17): charge to length 1.000, no chunk-window excess,
  and A-109's inode allowance over four files is four orders below the budget. The overflow was
  therefore pages the test wrote and did not count, which is the uncapped middle phase above; it is
  fixed, not owed. Worth keeping for whoever meets it next: the two symptoms are coupled, so an idle
  host can miss the overflow entirely — a run that catches on its first cycle writes the least and
  never approaches the bound, so `ENOSPC` only shows on runs that need a second or third cycle. That
  is why the halved budget looked sufficient over 30 runs and then failed 2 of 12 on the same tree.
- **`max_write` is advertised at 256 KiB but the kernel caps a request below it.** `wanted()` in
  `crates/bridge-fuse/src/init.rs` does not ask for `FUSE_MAX_PAGES` and the INIT reply writes
  `max_pages = 0`, so the kernel keeps its own default pages-per-request ceiling
  (`FUSE_DEFAULT_MAX_PAGES_PER_REQ`, 32 pages = 128 KiB on a 4 KiB-page host) and a 256 KiB write
  arrives as two requests. Found independently by two readings of the source; not measured, and no
  `strace` of the daemon's reads was taken to confirm it. It does not affect the measurements above,
  which counted catches and not requests.
