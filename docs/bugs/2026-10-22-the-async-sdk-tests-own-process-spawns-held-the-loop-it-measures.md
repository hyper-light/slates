# The async SDK test's own process spawns held the loop it measures

**Status:** fixed 2026-10-22 (approved by Ada in plan mode). The mechanism is confirmed by measurement and by a
red-check. One thing is still a hypothesis: that the runner's first exec of these binaries is slow because its disk
is cold. The fix makes every CI run print that figure.
**Severity:** a test defect, not a product defect.
`test_every_async_call_ends_across_restart_silence_cancellation_and_death` failed on the ubuntu lane, each time
charging the SDK with a hold the harness itself made. Its failure message then pointed the reader at the runner. For
three days the open item (GAPS §8a-sdk-loop) read as a possible SDK bug.
**Found:** GAPS §8a-sdk-loop, opened 2026-10-20. The failures' own probes gave it away.

## What happens

The test runs an independent ticker task on the event loop. The test fails if that ticker is ever late by more than
0.1 s plus the machine's measured noise. The ticker exists to catch the SDK holding the loop (AUD-29-19/20).

To find and inspect the daemon, the harness runs `pgrep` and `ps` through `_run_async`. Its comment said it "runs a
command without blocking the loop", which was false:
- It used `asyncio.create_subprocess_exec`, which calls `subprocess.Popen` on the loop's own thread.
- `Popen` returns only once the child has exec'd. On Linux, CPython vforks and the parent thread waits until the
  child calls exec. On macOS, it forks and reads an error pipe until the exec closes it.
- So the loop stood still for as long as each child took to reach its exec. Usually that is a millisecond. A child
  that must read its binary from a cold disk takes as long as the disk does.

## The evidence

### What CI recorded

There were five held-loop failures in the 60 runs of `main` from 2026-10-18 to 2026-10-22. All were on the ubuntu
lane, and all ran Python 3.11.17. The first two (runs 39538566246 and 39539155335: 0.298 s and 0.243 s late)
predate the phase report. The three that carry it:

| run | commit | late by | phase | CPU across the gap | sibling thread held | noise |
|---|---|---|---|---|---|---|
| 39570188927 | 82e44fd6 | 0.177 s | (1) | 0.003 s | 0.000 s | 0.000 s |
| 39749075454 | d7826c8b | 0.147 s | (1) | 0.002 s | 0.000 s | 0.002 s |
| 39797950813 | c5f5a212 | 0.110 s | (1) | 0.003 s | 0.000 s | 0.001 s |

Read these with the Ticker's own table, calibrated on 2026-10-20. No CPU with the sibling thread free means the
loop's thread was blocked in a call that released the GIL. The machine was not late (noise at most 2 ms), and
nothing computed.

**Phase (1) is where the job runs its first `pgrep` and its first `ps`.** No earlier step of the job and no earlier
test in the suite runs either one:
- the sync suite does not call them;
- the other async tests do not call them;
- `SlatesAsyncEveryCallEnds` sorts first among the async test classes.

Every later phase runs them again, by then warm.

### The mechanism, measured on CI's Python

Measured 2026-10-22 on Python 3.11.17, the version `setup-python` installs on the lane, in an arm64 container. The
probe was a `pgrep` spawn whose child takes 5.8 ms to reach its exec: a PATH of 2,000 entries that each fail by a
symlink loop.
- **Called on a plain thread:** a sibling thread sleeping 1 ms at a time was at most 1.1–1.8 ms late, because 3.11.17
  releases the GIL around vfork. The parent burned 0.7 ms of CPU.
- **Through `asyncio.create_subprocess_exec`:** a ticker on the loop was 5.2–5.5 ms late.

That is CI's signature at a small scale: the loop is held for the child's exec, with the GIL free and no CPU spent.

### The red-check

A preloaded shim sleeps 150 ms in the spawned child just before it execs `pgrep` or `ps`. That is roughly what a cold
read of the binary would cost. The shim changes nothing else, and nothing is traced.

Same container, Python 3.11.17, with `SLATES_TEST_RESTART_SLOWDOWN=10` as on the lane:

