# Every daemon under a CPU quota pinned its shard to CPU 1 (2026-09-29)

## Description

On the KIND lane, the daemons of an idle five-replica fleet under the wan profile were killed by their
anchors every minute or two for a lapsed heartbeat. Over 180 s the pods logged 319 lapse lines and 15
restarts (`lapse-probe2`, 2026-09-29 21:11–21:20 UTC). An image from before the day's changes did the same
(0–95 lapses and 0–4 restarts per pod in 180 s). A lone node of the same image in plain Docker did not.

The late heartbeats were phase-locked. Each ended 2.0–2.2 s into a 5 s cycle (phase 2.5–3.2 s for five of
them), on every pod and across every restart: the readiness probe's `slates status`, which the
StatefulSet's parallel start put in phase on all five pods. Most gaps showed the shard barely running
(4–32 steps, a timer overrun of 418–1,008 ms). The shard thread was runnable and not getting a CPU.

## Root cause

`crates/machine/src/facts.rs` listed a Linux process's cores as `0..available_parallelism()`. That count is
the smaller of the affinity mask and the cgroup CPU quota, and it names no core. Under the chart's two-CPU
quota every daemon listed cores `[0, 1]`. `slates-rt`'s `shard_cores` kept the first for control and fixed
the one shard to CPU 1. So every daemon in the Docker VM pinned its shard to the same CPU: the five
`slates-succession` pods and the three of a second KIND cluster, eight threads on one of 18 virtual CPUs.

Read through the kind node containers (`/proc/<tid>/status`, the pod cgroup's `cpu.stat` and
`cpu.pressure`):

- Every `slates-shard-0` thread had `Cpus_allowed_list: 1`.
- The VM was 90 % idle: 7,969 of 8,848 ticks over 5 s in `/proc/stat`.
- One shard thread ran 61 ticks per 5 s (12 % of a CPU) while in state `R`.
- The container's `cpu.pressure` was `some = full = 78–81 %`, accumulating 4.03 s of stall per 5 s.
- `nr_throttled` was 0, so the quota was not throttling anything.

A shard waiting its turn behind seven others on one CPU missed its heartbeat whenever the probe's client
added work.

A second failure mode had the same cause. On a cpuset that does not include the fabricated ids (say CPUs 2
and 3), the pin to CPU 1 was refused with `EINVAL`. `let _ = pin_current_thread(core)` discarded the
refusal, so the shard floated unnoticed.

A CPU quota is a share of time on the cores a process may run on, not a claim on any of them. Pinning
inside a pool shared in time stacks every tenant that derives its cores the same way on the same CPUs,
where the kernel can no longer move them.

## Impact

- Any fleet node under a Kubernetes CPU limit with the default CPU manager, or `docker run --cpus`, pinned
  its shards to the lowest-numbered CPUs. Several such daemons on one node shared those CPUs while the rest
  idled.
- On the KIND lane that meant anchor kills every minute or two. The restarts in turn re-seeded retired
  peers alive (the "revived seed ids" finding) and contaminated the council burst measurement of
  `8ab25de` (docs/wip/kind-lane.md Piece 7). That measurement is owed again on a fleet that does not
  restart.
- A refused pin was invisible.

## Exact edits

- `crates/machine/src/facts.rs`: the Linux core list is the calling thread's affinity mask
  (`sched_getaffinity`), and the Windows one is the process affinity mask. A refused query records no core,
  with a note. `Facts.cpu_budget` is new: the tightest cgroup v2 `cpu.max`, or v1 quota over period, up the
  process's cgroup path. It comes with `parse_cpu_max`, `parse_cfs_budget`, `CpuBudget::covers` and
  `CpuBudget::whole_cpus`, and `Facts::cpus_at_once`. The Windows process-mask query moved here from
  `probes.rs`, so the unsafe budget is unchanged.
- `crates/machine/src/placement.rs` (new): the one placement rule. The shard count is the fastest class's
  cores that the budget runs at once, less one, at least one. Shards are fixed to cores only when the budget
  covers every core the process may run on.
- `crates/machine/src/wake.rs`: the wake probe places its pair by the same rule, and runs unpinned where
  production does. The new `Pinning::Scheduled` says so rather than calling it a refusal. Its duplicate code
  table is gone.
- `crates/machine/src/profile.rs`: `PROFILE_VERSION` 2 → 3.
- `crates/rt/src/runtime.rs`: `shard_cores` reads the placement, and `pin` is set only when there are cores.
  A refused pin is counted per shard (`Counters::pin_refused`) and logged once where the OS pins.
- `crates/client/examples/provision_bench.rs`: the runnable concurrency is `cpus_at_once`.
- `deploy/helm/slates/values.yaml`: the CPU comment states the rule.

## Evidence

Failing tests first: `crates/rt/tests/placement.rs` in Docker on Linux, before the fix.

| Test | Shape | Before | After |
|---|---|---|---|
| `under_a_quota_below_the_cpuset_the_scheduler_places_every_shard` | `--cpus=2` | failed: shard 0 confined to `[1]` of `0-17` | passes |
| `on_an_owned_cpuset_each_shard_is_fixed_to_its_own_core_of_the_set` | `--cpuset-cpus=2,3` | failed: shard 0 may run on `[2, 3]` (pin refused) | passes |

After the fix both tests also pass, or skip loudly where their shape does not hold, under
`--cpuset-cpus=2,3 --cpus=2`, `--cpus=17` and no flags. `slates-machine` gains the placement unit tests, an
oracle over 676 cpuset masks × 5 quotas, and the budget parsers' hostile-input tests; 55 pass on macOS and
on Linux with and without a quota.

The KIND lane, same cluster and wan profile, idle five-replica fleet, 180 s window (`lapse-probe3`,
2026-09-29 22:13–22:16 UTC, host load 8–11):

- 0 lapse lines, 0 restarts and 0 late beats. Before, over the same window: 319, 15 and 9.
- Every shard thread had `Cpus_allowed_list: 0-17`.
- `cpu.pressure some avg10 = 0.00 %`, against 78–81 % before.
- `nr_throttled 0`.

Sibling found by the same measurement, recorded open and fixed next: an idle shard spins before every park
for good once any client has connected (`activate` in `crates/server/src/daemon.rs` sets the shard's active
flag at a handoff and nothing clears it). With the pinning fixed, each idle pod's one shard used 28–62 s of
CPU in its first 190 s: 15–32 % of a core, spent waiting for work that never came.
