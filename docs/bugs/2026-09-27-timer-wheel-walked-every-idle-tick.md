# The timer wheel walked every idle tick after a timer fired

Date: 2026-09-27. Contract: §4.3 (the runtime's timers, a hierarchical wheel, Varghese & Lauck). Found
profiling the congestion bake-off.

## Symptom

A timer 66,600 ticks away took 66,595 tick visits to reach once any timer had fired. The regression test
records the number.

## Root cause

`Wheel::advance` skipped idle ticks only while its cached earliest deadline was exact. Any firing marked
it inexact until the next `next_deadline_ns` rescan, so the following advances walked tick by tick. Even
when skipping, it stopped at every 64-tick boundary to check for cascades.

## Fix

Each step jumps straight to the next tick where some level has an occupied slot. `next_event_tick` scans
at most one rotation, 64 slots, per level. An entry at level L is always less than one rotation of that
level ahead, and its slot's boundary always lies after the current tick. The cost is per timer event, not
per idle tick, and the step no longer depends on the cached earliest deadline.

## Evidence

- `crates/rt/src/timer.rs` `advancing_across_an_idle_stretch_costs_per_event_not_per_tick`: 66,595 tick
  visits before; at most 12 now.
- The wheel's existing oracles pass: deadline order, stale cancels, idle skips that never miss a cascade,
  and a far deadline cascading through the levels.
