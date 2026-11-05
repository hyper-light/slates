# The placement test read only the root cgroup's quota (2026-11-05, §4.3, §4.1, A-208)

## Description

`crates/rt/tests/placement.rs` decides which of its two Linux cases holds from the CPU time the process's cgroup
grants. A process that owns its cpuset (no quota, or one that buys every CPU of the set) must have each shard fixed to
one core of the set; a process under a quota below its cpuset must have every shard left to the scheduler. The test
read that quota from `/sys/fs/cgroup/cpu.max` alone, the root of the process's cgroup namespace. The machine facts
(`crates/machine/src/facts.rs`, `cpu_budget`) take the tightest quota over the process's own cgroup and every ancestor
up to the root, since a parent's quota binds its children. So inside any cgroup below the root that carries a quota,
the facts saw it and the test did not.

## Measurement

Reported by the supervisor, 2026-11-05: on adacachy (Linux x86_64, 16 cores × 2 threads, `Cpus_allowed_list 0-31`),
inside a `systemd-run --user -p CPUQuota=1200%` unit, `linux::on_an_owned_cpuset_each_shard_is_fixed_to_its_own_core_of_the_set`
failed 3 of 3 on main a7804447 at `placement.rs:158`: "shard N may run on <mask>, not one core of its owned set". The
unit's 12-CPU quota is below the 32-CPU set, so the facts placed no shard (`Placement::of`: not owned), each shard's
mask was the whole set, and the test, reading no quota at the root, expected owned cores. CI's runners have no quota
below the root, so CI never took this path.

## Root cause

The test's oracle for "does the process own its cpuset" read a different, looser budget than the rule it judges: one
cgroup file where the rule reads the whole path.

## Impact

A test fault, not a placement fault: the placement did what §4.3 says for a quota below the cpuset. Any run of the
`slates-rt` suite inside a quota-limited cgroup below the root (a `systemd-run` unit with `CPUQuota`, a Kubernetes pod's
container cgroup seen without a cgroup namespace) failed the owned case and skipped the quota case it should have run.

## Edits

- `crates/rt/tests/placement.rs`: `cpu_budget` reads `/proc/self/cgroup` and takes the tightest budget over the
  process's cgroup and every ancestor, v2 `cpu.max` or v1 `cpu.cfs_quota_us` over `cpu.cfs_period_us`, as the facts do,
  reading the kernel's files itself (`budget_at`, `share_at_most`). Nothing else changes.

## Red check (adacachy, 2026-11-05 23:15 UTC, load 21.7)

Main (a7804447's test, unchanged on b502cf96) and the fix (b7d66ddc), each built in its own target dir and run three
times in each of two `systemd-run --user` units:

| unit | main | the fix |
|---|---|---|
| `CPUQuota=1200%` (unit `cpu.max` `1200000 100000`, root absent) | owned case **failed 3 of 3**: "shard 0 may run on [0, …, 31], not one core of its owned set"; quota case skipped ("None over 32 CPUs") | 2 passed, 3 of 3: owned case skipped loudly ("Some((1200000, 100000)) over 32 CPUs"), quota case ran and passed |
| no quota (unit `cpu.max` `max 100000`) | 2 passed, 3 of 3: owned case ran and passed | 2 passed, 3 of 3: owned case ran and passed |

The runs without a quota are also the Linux SMT sibling check: on adacachy's 16 cores × 2 threads each shard is fixed
to one core of its owned set, each to its own.

## Siblings

- The Windows and macOS placement paths take no cgroup; the test skips loudly there. The same SMT placement on Windows
  (jupiter) is owed a check that each shard is fixed to one core's first thread.
