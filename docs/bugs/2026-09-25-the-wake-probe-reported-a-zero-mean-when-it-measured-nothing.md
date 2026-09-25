# The wake probe reported a zero mean when it had measured nothing

Date: 2026-09-25. Contracts: §4.1 (the profile, its refusals: `MeasurementTimeout`), D-10, D-11.
Found by CI run 36201648573 (`ef02487`, macOS gates).

## Symptom

`profile::tests::a_profile_measures_within_its_budget_and_round_trips_through_json` failed on the macOS
runner:

```
assertion failed: profile.wake.mean_ns > 0     crates/machine/src/profile.rs:412
```

The same test passed on the run before (`ee823e0`), so the failure was intermittent.

## Root cause

The wake probe (`crates/machine/src/wake.rs`) splits its budget over five rounds. Each round spawns a
fresh waiter thread, waits for the operating system to confirm it asleep, and then times wakes until
the round's share of the budget runs out. The test's budget is 30 ms, so each round had 6 ms. On a
loaded three-CPU virtual machine, the setup alone (spawning the thread and seeing it asleep) can outlast
that, and such a round keeps nothing. In this run all five did.

`summarize` then took the missing mean interval as zero (`unwrap_or(MeanInterval { mean: 0, … })`), so
the probe reported a measurement it never made. Every consumer derives from that number: the spin
window, the step quantum, the timer tick and the runtime's wake-estimate prior. With a zero mean the
window is zero and the quantum is one nanosecond.

## Fix

- **Keep sampling while below the floor.** After the five planned rounds, while the pooled sample is
  below the stopping rule's floor (`MIN_SAMPLES`), up to five more rounds run, each with twice the
  previous round's budget. A slow machine gets exponentially more time, bounded at
  2 + 4 + 8 + 16 + 32 = 62 more round-budgets. `WakeLatency::rounds` reports how many rounds ran.
- **Refuse short of the floor.** If the floor is still not met, `wake` refuses with
  `MachineError::MeasurementTimeout { probe: "wake" }`, the refusal §4.1 names. It no longer reports a
  mean it did not measure.
- **The refusal propagates.**
  - `MachineProfile::measure` returns `Result`.
  - `refresh_cheap` and `refresh_if_power_changed` return the refusal and keep the wake already
    measured.
  - The CLI's `measure` (and so the anchor, the daemon and `slates profile`) fails by name.
  - The examples and benches report the refusal (`?`, or `expect` where they already allow it); the
    tests name what they need.

## Tests

- `a_probe_that_measured_no_wake_refuses_rather_than_report_a_mean`: given no time, no round can
  confirm a sleeper, and the probe refuses by name. The calling thread's CPU mask is restored.
- `a_profile_whose_wake_was_not_measured_is_refused`: the same refusal at the profile.
- `a_refused_wake_re_measure_keeps_the_measured_wake`: a refused re-measure leaves the previous wake in
  place.
- The confirmed-sleeper test now also requires the stopping rule's floor of samples.
- `slates-machine` passes 46 of 46 on macOS and on Linux (container). The server, client, CLI and
  memory suites pass on macOS. Clippy is clean.

The extension itself (a slow machine rescued by longer rounds) has no test of its own. Such a test would
depend on how loaded the machine is, which is timing, not behaviour. The zero-budget refusal pins the
bound's other side deterministically.

## Edits

- `crates/machine/src/wake.rs`, `profile.rs`.
- The callers: `crates/cli/src/{daemon,anchor,verbs}.rs`; examples in `machine`, `rt`, `vfs`, `client`,
  `cli`; tests in `client`, `mcp`, `mem`, `server`; `xtask/src/conformance/mount.rs`.
