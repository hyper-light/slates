# tests: the async SDK test's noise sampler and sibling waited through a primitive macOS delays, and late was charged as held

**Status:** fixed 2026-10-26, decided under the standing goal (G-mount step 6 needs CI green), not in plan mode.
Root cause confirmed by measurement on this Mac and on fedora, and by an injected reproduction that fails the old test
and passes the new one. The one link first inferred, that CI's macOS runner delays `nanosleep` as this Mac does, was
measured by the fix's own CI run (40251576842): the runner printed `time.sleep 76.5 ms, the selector 3.2 ms`. **Audit:** AUD-29-19,
AUD-29-20. **Design:** R6, D-19; GAPS 8a-sdk-loop. **Found by:** the macOS SDK packaging job, first attempts of CI runs
40227172048 (`60ce0142`) and 40239373189 (`becf5936`), CPython 3.11; both passed on rerun.

## Description

`test_every_async_call_ends_across_restart_silence_cancellation_and_death` runs a ticker on the event loop and failed
when the loop came back later than 0.1 s plus the "machine's own scheduling noise", measured by a separate process
that slept 10 ms at a time with `time.sleep`. The two failures read:

| run | worst lateness | phase | noise | CPU across the gap | sibling thread |
|---|---|---|---|---|---|
| 40227172048 | 0.268 s | (2) a live daemon stopped by signal | 0.134 s | 0.002 s | 0.079 s, hold not named |
| 40239373189 | 0.286 s | (1) overflow, then a restart | 0.150 s | 0.005 s | 0.034 s, named 0.128 s in: `kqueue.control` |

In the second, the sibling thread ran during the hold and found the loop thread in its event loop's own idle wait,
`selectors.KqueueSelector.select` → `kqueue.control(None, max_ev, timeout)`, with almost no CPU spent: the loop had
nothing to do, asked the kernel to wake it in 10 ms, and was woken about 0.28 s later. No SDK call was on its stack.
The harness's own spawns were short (slowest exec waits 0.028–0.029 s for `pgrep`). Over the last 40 runs on main
(2026-10-24 20:10Z to 2026-10-25 23:13Z) the job failed on its first attempt twice, these two.

## Root cause

**Three defects in the measurement, each found by measuring it.**

1. **The noise sampler measured a different primitive's timer delay.** The loop waits through its selector: a kevent
   timeout on macOS. The sampler waited through `time.sleep`: `nanosleep` on CPython 3.11 and later, `select` before.
   On macOS the two are delayed differently by the timer coalescing the kernel applies under a process's scheduling
   policy. Measured 2026-10-26 on this Mac (macOS, 18 cores, load 19 to 72), one process per primitive, from a shell
   that launchd started (`launchd` → `kiln-agent supervise` → kiln → claude → zsh, priority 20, nice 0), 10 ms waits:

   | primitive | p50 late | max late |
   |---|---|---|
   | kevent timeout (bare) | 1.24 ms | 6.87 ms |
   | asyncio tick, CPython 3.9 and 3.13 | 1.32–1.39 ms | 9.66–11.29 ms |
   | `nanosleep` (ctypes) | 76.50 ms | 81.49 ms |
   | `select` | 77.19 ms | 82.91 ms |
   | `time.sleep`, CPython 3.9 and 3.13 | 76.51–76.90 ms | 81.69–82.77 ms |
   | Rust `thread::sleep`, `park_timeout`, `Condvar::wait_timeout` | 76.35–76.90 ms | 80.14–100.49 ms |

   The delay scales with the interval asked and stops near 150 ms. `park_timeout` came back late by 6.0 ms (p50) at
   1 ms, 76 ms at 10 ms, 148 ms at 100 ms and 148 ms at 1 s, while a kevent timeout stayed at 0.25–2.01 ms at every
   interval. Under `taskpolicy -b` the sleeps came back 198 ms late at the median and the kevent timeouts' tail grew to
   185–547 ms. So the sampler's figure was mostly the sleep primitive's delay: on this Mac the old test read noise of
   0.080–0.087 s on every run while the loop's own worst was 0.005–0.008 s (and the 2026-10-23 record's table shows
   0.082–0.103 s, unremarked then). The loop's own tail, a different distribution, was judged against that figure.
