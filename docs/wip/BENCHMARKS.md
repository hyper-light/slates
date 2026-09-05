# Benchmarks

Every number here was measured, on a named machine, by a named command, with the interval the
measurement reported. A number without those three is not a benchmark and does not belong here.
Format: environment line, the command, the reading with its interval, and what it means for the
design (Part 6 of `SLATES_DESIGN.md`).

## Phase 0 baseline: the machine profile (2026-09-04)

Environment: Apple M5 Max (6 "Super" cores with 16 MiB L2, 12 "Performance" cores with 8 MiB L2),
128 GiB, macOS 26.4.1 (25E253), Rust 1.98.0, release profile, mains power.
Command: `cargo run --release -p slates-machine --example profile` (each probe bounded by the
ratified 250 ms wall budget; intervals are 95% bootstrap intervals around the median).

| Measurement | Reading | Interval / notes |
|---|---|---|
| Base page | 16,384 B | `hw.pagesize`; no huge pages on macOS |
| Cache line | 128 B | `hw.cachelinesize` |
| Timer overhead | 23 ns per `Instant::now()` | |
| Null syscall (`getppid`) | 164 ns | [164, 166] |
| Anonymous page fault | 845–901 ns per 16 KiB page | region of 256 pages: [230,083, 252,666] ns; map+unmap baseline 750 ns subtracted; two runs |
| Park/unpark wake | p50 2.0 µs, p99 4.6 µs | p50 interval [1,750, 1,792] on the first run; 64 samples |
| Core-to-core ring round trip | min 132, median 153, max 190 ns | 153 pairs, all 18 cores; affinity hint refused by macOS on Apple silicon, so the pairs are scheduler-placed |
| memcpy | 128 GB/s at 128 B–128 KiB; 94 GB/s at 1 MiB; 72 GB/s at 64 MiB; 36 GB/s at 128 MiB | the 64 and 128 MiB points stopped at the wall budget (quick) |
| BLAKE3 | 2.54 GB/s over 16 MiB | single thread |
| LZ4 (lz4_flex) | 1.74 GB/s compress, 7.6 GB/s decompress | 1 MiB corpus, half source-like half random; ratio 0.727 |
| zstd 1 | 1.32 GB/s, 4.2 GB/s | ratio 0.634 |
| zstd 3 | 1.04 GB/s, 4.5 GB/s | ratio 0.625 |
| zstd 9 | 198 MB/s, 4.4 GB/s | ratio 0.623; quick |
| zstd 19 | 14 MB/s, 5.1 GB/s | ratio 0.606; quick |
| Lock capacity | 116,823,110,451 B | `vm.user_wire_limit`; one-page confirming `mlock` succeeded |
| Whole profile | 472 ms | quick probes: memcpy (largest two points), codecs (zstd 9 and 19) |

Derived constants this profile yields (formula and anchors are printed by the example):

| Constant | Value | Formula |
|---|---|---|
| spin_before_park_ns | 2,042 | wake.p50 (2-competitive spin bound) |
| arena_region_bytes | 1 MiB | pages = 100 × 2 × syscall / fault, × page, rounded up to a power of two |
| timer_tick_ns | 2,300 | max(wake.p50, 100 × timer overhead) |
| task_step_budget_ns | 4,584 | wake.p99 |
| copy_versus_remap_bytes | never (u64::MAX) | no measured copy size costs more than faulting its pages on this machine |
| ring_entries | 32 | wake.p99 / syscall, rounded up to a power of two |

What it means: on this machine a fault costs five syscalls and a wake costs twelve, so the
runtime's spin window and the arena's region size come out where §4.2 and §4.3 expected; copying
beats remapping at every size the curve covers, which settles the copy-versus-remap question for
16 KiB pages (Linux 4 KiB pages will be measured on the reference Linux box in Phase 1). The
codec table is the first input to the cost model of §4.11; zstd 9 and 19 are an order of
magnitude too slow for a boot-time probe over the large chunk class, which is why the codec
corpus is the small class (64 pages) and Phase 7 measures the large class itself.

## Phase 0 baseline: memory (2026-09-04)

Environment: as above (Apple M5 Max, macOS 26.4.1, Rust 1.98.0, release profile).
Command: `cargo run --release -p slates-mem --example bench` (500 ms budget per row; 95%
bootstrap intervals; the harness batches sub-microsecond operations, batch shown).

