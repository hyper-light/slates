# The wake probe mixed two events and handed its consumers an unconverged tail

Date: 2026-09-22 to 2026-09-25. Contracts: §4.1 (the profile, the boot algorithm, derived constants),
§4.3 and §4.7 derived constants, D-10 (spin-then-park), D-11 and R3.

## Symptom

A daemon's derived constants changed by orders of magnitude between two boots of one image on one
machine. That was the root of the client-seat collapse
(`2026-09-22-client-ring-sized-by-the-wake-tail-not-littles-law.md`). The same instability still fed
the spin window, the idle window, the step quantum (the I/O harvest cadence, the destroy, archive and
pre-fault slices, and the long-step count), the timer tick, the runtime's inbound ring and the guest
credit's kick round trip.

## Evidence

**The old probe on its own image.** `docker run --cpuset-cpus=… slates:claudediag profile --json`, four
runs per CPU set, 2026-09-22:

| cpuset | wake p50 (ns) | wake p99 (ns) | samples |
|---|---|---|---|
| `0` | 417, 417, 417, 417 | 708, 1334, 709, 1042 | 64 each |
| `0-1` | 10041, 416, 417, 459 | 511042, 1291, 667, 63416 | 256, 64, 64, 576 |
| `0-3` | 459, 542, 8750, 458 | 75958, 21459, 65417, 4583 | 64, 192, 128, 64 |

Five consecutive profiles inside one KIND pod gave a wake p99 of 708, 18375, 21667, 25667 and 750 ns.

**Where the two modes come from.** A scratch prototype, 2,000 samples per run and 5 runs per mode per
CPU set, in Linux containers:

| mode | p50 across runs (ns) |
|---|---|
| the old method, unpinned | 458–10,417, split between runs |
| waiter confirmed asleep, unpinned | 542–10,459, split between runs |
| waiter confirmed asleep, pair pinned to two cores | 10,917–13,542, in every run |

Confirming the waiter asleep alone did not end the split; pinning did. The OS sometimes ran the woken
thread on the waker's CPU and sometimes elsewhere, and a run kept whichever it drew.

**Why the mean.** On the same two pinned cores:

| waiter | p50 | p99 | mean |
|---|---|---|---|
| spinning | 83–167 ns | 84–209 ns | 72–185 ns |
| parked, confirmed asleep | 10.9–13.4 µs | 285–639 µs | 31.1–43.5 µs |

The tail belongs to parking, not to the host in general: a spinning waiter never saw it. The
spin-then-park rule spins for the expected cost of parking (Karlin, Manasse, McGeoch and Owicki,
Algorithmica 1994), so the median understates that cost about threefold and the p99 overstates it ten to
fifteenfold. Measured spread: σ 170–286 µs, a coefficient of variation of 5–7, on the virtual machine.
Apple silicon, natively: the kernel ran the woken thread on the waker's CPU 84–92 % of the time
(prototype), cross-CPU mean 2.8–10.9 µs, coefficient of variation 1.1–1.8.

**Why the rounds compare medians.** The first verdict, strict overlap of the round means' intervals,
flagged Apple-silicon rounds about ten percent apart. With a margin of the probe's own precision, round
*means* still disagreed in six Linux runs of eight with the pair pinned: a 50 ms round sees only a few of
the tail's millisecond events, and a percentile bootstrap cannot invent events a round did not see.
Round medians show a placement mode (0.4 against 10 µs) and ignore sparse tail events.

## Root cause

The probe timed a round trip, not a defined event, and its stopping rule converged the median while the
derivations consumed the p99. A run-long mode is invisible to a within-run interval, so every old run
converged tightly and runs still differed 23×.

## Fix

`crates/machine/src/wake.rs`:

- **The event.** The waiter is confirmed asleep before each wake: Linux `/proc/self/task/<tid>/stat`
  state `S`, macOS `thread_info` `TH_STATE_WAITING`. Windows has no per-thread query short of a
  system-wide snapshot, so there the waiter's own announcement stands in and the result reports
  `asleep_confirmed: false`.
