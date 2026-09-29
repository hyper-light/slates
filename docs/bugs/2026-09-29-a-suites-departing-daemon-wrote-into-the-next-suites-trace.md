# A suite's departing daemon wrote into the next suite's trace

Date: 2026-09-29. Scope: the conformance harness's anchor lifecycle (`Anchor::stop` and its drop,
`xtask/src/conformance/slates.rs`) and the macOS hermeticity suite
(`xtask/src/conformance/hermeticity.rs`). Found by CI run 36565560556 on `38ff5b5`, macOS conformance lane.

## Symptom

The macOS NFS hermeticity suite counted two writes outside the granted target, and the lane failed:

```
VIOLATION line 1: write /Users/runner/work/_temp/conformance-scratch/native-macos-nfs/anchor-conf-workloads-1374.log
VIOLATION line 2: close /Users/runner/work/_temp/conformance-scratch/native-macos-nfs/anchor-conf-workloads-1374.log
```

That file is the log of the *workloads* suite's anchor and daemon, the suite that ran just before. Every
other count was clean: 157 write calls, 18 inside the target, 33 RAM-only, 104 to standard streams, none
unresolved. pjdfstest, fsx, fsstress and the workloads all passed.

## Root cause

`Anchor::stop` and the anchor's drop sent the anchor `SIGKILL` and reaped it. The daemon is the anchor's
child, so the kill orphans it, and nothing here can reap it. It notices its anchor is gone and leaves in its
own time. On the way out it writes its last lines to the log it shares with the anchor, then closes it.

So a suite could end, and the next one start, while the previous daemon was still leaving. On macOS the
hermeticity tracer (`eslogger`) keeps the events of every process running the slates binary, not only its
own lifecycle's. It therefore saw the workloads daemon's last write and close. That log was not its own
anchor's, so it could not classify the write as a standard stream.

On Linux the trace is `strace -f` on the suite's own anchor, so another suite's process never reaches it.

Measured locally (macOS, the debug CLI): a daemon was still running right after `stop()` returned in 3 of 3
runs. After the fix, `stop()` returns 126–163 ms after the anchor's kill, with the daemon gone. That span is
the window the next suite's trace used to see.

## Impact

- The hermeticity verdict could fail on a suite boundary. The daemon under test wrote nothing outside its
  target.
- More generally, a suite's daemon (its shards spinning, its ports held) could overlap the next suite for
  about 150 ms.

## Fix

- `Anchor::end`, used by `stop` and the drop, kills and reaps the anchor. It then waits until no process
  runs the instance's daemon command line (`slates --instance <instance> daemon`), for up to the harness's
  start wait. The daemon is found by its command line, never by pid, because a pid can be reused once the
  daemon leaves; the instance is this run's own.
- A daemon still running after that wait is killed (`pkill -KILL -f` on the same command line) and waited
  for as long again.
- `stop` now returns a typed failure when the daemon would not leave. The hermeticity suite passes it on, so
  a stuck daemon fails that suite with its reason instead of polluting the next one.

## Tests

- **Failing first:** `stopping_an_anchor_leaves_no_daemon_of_its_instance_running`
  (`xtask/src/conformance/slates.rs`). It starts a real anchor, checks that its daemon runs (non-vacuous),
  stops the anchor, and asserts that no process runs the instance's daemon. Before the fix it failed 3 of 3,
  the daemon's pid still listed. After it, it passes 3 of 3.
- It needs a built CLI (`SLATES_TEST_BINARY`) and skips loudly without one. CI's Linux conformance lane runs
  the harness's tests with one.

## Siblings

- **The CLI tests' `AnchorProcess`** (`crates/cli/tests/cli.rs`) also kills and reaps only the anchor.
  Those tests assert the daemon's departure by the instance answering exit 3, which is the behaviour under
  test. No trace spans them, so a departing daemon there touches no verdict. Unchanged.
- **The Linux hermeticity trace** follows only its own anchor's process tree, so it was never exposed.
  It is unchanged.