2. **The sibling thread slept through the same primitive.** It woke every 86 ms instead of every 10, sampled the loop
   that coarsely (the first run's 0.268 s hold was never named), and read the delay as its own lateness.
3. **The sibling's "held" figure was the growth of a running maximum**, not its lateness across the hold, so a sibling
   delayed earlier in the scenario read as free during a later hold.

And the rule itself charged every late tick as a hold. A loop idle in its selector is not held; the kernel woke it
late. The bound's noise term could only stand in for that if it measured the same wait.

**On the runner, inferred and then measured:** GitHub's macOS runner is a launchd job too, and its noise figures
(0.134 s, 0.150 s at CPython 3.11) fit a `nanosleep` delayed as here plus a tail. The fixed test prints each run's own
delays (`_wait_delays`), and the fix's own CI run (40251576842, job 121344444762) settled it: `this process's 10 ms
waits came back late by, at the median of 5: time.sleep 76.5 ms, the selector 3.2 ms`.

The SDK is not involved: in both runs the loop sat in its own selector wait with the process idle.

## Fix

In `crates/sdk-python/tests/test_sdk_async.py`:
- **The control waits as the loop does.** `Noise` runs, in its own interpreter, an event loop that ticks one period at
  a time beside a sibling thread like the Ticker's: the Ticker without the SDK and the harness, through the same
  primitive.
- **The sibling waits through the selector** (`selectors.DefaultSelector`: kevent on macOS, epoll on Linux), so it
  wakes every period and its lateness is comparable.
- **Late is charged as held only when something held the loop.** While a tick is more than a period overdue, the
  sibling samples the loop thread's frame at every wake (`Ticker._observe`). A tick is charged from the last moment
  the loop was seen idle in its selector (the frame is `selectors.DefaultSelector.select`, matched by code object)
  when it was seen there, the sibling was never late past the bound across the tick, and the process burned less than
  one period of CPU; otherwise with its whole lateness (`Ticker._charge`). A charge past the bound prints the stack
  the loop was first seen busy at.
- **Every run prints its figures**, passing or failing: the worst charge and its tick, the worst lateness and what of
  it was charged, the ticks late but uncharged, the control's worst, and the process's own 10 ms wait delays
  (`time.sleep` against the selector).

## Verification

**The rule, on the injected conditions** (this Mac, CPython 3.13 venv, the SDK and daemon built from `962af435`, load
23–26, 2026-10-26 00:46Z), each injected in phase (3) while the daemon is stopped:

| injected | old test | new test | new test's figures |
|---|---|---|---|
| I0: the loop's selector wait returns 0.3 s past its timeout, once (the kernel waking an idle loop late) | **fails**, 0.301 s against 0.199 s | **passes** | 0.304 s late, charged 0.012 s, 1 tick uncharged |
| R1: `time.sleep(0.3)` on the loop (a block with the GIL released) | — | **fails** | charged 0.438 s; stack ends at the `time.sleep` line |
| R2: a 0.3 s busy loop on the loop (the GIL held, CPU) | — | **fails** | charged 0.293 s, 0.273 s of CPU; stack at the `while` |
| R3: `ctypes.PyDLL(None).usleep(300000)` on the loop (the GIL held, no CPU) | — | **fails** | charged 0.503 s, the sibling 0.458 s late |
| R4: the same call on another thread while the loop idles | — | **fails** | charged 0.439 s, the sibling 0.433 s late |

The same rule, run without the SDK on a bare loop (`green`, I0, R1): passes, passes with 1 tick uncharged, fails
naming `time.sleep`, on this Mac (CPython 3.9.6 and 3.13.12) and on fedora (Linux 6.19.10, CPython 3.14, epoll).

**Green:** 26 runs of the new test on this Mac, all passing: 10 alternated with the old test under the inherited
policy, 4 under `taskpolicy -b`, then 8 and 4 more of the final text (charges 0.003–0.011 s inherited and 0.029–0.138 s
under `-b`, the control's worst 0.003–0.017 s and 0.032–0.138 s). The old test failed 3 of the 10 alternated runs under
the inherited policy, none at the bound: two at the scenario's own restart assertion (below), and one whose log was not
kept, which printed no figures and so failed before the bound was checked.

## Siblings (reported, not changed here)

- **Product timed waits on macOS.** The same delay stretches every std timed wait in shipped code under such a
  policy: the client's reconnect and transient-refusal backoff (`crates/client/src/client.rs`, `park_timeout`), the
  anchor's supervision pauses (`crates/cli/src/anchor.rs`), the daemon's heartbeat park (`crates/server/src/daemon.rs`),
  `recv_timeout` in `crates/rt` and `crates/server/src/observe.rs`. The runtime's own timers wait through kevent and
  are not affected. GAPS records it as an open item with these numbers.
- **The restart assertion with `SLATES_TEST_RESTART_SLOWDOWN` at 1.** Locally, phase (1)'s calls came back
  `Stalled { after_ns: 1000000000 }` in 2 of 10 old-test runs (a third failure's log was not kept) and 0 of 10
  new-test runs under the inherited policy. Ten runs cannot show a difference. A candidate mechanism, unmeasured: the product's stretched waits on the restart
  path, which a process that keeps precise kevent timers firing would pull back toward on time. CI sets the factor to
  10 and has never failed this way.
- **The Node SDK test and the Rust driver test** bound a kevent-timed loop by a sampler that waits 1 ms through a
  condition variable (`Atomics.wait`) or `thread::sleep`. Under this policy those come back about 6 ms late (at most
  10 ms) at 1 ms asked, so their noise figures run that much high and their bounds that much laxer. No false failure
  follows from it.
