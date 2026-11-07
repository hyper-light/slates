# A busy machine refused the anchor's boot at the wake probe (2026-11-07)

## Description

Under clients' frozen-daemon recipe on this Mac (eight nice-19 spinners a logical CPU, 144 on 18), the anchor itself
did not start: "slates: measure the machine profile: measurement timeout: wake", and the test ended with "the anchor
ended before its daemon served". It failed in 5 of 5 runs on the restart branch and in 2 of 3 on main. The anchor
cannot start without a profile, so on a machine that busy, nothing started at all.

## Root cause, measured

The wake probe (`crates/machine/src/wake.rs`) refuses when, after its planned and doubling rounds, it holds fewer
than the stopping rule's floor of samples (`MIN_SAMPLES`, 16) once a fixed wall of 250 ms (`PROBE_WALL_BUDGET`) has
passed. GAPS (2026-10-18, "the busy-machine discipline") recorded this as the standing limitation: "a machine too busy
for the floor in 250 ms still refuses".

`slates profile --quick --json`, debug build, twenty runs under the same recipe, before the fix: refused 19 times in
20. The one that measured kept 17 samples in 10 rounds: a mean wake of 94 ms, 68 ms at the median and 205 ms at
p99. At 68 ms a wake, the floor needs over a second, four times the wall. The probe was not stuck: each of its rounds
got wakes answered, only slowly. As with the anchor's start (`2026-11-07-the-anchor-killed-every-start-a-busy-machine-starved.md`),
a fixed wall could not tell a starved probe from a stuck one.

## Exact edits

- `crates/machine/src/wake.rs`: the rule that decides whether the probe runs another round is one function,
  `extends`, and the round's budget another, `next_round_budget`. Past the full probe budget's wall the probe runs on
  while the last round's waiter ran at all (it published itself, so the machine scheduled it), each round twice the
  last, from one window (250 ms) up to `longest_round()` (`PROBE_WALL_BUDGET · 2^(WAKE_ROUNDS − 1)`, 4 s), the same
  doubling the rounds before the wall use. A round whose waiter never ran is a stuck probe, and the probe refuses. A
  round past the doubling ones ends once the pooled sample reaches the floor. The extension past the wall is bounded by
  `starved_extension()`, 31 windows (7.75 s). An idle machine never reaches it, and a full profile is unchanged.

A first version gave each round past the wall exactly one window and stopped at the first round that got no wake
answered. It was still refused 2 times in 3 at load 267-324: a wake then took 174 ms at the median and 245 ms at p99,
and a fresh round needs its waiter scheduled, parked and woken before its first sample, about three scheduling
latencies, which do not fit one 250 ms window. Whether the waiter ran at all is the progress that tells starved from
stuck. Whether a wake came back inside a fixed window is the wall again.

## Tests

- `a_probe_whose_waiter_runs_measures_past_the_wall_and_one_whose_waiter_never_runs_refuses` and
  `a_round_past_the_wall_gets_a_window_and_doubles_to_the_longest_round`: both fail against the original rule (the
  probe stopped at the wall whatever its rounds did, and a round past the wall got what was left of it, nothing) and
  pass with the fix.
- `a_budget_too_short_for_its_rounds_extends_to_the_full_probe_budget`: its wall bound now includes
  `starved_extension()`.
- By use, the same twenty runs after the fix (144 spinners of the recipe plus other agents' work, 240 runnable
  processes; macOS's load average read from 13 to 325 across them): **20 measured, 0 refused**. Each took 15 rounds to
  the floor of 16 samples, with a mean wake of 120-244 ms, a p50 of 101-212 ms and a p99 of 181-364 ms. The whole debug
  quick profile took 69-95 s, most of it the other probes in a debug build.

## Left open

- A probe whose waiter is never scheduled within a whole round still refuses. That is a stuck probe, and the refusal
  names it.
- On a starved machine the anchor's boot can take the extension's 7.75 s more. Nothing supervises the anchor's own
  boot with a deadline.
- The other probes are wall-bounded too, but none refuses: each reports what it measured within its bound.
