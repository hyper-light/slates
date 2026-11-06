# A later receive worker's early end ended worker 0, and the drive never answered

**Amendment:** A-219, over A-215's combined call. The bug was on A-219's branches only (`d6c3f01d` through `e070f31e`
in the separate form, `5b5f7ba1` and `30067998`/`9f0974e5` in the combined form) and never reached main.

**Found by:** the live WinFsp oracles at one fixed worker, in diagnostics run 42147337198 (`diag-winfsp-overlap-24`,
2026-11-06, `windows-11-arm`). Each arm's worker count was fixed at build time (`SLATES_DIAG_RECEIVE_WORKERS`), with no
probe running.

## Description

At one fixed worker the combined form served nothing; at two fixed workers both live suites passed.
- **CLI mount tests:** `## cli mount tests, 1 workers, exit 101`. 12 of 13 tests panicked at `crates\cli\tests\winfsp.rs:190`
  with `E:\ never answered within 10s`, and the suite took 3,615 s.
- **Bridge oracles:** `## bridge oracles, 1 workers, exit 101`. `host::tests::a_read_taken_before_the_kill_is_settled_after_takeover`
  panicked at `tests/support/host.rs:242`: `left: None`, `right: Some(Released { id: 2 })`. The first server never took
  its read.
- **At two fixed workers, the same run:** bridge 83 of 83, CLI 13 of 13. The 40-kill oracle answered every reader's
  request with its own bytes or a cancellation, the second worker holding a request in 20 kills before its release and 20
  after.

## Root cause

`crates/bridge-winfsp/src/dispatcher.rs`, `receive_loop`, worker 0's start:

```rust
for release in releases {
  release.send(()).map_err(|_| WinFspError::MountThread)?;
}
```

`Worker::start` always starts worker 0 and each later worker. A later worker first learns the count. When the count
stops short of it, it ends at once and drops its end of the release channel. Worker 0 then releases it after recovery,
the send is refused, and the `?` ends worker 0 with `MountThread`. No thread is left receiving, so no request on the
drive is ever taken: the mount answers nothing.

With A-219's probe this was a race. The probe takes about a second, longer than recovery, so worker 0's release usually
came first and waited in the channel. But the count is measured once a process and kept. A second mount in a long-lived
daemon gets the cached count at once, so on x86_64, where the count is one, that mount would have lost its only worker.
Fixing the count at build time made the race certain.

Nothing in a request's handling waits on a second request, and no notification waits on the driver. A-215's own runs
served at one worker (diagnostics runs 1 to 4, 40 kills a lane), because they had no later worker to release. The model
oracles held throughout (A-215's 8,373 scenarios, A-219's 7,036): they never model a worker that ends before its
release.

## Impact

None on main. On the A-219 branches, every mount whose count was one, once the count was known before worker 0's
recovery finished, served nothing until it was unmounted. The likely case was a daemon's second and later mounts on
x86_64.

## Exact edits

- `dispatcher.rs`, worker 0's release loop: a refused release is that worker's own end. It is logged,
  `slates-winfsp: a receive worker had ended before worker 0 released it`, and worker 0 serves on (`-7` at `c078cde3`).
- The red test is the live suites at one fixed worker. Diagnostics run `diag-winfsp-overlap-27` runs them at one and at
  two fixed workers, one job a runner and a count, each step timed out at 15 minutes. That is three times diag-20's
  whole live step on the slower runner (4 min 52 s on `windows-latest`), so a hang ends as a failure with its log. It
  must show the log line at every one-worker mount, with both suites passing.
- No sibling: the only other place a worker sends to another is `purge_intents::Sender::publish`. It already skips a
  worker whose channel has closed and is refused only when none is left.