| Operation | Median | Interval | p99 | Batch |
|---|---|---|---|---|
| Slab insert+remove, 64-byte slot (free-list pop, generation bump, push) | 12 ns | [12, 12] | 13 ns | 256 |
| Buddy alloc+free, one 16 KiB page (no split) | 62 ns | [62, 62] | 63 ns | 64 |
| Buddy alloc+free, 64 pages beside a held page (split 6 levels, coalesce back) | 71 ns | [70, 71] | 75 ns | 64 |
| SPSC ring round trip between two threads (push, handoff, echo, pop) | 348 ns | [343, 349] | 354 ns | 8 |

Also proven, not timed: the zero-allocation test (`crates/mem/tests/no_alloc.rs`) counts system
allocator calls across eight rounds of 1,024 slab inserts and removes and 12 buddy allocations
and frees and finds none; loom explores every interleaving of the SPSC ring (one producer, one
consumer) and the MPSC ring (two producers, one consumer) and finds no lost or reordered word.

What it means: a slab operation costs a tenth of a syscall and a buddy operation a third; the
ring round trip is about two and a half core-to-core cache-line transfers on this machine
(the profile's ring matrix measured 132–190 ns per pair), which is the cost floor for a
cross-shard wake before the kick syscall (§4.3; the runtime baseline adds the kick).

## Phase 0 baseline: runtime (2026-09-04)

Environment: as above (Apple M5 Max, macOS 26.4.1, Rust 1.98.0, release profile); the kqueue
driver; one shard on the calling thread unless stated. Command:
`cargo run --release -p slates-rt --example bench` (500 ms budget per row; 95% bootstrap intervals).

| Operation | Median | Interval | p99 |
|---|---|---|---|
| One loop step with nothing to do (drain rings, expire timers, no task) | 35 ns | [35, 35] | 35 ns |
| Admission only (spawn a trivial task, cancel, detach) | 24 ns | [24, 25] | 43 ns |
| Spawn a trivial task and run it to completion | 179 ns | [177, 184] | 187 ns |
| One local wake (a task yields once and resumes) | 281 ns | [281, 281] | 302 ns |
| A zero-timeout `kevent` (the driver's poll) | 15.1 µs | [14.4, 15.7] | 16.3 µs |
| Timer lateness after a one-tick (100 µs) sleep, 200 runs | p50 99.8 µs | | p99 102.8 µs, max 107 µs |
| Foreign spawn onto a shard thread and a reply over a channel (two thread hops) | 8.7 µs | [8.6, 9.3] | 12.6 µs (5.0 µs on another run) |

Also proven, not timed: the same task program (children with yields, a sleep, joins in a fixed
order, a cancel) produces an identical trace on the kqueue driver and the simulation driver
(`crates/rt/tests/differential.rs`, AC-0.6 and AC-0.9); a lost driver cancels every task with a
terminal completion and the shard exits; a finishing parent cancels and joins its children; two
OS shards wake each other through the pair rings and the kick (`tests/cross_shard.rs`); 10,000
timers with random deadlines fire in order within one tick.

Idle spin (2026-09-05, the same command): with a client active a shard spins for the profile's
window (the measured wake p50, 1.3 µs here) before parking.

| Operation | Median | Interval | p99 |
|---|---|---|---|
| Cross-shard wake round trip, two tasks waking each other through the pair rings, both shards parking | 6.25 µs | [6.25, 6.25] | 8.2 µs |
| The same with both shards spinning (514 hits, 4 misses in 2,000 rounds) | 500 ns | [500, 541] | 708 ns |
| Foreign spawn and channel reply with the shard spinning (the bench thread's own hop exceeds the window: 0 hits) | 6.1 µs | [5.8, 6.4] | 7.6 µs |

What it means: the shard loop's fixed cost is below a cache miss and a task's whole life is under
two hundred nanoseconds, so the fifty-microsecond provisioning budget of §1.1 is spent elsewhere
(the IPC and the bridge). A cross-shard wake is one cache-line handoff plus the kick, as §4.3's
worked example says, and the spin removes the kick: twelve times faster between active shards.
Two measured OS facts shape Phase 1: a `kevent` poll costs fifteen microseconds on this macOS, so
the idle loop never polls the driver when it knows nothing is pending (the `has_pending` seam),
and a kernel timeout wake lands about one wheel tick late, which the spin absorbs for deadlines
inside the window. The spin is worth nothing to a peer slower than the window (the channel row),
which is why it is gated on a client being active rather than always on.

## Phase 0 baseline: wire (2026-09-04)

Environment: as above (Apple M5 Max, macOS 26.4.1, Rust 1.98.0, release profile). Command:
`cargo run --release -p slates-wire --example bench` (400 ms budget per row; 95% bootstrap
intervals; inputs made opaque to the optimizer).

| Operation | Median | Interval | p99 |
|---|---|---|---|
| Header encode (32 bytes) | 2 ns | [2, 2] | 2 ns |
| Header decode (magic, major, class checks) | 2 ns | [2, 2] | 2 ns |
| Body encode, 73-byte sample (a string, a 16-byte id, a u64, four u32s, an optional string) into a reused buffer | 8 ns | [8, 8] | 8 ns |
| Body decode of the same sample (two string allocations, one vector) | 96 ns | [95, 97] | 104 ns |
| Frame encode (header, schema word, body, CRC32C; two allocations) | 265 ns | [247, 273] | 494 ns |
| Frame decode (cap, kind, CRC32C, schema checks; body bytes copied) | 38 ns | [37, 39] | 41 ns |
| CRC32C over 1 MiB, hardware `crc32cx` | 172 µs | | 6.1 GB/s |

Also proven, not timed: the golden vector of the sample message and the recorded reflections
(`crates/wire/tests/golden.rs`) freeze the encoding for the major; every hostile shape (length
`u32::MAX`, truncated header, truncated body, a bit flip, an unknown kind, a foreign schema, an
oversized body at encode time) is a typed refusal that allocates nothing (AC-0.8, T-0.8); the
derive refuses `usize`, tuple structs and generics at compile time with the reason spelled out
(trybuild, `tests/ui`); the CRC32C check value matches RFC 3720 and the hardware path matches the
table path on ten thousand bytes.

What it means: a control frame costs a third of a microsecond to build and forty nanoseconds to
check, against a fifty-microsecond provisioning budget; the encode side's two allocations are the
obvious Phase 1 trim (encode into the ring's slot, as §4.7 has it). CRC32C at 6 GB/s is the
single-chain rate of the instruction; a three-way interleave would raise it and Phase 7 measures
whether the bulk path needs it, though bulk carries the BLAKE3 identity instead (§4.9).

## Ratchets (2026-09-05)

`ratchets.toml` holds the ceilings for this machine (identity `4c62b34d5f545407`, the Apple M5
Max above): 22 rows, each the highest upper interval edge across three runs of its bench
example. `cargo xtask ratchet` runs each example three more times and fails when a row's lowest
lower edge lies above its ceiling, so a regression must clear every recorded run to count; the
planted ceiling of 1 ns on the slab row was reported as a regression before the file was
restored. Ceilings only tighten (`--tighten`); `--reset` rebuilds the entry and is a deliberate
act; a raised ceiling is an edit with a reason in the file.

Between-run drift on this laptop, from the record run (the medians of the three runs):

| Row | Run medians | Drift |
|---|---|---|
| Slab insert+remove | 13, 13, 13 ns | 0 |
| Buddy alloc+free, one page | 56, 56, 56 ns | 0 |
| Ring round trip, two threads | 354, 349, 349 ns | 1.4% |
| Cross-shard wake, both parking | 6292, 6250, 6250 ns | 0.7% |
| Cross-shard wake, both spinning | 500, 459, 584 ns | 27% |
| One local wake | 208, 250, 177 ns | 41% |
| Spawn and run a trivial task | 132, 156, 112 ns | 39% |
| CRC32C over 1 MiB | 152, 137, 124 µs | 23% |
| Header encode | 2, 2, 1 ns | one step (nanosecond quantization) |

What it means: the wall clock on a laptop resolves a regression of a few percent on the
microsecond rows and only a large one on the nanosecond rows, because the machine itself moves
that much between processes. The design's instruction-count gate (D-20) is what sees the small
change; it waits on valgrind (GAPS §8a).
