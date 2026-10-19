# server: three observe tests injected a *busy* shard where the product means a *stopped* one

**Date:** 2026-10-18
**Area:** `crates/server/tests/observe.rs`, `crates/server/src/daemon.rs` (§4.14 observations, A-65)
**Status:** fixed
**Found by:** `cargo test --workspace` (run ws8) and reproduced deterministically on this 18-core Mac with
24 CPU burners: `a_budget_that_elapses_first_is_named_and_the_late_reply_is_discarded_and_counted`,
`a_full_control_channel_is_retried_then_named_by_the_deadline`, and
`an_unavailable_shard_is_told_apart_from_an_observed_zero` fail every time under that load and pass every
time without it.

## Description

Three tests make a shard unavailable and assert that an observation of it ends at its deadline. They did it
with `Daemon::starve_control_shard`, whose hold is a task that **spins** on the shard for a span. Under load
the observations were answered instead:

- `Ok(42)` where an admission-stage deadline was expected,
- `Ok(0)` where any deadline was expected,
- `Err(Deadline { stage: Admission, .., last_refusal: Some(ControlFull) })` where a submission-stage one was.

## Root cause

Not a product defect. The product is doing exactly what it was changed to do on 2026-10-03
(`docs/bugs/2026-10-03-an-observation-read-a-starved-shard-as-wedged.md`): **an observation's budget is the
target shard's own CPU time, not wall time.** A shard the host is starving keeps its budget and is answered
late; only a shard that is not running ends the observation. That is the whole point of the fix — a starved
but working shard must not read as wedged.

A *spinning* hold exhausts a CPU budget only on a machine idle enough to run the spin at the wall clock's
pace. Measured here, with the hold and the extension rule instrumented:

```
HOLD-TRACE spin begins shard_now=414560625 end=2414560625 asked=2000000000
EXT-TRACE spent=117273000 began=Some(72392000) budget=300000000 progress=117273000 mark=Some(72392000)
EXT-TRACE spent=165654000 began=Some(72392000) budget=300000000 ...
EXT-TRACE spent=194044000 began=Some(72392000) budget=300000000 ...
EXT-TRACE spent=217415000 began=Some(72392000) budget=300000000 ...
EXT-TRACE spent=225009000 began=Some(72392000) budget=300000000 ...
HOLD-TRACE spin ends shard_span=2058986750 wall=2.058987042s asked=2000000000
```

The hold ran its full span — 2.059 s of wall time — and advanced the shard's CPU clock by **153 ms**
(72 ms → 225 ms), because 24 burners left the shard about 7% of a core. The 300 ms budget was never reached,
so `ShardTime::extend` granted window after window, and the question was admitted 197 ms after the hold
ended and answered. Written as an assertion, these tests said *a busy shard is unavailable*, which the
product deliberately denies.

Two earlier hypotheses were wrong and are recorded so they are not retried: that the hold's start barrier
(`confirm.recv_timeout`, whose result is discarded) was timing out — it was not, confirmations arrived in
10.4 µs to 515 ms against a 10 s bound — and that the budget overshoot (2.33 s against 300 ms) was an
unbounded retry — it is the documented CPU-budget extension, bounded by the shard's own progress.

## Impact

Test-only, but it made a whole suite a measurement of the machine: three tests red under load and green
alone, which is the shape that gets called a flake and ignored. No product behaviour changes.

## Fix

A second injection, for the other half of the rule. `Daemon::wedge_control_shard(span_ns)` holds the shard
with a task that **sleeps**: the shard runs nothing else and consumes no CPU, so its progress does not rise,
the extension ends at the first window, and the deadline fires on any machine. The three tests use it; their
docs state why a wedge and not a spin. `starve_control_shard` keeps its spin — the fleet suite uses it in
three places for real CPU starvation, where wall-time silence on the probe plane is the point — and its doc
now names the distinction.

A fourth test, `a_wedged_shard_ends_the_observation_long_before_its_wedge_does`, failed under the same load
on a different line: its behaviour assertion (a deadline in the execution stage) passed, and only its wall
bound — 900 ms — did not, at 1.33 s. That bound had already been widened once. It now asserts what holds on
any machine (the observation ended before the wedge did, so the deadline cut it short rather than the sleep
running out) and attributes the tighter bound with `getrusage`'s involuntary-switch count, the second
busy-machine discriminator.

Exact edits:

- `crates/server/src/daemon.rs` — `wedge_control_shard`, with the measurement above in its doc.
- `crates/server/tests/observe.rs` — the three tests wedge; the wedge test's bound is attributed.
- `crates/machine/src/bench.rs` — `involuntary_switches` is now one public function rather than three
  copies (`crates/vfs/examples/vfs_bench.rs`, `crates/client/tests/driver.rs`,
  `crates/ipc/tests/rendezvous.rs` now call it), so the fourth site did not become a fourth copy.

## Sibling sweep

Every caller of `starve_control_shard`: `crates/server/tests/fleet.rs` lines 1160, 8327 and 11186 want wall-time
starvation of the probe plane and a lease, not a CPU-budget deadline, so the spin is right for them;
`crates/server/tests/observe.rs:208` keeps a question pending across a stop and does not assert a deadline, so
either hold serves and it is left alone. No other test in the tree asserts an observation deadline.
