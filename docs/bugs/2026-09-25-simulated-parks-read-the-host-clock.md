# Simulated parks read the host clock

Date: 2026-09-25. Contracts: D-20 (the simulation makes no OS call), §4.3 (the online wake estimate,
A-31), CLAUDE.md §4 (Miri runs the runtime's simulation tests). Introduced by `2c76bcb`, found by CI run
36189146259 on `fdf803c`.

## Symptom

Two CI jobs failed on the push that landed the online wake estimate:

- **Instruction counts.** Callgrind died with signal 11 on `callgrind::runtime::step_idle idle:sim()`.
  The four memory benches before it had passed.
- **Miri.** `parking::tests::a_kicked_park_learns_its_kicks_stamp` panicked inside rustix
  (`backend/libc/time/syscalls.rs:140`: `Os { code: 22, kind: InvalidInput }`).

Both jobs passed on the two earlier runs (35791239154 on `628962b`, 35615970514 on `3b38d15`).

## Evidence

Reproduced in an aarch64 Linux container (valgrind 3.24.0, `iai-callgrind-runner` 0.16.1, run
unconfined so the runner may disable address randomization). Valgrind's log for the bench:

```
Process terminating with default action of signal 11 (SIGSEGV)
 Access not within mapped region at address 0xFFFFF7FFE000
   at rustix::backend::param::auxv::check_elf_base
   by rustix::backend::param::auxv::init_from_aux_iter
   by rustix::backend::param::auxv::init_auxv_impl
   by rustix::backend::vdso_wrappers::init_clock_gettime
   by slates_rt::sim::SimRuntime::run_until_idle
```

The Miri log gives the other half. Miri does not model `CLOCK_BOOTTIME`, the clock
`slates_machine::clock::monotonic_ns` reads on Linux, so the first read returns `EINVAL`.

## Root cause

`Parking::kick_if_parked` stamped the host clock on every kick of a parked shard, and
`Parking::park_unless_pending` read it three times on every park, whatever the shard. A simulated
shard discards those readings — it neither tracks an estimate nor keeps real time — but it still made
the OS call. That broke the simulation's premise, stated in the bench's own header: "all on the
simulation driver so no OS call is counted (D-20)". Under callgrind, the first such call entered
rustix's vDSO setup, which faulted on the auxiliary vector valgrind supplies. Under Miri it failed
outright.

## Fix

`Parking` carries a `timed` flag, off by default. A shard sets it once, as it is built, only when it
tracks an estimate on a real clock (`ShardContext::build`: `wake_tracking.is_some()` and a
non-simulation driver). Both halves consult it:

- A kick stamps only a timed shard's park.
- A park reads the clock only when timed and reports `Parked::Waited(None)` otherwise.

`park_unless_pending` now returns `Parked` (`Pending`, or `Waited` with the wake it learned) instead
of an `Option` whose `None` meant two different things.

## Tests

- `an_untimed_park_waits_and_reads_no_clock` (new): an untimed park waits, is kicked, and learns
  nothing. It runs under Miri, where a boot-clock read would fail, so a pass there shows no read
  happened.
- `a_kicked_park_learns_its_kicks_stamp`: now also checks that an untimed park left no stamp behind for
  a later timed one. It reads the boot clock, so it is ignored under Miri, as the OS-driver tests are.
- Results after the fix:
  - Callgrind, aarch64 container: all 14 benches pass. The runtime ones count `step_idle` 6,048,
    `spawn_and_run` 7,218 and `local_wake` 7,861 instructions.
  - Miri, Linux target: `--lib` 23 passed, 2 ignored; `--test differential` 2 passed, 2 ignored.
  - Loom: the parking model explores 27 interleavings again, the count `docs/wip/concurrency.md`
    records (the pushed commit showed 31, with the stamp's atomics in the model).
  - `slates-rt` and `slates-ipc` suites pass on macOS and Linux; `wake_estimate` 20 of 20 repeats
    on Linux.
  - Clippy clean on macOS, natively on Linux, and for Windows.

## Sibling sweep

Other host-clock or thread-account reads the online estimate added:

- `attribution::thread_account` and `voluntary_switches_now` are behind `real_time` and return `None`
  under Miri.
- `note_wake` returns before any read on a simulated shard.
- The IPC client's `monotonic_ns` reads happen only in a real client's park. The IPC crate has no
  simulation, and Miri does not run its tests.

None reads a clock on a simulated shard.

## Edits

- `crates/rt/src/parking.rs`: `timed`, `time_wakes`, `Parked`; the tests; the loom model's call.
- `crates/rt/src/shard.rs`: the build marks a timed shard; `park` matches `Parked`.
- `docs/wip/SLATES_DESIGN.md` (§4.3 status); this record.
