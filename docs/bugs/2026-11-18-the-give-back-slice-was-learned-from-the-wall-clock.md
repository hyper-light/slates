# The idle give-back's slice was learned from the wall clock (2026-11-18)

Contracts: CLAUDE.md "Bounded work everywhere" (a loop over user-scaled data runs in slices under the shard's
per-iteration budget), A-226 (an idle shard gives back its dead recovery memory a slice a step, each slice what one step
quantum gave back the step before), and `slates_rt::registry::calling_thread_cpu` ("for a caller that judges its own
work by the CPU it spent rather than the wall time it took, which a loaded machine stretches").

## Description

`daemon::learn_give_back_rate` sized each give-back step (the recovery memory no restart reads, and the emptied write
log's pages) from the step before: the bytes it walked over the time it took, scaled to the step quantum. It read the
time on the shard's clock, the host's monotonic wall clock. GAPS recorded the effect on 2026-11-08: at load about 100 a
Mac's give-back of 18 to 21 ms of CPU took 2,572 to 3,042 steps, not hundreds. g083 found the recovery test's give-back
case (`a_boot_give_back_holds_its_shard_a_slice_of_cpu_a_step`) failing once under load on adacachy (main's r271),
passing on its rerun, and asked whether the slice was judged by the wall clock. It was.

## Root cause

A step the host descheduled took longer on the wall by every moment it waited, so it taught a slice smaller by as much,
and the slice followed the host's scheduling rather than the shard's own work, which the per-iteration budget holds to
a quantum of the shard's CPU.

## Impact

Under load a give-back took many more, smaller steps than its CPU needed, each a turn of an otherwise idle shard; the
recovery memory stayed resident longer. No request waited behind it (A-226's amendment yields a step to any other
work), and nothing was lost.

## Exact edits

- `crates/server/src/daemon.rs`: `GiveBackRate` (the slice and what the steps since it walked and spent) with
  `GiveBackRate::fold` (the bytes per quantum of the CPU shown; steps a coarse clock shows none for folded into the next
  it shows); `learn_give_back_rate` folds a step's walked bytes and the CPU it spent; `shard_thread_cpu_ns` and
  `spent_since`; `return_dead_recovery` and `purge_write_log` take their step's clock as `*_timed`, production passing
  the shard thread's CPU clock.
- `crates/server/src/state.rs`: `ShardState::give_back_rate` replaces `give_back_slice_bytes`.
- Design: A-226's note and the A-491 amendment; GAPS: the give-back slice's entry closed.

## Test

`a_give_back_step_learns_its_slice_from_its_own_cpu_not_the_wall_clock` (`daemon`'s unit tests): real give-back steps on
a shard whose recovery memory is all dead, timed by a controlled CPU clock (a quantum, half a quantum, then a coarse
clock's none, none and a quantum). It expects slices of a page, two pages, two, two, then six. Timed by the wall clock,
the steps learn their real wall time instead.