- **The placement production runs under.** The waker is pinned to the fastest class's first core (the
  control core) and the waiter to each shard core in turn, as `slates-rt`'s `shard_cores` places them.
  Wakes the OS still ran on one CPU are dropped and counted. Where the OS will not pin (macOS) both
  threads are unpinned, every sample is kept, and the same-CPU share is recorded.
- **The statistic.** The probe converges the mean's 95 % bootstrap interval to the stopping-rule width,
  or reports `quick` at its wall budget. The p50 and p99 are kept for the record.
- **Rounds.** Five rounds with fresh threads. They disagree when a round's median interval misses the
  pooled one by more than the probe's precision; a disagreement lists the probe as degraded
  (`wake.rounds`).
- **Consumers.** The spin window, the step quantum and the timer tick read the mean; the runtime's
  inbound ring reads the p99 at its overflow target (Little's law: one message per syscall for a wake at
  its p99); the guest credit's bandwidth-delay product reads the mean.
- **Format.** The profile format goes to version 2. `MachineProfile::from_json` refuses another version
  by name, and a daemon handed an older anchor's profile measures its own (`Published::OtherFormat`).

## Validation

- **Linux containers, a quiet host (2026-09-25), ten runs of the new probe.** The pair was pinned in
  every run, with 0 same-CPU wakes.

  | CPUs | p50 (µs) | mean (µs) | p99 (µs) |
  |---|---|---|---|
  | two | 8.9–10.8 | 10.7–21.7 | 28–224 |
  | four | 9.5–10.0 | 15.1–17.6 | 45–94 |

  Every run was `quick`: 250 ms buys 3,000–6,000 wakes. The rounds disagreed in five runs of ten; the
  host's own load (Docker Desktop's Kubernetes) drifted during the probe.
- **macOS, nine native runs.** Mean 2.03–3.27 µs, p50 2.0–2.4 µs; 63–74 % of wakes ran on the waker's
  CPU; every waiter was confirmed asleep; placement is `Refused`, because Apple silicon ignores the
  affinity hint.
- **The earlier numbers were contaminated.** Runs before the quiet-host ones showed p99 of 1.9–2.9 ms.
  My own leftover KIND cluster was pinned to the same two CPUs and used about 35 % CPU per node. I deleted
  it before measuring again.
- **Tests.** `slates-machine`: 40 unit tests, including the confirmed-sleeper wake on the production
  placement, the pairs following `shard_cores`, the rounds verdict, the heavy-tail mean interval, and the
  refusal of another format version. `slates-rt`, `-ipc`, `-client` and `-bridge-virtiofs` pass (165
  tests); the server's unit and daemon suites pass (105 and 14); the CLI flow passes 10 of 10. Clippy is
  clean (`-D warnings`) and rustfmt applied. The unsafe budget of `slates-machine` goes from 45 to 49,
  for three macOS thread queries and one Windows one, each named in `unsafe-budget.toml`.

## Owed

1. **The boot mean is quick on a virtual machine.** At a coefficient of variation of 5–7, ten percent
   needs about twelve thousand wakes and 250 ms buys four thousand. The runtime refining the estimate
   from its own wakes is the next change.
2. **The long-step count still reads wall time.** A preempted poll counts as long.
3. **Siblings.**
   - Linux core facts number cores `0..parallelism()`, which is wrong in a container whose cpuset does
     not start at 0. The probe falls back to unpinned when a pin is refused, but the runtime's own shard
     pinning inherits the same assumption.
   - An idle three-pod KIND fleet used about 35 % of a CPU per pod (`docker stats`, 2026-09-25). It is
     not explained yet.

## Edits

- `crates/machine/src/wake.rs` (new), `stats.rs` (the mean and its interval), `probes.rs` (the old
  probe removed; `SavedAffinity`, `weaker`), `profile.rs` (the consumers; version 2; the version
  refusal), `lib.rs`.
- `crates/server/src/config.rs` and `daemon.rs`: labels and the guest credit.
- `crates/cli/src/daemon.rs`: an older anchor's profile.
- `unsafe-budget.toml`.
- `docs/wip/SLATES_DESIGN.md` (§4.1 status, §4.1/§4.3/§4.7 derived constants, A-30), `GAPS.md`,
  `TBD_FIXES.md`.
