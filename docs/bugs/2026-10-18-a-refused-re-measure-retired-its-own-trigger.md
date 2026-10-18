# A refused power-state re-measure retired the trigger that would have retried it

**Date:** 2026-10-18
**Area:** `crates/machine/src/profile.rs` (§4.1 machine profile, D-11 "re-measured on power-state change")
**Status:** fixed
**Found by:** `profile::tests::a_power_state_change_is_detected_and_triggers_a_refresh` failing in a
`cargo test --workspace` run at load 56 on this Mac (run ws6). The same test is green on the Linux box
(56/56, 1.03 s), and green here when the machine is quiet — it is a load-sensitive failure, and the
defect under it is not.

## Description

`MachineProfile::refresh_cheap` re-measures the power-sensitive subset of the profile (timer overhead,
syscall cost, wake latency) when the machine moves between battery and mains.
`MachineProfile::power_changed` is how a caller knows to ask: it compares the live power state against
the one recorded in the profile.

A refused wake probe — the machine too busy to produce a measurement inside the budget — returned its
refusal, but the profile had already been changed. The caller saw an error; the profile saw a completed
refresh.

## Root cause

The field order inside `refresh_cheap`:

```rust
self.facts.power = Facts::query().power;   // the trigger is retired here …
self.timer_overhead_ns = timer_overhead_ns();
self.syscall = probes::syscall(budget);
self.wake = wake::wake(budget, …)?;        // … and the measurement can refuse here
```

On a refusal the function returned early with the new power state already recorded. From then on
`power_changed()` reads false, so nothing asks again: the profile carries one power source's wake
latency under the other source's label, and the three tunables derived from wake (the spin bound, the
quantum, the park decision) stay at the wrong power source's values for the rest of the process's life.
The profile left behind was also torn — a new timer and syscall cost beside the old wake.

The function's own doc comment already stated the correct rule and the code contradicted it: *"a refusal
here means the machine was too busy to measure, not that nothing changed."*

## Impact

Latent today and bounded: `refresh_cheap`, `refresh_if_power_changed` and `power_changed` have no caller
outside this file's tests, which is itself the open D-11 gap (the re-measurement half of "every tunable
derived at boot … re-measured on power-state change" is unwired). The defect is in a `pub` API that the
wiring would have inherited, on exactly the machine slates targets: a laptop that is unplugged while
busy would have measured once, refused, and never retried.

## Fix

Guard then apply, the house pattern from `crates/db/src/partition.rs` (`check` refuses before the append;
`apply` is unconditional, so replay is deterministic). Every fallible measurement is taken into a local
before a single field moves, so a refusal leaves the profile whole — including the power state, which is
what makes the next poll try again.

Exact edits:

- `crates/machine/src/profile.rs` — `refresh_cheap` measures into locals (`power`, `timer_overhead_ns`,
  `syscall`, `wake`) and assigns all four only after the `?`; the doc comment records why the order is
  load-bearing.
- `crates/machine/src/profile.rs` — `a_refused_wake_re_measure_keeps_the_measured_wake` now forces the
  refusal with `Duration::ZERO` (deterministic, no load dependency) *and* asserts the power state is
  unmoved and `power_changed()` still true. Red-checked: reinstating the old field order fails this test,
  restoring it passes.
- `crates/machine/src/profile.rs` — `a_power_state_change_is_detected_and_triggers_a_refresh` asserts the
  two outcomes apart instead of requiring the probe to succeed: a refresh that measured records the state
  it measured under; a refused one keeps the trigger and the measurement it had no replacement for. It was
  a load test as written (the fourth shape in the 2026-10-18 busy-machine section of `GAPS.md`).

## Sibling sweep

Every other `&mut self` method in `profile.rs` that can refuse: `refresh_if_power_changed` is a guard plus
a call to `refresh_cheap` and holds no state of its own. `MachineProfile::measure` and `measure_within`
build a fresh profile and return it whole or not at all — a refusal there leaves no half-written profile
because there is no profile yet. No other site in the crate assigns before a fallible measurement.
