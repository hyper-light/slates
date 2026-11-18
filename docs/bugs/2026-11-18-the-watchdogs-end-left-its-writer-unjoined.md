# The watchdog's end left its writer unjoined (2026-11-18)

## Description

Main's TSan lane (`cargo xtask tsan`) failed both stuck-shard tests of `crates/client/tests/lone_daemon.rs`
(`a_lone_daemons_wedged_shard_ends_its_process_and_the_call_ends_daemon_gone` and its spinning sibling) from the
push that landed A-232, run 44210937388: "the lone daemon was still running 10s after its shard was held". The same
tests pass uninstrumented on every lane.

## Root cause

`watchdog::end_process` logs the verdict on a thread of its own, waits up to a beat for the line, and calls
`std::process::exit(4)`. It never joined the writer, even when the writer had said the line was written.

Under ThreadSanitizer, `exit` runs the sanitizer's finalizer. It sleeps `atexit_sleep_ms` (1,000 ms by default), so
the writer finishes in that time, and then it reports every finished thread nobody joined as a thread leak:

```
WARNING: ThreadSanitizer: thread leak (pid=6249)
    #0 pthread_create
    ...
    #5 slates_server::watchdog::end_process crates/server/src/watchdog.rs:647
SUMMARY: ThreadSanitizer: thread leak ??:? in pthread_create
```

Symbolizing that report in the instrumented test binary held the exit 26.3 to 26.8 s, and the process then exited
66, the sanitizer's exit code once a warning is reported, instead of 4 (diag-g218-tsan-2, run 44217144677, with the
test's give-up raised to 120 windows on that branch only). The test gives up after 10 windows, so it saw a daemon
that did not end.

The watchdog itself was on time: in diag-g218-tsan-1 (run 44216526894) and diag-g218-tsan-2, every run logged the
verdict between 1.089 and 1.099 s after the hold, within a window and a beat. With `atexit_sleep_ms=0` the writer was
still running at the exit, nothing was reported, and both tests passed with exit 4 in 1.10 s.

CLAUDE.md bans a spawned thread without an owner that joins or cancels it (§2, item 9). The writer had none in the
common case, where it finishes; the sanitizer found it.

## Impact

The TSan lane, red on every push and nightly from 2026-11-18. In production, none: the exit ends the process
either way. The exit code is 4 without the sanitizer.

## Exact edits

- `crates/server/src/watchdog.rs` (`end_process`): the writer's handle is kept and joined once it says the line is
  written. Sending that word is its last act, so the join returns at once. A writer still blocked after a beat
  (wedged on standard error's lock) is not joined, and the exit ends it, as A-232 decided.
- `docs/wip/SLATES_DESIGN.md` (A-232's account of the end) and GAPS.

## Siblings

Swept every `process::exit` in `crates/*/src`. The only other production one is in `crates/guest-probe/src`, whose
process starts no thread before it.
