# tests: the async SDK test's first hand-off to a worker thread imported a module on the loop it measures

**Status:** fixed 2026-10-23 (approved by Ada in plan mode). Root cause confirmed by an on-demand reproduction on macOS
and Linux, with the hold dump naming the call. **Audit:** AUD-29-19, AUD-29-20. **Design:** R6, D-19; GAPS 8a-sdk-loop.
**Found by:** the ubuntu SDK packaging job of CI run 39974152220 (`bff7f799`, CPython 3.11.17).

## Description

`test_every_async_call_ends_across_restart_silence_cancellation_and_death` measures the event loop with a ticker and
fails when the loop comes back later than its bound (0.1 s plus the machine's measured noise). The run read:

```
worst lateness 0.672 s in phase 'connecting', bound 0.1 s plus the machine's own scheduling noise 0.000 s. Across that
gap this process burned 0.006 s of CPU and a plain Python thread of its own was held 0.000 s
```

with the harness's slowest exec wait `pgrep 0.054 s`. GAPS 8a-sdk-loop had closed the previous cause on 2026-10-22: the
harness spawned processes on the loop's own thread. The spawns were now off the loop, so by that entry's own rule the
cause lay elsewhere.

## Root cause

**The harness's first hand-off to a worker thread imports a module, on the loop's thread.**
- `_run_async` runs each command through `asyncio.to_thread`. Its first call in the scenario is the `bootstrap root`
  command, in the 'connecting' phase.
- That first call makes `run_in_executor` create the loop's default `ThreadPoolExecutor`. In CPython,
  `concurrent.futures` imports its `thread` module lazily, on first access to `ThreadPoolExecutor`
  (`Lib/concurrent/futures/__init__.py`, `__getattr__`). So the first `to_thread` imports
  `concurrent.futures.thread`, and `queue` with it, on the loop's thread.
  - Measured: after `import asyncio` neither module is loaded, and after the first `to_thread` both are. This holds on
    CPython 3.9.6 (this Mac) and 3.14.3 (Linux), and the 3.11 source has the same `__getattr__`.
- An import reads its files from disk, and a blocking read releases the GIL. On a cold page cache over a slow disk, the
  loop's thread waits in that read with the GIL released and no CPU spent. That is the reading CI printed: 'connecting',
  no CPU, the sibling thread free, no machine noise.
- **The run's disk was slow.** In the same run, `pgrep`'s slowest exec wait was 54 ms. An exec reads its binary from
  disk, and the healthy ubuntu runs print 1–2 ms.

The SDK is not involved. Its connect path never waits (the claim is non-blocking and its answer is read on the loop's
timers), and the extension never releases the GIL, so a hold of its own would have held the sibling thread too.

## Evidence

**The instrument** (`ce26656a`). Before each tick, the ticker arms `faulthandler.dump_traceback_later` for one period
plus the bound, so a hold prints every thread's Python stack while it lasts. On Linux, a deliberate 0.3 s hold printed
the loop thread's own frame, whether the hold released the GIL or kept it.

**On demand.** A `sitecustomize` import hook sleeps 0.3 s when `concurrent.futures.thread` is imported, as a slow read
would; the sleep releases the GIL, as a read does. Under it, the scenario fails every time with CI's reading:

| machine | runs failed | lateness | noise | CPU | sibling held | dump |
|---|---|---|---|---|---|---|
| fedora (Linux, CPython 3.14.3, load 8–11) | 3 of 3 | 0.293–0.299 s | 0.000–0.001 s | 0.002–0.005 s | 0.000 s | `__getattr__` in all 3 |
| this Mac (CPython 3.9.6, load 9–26) | 5 of 5 | 0.345–0.505 s | 0.082–0.103 s | 0.015–0.022 s | 0.000–0.003 s | `__getattr__` in all 5 |

Every run failed in phase 'connecting'. The delayed thread was the loop's own, its id equal to the one the ticker
printed. The loop thread's dumped frames run:

```
concurrent/futures/__init__.py, line 49 in __getattr__
asyncio/base_events.py, line 809 in run_in_executor
asyncio/threads.py, line 25 in to_thread
test_sdk_async.py, line 377 in _run_async
test_sdk_async.py, line 489 in _every_call_ends
```

**Without the hook** nothing failed:
- the CI ubuntu lane with the dump armed: 2 of 2 passed;
- the box (CPython 3.13.16, load 35–67): 20 of 20;
- fedora: 10 of 10.

Those disks hold the modules in their page cache, which fits.

**Not caught.** CI's own hold was not caught by a dump, because the instrument landed after it. The attribution rests
on three things: the reading matches field for field, the mechanism reproduces it, and that run's disk was slow. The
instrument stays in the test, so a recurrence prints its call.

## Impact

The ubuntu SDK lane fails spuriously when a runner's disk is slow at the first hand-off: 1 of the runs since the
2026-10-22 fix, after 3 of 60 before it for the related spawn cause. No SDK behaviour is wrong.

## The first instrument perturbed what it measured (measured and rejected)

The hold dump of `ce26656a` armed `faulthandler.dump_traceback_later` before each 10 ms tick's sleep and cancelled it
after. In CPython 3.11's `Modules/faulthandler.c`:
- every arm cancels the previous watchdog (line 771) and starts a new OS thread (line 787);
- every cancel waits for the old thread to exit while holding the GIL (line 657).

Both fell inside the interval the ticker measures. At load average 14 to 17 on this Mac, over 2000 ticks, the pair
cost:

| p50 | p99 | p999 | worst |
|---|---|---|---|
| 1.134 ms | 19.7 ms | 64.9 ms | 74.7 ms |

While it was in place, the fixed test failed 2 of 5 on the Mac at load about 20, with no CPU and no hold dumped:
- 0.424 s late in phase (4), with 0.194 s of noise;
- 0.277 s late in phase (1), with 0.129 s.

Those 5 runs were under the slow-import hook, and in all 5 the fix had already moved the slow import before the ticker;
the failures came later, in other phases.

**The replacement.** The sibling thread, which already wakes every period, watches a heartbeat the ticker writes after
each sleep. When the loop has not run for one period plus the bound, it prints the loop thread's Python stack
(`sys._current_frames()`), once per hold. Nothing is created or joined per tick. Measured with 0.3 s holds, 5 runs of
each (CPython 3.9.6, macOS):
- **a hold that releases the GIL** (`time.sleep`) was named while it lasted, 0.117–0.153 s in;
- **a hold that keeps it** (`ctypes.PyDLL` usleep) was named the moment it ended, 0.352–0.382 s in, at the holding
  line. The interpreter hands the GIL to a thread that has waited a switch interval at the holder's next instruction.
- Each was named once, and a clean run named nothing.

## Exact edits

All in `crates/sdk-python/tests/test_sdk_async.py`, the harness only:
- `_every_call_ends` calls `await asyncio.to_thread(lambda: None)` before `Ticker()`. The executor's module is imported
  and its first thread started before anything is measured. `_run_async`'s docstring says why.
- `Ticker`:
  - `faulthandler` is gone;
  - `_tick` writes `heartbeat`, and the sibling names a hold (`_name_a_hold`), its docstring keeping the rejected
    instrument's numbers;
  - `stop()` ends the task and the sibling together, so the scenario's own teardown is neither measured nor named. It
    replaces the scenario's two separate stop sites.
- The assertion and its bound are unchanged.

Docs: GAPS 8a-sdk-loop.

## Verification

The slow-import hook delays the first import of `concurrent.futures.thread` by 0.3 s.

| run | machine | result |
|---|---|---|
| hook, no warm-up (red) | fedora, CPython 3.14.3 | failed 3 of 3 in 'connecting': 0.293–0.318 s late, noise 0.001 s, CPU 0.006–0.009 s, sibling 0.000 s; the watchdog named the hold once each, at `concurrent/futures/__init__.py __getattr__` |
| hook, fixed | fedora | passed 5 of 5, nothing named |
| hook, fixed | macOS, load 16–53 | passed 5 of 5 |
| no hook, fixed | fedora | passed 10 of 10, nothing named |
| no hook, fixed, whole Python suite | macOS | passed (6 tests), nothing named |
| A/B, interleaved: faulthandler dump against the watchdog, both with the warm-up | macOS, load 11–47 | 6 of 6 and 6 of 6 passed |

- **The hook ran before the ticker in every fixed run:** its line printed before the ticker's.
- **One passing run named a hold.** At load average 31 the loop was silent 0.155 s in phase (1), its thread at the
  scenario's synchronous overflow burst (`calls.append(client.list())`, `limit × 3` calls without an await). That is
  CPU work on the loop, within the noise-adjusted bound. It shows the watchdog names busy holds too.

## Siblings

- **First-time imports inside the measured window** (`python -X importtime`): only these three (`_queue` 14.7 ms,
  `queue`, `concurrent.futures.thread`; 20.0 ms cumulative on a warm Mac), now all before the ticker.
- **The Node SDK's async test has the same shape.** `runAsync` creates its spawner worker lazily (`spawner ??=
  startSpawner()`), inside the measured window. On Node v24.14.1 here, `new Worker(…, {eval: true})` blocked the main
  thread 1.8–14.6 ms (the first, cold one 14.6 ms). Node's internals load from the binary, not the disk, so the cost
  is smaller. The same one-line change, starting the spawner before the ticker, would remove it. It is a candidate for
  that test's unexplained 290 ms hold (GAPS 8a), not shown. Reported, not changed.

## Open observation

The two Mac failures above (phases (4) and (1), no CPU, no hold dumped, load about 20) did not come back in the 6
interleaved runs of the old instrument, nor in the 6 of the new one, at load 11–47. The old instrument's measured
per-tick cost was inside every measured interval then, and is gone now. Whether it caused them is not established.
