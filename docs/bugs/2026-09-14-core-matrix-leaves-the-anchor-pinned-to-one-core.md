# The core matrix leaves the anchor's thread pinned to one core, so its daemon computes another machine identity

Date: 2026-09-14
Area: `crates/machine/src/probes.rs` (`core_matrix`, `ring_round_trip`, `platform::pin_current`)
Severity: the Linux anchor+daemon deployment path — every `slates anchor` without `--quick` on Linux
(a full machine profile) produced a daemon that refused the anchor's segment and crash-looped; the
production image (docs/deploy.md) could not start.

## Symptom

The KIND lane's first image proof (`cargo xtask kind smoke`, 2026-09-14 10:55 CDT, the `slates:lane`
image in plain Docker with `--memory 1g --cpus 2`, a one-node manifest):

```
slates: attach: segment is for another machine (581fce50…368b != 48f7fa96…822c)
slates anchor: instance default up; daemon pid 15; 1 shards
slates anchor: daemon exited (Some(4)); restarted (1 in the window)
slates: attach: segment is for another machine (…)
slates: the daemon is in a crash loop (last exit Some(4)); giving up
```

The anchor wrote the segment header with the identity hash it computed; the daemon it spawned computed
a different one and refused (`AnchorError::Layout`, "segment is for another machine"). The same
container with `anchor --quick` (no core matrix) attached fine. `slates profile --json` run three times
in the same container printed one identity each time (`cores: 2` under the quota), and the full profile
printed the same identity as the quick one — the identity is deterministic per *process*; it is the
*child* that saw a different machine.

## Root cause

The identity line hashes `cores`, which is `std::thread::available_parallelism()`; on Linux that reads
the calling thread's scheduler affinity mask (`sched_getaffinity`) and the cgroup quota. The full
profile's core-to-core matrix (`core_matrix` → `ring_round_trip`) pins the calling thread to core `a`
of every pair with `sched_setaffinity` and never restores the mask, so after the measurement the
anchor's main thread is pinned to the last measured core. A process spawned by that thread inherits
its mask: the daemon's `available_parallelism()` read 1, its identity said "1 core", the hash
differed, and the attach was refused by the very check that protects a segment from another machine.
(`slates profile` alone never showed it: its own identity was read before the matrix ran.)

The anchor also logged "1 shards" for a 2-CPU quota for the same reason: the runtime's shard count
follows the cores the thread could use after the matrix. Windows pins the same way (`SetThreadAffinityMask`)
and would inherit it too; macOS only hints (an affinity tag), and `available_parallelism` does not read
it, which is why the anchor+daemon flow passed on this box and on the macOS CI lane.

## Fix

`core_matrix` captures the calling thread's affinity before the first pair (`platform::current_affinity`:
the scheduler's `CpuSet` on Linux, the process affinity mask on Windows, the null tag on macOS) and puts
it back after the last (`platform::restore_affinity`). A mask that cannot be put back is reported as
`Pinning::Refused` — the thread is then mis-pinned and nothing measured after it can be vouched for —
rather than swallowed. Paired per-platform functions with one seam, as the design's platform rule asks.

Test first: `the_core_matrix_gives_the_calling_thread_its_affinity_back` (`crates/machine/src/probes.rs`)
asserts `available_parallelism()` is the same after the matrix as before. On Linux in the `rust:1.98`
container (18 cores) before the fix: `1 != 18` (the thread left on the last measured core); after: pass.
The smoke proof is the by-use gate: the anchor's daemon attaches and answers `status`.

## Siblings

- `slates_rt::Runtime::start` pins shard threads with `pin_current_thread` — those are the shards' own
  threads for their whole life, spawned from the runtime's thread, not the anchor's main thread; nothing
  is spawned from them. Not affected.
- The anchor's `--quick` path (no core matrix) is what every CI CLI flow runs (`SLATES_TEST_CLI=1`
  starts anchors with `--quick`), so the full-profile Linux path had no test until the image proof.
  The KIND lane runs the production profile.
