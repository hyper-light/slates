# The wake estimate froze at boot, and the long-step count blamed tasks for preemptions

Date: 2026-09-25. Contracts: §4.1 (derived constants), §4.3 (the loop, the failure matrix, the derived
constants), §4.7 (the wake strategy, the region), D-10 (spin-then-park), D-11 and R3, R4. Amendment:
A-31. Follows `2026-09-22-wake-probe-mixes-two-events-and-reports-an-unconverged-tail.md`, whose two
owed items this closes.

## Symptom

1. **A quick boot mean, frozen.** The wake probe converges the mean of a confirmed sleeper's wake, but
   on a virtual machine its 250 ms budget ends first. At a coefficient of variation of five to seven,
   ±5 % needs `(1.96 · cv / 0.05)²` = 38,000–75,000 wakes; the budget buys three to six thousand, so
   the boot mean is ±20–30 %. Every spin window, idle window, step quantum and slice then used that one
   sample for the life of the process, on a host whose neighbours change.
2. **Preemptions counted as bugs.** The long-step count — the bounded-work rule's bug signal (§4.3
   failure matrix) — counted a poll longer than the quantum by the wall clock. Since the quantum became
   the mean wake (A-30: 2.0–3.3 µs on Apple silicon, 10.7–21.7 µs in a Linux container), any
   preemption of a correct poll counts: a time slice handed to another runnable thread is far longer
   (Linux's EEVDF base slice is 0.75 ms, `sysctl_sched_base_slice`, from memory).

## Root cause

1. The profile's wake mean was consumed as a constant. Nothing measured a wake after boot, although
   every shard park a kick ends and every client park a reply ends is one.
2. The count had one clock. A poll's wall time is its CPU time, its time waiting inside a call, and its
   time runnable but off the CPU; only the last is the host's, and the count could not tell them apart.

## Fix

**The online estimate (`slates_machine::wake::WakeEstimate`).** An exponentially weighted mean over
about `2^shift` wakes, seeded with the boot mean, in fixed point (the accumulator is the mean shifted
left, so a deviation under `2^shift` is never rounded away). `shift` = ⌈log₂ N⌉ with
`N = (1.96 · sd / (0.05 · mean))²` from the probe's own spread (`WakeLatency::estimate_window`): the
sample size at which the estimate is as precise as the probe asks of itself. Measured: 9 on this Mac
(512 wakes); a coefficient of variation of six gives 16 (55,696 wakes).

**What a sample is.** Only the probe's event: a sleeper woken.

- *Shards* (`slates-rt`). The first sender to kick a park stamps the host clock (`Parking::kicked_at`,
  a `Relaxed` measurement word outside the protocol). The woken shard takes the stamp and times it to
  its return (`Woken`). A stamp from before the announcement is stale (a sender that saw an earlier
  park). One from the park's setup, before the wait began, found the shard awake. On Linux a wait
  across which the thread's voluntary context switches did not move never slept. Both of those are
  counted `wake_unslept`. None of the three is measured.
- *Clients* (`slates-ipc`). The daemon stamps `reply_stamp` before it wakes a parked client and sets
  the stamp's top bit when its wake call reports a sleeper found (`futex_wake`'s count;
  `os_sync_wake_by_address_any` refuses `ENOENT` when there is none). The client learns only a
  confirmed stamp inside its wait. A confirmation that lands after the woken client read the stamp is
  settled at its next wait. Windows' auto-reset Event cannot tell, so there an unconfirmed stamp inside
  the wait is taken: the stand-in the boot probe also uses there.

**Why confirmation.** Without it, `ipc_bench`'s daemon — replying the instant the parked flag rose —
taught the client 611–974 ns on Apple silicon (five runs, 2026-09-25) against the boot probe's
2.0–3.3 µs for confirmed sleepers. Such a reply lands while the client's wait is being set up, and the
wait returns without sleeping. With confirmation, 2, 18, 11, 1 and 1 of the 2,000 parked trips per run
slept (Linux: 3, 2, 1, 2, 3).

**Consumers read the estimate live.** The step quantum (`ShardContext::quantum_ns`; the I/O harvest
cadence, the long-step judgement, `slates_rt::futures::step_budget_ns`), the shard's idle spin
(estimate × `WakeTracking::idle_ratio`, the daemon's `IDLE_WINDOW_RATIO`), the destroy and archive
slices (`DaemonConfig::step_quantum_ns`, `archive_slice_bytes`), the serve loop's idle window
(`futures::wake_cost_ns`), and the client's spin and reconnect pause. The timer tick is fixed by the
wheel's construction and keeps the boot mean. `ShardPulse::wake_cost_ns` reports each shard's
estimate, and the fleet suite's diagnostic line prints it (`wake_us=`).

**Attribution (`slates_rt::attribution`).** A poll past the quantum by the wall clock is judged in a
window: from a reading of the thread's account (CPU time; on Linux, voluntary switches) to the poll's
end. The poll's CPU lies between the window's CPU less the wall time before the poll and the window's
CPU. The verdicts:

- Past the quantum at the least: the task's (`long_steps`).
- Within it at the most, having waited inside a call: the task's (`blocked_steps`, within
  `long_steps`). A voluntary switch on Linux, or the runtime's own yield to a full peer ring.
- Within it at the most, never waiting: the host's (`preempted_steps`).
- Otherwise unattributed (`unattributed_steps`): no window open yet; a window whose earlier work
  leaves the poll's share undecided; off the CPU on macOS, which counts no per-thread voluntary
  switches; anywhere on Windows, whose thread times tick at about 15.6 ms.

A window opens at every long poll's end reading. While a long poll has gone unattributed, one also
opens at each step's start and each wait's end. A wait closes it, so a park's own block is never a
poll's. A busy period that runs no long poll stops the reads, so a healthy shard reads nothing. A
simulated shard's long polls are the task's: nothing preempts a simulation.

