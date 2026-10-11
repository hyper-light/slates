# A landing's durability waits held its shard on a busy machine (2026-10-10)

## Description

A granted landing runs in slices on the shard that owns its volume (AUD-29-25), and the shard serves its other clients
between slices. Each slice runs units: a file written, a batch's barrier, a placement. A unit cannot be interrupted, so a
slice overruns its budget by at most its last unit. That bound holds only while every unit is short.

On a busy machine — Ada's default ("A busy machine is default. Shouldn't we be robust against this?") — the units that
wait for durability are not short. Their wait belongs to the device and to every other writer on the filesystem, not to
the landing. While one ran, every other client of the shard waited with it: reads, provisioning, lease takes.

## Evidence

The landing-fairness lane, 600 files with a prober on the same shard, scratch timers around every unit and every host
call (not kept). Raw outputs are in the session scratchpad.

**Linux, ext4 in Docker's VM, a `--cpus=2 --memory=256m` container, release build:**

| condition | shard's longest step | probes inside the landing (max) | probes before it (max) | units over 1 ms |
|---|---|---|---|---|
| the VM as it was (other sessions' load) | 159.5 ms | 124–187 ms | 1.2–7.6 ms | — (uninstrumented run) |
| quieter | 22.9 ms | 15–23 ms | 14–73 µs | barrier only: 36 of 38, max 20.8 ms |
| a buffered writer beside it | 40.0 ms | 3.5–4.4 ms | 12–120 µs | barrier only: 38, max 3.7 ms |
| a syncing writer beside it | 52.9 ms | 4.1–70.9 ms | 8–124 µs | barrier only: 34, max 48 ms |
| two CPU hogs in the cgroup | 131.1 ms | 78–242 ms | 9–45 µs | barrier: 37, total 1.36 s; one write 3.8 ms; one directory sync 2.4 ms |

On Linux the batch barrier was the only landing unit that ever passed 1 ms, outside one write and one directory sync
under the CPU hogs.

**macOS, APFS on this Mac at load 17–25 from other sessions:**
- The shard's longest step was 227 ms. Probes inside the landing waited up to 74–265 ms, against 46–168 µs before it.
- Units over 1 ms:
  - the write unit: 35 units, max 206 ms. It held the per-file `fsync`, the write-out.
  - placement (`renameatx_np`): 19 units, max 11.6 ms.
  - the barrier (`F_BARRIERFSYNC`): 5 units, max 18 ms.

## Root cause

**Durability waits ran on the shard thread.** The seam was synchronous, `LandFs::write_out` and `LandFs::barrier`, and
the engine called both inside its units:
- Linux's barrier ran its first stream of `fdatasync`s on the calling shard thread and joined the other streams there.
- macOS's write-out was `fsync`, so every written file waited for the device on the shard.

**Nothing in the slicing could bound that wait.** It is set by the filesystem's journal and the device queue, which
other processes fill:
- `fdatasync` waits for the journal commit that covers the file, and on ext4 that commit also waits for every other
  writer's ordered data in the transaction.
- APFS `fsync` waits for the device.

**Two tests were blind to it:**
- The fairness lane's assertions bounded the overrun by "one unit" and the slices by half the landing, so a 160 ms
  barrier passed.
- No test ran on a busy machine as its condition.

## Impact

- Every client of a shard that runs a granted landing waited up to the landing's slowest durability wait on a busy
  machine: 100–265 ms observed. That is three to four orders of magnitude over the shard's step quantum and over R9's
  50 µs provisioning floor.
- Nothing was lost or torn, and durability order held: the barrier still preceded every placement. The fault was
  latency and fairness only.

## Exact edits (A-132)

- **`crates/vfs/src/host/mod.rs`:**
  - `Barrier`, a batch's barrier as a job that runs anywhere.
  - `LandFs::begin_barrier` and `LandFs::end_barrier` replace `LandFs::barrier`.
  - `write_out` never waits on the device.
- **`crates/vfs/src/host/sim.rs`:**
  - The barrier is taken at its begin, the crash point, and its durability applied at its end, to the files it found
    written out and unchanged since.
  - `SimVerb::BarrierRun` refuses a barrier while it runs.
- **`crates/land/src/os.rs`:**
  - `OsBarrier` holds `dup`ed descriptors, so the host may close a file while the barrier runs.
  - macOS's `fsync` and the other unixes' `fdatasync` move from the write-out into the barrier, on the same ⌈√n⌉
    streams as Linux's, then macOS's `F_BARRIERFSYNC` per device.
- **`crates/land/src/engine.rs`:**
  - The barrier unit takes the barrier, and the run waits: `LandingRun::take_barrier`, `barrier_ran` and
    `awaiting_barrier`.
  - A later unit ends the barrier.
  - The one-call landing runs a barrier in place.
- **`crates/server/src/landing.rs`:**
  - `BarrierThread`: the barrier on a thread the landing's task owns. The thread sends the outcome over a channel of
    one, then wakes the task, and is joined when the outcome comes or on drop.
  - `run_barrier_away` keeps the lease alive while the barrier waits.
  - Two counters: `landing.barriers_away` and `landing.barrier_thread_refused`.
- **Tests:**
  - `crates/land/tests/common/mod.rs`: the sliced driver can hold each barrier away for N slices, asserting the run
    steps nothing meanwhile.
  - `oracle.rs`: `a_landing_waits_on_each_barrier_away_and_lands_exactly_as_the_one_call`, and
    `a_temporary_placed_before_its_barrier_is_recorded` with a change while the barrier runs.
  - `durability.rs`: `a_barrier_refused_while_it_runs_away_fails_its_whole_batch_and_shows_none_of_it`.
  - `crates/server/tests/landing_fairness.rs`: every barrier runs away.

## Siblings found, not fixed here (each measured as the next piece)

- **Directory syncs and the media barrier** still run on the shard, one unit per touched directory. One directory sync
  took 2.4 ms under CPU hogs.
- **Metadata calls stay on the shard:** create, rename, `fchmod`, `futimens`. Placement took up to 11.6 ms on the busy
  Mac. Seastar runs every one of these on its per-reactor syscall thread (`reactor::rename_file` and the rest via
  `_thread_pool->submit`).
- **Base-plane reads of cold host files** run on the shard for every overlay read, not only landings. Unmeasured.
- **The fleet test `a_nodes_status_reports_each_record_session_with_its_congestion_state`** reads the detector's
  lateness once, without polling, and failed once on a loaded Mac ("measured: 0"). It passed 6/6 on repeat.
