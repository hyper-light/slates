# ThreadSanitizer's slow polls ended in-process daemons' test binaries as spinning (2026-11-18, §4.14, A-318)

## Description

The liveness watchdog calls a shard spinning once it has run a window of CPU without completing a step (A-228), and a
daemon nobody supervises ends its own process on that verdict (A-232). An in-process daemon in a test binary is such a
daemon, so its verdict ends the whole binary. The window was the liveness budget, one second, on the ground that a
cooperative task yields within the shard's step budget, microseconds. That ground holds for the build the budget was set
for. Main's ThreadSanitizer lane runs every poll many times slower, its runtime called at every memory access and every
function's entry and exit, and polls past a second of CPU were judged spinning:

- run 44238377555 (main 51ce4bcc6): the `client` test binary died with SIGABRT: "the watchdog ends the daemon's
  process (exit 4): shard 4 is spinning, 1054 ms of CPU without a step against the 1000 ms window";
- diag-b13-2 (the integrator's TSan diagnosis): the server's library tests the same way, "shard 0 is spinning, 1060
  ms of CPU without a step".

The lane had been red on every main push since A-232 landed (the lone daemon tests' half of it was the unjoined end
writer, fixed by 96c47435f).

## Root cause

The spin window was one fixed number of CPU nanoseconds in every build, but a poll's CPU is the build's: the same
poll costs what the build makes it cost. Nothing told the watchdog which build it ran in.

## Fix (exact edits)

- `crates/machine/src/build_cost.rs` (new): one dependent chain over a page of words, `acc = acc × K + word`, compiled
  by this build (`compiled_chain`) and written in machine code (`machine_chain`, an `asm!` block for x86-64 and one for
  AArch64, which no compiler flag instruments or deoptimizes). `measure_build_cost` times each in batches of at least
  `TIMER_OVERHEAD_FACTOR` clock readings' cost, alternately over `MIN_SAMPLES` rounds, keeps the least of each, and
  refuses if the two compute different values; `build_cost` measures once a process. `BuildCost::scale` holds a time to
  the build, never below it. Miri runs no machine code: refused, the budget stands.
- `crates/server/src/watchdog.rs`: `spin_window_ns` (the budget scaled by the build's cost); `ShardWatch::judge` takes
  it and calls a shard spinning past it; `Verdict::Spinning` names it; the start line names the window and the measure;
  `Watchdog::spin_window_ns`.
- `crates/server/src/daemon.rs`: `Daemon::spin_window_ns`.
- Tests: `crates/server/tests/liveness.rs` (the spinning test waits for the daemon's spin window; the new slow-poll
  test), `crates/client/tests/lone_daemon.rs` (the spinning case reads the child's spin window from its start line).
- `unsafe-budget.toml`: slates-machine 70 → 72 (the two `asm!` blocks).

## Tests

- `a_poll_is_judged_spinning_only_past_the_window_held_to_its_build` (`crates/server/src/watchdog.rs`): a whole CPU in
  one poll, judged with a spin window of three budgets, is alive past one budget and spinning at three.
- `the_spin_window_is_the_budget_held_to_the_builds_measured_cost`: two, one, one and one budgets for costs of twice,
  equal, half and refused.
- `a_poll_slower_than_the_budget_within_the_builds_spin_window_keeps_the_beat` (`crates/server/tests/liveness.rs`): an
  anchored daemon's shard runs one poll of a budget and a half of CPU; where the build's spin window is two and a half
  budgets or more the beat never gaps a window and no look withholds it; a build measured nearer machine code skips,
  loudly.
- `the_machine_code_chain_computes_what_the_compiled_one_does`, `an_unoptimized_build_measures_slower_than_machine_code`,
  `scaling_holds_a_time_to_the_build_never_below_it` (`crates/machine/src/build_cost.rs`).

## Siblings (open, from reading)

- The anchor's start judgment holds a starting daemon's busiest thread to a window of CPU (`crates/api/src/role/anchor.rs`).
- An observation's budget is counted in the observed shard's CPU (`crate::observe::ShardTime`).

Neither is held to the build yet; neither has been seen to fail under ThreadSanitizer.