Measured in a Linux container on this Mac (Docker, four CPUs, 2026-09-25, best of five rounds of
200,000 calls): `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` 155–161 ns, `getrusage(RUSAGE_THREAD)`
130–134 ns, `pread` of a kept `/proc/thread-self/schedstat` 217–223 ns. Schedstat's run delay was
rejected: it misses hypervisor steal, which the guest clock excludes from CPU time without recording
it as a run-queue wait, so steal would read as a block.

## Validation

- `slates-rt`: `crates/rt/tests/wake_estimate.rs`.
  - A shard kicked a millisecond after it announced its park learns every such wake, and its estimate
    leaves a one-second prior.
  - A shard with no prior learns nothing and keeps its fixed quantum.
  - A poll busy on the CPU is the task's.
  - A sleeping poll is the task's (blocked) on Linux and unattributed on macOS.
  - A poll yielding to a competitor pinned on its own CPU is the host's (Linux).
  - The attribution module's three cfg-free unit tests and the parking stamp's two.
  - 30 of 30 repeated runs of the file on macOS and on Linux (io_uring allowed).
- `slates-ipc`: `crates/ipc/tests/rings.rs`. A client parked a millisecond before each reply learns
  its wake; a client that never parked learns nothing. The hostile-slot test's layout constant follows
  the words block to 320 bytes. 20 of 20 repeated runs on Linux.
- Suites, macOS: `slates-rt`, `-machine`, `-client` and `-ipc` (all green); the server's unit (105),
  daemon (14), observe (6), recovery (6), mount and attach suites; fleet 48 of 48 (316 s); CLI 10 of
  10.
- Suites, Linux: the server's unit (105), daemon (15), observe (6), recovery (6); `slates-client`,
  `-machine`, `-rt` and `-ipc`; CLI 10 of 10.
- Clippy `-D warnings`: workspace on macOS and natively on Linux; `slates-rt` and `slates-ipc` for
  Windows. rustfmt applied. `cargo xtask check` passes. `slates-rt` uses 58 unsafe sites of its 59;
  the `getrusage` call is named in `unsafe-budget.toml`.

## Found on the way (not changed here; reported)

1. **`ipc_bench`'s parked row does not time a wake.** `ring_round_trip_parked_and_woken`, ratcheted at
   1,233 ns on this machine and described in `BENCHMARKS.md` as the cost the spin window is compared
   against, is mostly a reply racing the park's setup (the counts above). The bench now prints how
   many parked trips slept. A row that times a sleeper woken is owed: the bench's daemon must hold its
   reply until the client sleeps. The confirmed-stamp mean is that measurement.
2. **A step costs the whole ready set, not a batch.** `LocalQueue::take_ready` swaps out every ready
   slot, and `step` re-pushes each one it did not poll, so a step with R ready tasks costs O(R) while
   polling only `batch`. `observe.rs`'s full-arena test admits 82,245 busy-yielding tasks on this Mac
   (a one-shard daemon derives 41,120 client seats since the Little's-law ring, 4a09b1d), and takes
   95–97 s on the committed HEAD and on this change alike. Measured against an extracted HEAD tree with
   its own target directory; the binaries were checked for each tree's strings. The fill is quadratic.
3. **The archive walk's unit exceeds the quantum.** `SnapshotArchiver::advance` does at least one unit
   (a piece of a file) per slice. With the quantum at the mean wake, the derived slice is smaller than
   a unit: 136 bytes on a debug build here, where BLAKE3 ran at 117 MB/s. Every archive slice therefore
   overshoots the quantum, and the attributed count will report it as the task's — a true signal. The
   unit should be sized by the slice (BLAKE3 hashes incrementally).
4. **An idle three-pod KIND fleet at about 35 % of a CPU per pod.** Still unexplained; carried from the
   2026-09-22 record.

## Edits

- `crates/machine/src/wake.rs` (`WakeEstimate`, `estimate_window`, `estimate_shift`, `sd_ns`),
  `stats.rs` (the standard deviation).
- `crates/rt/src/runtime.rs` (`WakeTracking`), `parking.rs` (the kick stamp, `Woken`), `shard.rs`
  (the estimate, the live quantum and spin, `note_wake`, the attribution tracker, the counters),
  `attribution.rs` (new), `registry.rs` (the pulse's wake cost), `futures.rs` (`step_budget_ns`,
  `wake_cost_ns`), `task.rs`, `lib.rs`; `crates/rt/tests/wake_estimate.rs` (new).
- `crates/ipc/src/region.rs` (layout version 2), `endpoint.rs` (the stamp, its confirmation, the
  client's estimate), `wake.rs` (a wake reports a sleeper found), `rendezvous.rs`, `lib.rs`;
  `crates/ipc/tests/rings.rs`, `rendezvous.rs`; `crates/ipc/examples/ipc_bench.rs`.
- `crates/server/src/config.rs` (the tracked idle ratio, `spin_shift`, `step_quantum_ns`,
  `archive_slice_bytes`), `daemon.rs` (the live idle window, `ShardPulse::wake_cost_ns`),
  `verbs.rs` (destroy slices), `fleet.rs` (archive slices); `crates/server/tests/fleet.rs`
  (diagnostics); `crates/client/src/client.rs`.
- `wake_tracking: None` in every hand-written `RuntimeConfig` (tests, benches, the DNS and NFS helpers,
  the transport's endpoint).
- `unsafe-budget.toml`; `docs/wip/SLATES_DESIGN.md` (§4.1, §4.3, §4.7, A-31), `GAPS.md`,
  `TBD_FIXES.md`, `BENCHMARKS.md`.
