# Shards counted hardware threads as cores (2026-11-05, A-208)

## Description

§4.1 derives the shard count as "performance-class physical cores minus one control core (minimum one)". The
placement (`crates/machine/src/placement.rs`, `Placement::of`) counted the fastest class's *logical* CPUs. The machine
facts (`CoreFacts`) had one entry per logical CPU and nothing that said which of them share a physical core.

On jupiter (AMD Ryzen AI MAX+ 395: 16 cores, 2 hardware threads each, 32 logical CPUs, Windows 11) the daemon derived
31 shards. `provision_bench` printed "8 clients oversubscribes 1 runnable core(s)", because it leaves the client
threads only what the shards don't take.

## Measurement

`target/release/examples/provision_bench.exe` at `76b450a1`, jupiter, 2026-11-05 06:35 UTC, the processor 0.7 % busy
before the run:

| row | p50 | p99 (median of 3; all 3) |
|---|---|---|
| spinning, 1 client | 21.1 µs | 123.6 µs [5,082.6, 123.6, 123.1] (R9's floor is 50 µs: FAIL) |
| spinning, 8 clients | 21.3 µs | 530.6 µs |
| parked, 1 client | 646 µs | 12.06 ms |

adacachy (AMD Ryzen 9 9950X3D2, 16 cores × 2 threads, Linux) has the same shape: `thread_siblings_list` of cpu0 is
`0,16`. It derives 31 shards too.

## Root cause

Two spinning shards on the two threads of one core share that core's pipeline. With 31 shards on 32 threads, most
cores carry two spinning shards, and every client thread that spins for its reply (R9's spinning form) shares the one
remaining thread with the operating system. The rule in §4.1 is right; the code counted the wrong thing, because the
facts could not name the right thing.

## Impact

Every Linux and Windows machine with simultaneous multithreading, which is most x86_64 servers and desktops: about twice
the shards the design asks for, each pair of shards contending for one core, and no thread left for the clients. Apple
silicon (one thread per core) is unaffected.

## Edits

- `crates/machine/src/facts.rs`: `CoreFacts::physical`, the lowest OS id among the hardware threads of the CPU's
  physical core. Linux reads `topology/thread_siblings_list`; Windows reads the `RelationProcessorCore` entry of
  `GetLogicalProcessorInformation` that holds the processor; macOS names each CPU itself. A CPU with no topology is
  counted as its own core, with a note.
- `crates/machine/src/placement.rs`: `Placement::of` reduces the fastest class to one logical CPU per physical core,
  the lowest id the process may run on, before it counts and fixes shards. The second threads run no shard.
- `crates/machine/src/profile.rs`: `PROFILE_VERSION` 4, so a profile cached by an older build is refused by name and
  measured again (§4.1's rule for a format change).
- Tests: `hardware_threads_of_one_core_count_as_one_core_and_only_its_first_runs_a_shard` (16 cores × 2 threads in
  Windows' and Linux's numbering: 15 shards, each on a core's first thread). It failed before the fix (`left: 31,
  right: 15`). The placement oracle now draws three layouts (one thread a core, adjacent siblings, siblings numbered
  after every first thread) and checks that no two shards share a physical core, that no shard shares control's
  core, and that every fixed CPU is its core's first. With the old counting, both tests fail.

## Siblings found

- `crates/machine/src/wake.rs` places the boot wake probe by the same placement, so it now measures the control core
  waking a shard on another physical core, which is what production does.
- The Windows facts still record no core class (`CoreClass::Unknown` for every processor), so a hybrid Windows machine
  (performance and efficiency cores) still counts every core as the fastest class. That is a separate gap and not
  changed here.