| harness | exec delayed 150 ms | not delayed |
|---|---|---|
| before (spawns on the loop's thread) | **failed 3 of 3**: 0.150–0.151 s late; phases (1), (1), (2); CPU 0.001–0.002 s; sibling thread 0.000–0.003 s | passed 2 of 2 |
| after (spawns on a worker thread; a scratch copy) | passed 3 of 3 | passed 2 of 2 |

The same shim was then run against the committed file, with the pre-fix file again as the red:

| file | exec delayed 150 ms | not delayed |
|---|---|---|
| pre-fix (`HEAD`) | **failed 2 of 2**: 0.153 s and 0.152 s late, phase (1), CPU 0.003 s and 0.002 s, sibling thread 0.000 s and 0.004 s | — |
| committed | passed 3 of 3; printed exec waits `pgrep` 0.155–0.159 s, `ps` 0.152–0.160 s | passed 3 of 3; exec waits at most 0.001 s |

The printed figure is exactly the wait that used to hold the loop. In the second run the machine's own noise was
0.025–0.032 s (other work on the host), and the pre-fix file still failed by more than that.

In the emulation every `pgrep` and `ps` is slow, so a phase-(2) run can be the worst. On CI only phase (1) has
failed, which is what a cold *first* exec predicts.

A first attempt delayed the execs with `strace -f -e inject=execve:delay_enter=…`, and it was void:
- ptrace attaches the daemon too;
- after a `SIGSTOP` the daemon then sits in tracing stop (`t`, not `T`), so the test's stop-wait never saw it
  stopped;
- the anchor killed it as hung, and both harnesses errored alike at the kill.

Delay a child with a shim, not a tracer, when the test stops processes by signal.

## Root cause

The harness's process spawns ran on the loop's thread, so the loop-responsiveness test measured them.
`create_subprocess_exec` makes only the wait for the child's *exit* asynchronous, never the spawn. So the comment
claiming otherwise was wrong from the day it was written.

## What it was not

**The SDK.** Its async verbs and its reader and timer callbacks never release the GIL: `allow_threads` appears
nowhere in `crates/sdk-python/src/lib.rs`. A block inside them would hold the sibling thread too, and the failures
show that thread free.

The 2026-10-21 record measured phase (1), driven directly, at 5 ms of lateness (its hypothesis 2). That is this
spawn cost on a warm machine.

**Still a hypothesis:** that the runner's first exec of `pgrep` and of `ps` takes 100–180 ms because its disk is
cold. The fix prints each command's slowest exec wait at the end of every run, so the lane's next log says.

## Exact edits

All in `crates/sdk-python/tests/test_sdk_async.py`:
- **`_spawn_and_wait(argv)`** (new) runs `Popen` and then `communicate()` on the calling thread. It returns the
  exit status, the stdout, and how long `Popen` took.
- **`_run_async`** awaits `asyncio.to_thread(_spawn_and_wait, argv)` (available since 3.9, the wheel's floor). It
  records each command's slowest exec wait in `_exec_waits`, and its comment states the mechanism and points to this
  record.
- **`_every_call_ends`** prints one line on stderr before its assertion:
  `the harness's own process spawns, each off the loop: slowest exec wait …`.
- **The failure message's three readings**, and the Ticker comment's table, are corrected. The message had called
  "no CPU, thread free" "the OS kept this whole process off a core, which is the runner and not the SDK". That is the
  opposite of the comment above it and of its own calibration, in which a GIL-releasing sleep read 1.0 ms of CPU and
  0.2 ms of thread.

## Verification

- **Container, the committed file** (Python 3.11.17, arm64): the second red-check table above.
  - Under the shim it passed 3 of 3, while the pre-fix file failed 2 of 2.
  - Without the shim it passed 3 of 3.
  - Each run printed its exec waits.
- **Linux, starved:** the fixed file with per-phase probes, in a container of one CPU with four spinners, passed 12 of
  12. Every phase-(2) call ended `Stalled` after 1.001–1.003 s.
- **macOS:** the existing Python 3.9.6 venv, so the file runs on the wheel's floor. Load was 12.6–24.7 from other
  sessions.
  - The committed file passed 2 of 3.
  - The failure was a different one, in phase (2): the call to the stopped daemon ended
    `DaemonGone { after_ns: 2000000000 }` instead of `Stalled`.
  - Run interleaved after it, instrumented copies of the pre-fix and fixed files passed 6 of 6 each. Every phase-(2)
    call ended `Stalled` after 1.000–1.006 s.

  That failure is not this one, and it is not made by this change. It is recorded as GAPS §8a-sdk-stamp, with its own
  hypothesis.

## Siblings

- **The Node SDK's async test has the same defect.** `runAsync` in `crates/sdk-node/tests/sdk_async.test.mjs` carries
  the same comment. Node's `spawn()` returns only once the child has exec'd. Measured 2026-10-22, Node v24.22.1 in a
  container, with a ticker on a 1 ms interval:
  - each exec of `pgrep` delayed 150 ms (strace; no daemon involved): `spawn()` returned after 154–155 ms, and the
    ticker was 153–155 ms late;
  - no delay: 0–1 ms and 2 ms.

  It is latent on CI so far, though its comment records an unattributed 106 ms hold on the macOS runner (2026-10-01).

  **Fixed 2026-10-22, the same way.** One worker thread runs the harness's spawns on its own loop, reports each
  command's slowest exec wait, and the test prints it. The shim, extended to `execvp` and `execvpe` because libuv
  reaches exec through them, made the pre-fix file fail 3 of 3 (154, 149 and 733 ms late). The fixed file passed:
  - 26 of 27 under the shim;
  - 12 of 12 starved;
  - both suites, 6 of 6 in each of 3 runs, on macOS.

  The one fixed failure, a 290 ms hold during a period of host load 58–67, is recorded in GAPS §8a-sdk-loop as an
  open observation.
- **The sibling-thread figure can read zero for a real hold.** `worst_thread` is `thread_late - thread_before`, a
  difference of running maxima. A hold shorter than one the sibling saw earlier therefore reads 0.000. It did not
  mislead here, because there was no earlier hold of that size, but it can.
- **Not affected:**
  - The Rust driver test (`crates/client/tests/driver.rs`) times the driver's own pump and tick, not a loop with
    harness work on it.
  - The Python round-trip test has no ticker.
