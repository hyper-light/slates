# w-provision-gate -> main: 84a macOS closed on data, the shards' arm (TB) landed (A-831), 2026-11-21 ~12:45 PST

**Item:** 84a macOS. A/B of Mach's THREAD_TIME_CONSTRAINT_POLICY for the shards and the client's reply wait, on main's
nap and A-799 (b01a0ef7b was rejected and is not on main), on CI macos-latest. Your 10:31 rule: land TB if it wins on
loaded p99/p999 with the quiet p50 unchanged; record TC as rejected.

**Commit on origin/main:** 6865fe121 (merge 25efd3aa4).
- slates_machine::time_constraint.
- slates-rt: a latency-tracking runtime's shards (the daemon's) take the policy at thread start.
- slates-ipc: a client's synchronous reply wait takes it once per thread.
- Terms: aperiodic, computation = constraint = the measured window, doubled until the kernel takes it.

**Measurement** (BENCHMARKS "84a on macOS", every round; runs 44934544995 and 44934809949, 3 rounds each against
main):

| | main | TB (landed) |
|---|---|---|
| loaded accept p99 | 14.0-17.4 ms | 0.41-2.11 ms |
| loaded accept p999 | 16.6-20.8 ms | 0.70-4.52 ms |
| loaded conflict p99 | 14.4-16.4 ms | 0.55-1.85 ms |
| loaded conflict p999 | 17.7-20.2 ms | 0.80-4.56 ms |
| busy kevent harvests >= 1 ms | 8.7-12.2% (g222's split, confirmed) | 0.001-0.03% |
| quiet accept_f10 p50 | 451-549 µs | 377-479 µs |
| R9 uncontended p99 (3-CPU runner) | 196-344 µs | 180-338 µs |
| load throughput, M spinner turns/s | 213-242 | 98-137 |

- Neither arm meets R9's 50 µs uncontended p99 on this 3-CPU runner; TB leaves it where main has it.
- The spinners are not starved: the daemon finishes its work about 10x sooner and runs ahead of them while it works.
- No demotion or refusal was logged.

**TC (client only): measured and rejected.** p999 up to 29.9 ms, conflict p99 up to 4.4 ms.

**Red first:** crates/rt/tests/time_constraint.rs. On main a tracking runtime's shard is not under the policy (red);
after the change it is.

**Focused tests green:**
- slates-machine time_constraint;
- slates-rt time_constraint, idle_spin, pollers, spin_kick;
- slates-ipc endpoint;
- slates-server closed_loop, idle_window;
- slates-client waits, edit_and_submit;
- fmt.

**Still open (GAPS 84a):** 84a's bar on macOS. TB's p99 is under 1 ms in 11 of 12 round medians, its p999 in 4 of 12.

**For you to delete:** branches diag-measurer-w-provision-gate-tc, diag-measurer-w-provision-gate-tck,
diag-measurer-w-provision-gate-tc2 (local only, if present).
