# The crash-loop test timed its children on the wall (2026-11-07)

## Description

`a_crash_loop_is_restarted_after_a_doubling_backoff_never_given_up` (`crates/anchor/tests/anchor.rs`, A-119) failed
under load. It failed on main 4326a9d8 2 times in 3 on this Mac under clients' recipe (eight nice-19 spinners a CPU,
212-238 runnable processes), and once on windows-latest beside two spinners a CPU (diag-liveness-10): "the backoff
was recorded as a crash loop" (no crash loop seen) or "the daemon was started again after the backoff".

## Root cause

The test drove the supervisor with the wall clock, under a 300 ms window holding one restart: a crash loop needs two
exits inside 300 ms. Its child is the test binary re-invoked, and under that load a child took longer than the window
to start, beat and exit. So no two exits fell in one window, and the supervisor rightly saw restarts, not a loop.
Production does not fix the window either: the anchor derives it from the longest start (`RestartPolicy::derive`). The
product was right; the test measured the machine.

## Exact edits

- `crates/anchor/tests/anchor.rs`: the supervisor reads no clock of its own (every `start` and `step` takes the
  caller's `now_ns`), so the test drives it with its own: the clock stands still while a child lives and jumps to each
  backoff's end. The test's claim is about a daemon that exits **at once**, and on this clock it does, however long the
  machine takes to run it. A child that neither exits nor is restarted within `CHILD_WAIT_MS` of wall time still ends
  the test.

## Tests

The same test under the same recipe: 5 of 5 passed, at 162-233 runnable processes (main: 1 of 3).
