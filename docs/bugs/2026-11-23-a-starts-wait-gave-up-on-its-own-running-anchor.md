# A start's wait gave up on its own running anchor when no process search named it (2026-11-23)

## Description
CI run 45235122249 (main 62d22aefe), job 137905802468, the conformance lane on ubuntu-latest:
`conformance::slates::tests::the_hermeticity_tracer_allows_startup_and_observes_writes` panicked at
`xtask/src/conformance/slates.rs:1415`:

```
the daemon did not come up before its processes stalled, spent their CPU budget or left (None; last exit 3);
the anchor is still running (pid 7376); anchor log, as drained so far:
```

The refusal came about 40 ms after the test began (18:25:35.710 the previous test's end, 18:25:35.754 the panic),
before the anchor had written a line. CI run 45176536354 failed the same way earlier the same day. The next test in the
job (the workloads cell over Linux FUSE) passed.

## Root cause
`Anchor::wait_ready` judged whether the start could still come up only by `process_wait::InstanceWait::look`. That
lists the instance's processes by command line (`pgrep -f -- "--instance NAME "`). `seen: None` means the look found
none. Meanwhile the harness's own child, the strace the anchor runs under (pid 7376), was running, as `never_up`'s
`try_wait` showed. So the wait took "no process found" for "the instance's processes left", and refused a live start at
its first look. The probe's exit 3 is `slates volume list` finding no instance yet, as expected 40 ms in.

Why the search found nothing, while the strace's command line names the instance, is not shown by the log: `pgrep`'s
exit status and error words were thrown away. The new diagnostic keeps them, and the processes `ps` lists. Whatever the
cause, the judgment was wrong: a wait must not give up on a process it started and knows to be running.

## Impact
The hermeticity tracer's start test failed intermittently on Linux CI (twice on 2026-11-23, 0 of 40 on the runner by
hand), failing the conformance lane. No slates code was wrong. Any other start through `Anchor::start` could have met the
same refusal.

## Edits
- `crates/cli/tests/support/process_wait.rs` (shared with xtask):
  - `InstanceWait::look_owning(instance, owned)` counts a process the waiter started and knows to be running, its CPU
    judged as the others'. `look_all` and it share one `look_at`.
  - `instance_listing(instance, parent)`: `pgrep`'s exit and error words, and every process `ps` lists whose command
    line names the instance or whose parent is the anchor.
- `xtask/src/conformance/slates.rs`:
  - `wait_ready` counts the anchor while `try_wait` says it runs.
  - `never_up`'s message carries `instance_listing`.
  - New test `a_start_waits_on_its_own_anchor_while_no_process_search_finds_it`.

## Evidence
- Before (the MacBook, red commit 756ba95eb): the new test, whose anchor stand-in execs at once into
  `sh -c 'sleep 1; exit 7'` (the same process, its command line naming no instance), was refused at the first look:
  "(None; last exit 1); the anchor is still running (pid 63555); anchor log, as drained so far:", the CI red's shape.
- After: it waits for its anchor's exit and names it (status 7). `xtask --bin xtask conformance::slates::tests::a_start`
  2 of 2, `conformance::process_wait` 2 of 2.
- Owed: the conformance lane on ubuntu-latest (main's).
