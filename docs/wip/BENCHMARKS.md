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
Command: `cargo run --release -p slates-mem --example mem_bench` (500 ms budget per row; 95%
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

**Validated frees (2026-09-30, AUD-29-10).** A free now names its allocation exactly (offset on its size's
boundary, exact length, incarnation, arena); the granule is a power of two, so every conversion is a shift.
Instruction counts, `cargo bench -p slates-mem --bench callgrind --features slates-mem/instruction-counts`
in a Linux container (aarch64, valgrind 3.24, `--security-opt seccomp=unconfined` for `setarch`), each tree
in its own target directory:

| Operation | Before (`5922adf`) | After | Change |
|---|---|---|---|
| Buddy alloc+free, one page | 1,295 | 1,218 | −77 (−5.9%) |
| Buddy alloc+free, 64 pages split and coalesced | 1,507 | 1,479 | −28 (−1.9%) |

Measured and rejected, same day:
- **The bench as it stood.** It counted the allocator's destruction inside each alloc+free: freeing its
  tables was 763 of 2,077 instructions, and one added table read as a 142-instruction regression of
  operations that had become 82 cheaper (`callgrind_annotate` per function). The two buddy benches now
  borrow the allocator.
- **`#[inline]` on the allocator's checked helpers.** No better on the wall clock (one page 38–39 ns
  against 32–34), so it is not kept.
- **The wall clock.** At load average 11 it could not resolve the few-nanosecond question: the same
  binary's one-page row ranged 31–44 ns between rounds. The instruction count decided it.

**Typed shared-memory access (2026-09-30, AUD-29-09).** One push and one pop of a 64-slot client ring,
single-threaded, instruction counts under callgrind in a Linux container (a probe example run for the
measurement, not kept), each tree in its own target directory:

| Build | Instructions per push + pop |
|---|---|
| Before (whole-map byte slices, unchecked) | 307 |
| First cut: every word and copy checked by searching the layout — **rejected** | 1,186 |
| Final: words and slot bodies resolved once, reached by constant-time validated arithmetic | 450 |

The rejected cut spent ~880 instructions in `Layout::holds`/`touches` searches and ~50 building and
dropping an unused refusal per pop. Wall clock, `cargo run --release -p slates-ipc --example ipc_bench`,
best of six at load 11: spinning round trip 212 ns before, 221 ns after (within the between-run noise,
which ranged 212–441 ns for the unchanged binary); parked round trip 1,043 ns before, 934 ns after. A
power-of-two fast path for strided-run arithmetic was also tried and measured no better; it is kept
only because it is exact and cheaper in instructions.

**Single-owner ring halves (2026-09-30, AUD-29-33).** `ring_push_pop` (a split, one push, one pop), the
commit before and after, with the ring borrowed by the measured closure in both trees (moving the 384-byte
ring into it cost a 54-instruction copy, and its teardown dominated the row: 281 before and 366 after as
first measured): 54 → 63 instructions. The nine are the split's one-time claim, a 5-instruction swap, and
its check; push and pop are unchanged. The runtime now splits each pair ring once at start instead of on
every send and drain. Its rows moved by 1–4 instructions (`step_idle` 5,631, `spawn_and_run` 7,289,
`local_wake` 7,943).

**Owner-bound runtime lends (2026-09-30, AUD-29-08).** Instruction counts under callgrind in a Linux
container (`cargo bench -p slates-rt --bench callgrind --features slates-rt/instruction-counts`), the
commit before and after, each tree in its own target directory:

| Row | Before | After |
|---|---|---|
| `step_idle` (bare simulated steps to idle) | 5,619 | 5,630 |
| `spawn_and_run` | 7,273 | 7,287 |
| `local_wake` | 7,922 | 7,939 |
| `kept_lookup` (new: one validated lookup of a kept value) | — | 80 |

The step rows pay the restore of the thread's current shard at each step's end (the old step left it
published, which was the unsoundness). Skipping the writes when the shard is already current was tried and
**rejected**: it cost bare steps 13 more instructions (`step_idle` 5,643) for a saving on the worker's `run`
that no row measures. `kept_lookup` is what a demultiplexer pays per routed datagram to reach itself; it
replaced a thread-local table lookup of a `&'static` of about the same shape (TLS, `RefCell` borrow, index)
and adds the registration and type checks.

What it means: a slab operation costs a tenth of a syscall and a buddy operation a third; the
ring round trip is about two and a half core-to-core cache-line transfers on this machine
(the profile's ring matrix measured 132–190 ns per pair), which is the cost floor for a
cross-shard wake before the kick syscall (§4.3; the runtime baseline adds the kick).

## Phase 0 baseline: runtime (2026-09-04)

Environment: as above (Apple M5 Max, macOS 26.4.1, Rust 1.98.0, release profile); the kqueue
driver; one shard on the calling thread unless stated. Command:
`cargo run --release -p slates-rt --example rt_bench` (500 ms budget per row; 95% bootstrap intervals).

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
`cargo run --release -p slates-wire --example wire_bench` (400 ms budget per row; 95% bootstrap
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

## Phase 1 baseline: the volume core (2026-09-05)

Environment: as above (Apple M5 Max, macOS 26.4.1, Rust 1.98.0, release profile, mains power,
thread not pinned: macOS refuses affinity on Apple silicon). Command:
`cargo run --release -p slates-vfs --example vfs_bench` (500 ms budget per row; 95% bootstrap
intervals; heap measured through a counting global allocator in the bench binary; the volume's
clock is the host clock). Trees have the shape of a `cargo build` output tree measured on this
workspace: 36 files per directory (40,052 files in 1,117 directories under `target/debug`) and
49-byte names (the mean over 42,155 files), in 64 groups of leaf directories. The op log
budget is 64 KiB so it is full at every size and each mutation pays the same eviction.

| Operation | 10^3 files | 10^5 files | 10^6 files | Interval / notes |
|---|---|---|---|---|
| Lookup one name in a 36-entry directory (fold policy) | 171 ns | 166 ns | | [171, 177]; [166, 166] |
| Resolve a three-component path | 312 ns | 333 ns | | [312, 312]; [323, 333] |
| Readdir of a 36-entry directory (rows borrow the names) | 406 ns | 406 ns | | [406, 427]; [396, 406] |
| Create a file and unlink it | 1,583 ns | 1,500 ns | | [1,542, 1,666]; [1,500, 1,542]; includes the op-log record with its path |
| Rename within a directory, there and back (two renames) | 2,750 ns | 2,667 ns | | [2,709, 2,791]; [2,666, 2,708] |
| Write 4 KiB in place, same epoch | 395 ns | | | [395, 395] |
| Read 4 KiB | 24 ns | | | [24, 24] |
| Write 4 KiB into a fresh chunk window, then truncate it away | 666 ns | | | [646, 666]; one page from the buddy and back |
| Snapshot and destroy the snapshot | 45 ns | 44 ns | 45 ns | [45, 46]; [42, 44]; [44, 46]; AC-1.3: growth 0 ns against the 11 ns timer resolution |
| Clone and destroy the untouched clone | 265 ns | 260 ns | 244 ns | [265, 270]; [255, 260]; the destroy walk is pruned at the origin epoch |
| Heap per file (slabs, blocks, names, trie, op log) | 697 B | 472 B | 468 B | AC-1.5 budgets from the counted-object formula: 921, 565, 561 B |
| Build the tree | 1 ms | 107 ms | 1,038 ms | 1.04 µs per create at 10^6 |
| Destroy, per release unit | | | 14 ns in one slice; 15 ns under 6 µs slices | 1,152,317 units; the slices' clock read every sixteen units costs about 1.5 ns per unit (re-measured 2026-09-05 after the base plane and the 64-bit timestamps; 12 ns before the time-budgeted slices) |
| Destroy slice under the shard's step budget (6,958 ns from the profile) | | | p50 7,083 ns, p99 8,167 ns, longest 24,042 ns | AC-1.8: 0 slices past budget + 16-unit clock-check allowance (6,000 ns) + measured scheduling jitter (16,500 ns); 0 ns of the longest inside `dealloc` |
| Create burst of 190,000 files (T-1.7), best of 3 | | 1,050 ns per file | | all runs 1,050, 1,053, 1,088 ns; 469 heap bytes per file |

The directory representation probe, from the same command (inline array against the block
tree, both at the sizes shown; the cut-over is recorded as `dir::MEASURED_CUTOVER = 2`):

| Directory | Lookup | Insert and remove |
|---|---|---|
| Inline, 2 entries | 91 ns [88, 91] | 80 ns [78, 85] |
| Tree, 2 entries | 145 ns [145, 151] | 78 ns [78, 80] |
| Tree, 4 entries | 156 ns [156, 156] | 75 ns [75, 80] |
| Tree, 32 entries | 182 ns [177, 187] | 78 ns [78, 80] |
| Tree, 128 entries (two blocks) | 203 ns [203, 208] | 140 ns [140, 145] |

Measured and rejected on the way, same machine and day, each replaced in the same change:

| Experiment | Reading | Why it lost |
|---|---|---|
| Name folding by collecting an NFC string per comparison | lookup 1,208–1,292 ns at every directory size | the fold dominated; the allocation-free fold with an ASCII fast path gives 104 ns on the same rows |
| `BTreeMap<(hash, Box<str>), Entry>` for indexed directories | 552 B per file; every name stored twice | one copy of each name and a hash-keyed map: 465 B |
| One `Box<str>` per name, map nodes from the global allocator | destroying 10^6 files: two slices of 2.4 and 2.7 ms, 2,668,955 of 2,698,667 ns inside `dealloc` | the allocator returning pages; blocks in a slab never return per item: longest slice 24 µs with 0 ns in `dealloc` |
| One `Vec<u8>` name buffer per directory | 516 B per file (growth slack), create 2.2 µs | the block holds names and entries together and the small form is inline: 468 B, 1.04 µs |
| Path building by scanning the parent for the child's handle | create 2,209 ns in a tree whose group directories hold 434 entries | the node carries its own name: 1,417 ns, and the parent re-pointing after a copy is one keyed update |
| Snapshot removal by scanning every snapshot slot for `previous` links | snapshot and destroy 49–72 ns, rising with the slab's slot count | doubly linked records: 45 ns, flat across sizes |
| Destroy slices counted in objects, calibrated on the first percent of the queue | slices of 199 objects; p99 121 µs because directory releases cost 30× a trie node | slices cut by the volume's clock against the step budget, weighted units: p99 8.2 µs |

What it means: a directory operation costs a fold and a probe, not a search; a snapshot is
forty-five nanoseconds at a million files; the memory per file is within a formula that names
every object; and a volume of a million files is destroyed in slices the shard can schedule
between requests, with the allocator out of the picture. The remaining cost in the create path
is the op-log record and its path string, which §4.16's deriver will read; the readdir rows
already borrow their names.

## Phase 1 baseline: the base plane (2026-09-05)

Environment: as above (Apple M5 Max, macOS 26.4.1 on APFS, Rust 1.98.0, release profile).
Command: `cargo run --release -p slates-base --example base_bench` (500 ms budget per row; 95%
bootstrap intervals). The directory under test is this workspace's own `target/debug/deps`
(44,602 entries after `cargo test --workspace`), read only; the copy-up rows run an overlay
volume over `target/debug` (a dozen entries) so a sample pays the copy-up, not a listing.

| Operation | Median | Interval | Notes |
|---|---|---|---|
| List a 44,602-entry directory, per entry (`getdents` plus one `statat` per entry) | 3,337 ns | [3,332, 3,809] | informational (the directory's size follows the build); a whole listing is about 150 ms and is cached until the directory's fingerprint moves |
| Fingerprint a directory (`fstat` of its descriptor) | 218 ns | [213, 218] | paid once per lookup or read of an untouched entry |
| Open and `fstat` a file, then close (the drift check) | 7,250 ns | [6,958, 7,583] | |
| `pread` up to 4 KiB | 260 ns | [260, 260] | |
| Copy up a small-class file (up to 4 KiB) on its first write | 30,542 ns | [29,834, 31,416] | volume create, the listing, open, `fstat`, read, BLAKE3, witness, the write |
| Copy up a large-class file, one window, on its first write | 6,768,875 ns | [6,751,875, 6,793,916] | informational; the file is 5,572,600 bytes and the witness hashes it whole, so the row is the hash and the read of the file, proportional to its size (D-6's tripwire) |

Measured on the way (2026-09-05): the 128-bit timestamps the inode and the fingerprint
carried cost 32 bytes per inode; as 64-bit nanoseconds (good to the year 2262, saturating) the
inode is 264 bytes and the heap per file at 10^6 files fell from 468 to 418 bytes even after
the deriver's 24-byte home was added. The destroy gate (AC-1.8) now excludes slices during
which the process's involuntary context-switch count moved (`getrusage`): in two runs the
scheduler preempted two and three slices of about 25 µs each, and every other slice sat within
the budget plus the clock-check allowance (longest 8.8 µs and 6.5 µs against budgets of 6.7 µs
and 2.5 µs).

What it means: an overlay volume pays a directory `fstat` per untouched lookup, a listing per
directory it enters (once, until the directory changes), about thirty microseconds to copy a
small file up, and a hash of the whole file for a large one. The per-entry `statat` is the
listing's cost on macOS; `getattrlistbulk` is the bulk call the design names for this platform
and its gain is an owed measurement (GAPS §8c).

## Phase 1 baseline: the landing (2026-09-05)

Environment: as above (Apple M5 Max, macOS 26.4.1, Rust 1.98.0, release profile). Command:
`cargo run --release -p slates-land --example land_bench` (best of 5 runs, all shown; the row is
the median with the lowest and highest run as its edges). The simulated rows run the engine over
`SimHost` (an in-memory disk), so they measure the engine's own work per entry: plan, the
verdict pass, the temporary, the exchange, the verify, the syncs, the advance. The OS rows
(T-1.17: a 10k-entry delta into a 10^6-entry tree by the OS writer against `cp -r` of the same
delta, with the ramp's settled depth) run on Unix in the build output beside the example's
binary, on the host's disk (A-50: a landing writes the host's disk; never `/tmp` and never a RAM
directory). Until 2026-09-30 they ran only on the Linux lane's `/dev/shm`, so the numbers recorded
below are tmpfs numbers; a disk row replaces them at the next recorded run.

| Operation | Median | Runs | Notes |
|---|---|---|---|
| Plan, per diverged entry (1,000 replacements over a 100,000-entry base) | 594 ns | [579, 579, 594, 615, 656] | the diverged walk, the bytes read and hashed for the identity, the canonical encoding |
| Land, per entry, engine only (the same delta; verdict pass, temporary, exchange, verify, syncs, advance) | 5,633 ns | [5,482, 5,531, 5,633, 5,658, 5,772] | every seam call is an in-memory map operation here; the disk's share arrives with the OS rows |
| Re-plan after the landing (nothing diverged) | 1,583 ns | [1,167, 1,250, 1,583, 1,750, 1,875] | the walk over the loaded nodes finds nothing; the idempotent re-run's cost |

Measured on the way (2026-09-05): the first run of the land row read 813 µs per entry, and the
second 166 µs, both the simulated host's cost, not the engine's: its `fstat` looked an open
descriptor's inode up by walking the whole 100,000-node tree, first for every verify (the
displaced file sits under its hidden name after the exchange) and then for every read. The
descriptor now remembers where it was opened and looks there first, then among that
directory's siblings, then walks; the row fell to 5.6 µs. A simulated host on the oracle's leg
must not be the slow leg, or the bench measures it.

The workspace's own tree served as the proportionality check (AC-1.14, in the oracle): the
worked example's sixteen entries take the same number of seam calls over a 1,000-entry base
and a 100,000-entry one.

Re-measured 2026-09-29 for A-43 (AUD-29-04). Every removal now moves the entry to a checked aside name,
and a replacement's temporary takes a path-hashed aside name. The command is the same. The old engine
(`9b98390`'s `engine.rs` in a worktree with the new seam) and the new one ran in three interleaved rounds
on this machine at load average 8–10 (other sessions' KIND clusters). Land per entry, each the median of
five runs:

| Engine | Round 1 | Round 2 | Round 3 | Run range |
|---|---|---|---|---|
| Old | 8,678 ns | 8,956 ns | 8,566 ns | 8,083–9,274 ns |
| New | 9,029 ns | 8,836 ns | 8,877 ns | 8,192–9,285 ns |

The per-round difference, −120 to +351 ns, is inside that spread: no regression is claimed, and none
below it is excluded. The absolute numbers sit above the 2026-09-05 row because of the load. The bench's
delta is replacements only; the removal kinds' costs are recorded as seam calls per landing in
`docs/bugs/2026-09-29-a-landing-removal-could-remove-an-outsiders-replacement.md`.

## Phase 2 baseline: the database (2026-09-05)

Environment: as above (Apple M5 Max, macOS 26.4.1, Rust 1.98.0, release profile). Command:
`cargo run --release -p slates-db --example db_bench` (best of 5 runs, all shown; the row is
the median with the lowest and highest run as its edges). The segment is a 128 MiB `shm_open`
object; the recovery row builds 10,000 volumes and 1,000,000 accounting records (69.5 MB of
log) with snapshots off, drops the database, and recovers it from the log alone.

| Operation | Median | Runs | Notes |
|---|---|---|---|
| Adaptive radix tree insert, per key, 10^5 sixteen-byte keys | 53 ns | [47, 50, 53, 59, 70] | one node allocation per key; the prefix compare |
| Adaptive radix tree lookup, per key, 10^5 keys | 18 ns | [17, 18, 18, 18, 23] | one node per key byte at most |
| One mutation (guard, encode, append to the ring, apply) | 206 ns | [206, 206, 206, 208, 217] | an accounting record; the checksum is the CRC32C of §4.9 |
| Recovery of 10^4 volumes from 10^6 records (AC-2.7) | 96 ms | [76, 86, 96, 97, 104] ms | the 1 s budget of §4.8; 720 bytes per µs replayed, so the derived cadence snapshots every 720 MB of log |
| Replay, per record | 95 ns | [75, 84, 95, 95, 102] | verify (magic, length, sequence, schema, CRC32C), decode, apply |

What it means: at 206 ns a mutation is under a percent of the 50 µs provisioning budget; a
partition of 10^4 volumes recovers in a tenth of the budget from a million records, and the
snapshot cadence the measurement derives keeps any log tail inside that budget.

## Phase 2 baseline: IPC (2026-09-05)

Environment: as above (Apple M5 Max, macOS 26.4.1, Rust 1.98.0, release profile). Command:
`cargo run --release -p slates-ipc --example ipc_bench` (best of 5 runs, all shown; the row is
the median with the lowest and highest run as its edges). Two threads over two mappings of
one client region (an `shm_open` object): the client end and the daemon end; the OS places the
threads (macOS refuses pinning), so a cross-cluster placement widens the spread, as the
runtime's ring rows already record.

| Operation | Median | Runs | Notes |
|---|---|---|---|
| One ring round trip, both ends spinning | 278 ns | [241, 253, 278, 302, 708] | a request slot and a reply slot: two cache-line transfers and the per-slot sequence stores; the 708 ns run is a cross-cluster placement |
| One ring round trip, the client parked and woken | 1,051 ns | [924, 1,006, 1,051, 1,122, 1,154] | `os_sync_wait_on_address(SHARED)` and the wake: the cost the spin window is measured against (the profile's wake p99 is the published window) |

What it means: the ring's floor is under 0.3 µs of the 50 µs provisioning budget, and a park
costs about a microsecond here, so the 2-competitive spin window keeps the parked path rare
under load and cheap when taken.

> **Correction (2026-09-25): the parked row does not time a wake.** The bench's daemon replies the
> instant the client's parked flag rises, so the reply usually lands while the client's wait is being
> set up and the wait returns without sleeping. Since A-31 the daemon confirms a reply stamp only
> when its wake call finds the client asleep (`futex_wake`'s count; `os_sync_wake_by_address_any`'s
> `ENOENT`), and the bench prints how many parked trips slept. Same command, release profile, five runs
> of 2,000 parked trips each:
>
> | Host | Parked row (ns) | Trips that slept, per run | Load |
> |---|---|---|---|
> | Apple M5 Max, macOS 26.4.1 (2026-09-25) | 1,195–2,147 (median 1,437) | 2, 18, 11, 1, 1 | load average 3.2–3.8, not quiesced |
> | Linux container on it (Docker, four CPUs, rust:1.98) | 503–545 (median 507) | 3, 2, 1, 2, 3 | same host |
>
> A confirmed sleeper's wake is 2.0–3.3 µs on this Mac and 15–17 µs mean in the container (the wake
> probe, §4.1), so "a park costs about a microsecond" above was the setup race, not a park. The row's
> ceiling in `ratchets.toml` (1,233 ns) gates that race; a row timing a sleeper woken — the reply held
> until the client sleeps, the confirmed-stamp mean reported — is owed (TBD_FIXES). Without the
> confirmation the client's own estimate learned the race too: 611–974 ns here
> (`docs/bugs/2026-09-25-wake-estimate-frozen-at-boot-and-preemptions-counted-as-long-steps.md`).

> **Measured (2026-09-25): the long-poll attribution's readings.** A Linux container on the M5 Max
> (Docker, `--cpuset-cpus=0-3`), a scratch program outside the tree, best of five rounds of 200,000
> calls: `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` 155–161 ns, `getrusage(RUSAGE_THREAD)` 130–134 ns,
> `pread` of a kept `/proc/thread-self/schedstat` 217–223 ns. Measured-and-rejected: schedstat's run
> delay (it misses hypervisor steal, which the guest excludes from CPU time without counting it as a
> run-queue wait, so steal would read as a blocked call); the shard reads the first two, and only
> around long polls (§4.3 status, A-31).

## Ratchets (2026-09-05)

`ratchets.toml` holds the ceilings for this machine (identity `4c62b34d5f545407`, the Apple M5
Max above): one per gated row, each the highest upper interval edge across three runs of its
bench example. `cargo xtask ratchet` runs each example three more times and fails when a row's
lowest lower edge lies above its ceiling, so a regression must clear every recorded run to
count; the planted ceiling of 1 ns on the slab row was reported as a regression before the file
was restored. Ceilings only tighten (`--tighten`); `--reset` rebuilds the entry and is a
deliberate act; a raised ceiling is an edit with a reason in the file.

Rows whose cost depends on where the OS placed two threads (the two-thread ring round trip, the
cross-shard wakes, the foreign spawn round trips) are gated only where the OS pins threads
(Linux, Windows). macOS on Apple silicon refuses pinning, so here they are informational: the
same ring binary measured 239, 364 and 364 ns in three runs, and 322, 447 and 520 ns in three
others, as the scheduler placed the pair within or across core clusters; a cross-cluster
placement is not a regression of the code, and the gate must not say it is.

The gate caught a real one on 2026-09-05: the first unsafe-reduction commit raised the idle step
from about 30 ns (ceiling 34) to 37–45 ns, the cost of a `RefCell` borrow per phase and a
control-channel poll per step. Recovered without unsafe (one borrow before the polls and one
after, the registry entry cached, the channel polled only behind a pending flag): 22–30 ns.

A raised ceiling, 2026-09-05: the landing commit moved `vfs.readdir_of_a_36_entry_dir` from
406 to 468–489 ns with no source change on its path (two runs of each binary in one session,
the previous commit built in a scratch checkout; `otool -tv` shows the directory iterator and
the lookup instruction-identical, readdir itself inlined into the bench). A code-placement
shift; the ceiling is raised to the widest edge measured, with the reason in the file, and
tightens again when that path is next touched. The same run tightened eleven other rows.

The database rows were recorded 2026-09-05 (7 rows; 12 others tightened in the same run). A
first attempt aborted because the volume-core bench's own acceptance gate (AC-1.3 or AC-1.8,
timing-sensitive) failed once under the ratchet's load and passed when the bench ran alone;
the ratchet now prints a failing bench's `ac-` lines so the gate is named, not guessed.

The rule gained a condition on 2026-09-05: a ceiling tightens only when the improvement is
larger than the row's own between-run drift. Before that, three rows tightened by a cold run
"regressed" by 1-5% on warm runs, one of them in the memory crate, untouched since Phase 0
(the buddy split-and-coalesce row: 71 ns recorded, then 72-75 ns in every run). The baseline
was reset under the new rule (60 gated rows, 10 informational, 0 regressions); the facts the
old ceilings had recorded are kept in the file's header.

The Phase 1 record run (2026-09-05, `cargo xtask ratchet --tighten`) added 41 volume rows
(`vfs.*`) and tightened five wire rows; the two destroy-slice rows (p99 and longest) are
informational because the step budget they are cut to is derived per run from the profile
(4,875–8,375 ns across the day's runs) and the longest slice follows the scheduler; the
ac-1.8 verdict inside the bench gates them against a floor it measures itself.

Between-run drift on this laptop, from the record run (the medians of the three runs):

| Row | Run medians | Drift |
|---|---|---|
| Slab insert+remove | 13, 13, 13 ns | 0 |
| Buddy alloc+free, one page | 56, 56, 56 ns | 0 |
| Ring round trip, two threads (informational here) | 364, 364, 239 ns | 52% |
| Cross-shard wake, both parking (informational here) | 6292, 6250, 6250 ns | 0.7% |
| Cross-shard wake, both spinning (informational here) | 500, 459, 584 ns | 27% |
| One local wake | 208, 250, 177 ns | 41% |
| Spawn and run a trivial task | 132, 156, 112 ns | 39% |
| CRC32C over 1 MiB | 152, 137, 124 µs | 23% |
| Header encode | 2, 2, 1 ns | one step (nanosecond quantization) |

What it means: the wall clock on a laptop resolves a regression of a few percent on the
microsecond rows and only a large one on the nanosecond rows, because the machine itself moves
that much between processes. The design's instruction-count gate (D-20) is what sees the small
change; it runs in CI's `callgrind` lane under valgrind (GAPS §8b).

**The instruction-count baseline (2026-10-01, AUD-29-32).** Recorded from CI's first run of the gate,
which failed as designed on the missing baseline and printed its counts for review.
- Run: 36849806140, commit `8cd9108`, GitHub `ubuntu-latest` x86_64.
- Command: `cargo bench --workspace --bench callgrind --features
  slates-mem/instruction-counts,slates-rt/instruction-counts,slates-wire/instruction-counts --
  --save-summary=json`, then `cargo xtask callgrind --iai target/iai`.
- The counts are committed as `xtask/callgrind-baseline.json` (`x86_64-linux`). Instructions per
  operation:

  | Bench | Ir | Bench | Ir |
  |---|---|---|---|
  | buddy one page | 1,554 | wire header encode | 29 |
  | buddy split and coalesce | 1,865 | wire header decode | 39 |
  | ring push and pop | 56 | wire frame encode | 2,100 |
  | slab insert and remove | 1,236 | wire frame decode | 510 |
  | runtime kept lookup | 66 | wire body encode | 1,469 |
  | runtime one local yield | 7,673 | wire body decode | 1,260 |
  | runtime spawn and run | 6,982 | CRC32C over a page | 1,279 |
  | runtime idle step | 5,400 | | |

- From here a count more than 1% above its row fails the lane. A lower count passes and is reported,
  and the bar is lowered with `cargo xtask callgrind --record` after review.

The Phase 2 task 5 run (2026-09-05, `cargo xtask ratchet` after the client and the CLI landed):
82 rows against the baseline, 0 regressions; no new rows, since the client's cost is the IPC
round trip already gated (`ipc.*`) and the CLI's is a process start plus one rendezvous. The
suites themselves are the facts of the day: `cargo test -p slates-client --test client` 1.2 s
for two daemons and a restart; `cargo test -p slates-cli --test cli` 1.2 s for a real anchor,
a real daemon, fourteen verbs through the binary, and the daemon leaving after the anchor is
killed (Apple M5 Max, macOS 26.4.1). The provisioning histogram (AC-2.1, T-2.6) is task 6's.

The Phase 2 task 6 run (2026-09-05, `cargo xtask ratchet`, Apple M5 Max, macOS 26.4.1, 18
cores, best-of-3 with all three shown by the tool) added the provisioning histogram
(`provision.*`) and raised the row count to 102. Provisioning is measured from the Rust client
through the real rendezvous and rings against an in-process daemon:

| Row (spinning, 1 client) | p50 | p99 | p999 |
|---|---|---|---|
| provision.spinning_1 | ~9 us | ~25 us | ~31 us |

The 50 us floor (R9, AC-2.1) is gated on that single-client p99. Eight clients spinning measure
p99 34-45 us (recorded, ratcheted); sixty-four oversubscribe the laptop's runnable cores
(thirteen, past five shards) and are informational at p99 ~2 ms; the parked form is p99 ~250 us,
reported separately as AC-2.1 asks. A status round trip (the ring plus the completion record) is
p99 ~9 us. Recovery held at ~92 ms for 10^4 volumes from 10^6 records against the 1 s budget
after the log record became a `LogEntry` (a verb's effects and its completion in one record).
One ceiling was raised with a dated reason (vfs.create_burst 1082 -> 1091: a loaded back-to-back
ratchet run drifted the row 0.2% over its ceiling; three isolated runs measured 1077-1091 ns on
code untouched since Phase 1).

The Phase 2 task 8 change (2026-09-05) pulled the provisioning histogram out of the omnibus
`cargo xtask ratchet`. The histogram spawns a daemon (several shard threads that spin) and one
client thread per concurrency level; run back-to-back with the deterministic microbenches on a
loaded laptop, its p99 measured scheduler contention, not the provisioning path (a single-client
p99 of 25 us in isolation read as 1.5 ms under the omnibus). It is now its own recorded command,
run on a quiescent machine and, on the reference machines, as its own CI lane (AC-2.1, R9):

```
cargo run --release -q -p slates-client --example provision_bench
```

Its rows were removed from `ratchets.toml` (the omnibus ratchet gates the deterministic
microbenches; the provisioning histogram is the daemon-level AC-2.1 gate). A bench-harness flake
was fixed the same day: `vfs_bench`'s size-independence check (ac-1.3) now allows the
measurements' own bootstrap-interval widths rather than the bare timer resolution, so a
lucky-fast small-size sample under load no longer reads as per-file scaling; and `ipc_bench`'s
parked round trip asserts at least one park per trip (a spurious futex wakeup can add one).

## NFS transports (2026-09-26)

One release-built daemon, mounted by the Linux kernel's own NFSv3 and NFSv4.2 clients with the same
options, runs the same file calls. Each round alternates the transports, so a drift in the host's load
falls on both (`xtask/src/conformance/bench.rs` states each phase). The Linux NFSv3 lane's slug is still
`native-linux-fuse`, but it mounts `vers=3` (`xtask/src/conformance/slates.rs`).

```
docker run --rm --privileged -v slates-linux-target:/lt -e CARGO_TARGET_DIR=/lt -w /src -v $PWD:/src \
  rust:1.98 bash -lc 'apt-get install -y sudo nfs-common procps &&
  cargo xtask conformance bench --scratch /tmp/bench-scratch --rounds 5'
```

Hardware: Apple M5 Max host; Docker Desktop's Linux VM (18 CPUs, 62.7 GiB, kernel 6.12.76-linuxkit),
with io_uring (a privileged container has no seccomp filter). The VM also runs the KIND lane's cluster:
load average 10.27 at the start of the "after" run and 9.08 at its end, and about 9 during the "before"
run. Times are milliseconds for the whole phase: 256 small files, or 32 MiB sequential. All five rounds
are shown.

"Before" is `2bf912e` with the v4 transfer-size fix in the working tree (without it the NFSv4.2
`seq-write` fails with `EIO`), and neither runtime fix. "After" is A-39: the spin polls the driver, and
the io_uring harvest never sleeps. The before rounds barely vary (±0.2 %) because each request carried
a fixed sleep: the spin window, then an hrtimer wake per harvest.

| transport | phase | before best | before rounds | after best | after rounds |
|---|---|---|---|---|---|
| NFSv3 | create | 1544.61 | 1547.02, 1544.61, 1546.17, 1544.77, 1544.83 | 212.19 | 214.66, 245.74, 227.37, 215.65, 212.19 |
| NFSv3 | stat | 782.27 | 782.27, 785.13, 782.28, 784.50, 783.39 | 6.46 | 8.97, 9.34, 8.35, 6.46, 8.30 |
| NFSv3 | read | 2329.23 | 2330.09, 2329.23, 2329.96, 2329.95, 2329.86 | 29.68 | 30.65, 30.91, 30.18, 30.04, 29.68 |
| NFSv3 | unlink | 773.65 | 775.07, 774.00, 774.05, 774.02, 773.65 | 87.56 | 92.66, 101.61, 103.28, 87.76, 87.56 |
| NFSv3 | seq-write | 418.28 | 418.68, 422.65, 423.23, 418.28, 419.23 | 65.70 | 65.70, 66.34, 67.10, 67.16, 66.03 |
| NFSv3 | seq-read | 403.30 | 403.53, 404.07, 403.61, 403.36, 403.30 | 7.98 | 8.78, 9.00, 8.68, 7.98, 8.17 |
| NFSv4.2 | create | 5413.93 | 5449.93, 5442.86, 5413.93, 5472.69, 5416.00 | 214.62 | 220.59, 214.62, 228.31, 220.83, 238.20 |
| NFSv4.2 | stat | 1312.73 | 1312.73, 1313.02, 1312.90, 1313.01, 1313.05 | 10.93 | 11.18, 11.18, 11.03, 11.14, 10.93 |
| NFSv4.2 | read | 5444.56 | 5445.73, 5444.56, 5445.45, 5445.19, 5445.05 | 47.27 | 48.60, 48.46, 47.27, 48.58, 53.66 |
| NFSv4.2 | unlink | 773.75 | 774.04, 773.97, 774.03, 773.75, 773.96 | 82.19 | 90.17, 86.32, 87.50, 87.07, 82.19 |
| NFSv4.2 | seq-write | 707.67 | 707.67, 710.73, 708.69, 710.08, 709.82 | 68.21 | 68.21, 70.28, 69.38, 68.39, 68.80 |
| NFSv4.2 | seq-read | 425.54 | 426.11, 425.54, 426.33, 426.38, 425.60 | 12.49 | 12.75, 13.54, 14.32, 12.55, 12.49 |

Per request after the fix: a stat (LOOKUP and GETATTR, the client's caches dropped first) is 25 µs over
NFSv3 and 43 µs over NFSv4.2. NFSv4.2's `read` costs more than NFSv3's (47 against 30 ms) because each
open and close is a stateful OPEN and CLOSE round trip that NFSv3 does not make. An in-process GETATTR
through the kernel client measured 27 µs (epoll) and 999 µs (io_uring) after the spin fix alone. The
io_uring gap was the harvest's sleep (`docs/bugs/2026-09-26-io-uring-zero-timeout-harvest-sleeps.md`).

## Session-plane congestion control and scheduling bake-offs (2026-09-28)

**Hardware:** Apple M5 Max, 18 cores, 128 GiB. **Load:** 2.8–4.4 load average (another project's tests were
running; the runs are in simulated time, so the load does not change their numbers).
**Commands (the contract):**

- `cargo run --release -p slates-transport --example congestion_bakeoff`
- `cargo run --release -p slates-transport --example scheduler_bakeoff`

**Setup.** Real endpoints — TLS 1.3, packet protection, the clocked connection — over the simulated network
(`crates/rt/src/sim.rs`): a bottleneck with a drop-tail queue of one BDP, random and burst loss, reordering,
and the 1,200-byte floor as MTU. Three seeds per scenario. The selection rules were fixed in each harness's
module doc before any run.

**Congestion control — 57 scenarios, 5 laws.** The grid is rate {64 k, 1 M, 10 M, 100 M} × RTT
{20, 100, 300 ms} × loss {0, 0.1, 1, 5 %}, plus buffer depths, reordering, burst loss, a bandwidth step,
same-RTT and RTT fairness, and coexistence with CUBIC. Deciding round `0def3b4`, raw rows in
`docs/wip/research/data/2026-09-28-congestion-grid-0def3b4.csv`:

| law | ping p99 vs best (geomean) | worst p99 vs best | goodput shortfall (geomean) | verdict |
|---|---|---|---|---|
| **Copa** (NSDI'18, δ = 0.5) | 1.268 | 3.05 (1 M, 20 ms, 5 %) | **1.068** | **selected** |
| NewReno (RFC 9002) | 1.207 | 3.86 | 15.83 | rejected: stalled at 100 M with 1 % and 5 % loss; RTT fairness Jain 0.893 |
| CUBIC + HyStart++ (RFC 9438/9406) | 1.231 | 3.09 | 14.04 | rejected: stalled at 100 M with 1 % and 5 % loss |
| BBRv3 (draft-06) | 1.343 | 10.81 | 2.56 | rejected: stalled at 100 M, 300 ms, 5 % |
| Copa-Meta (δ = 0.04) | 1.450 | 3.46 | 1.083 | rejected: RTT fairness Jain 0.840 |

Where Copa trails on p99, the better number usually comes from a loss-based law whose bulk flow collapsed:
NewReno at 100 M, 0.1 % loss carries 3.6 % of the link, so its pings cross an idle path. Copa's genuine cost
is thin links with little or no loss: at 64 kbit/s and no loss its p99 is 678 ms against NewReno's 289 ms,
because Copa holds a small standing queue by design (about 1/δ packets, each 146 ms at that rate).

**Earlier rounds** (their raw files were lost with the session scratchpad; the numbers below are this
session's record and the bug records). Each round's anomaly was a transport or controller bug, not a
property of a law:

- `3a0d86e` (partial): one Copa run spun 2.5 h with no stall bound, so the harness gained a per-run bound.
- `2e60c2f`: Copa's p99 at 100 M/20 ms was 103 ms with 92,500 queue drops per run
  (`docs/bugs/2026-09-28-copa-froze-an-overshot-window.md`).
- `8907c6f`: every law's thin-link p99 was about 7 s
  (`docs/bugs/2026-09-28-idle-peer-acks-inflated-the-rtt.md`).

The first scheduler grid (`4a3f6d7`) found packets past the datagram floor
(`docs/bugs/2026-09-28-packets-grew-past-the-datagram-floor.md`).

**Scheduler — 13 scenarios, 3 schedulers.** The grid is rate {1, 10, 100 M} × RTT {20, 100 ms} × loss
{0, 1 %}, plus burst loss. Each session carries 4 bulk transfers, metadata at 5 % of the link and control at
1 %, all under Copa. Raw rows in `docs/wip/research/data/2026-09-28-scheduler-grid-0def3b4.csv`:

| scheduler | control p99 vs best (geomean) | worst | metadata p99 (geomean) | goodput shortfall | verdict |
|---|---|---|---|---|---|
| **strict priority** | **1.023** | **1.153** | **1.027** | 1.075 | **selected** |
| round-robin | 1.222 | 4.822 | 1.219 | 1.081 | rejected: control tail 4.8× |
| weighted (deficit round-robin) | — | — | — | 1.006 | rejected: starved control and metadata |

The weighted scheduler completed zero control and zero metadata exchanges at 100 M, 100 ms, 0 % loss in all
three seeds. That is almost certainly a defect in its deficit accounting rather than the algorithm. It was not
repaired: strict priority is optimal for the top class by construction and already sits within 1.15× of the
best everywhere, so a correct deficit scheduler could win only the secondary goodput criterion.

## Session-plane path MTU discovery (2026-09-28)

**Hardware:** Apple M5 Max, 18 cores, 128 GiB. **Load:** 4–6 load average for the loopback runs (measured
back to back, below); the simulated grids run in virtual time, so load does not change their numbers.
**Commands (the contract):**

- `cargo run --release -p slates-transport --example path_mtu_bench`: real endpoints on the real runtime and
  real loopback sockets, a 256 MiB transfer between two shards, best of 5.
- `cargo run --release -p slates-transport --example congestion_bench`
- `cargo run --release -p slates-transport --example class_latency_bench`

Each harness was run at 20 seeds rather than its usual 3, set in a scratch copy only.

**What it buys: 4.8× on loopback.** Back to back at load 5. Before discovery (`ed613fe`) every packet is
1,200 bytes. With discovery the search confirms 9,209 bytes, just under macOS's 9,216-byte UDP datagram cap
(`net.inet.udp.maxdgram`). It takes 12 probes: 8 acknowledged, 4 refused locally at once (`EMSGSIZE`), none
lost. Raw rows: `docs/wip/research/data/2026-09-28-path-mtu-bench-loopback{,-ed613fe}.csv`.

| build | goodput, best of 5 | runs |
|---|---|---|
| `ed613fe` (floor) | 1,625 Mbit/s | 1,595 / 1,622 / 1,625 / 1,577 / 1,586 |
| path MTU discovery | **7,840 Mbit/s** | 7,303 / 7,811 / 7,840 / 7,806 / 7,716 |

**What it costs on a floor path: nothing measurable.**
- **Setup.** The congestion and class grids model every path at the 1,200-byte floor, the worst case for a
  prober, since every probe above the floor is lost. Each host is modelled as Ethernet: its interface
  refuses datagrams over 1,500 bytes at the send, as a real host with don't-fragment set does
  (`sim_udp_set_interface_mtu`, RFC 8899 §4.4).
- **Congestion grid** (1,120 runs, against `ed613fe`): steady-state ping p99 geomean ×1.011, per-scenario
  median ×1.000 (range 0.49–1.30), goodput ×0.999, no stalls.
- **Class grid** (260 runs): control p99 ×1.013.
- **Noise floor.** The same baseline build on two disjoint seed sets gives per-scenario median ratios of
  0.79–1.22 (95 % within 0.79–1.17). The differences above are inside that band.
- **Metadata p99 outliers are phase artifacts.** The two outliers were lossless paths, which are
  deterministic, so each is a single sample of a bimodal p99 (157 exchanges: the second-worst one).
  - 10 M/20 ms: shifting the baseline's metadata start by 1–3 ms reproduces the "regressed" value exactly
    (80.3–83.0 ms against 81.8).
  - 100 M/100 ms: over a 10-offset phase sweep the baseline spans 190–631 ms (median ≈ 387) and discovery
    169–543 ms (median ≈ 270).
- Raw rows: `docs/wip/research/data/2026-09-28-{congestion,class}-grid-20seeds-{ed613fe,pmtud}.csv`.

**Measured and rejected (each replaced in the same change):**

- **A raise that restarts the whole search from the peer's limit.** Every 600 s it spent about 36 lost probes
  on an unchanged floor path. Each lost probe leaves a gap the peer reports as an extra ACK range, which
  lengthened packets enough to shift a thin link's dynamics. The trace found the first divergence: an RTT
  sample 1.5 ms higher at 64 kbit/s, the serialization time of one 16-byte range.
  - 64 kbit/s, 300 ms, lossless ping p99: 483 → 587 ms.
  - Overall steady p99 geomean: ×1.036.
  - Replaced by a recheck that probes the last failed size alone (at most 3 probes per raise): 527 ms, and
    geomean ×1.022.
- **A refused probe that kept its packet number.** A burst of refusals left gaps.
  - Burst-loss 10 M/100 ms over 100 seeds: p90 883 → 1,530 ms, and 12 → 17 seeds over 800 ms.
  - Replaced by giving the number back (the datagram never left the host): median 132 ms, p75 222, p90 857,
    mean 340, 10 seeds over 800 ms, against the baseline's 135 / 430 / 883 / 449 / 12.
- **Probe acknowledgements as RTT samples.** A probe pokes a possibly idle peer, which acknowledges it at its
  next wake: 1.1 ms on a 1 ms path. Probes now give no RTT sample.

## Session-plane adaptive reordering tolerance (2026-09-28)

**Hardware:** Apple M5 Max, 18 cores, 128 GiB. **Commands (the contract):**
`cargo run --release -p slates-transport --example congestion_bench` and `... --example class_latency_bench`,
each at 20 seeds (set in a scratch copy only). The baseline is the path-MTU-discovery grid above.

**What it buys: 2.1× on a reordering path.** The reorder-jitter scenario (10 Mbit/s, 20 ms, 8 ms of jitter,
no real loss). RFC 9002's fixed thresholds read the reordering as loss and retransmitted what had arrived.

| build | capacity share, median (range) of 20 | goodput median | steady ping p99 median |
|---|---|---|---|
| path MTU discovery | 0.254 (0.253–0.258) | 2.54 Mbit/s | 29.6 ms |
| adaptive reordering | **0.540** (0.522–0.554) | 5.39 Mbit/s | 29.3 ms |

**What it costs elsewhere: nothing measurable.**
- **Congestion grid** (1,120 runs): steady ping p99 geomean ×1.002 (worst scenario ×1.22 at the noise band's
  edge), capacity share ×1.014, Jain fairness ×1.000, no stalls.
- **Class grid** (260 runs): control p99 ×0.998, control p999 ×0.996, metadata p99 ×0.996 (every scenario
  within 0.95–1.05).
- **Why it is neutral.** The tolerance only moves after a spurious loss. The bulk sender counted **zero**
  spurious losses in the lossless 100 Mbit/s, 20 ms scenario and in the lossy 1 Mbit/s, 20 ms scenarios (1 %
  and 5 %), so there it runs RFC 9002's thresholds unchanged.
- **The worst whole-run p99 (×1.49, 1 Mbit/s, 20 ms, 5 % loss) is run-to-run spread, not this change.**
  The same build does not always give the same whole-run p99 for the same seed. `fd4f0ef` gave seed 2
  833.3 ms on one run and 397.0 ms on the next. The parent `8700a7f` gave seed 3 424.1 ms, then 350.7 ms
  twice. Whole-run p99 over a small seed set is read only against the measured noise band, never per seed.
- **An earlier comparison against a stale baseline was wrong.** Against an older tree (before path MTU
  discovery) the grid seemed to move the lossless 100 Mbit/s, 20 ms whole-run ping p99 ×2.26 and the
  10 Mbit/s, 20 ms metadata p99 ×1.64. The parent `8700a7f` without the change gives the same 90.3 ms on
  every seed, so the move predates this change.
- Raw rows: `docs/wip/research/data/2026-09-28-{congestion,class}-grid-20seeds-reorder.csv`.

## Consensus: leadership transfer (2026-09-28)

**Hardware:** Apple M5 Max, 18 cores, 128 GiB. **What:** in-process three-daemon fleets over real loopback
sessions (the daemon's own transport, record plane and drive loops), heartbeat 100 ms, election timeout at its
floor of ten periods (1 s; no WAN path measured). **Commands (the contract):**

- `cargo test -p slates-server --test fleet a_council_leader_hands -- --nocapture` (council; prints the handoff,
  then stops the new leader and prints the survivors' election)
- `cargo test -p slates-server --test fleet a_root_leader_hands -- --nocapture` (root group, three regions)

**What it buys: about 13× less leaderless time for a planned move.**

| | runs | median |
|---|---|---|
| council handoff (transfer to a named voter) | 0.203 / 0.103 / 0.101 / 0.103 / 0.109 s | 0.103 s |
| council election after the leader is stopped | 1.817 / 1.117 / 1.019 / 1.316 / 1.520 s | 1.316 s |
| root-group handoff across three regions | 0.092 / 0.096 / 0.093 s | 0.093 s |

The loss path waits out the election timeout (1 s here, ten times the measured broadcast tail on a WAN) before
any follower campaigns, then runs a pre-vote and a vote round; the handoff runs one catch-up, one invitation
and one vote round. On a WAN both grow with the round trip, the loss path by ten tails before it starts.

### The graceful drain (2026-09-28)

**Commands:** `cargo test -p slates-server --test fleet a_draining_council_leader -- --nocapture` (in-process)
and `SLATES_TEST_CLI=1 cargo test -p slates-cli --test cli a_terminated_council_leader -- --nocapture`
(three real `slates daemon --fleet` processes; `SIGTERM` to the council leader).

| | runs |
|---|---|
| in-process drain, start to successor in office | 0.304 / 0.310 / 0.306 / 0.305 / 0.309 s |
| real processes, `SIGTERM` to a survivor leading | 0.111 / 0.127 / 0.106 / 0.125 s (exit 0 each) |

Before the drain the anchor killed the daemon at once, so the survivors waited out the 1 s election timeout
before campaigning (the leader-loss elections above: median 1.316 s).

### The pre-vote audit on the timed simulation (2026-09-28)

**Command:** `cargo test -p slates-cluster --release --test prevote -- --nocapture` (virtual time; twenty seeds;
three voters; heartbeat 100 ms; election timing derived from the modelled round trips).

| scenario | cluster (0.25 ms one way) | multi-region (80 ± 20 ms one way) |
|---|---|---|
| isolated follower, pre-vote: leader changes on the heal / term inflation | 0 / 0 (every seed) | 0 / 0 (every seed) |
| isolated follower, direct control: leader changes / term inflation | 1 / 22 (every seed) | 1 / 12–14 (every seed) |
| isolated follower, pre-vote: longest commit gap | 100 ms (the proposal cadence) | 168–284 ms |
| isolated leader: successor elected after | 1.42–1.59 s | 3.0–3.6 s (one seed 8.3 s) |
| isolated leader, before the jitter fix | — | 3.0–3.4 s typically; 12.0, 19.3 and 19.9 s on three seeds |
| isolated leader, after the lease fix (2026-09-29) | 1.24–1.39 s | 2.80–3.25 s (one seed 8.3 s: two tied survivors split) |

Re-run 2026-09-29 after the lease fix (`docs/bugs/2026-09-29-a-yielding-voter-refused-the-voter-it-yielded-to.md`):
every other row is unchanged except the multi-region pre-vote commit gap, 168–281 ms.

### Consensus log compaction and bounded replication (2026-09-28)

**Command:** `cargo test --release -p slates-cluster --lib measure_a_long_council_history -- --ignored --nocapture`.
The "before" column is the same drive against `HEAD` `90874fc` in a scratch export. Setup: a sole-voter council
commits `n` membership changes (alternately admitting and retiring a transient member), each a propose, commit
and apply — the path a leader period runs. "Retained" is the encoded `SavedRaft`, the publication the control
shard re-encodes and checksums before every consensus reply. Three runs each, all shown; this box (Apple M5
Max, macOS 26.4), 2026-09-28.

| changes | retained bytes (before → after) | log entries (before → after) | time per change, µs (before; after) |
|---|---|---|---|
| 250 | 5,708 → 3,182 | 251 → 67 | 2.35 / 2.05 / 2.13; 0.59 / 0.54 / 0.51 |
| 1,000 | 22,583 → 9,601 | 1,001 → 100 | 5.69 / 5.69 / 5.31; 0.50 / 0.48 / 0.42 |
| 4,000 | 90,083 → 45,607 | 4,001 → 928 | 25.39 / 25.48 / 25.50; 0.62 / 0.62 / 0.57 |

- **Time per change.** Before, it grew with the history's length: each apply copied the whole committed log,
  so the total was quadratic (102 ms for 4,000 changes). After, it is flat (2.3–2.5 ms for 4,000; 41×).
- **Retained bytes.** After compaction they are bounded by three times the configuration plus the uncommitted
  tail (`crate::fold`). What still grows here is the configuration itself: its `epochs` map keeps every host
  ever admitted (`docs/wip/GAPS.md`, 2026-09-28).
- **Catch-up round trips** (the core's tests): an empty follower behind 20 entries is found in 1 refusal
  (before, 20); a stale term's run of 8 entries is skipped in 1 (before, 8).
- **Measured and rejected: compacting the moment a majority commits.** The follower one round behind was sent
  the whole snapshot in place of the one entry it lacked, at every compaction. The third voter of a
  three-voter council never compacted itself, having been sent a snapshot each time. The leader now waits for
  its followers while the log is within twice the threshold.

### Learners: the availability gap of the thesis's Figure 4.4(a) (2026-09-28)

**Command:** `cargo test -p slates-cluster --lib a_staged_newcomer_leaves_no_availability_gap_where_a_direct_one_does -- --nocapture`
(deterministic; replication rounds, not wall time). Setup: voters {A, B, C} hold 40 entries; D joins with an
empty log and the voters become {A, B, C, D}; then C fails. Appends carry about two entries.

| newcomer | rounds from C's failure to the next commit |
|---|---|
| added directly | 21 |
| staged first (thesis §4.2.1) | 1 |

At the fleet's 100 ms heartbeat, that is about 2.1 s without a commit against 0.1 s.

**Drain timing after staging.** Command: `SLATES_TEST_CLI=1 cargo test -p slates-cli --test cli
a_terminated_council_leader_process_hands_off_before_it_exits -- --exact --nocapture`, five runs.

- `SIGTERM` to a survivor in office: 208 / 205 / 171 / 204 / 186 ms (106–127 ms at `4e38d3e`).
- One timestamped run splits it: the drain's start to the successor winning took 109 ms (one period: the
  invitation at +33 ms, the successor's next period at +109 ms); `SIGTERM` to the drain's start took about
  95 ms. The daemon checks for a stop once per 100 ms heartbeat.

### Priority elections across published inter-region round trips (2026-09-28)

**Command:** `cargo test -p slates-cluster --release --test priority -- --nocapture` (virtual time; twenty
seeds; Microsoft's "Azure network round-trip latency statistics", P50, page dated 2026-07-30, directional,
one way = half the round trip, ± 5 ms jitter; heartbeat 100 ms; a proposal every 50 ms; regions placed on
hosts by a seed-dependent permutation).

| region set | order | final leaders (seeds) | median commit latency, median of seeds |
|---|---|---|---|
| East US, West Europe, Japan East (quorum 83 / 85 / 162 ms) | first timeout | East US 20 | 137 ms |
| same | priority | East US 20 (0 transfers) | 137 ms |
| + Southeast Asia, Brazil South (quorum 117 / 169 / 162 / 169 / 185 ms) | first timeout | East US 14, West Europe 6 | 189 ms |
| same | priority | East US 20 (6 transfers) | 171 ms |
| five regions, East US down 20–40 s | first timeout | Brazil South 3, Japan East 6, Southeast Asia 5, West Europe 6 | 234 ms |
| same | priority | East US 20 (27 transfers) | 201 ms |

About half a 100 ms period of every commit latency is the drive's cadence: a proposal waits for the next
period before it is replicated.

Re-run 2026-09-29 after the lease fix: every row is unchanged but the outage's. By priority, East US 20 with 26
transfers (201 ms); by first timeout, Brazil South 2, Japan East 6, Southeast Asia 5, West Europe 7 (234 ms).

### The slot model: exhaustive searches of the fast track's recovery (2026-09-28)

**Hardware:** Apple M5 Max (18 cores: 6 performance, 12 efficiency), 128 GB, macOS 26.4.1, rustc 1.98.0,
release. Other sessions' builds ran beside it (load average 11–12), so wall times are upper bounds; the
state counts are exact.

**Commands:** `cargo test -p slates-cluster --release --test slot_model -- --ignored --test-threads=1
--exact the_ballot_recovery_keeps_agreement_at_full_scope
the_published_recovery_loses_agreement_when_its_leader_decides_by_votes --nocapture`. One scope:
`SLATES_SLOT_SCOPE=5,1,2,4 cargo test -p slates-cluster --release --test slot_model -- --ignored --exact
one_scope_from_the_environment --nocapture` (`SLATES_SLOT_LEVELS=1` prints each level's accounting).

| Search | Scope (nodes, indices, values, terms) | Classes | Wall | Peak resident |
|---|---|---|---|---|
| ballot rule, parallel breadth-first, fingerprints | 4, 1, 2, 4 | 463,715 | 0.16 s | 69 MB |
| same | 5, 1, 2, 3 | 1,586,398 | 0.65 s | 216 MB |
| same | 5, 1, 2, 4 | 23,552,907 | 10.6 s (161 s CPU) | 2,722 MB |
| same | 4, 1, 3, 4 | 642,654 | 0.22 s | 96 MB |
| published rule, serial breadth-first, whole keys, to the first disagreement | 4, 1, 2, 4, as written | 1,049,232 | — | — |
| same | once per term | 1,199,113 | — | — |
| same, whole space (no disagreement) | keeping leader-approved entries | 999,583 | — | — |
| whole model suite, full scope | all of the above | — | 17.3 s | 2,650 MB |

Measured and rejected:

- **The first model** kept each leader's tally of votes and stored every labelling of a state. It held
  18,064,834,560 bytes after 300 s without finishing the published rule's scope (4, 1, 2, 4), and was
  stopped by an alarm. Replacing the tally with decisions that read a chosen majority's current entries,
  and storing one representative per class (nodes and values renamed), brought the same scope to 1,072,719
  classes, 2.2 s and 184 MB.
- **Depth-first search.** On the same model and scope (4, 1, 2, 4), depth-first over fingerprints visited
  the same 463,715 classes as breadth-first over whole keys: 1.16 s and 38 MB, against 1.13 s and 94 MB.
  The traversal order does not change the work, since each class is visited once and each action is tried
  once. What costs is finding each successor's representative. Depth-first saves memory but no time, loses
  the shortest counterexample, and does not spread across cores. The parallel breadth-first search does
  the same scope in 0.16 s.
- **The memory ceiling from counted bytes alone.** Counting fingerprints, frontier and buckets gave
  1,556 MB at the widest level of (5, 1, 2, 4), while the process held 2,689 MB: the buckets' and frontiers'
  doubling capacities, and pages the allocator retains. The ceiling check now multiplies the counted bytes
  by the measured ratio, rounded up to 2.

### The prefix model: the dialect's design, searched exhaustively (2026-09-28)

**Hardware:** as the slot model above (Apple M5 Max, 18 cores, 128 GB; other sessions' builds alongside, so
wall times are upper bounds; state counts are exact). **Commands:** `cargo test -p slates-cluster --release
--test prefix_model -- --ignored --test-threads=1 --exact the_design_keeps_agreement_at_full_scope
each_rejected_alternative_loses_a_committed_entry_or_resurrects_a_stale_one --nocapture`. One scope:
`SLATES_PREFIX_SCOPE=3,3,2,3 [SLATES_PREFIX_VARIANT=drop-covered|commit-from-windows|report-logs-too]
cargo test -p slates-cluster --release --test prefix_model -- --ignored --exact
one_scope_from_the_environment --nocapture`.

| Variant | Scope (nodes, indices, values, terms) | Classes | Result | Wall | Peak |
|---|---|---|---|---|---|
| design | 3, 3, 1, 2 (default suite; 1.2 s in debug) | 331,522 | no fault | — | — |
| design | 3, 3, 1, 3 | 2,228,602 | no fault | 0.85 s | 377 MB |
| design | 3, 3, 2, 3 | 14,625,406 | no fault | 4.5 s | 1,887 MB |
| design | 4, 2, 2, 3 | 1,220,407 | no fault | 0.52 s | 221 MB |
| design | 3, 2, 2, 4 | 515,747 | no fault | 0.21 s | 100 MB |
| commits counted from windows | 3, 3, 1, 3 | 175,596 through the faulting level (the serial search's shortest history: 232,363) | a committed entry lost after 12 steps | 0.48 s | 165 MB |
| window slots dropped once covered | 3, 3, 2, 3 | 2,773,326 through the faulting level (serial: 2,983,719) | a committed entry lost after 17 steps | 8.3 s | 1,338 MB |
| voters report their logs too | 3, 3, 2, 3 | 14,648,981 | no fault; 104,206 recoveries resurrect a deposed leader's entry | 4.4 s | 1,896 MB |
| design | 4, 3, 1, 3 and 5, 2, 1, 3 | over 22,016,505 and 11,870,838 | past the 4 GiB ceiling, stopped | 8.8 s, 7.1 s | 2.7 GB, 2.5 GB |

The whole full-scale set runs in 18.4 s here and holds 2.43 GB at its peak.

**Superseded 2026-09-29:** the design this table verifies drops a synced node's older slots at the sync, and
that loses a chosen value at a scope the table never reached (three indices with four terms). The corrected
design and its numbers follow.

### The fast track's crossover across five regions (2026-09-29)

**Hardware:** Apple M5 Max, 18 cores, 128 GB, macOS 26.4.1, rustc 1.98.0, release; deterministic simulation.
**Command:** `SLATES_FAST_SEEDS=20 SLATES_FAST_STREAM_S=30 cargo test -p slates-cluster --release --test
fast_track -- --ignored --exact the_fast_track_across_regions_and_loss --nocapture` (9.7 s).

**Setup:** the five published Azure regions (`support::azure`), 5 ms jitter, elections by priority, windows
derived as the daemon derives them, a proposer placed in each region in turn proposing 20 commands a second for
30 s (about 11,900 per 20 seeds). Classic: forwarded to the leader. Fast: to every voter, each vote to the
leader, the leader filling a stalled index after two round trips. Latency to when the proposer learns the
commit. Median / p99 in ms (the median over 20 seeds of each seed's):

| Proposer | No loss, classic → fast | 1 % | 4 % | 10 % |
|---|---|---|---|---|
| East US | 156/206 → 201/250 | 197/244 → 243/314 | 198/255 → 246/642 | 199/303 → 448/1,798 |
| West Europe | 276/325 → 272/321 | 318/443 → 312/328 | 318/820 → 315/764 | 320/1,003 → 598/5,455 |
| Japan East | 377/426 → 274/324 | 417/481 → 315/356 | 419/1,026 → 318/635 | 420/1,142 → 486/2,055 |
| Southeast Asia | 435/485 → 275/325 | 472/593 → 318/434 | 473/1,179 → 322/816 | 479/1,277 → 584/2,318 |
| Brazil South | 317/366 → 300/349 | 358/451 → 342/422 | 359/1,062 → 346/799 | 362/1,144 → 525/1,798 |

At 10 % loss the fast track also commits fewer (9,585–11,382 against 11,831–11,925) and fills 436–570
stalled indices; at 4 %, 18–35.

**Measured and rejected:** a fast track with nothing to fill a stalled index. At 4 % loss (4 seeds, 10 s
streams) proposers committed 348 (Southeast Asia), 372, 589, 682 and 716 of about 780, since every index behind
an unfilled one waited for the next election.

### Pipelined replication across five regions (2026-09-29)

**Hardware:** Apple M5 Max, 18 cores, 128 GB, macOS 26.4.1, rustc 1.98.0, release. The timed simulation is
deterministic, so a run's numbers are exact for its seeds; the wall time is this machine's. **Command:**
`SLATES_PIPELINING_SEEDS=20 SLATES_PIPELINING_STREAM_S=30 cargo test -p slates-cluster --release --test
pipelining -- --ignored --exact pipelining_across_rates_and_loss --nocapture` (12.6 s).

**Setup:** five voters on Microsoft's published P50 matrix (East US, West Europe, Japan East, Southeast Asia,
Brazil South; `support::azure`), 5 ms jitter, elections by priority, the council drive's one append per
follower per 100 ms period with its batch budget (4,367 bytes: about 208 of the stream's 21-byte entries),
proposals from 10 s for 30 s. Each row is the median over 20 seeds of each seed's median and 99th percentile
commit latency and commits a second; messages and bytes are summed.

| Offered, loss | Window | Median / p99 | Commits a second | Bytes sent | Batches ahead |
|---|---|---|---|---|---|
| 20/s, none | 0, 1 batch, 4 batches | 156 / 206 ms | 19 | 9,676,654 (each) | 0 |
| 500/s, none | 0, 1, 4 | 172 / 221 ms | 497 | 57,877,534 (each) | 0 |
| 1,000/s, none | 0 | 172 / 222 ms | 994 | 96,467,659 | 0 |
| 1,000/s, none | 1 batch | 172 / 222 ms | 994 | 83,126,233 | 5,956 |
| 2,000/s, none | 0 | 7,319 / 14,378 ms | 1,029 | 110,932,564 | 0 |
| 2,000/s, none | 1 batch | 172 / 222 ms | 1,988 | 109,189,186 | 15,932 |
| 2,000/s, none | 4 batches | 172 / 222 ms | 1,988 | 108,321,466 | 17,930 |
| 4,000/s, none | 0 | 11,169 / 22,010 ms | 1,030 | 111,889,534 | 0 |
| 4,000/s, none | 1 or 4 batches | 7,349 / 14,406 ms | 2,058 | 111,889,534 | 15,940 / 17,940 |
| 1,000/s, 1 % | 0 | 183 / 267 ms | 993 | 97,175,833 | 0 |
| 1,000/s, 1 % | 1 batch | 174 / 241 ms | 994 | 84,700,216 | 5,806 |
| 2,000/s, 1 % | 0 | 7,357 / 14,483 ms | 1,022 | 111,380,905 | 0 |
| 2,000/s, 1 % | 1 batch | 260 / 458 ms | 1,980 | 110,712,202 | 15,357 |
| 2,000/s, 1 % | 4 batches | 201 / 321 ms | 1,985 | 109,822,390 | 17,503 |
| 4,000/s, 1 % | 0 / 1 / 4 batches | 11,187 / 7,520 / 7,432 ms median | 1,024 / 2,005 / 2,033 | 111,870,940 (each) | 0 / 15,365 / 17,512 |

**The derived window** (each node's own, set every period from its measured paths as the daemon sets it:
one batch for each period a lost batch takes to repair, ⌈2 × tail / heartbeat⌉) equals the best fixed window
in every row: at 2,000 proposals a second with 1 % loss 201 / 321 ms, 1,985 a second, 109,822,117 bytes; at
1,000 a second 172 / 222 ms and 83,126,233 bytes; at 4,000 a second 2,058 a second without loss and 2,033
with.

**Measured and rejected:** going ahead whenever a resend would not reach the next index (the first cut). At
2,000 proposals a second each period's batch fell just short of full, so every resend reached past it: no batch
went ahead in any seed, and the group committed 1,018 a second at a 2,497 ms median, as with no window.

### A leader's cost per proposal at a growing backlog (2026-09-29)

**Command:** `SLATES_BACKLOG_ENTRIES=50000 cargo test -p slates-cluster --release --test pipelining --
--ignored --exact a_proposal_costs_the_leader_the_same_at_any_backlog --nocapture`: a leader of five whose
followers acknowledge nothing, each thousand proposals timed. Hardware as above.

| Backlog | Before (each scan of the log) | After |
|---|---|---|
| 1,000 | 129 µs | 102 ns |
| 2,000 | 774 µs | — |
| 3,000 | 5.7 ms | — |
| 4,000 | 12.1 ms | — |
| 5,000 | 20.0 ms | 96 ns |
| 10,000 to 50,000 | not reached | 61 ns to 67 ns |

### A leader loss across published inter-region round trips: the lease fix (2026-09-29)

**Hardware:** Apple M5 Max, 18 cores, 128 GB, macOS 26.4.1, rustc 1.98.0, release; deterministic simulation.
**Command:** `SLATES_LEADER_LOSS_SEEDS=200 cargo test -p slates-cluster --release --test priority -- --ignored
--exact a_leader_loss_measured --nocapture` (0.4 s). "Before" is the same tool against `HEAD` `462b63d` in a
scratch export.

**Setup:** the first three or five published Azure regions, 5 ms jitter, a proposal every 50 ms, the leader cut
off at 20 s for 20 s. Per seed: the time from the loss to a successor, and the campaigns in between.

| Regions, order | Before: successor p50 / p90 / p99 / max, ms | After | Campaigns (before → after) | Successors after |
|---|---|---|---|---|
| 3, priority | 6,766 / 6,989 / 12,109 / 12,143 | 3,322 / 3,549 / 3,680 / 6,063 | 562 → 201 | West Europe 200 (before: Japan East 195) |
| 3, first timeout | 3,529 / 3,620 / 9,410 / 14,809 | 3,085 / 3,371 / 9,410 / 14,809 | 417 → 332 | Japan East 85, West Europe 115 |
| 5, priority | 4,430 / 4,777 / 4,828 / 8,616 | 4,158 / 4,773 / 6,472 / 7,103 | 602 → 507 | Japan East 111, Southeast Asia 68, West Europe 21 |
| 5, first timeout | 4,307 / 4,803 / 8,452 / 9,794 | 4,090 / 6,435 / 8,452 / 9,888 | 740 → 640 | Japan East 81, Southeast Asia 53, Brazil South 44, West Europe 13, East US 9 |

The five-region p99 under priority is 11 seeds of 200 near 6.4 s. Each is a split vote among survivors whose
priorities tie within their spread; the unfixed lease had serialized them, and it had serialized the
unprioritized control too, whose p90 rises from 4,803 to 6,435 ms.

**Measured and rejected** (the same 200 seeds, after the fix):

| Mitigation | 3 regions, priority | 5 regions, priority | 5 regions, first timeout |
|---|---|---|---|
| none (the fix alone) | 3,322 / 3,549 / 3,680 / 6,063 | 4,158 / 4,773 / 6,472 / 7,103 | 4,090 / 6,435 / 8,452 / 9,888 |
| a span of the base (`[T, 2T]`) | 4,009 / 4,548 / 4,611 / 4,705 | 4,321 / 5,931 / 7,824 / 15,316 | 4,318 / 5,032 / 7,985 / 9,324 |
| a strict order among tied voters | 3,322 / 3,549 / 3,680 / 6,063 | 4,177 / 7,295 / 7,563 / 7,640 | — |
| deferring a campaign after granting a pre-vote | 3,321 / 3,544 / 3,612 / 6,063 | 4,408 / 4,807 / 6,529 / 6,681 | 4,186 / 4,629 / 8,368 / 10,855 |

Each loses on the order the daemon runs. The strict order costs a whole yield when the first voter cannot win
(Ongaro & Ousterhout 2014 §5.2 abandoned ranking for this).

### A leader loss on real pods: KIND succession (2026-09-29)

**Hardware:** Apple M5 Max, macOS 26.4.1; Docker 29.3.1 (18 CPUs); kind v0.33.0, one control plane and five
workers; the release image built from the tree named. **Command:** `cargo xtask kind succession --cluster
slates-succession --tag TAG --trials 6` (about 3 min 45 s). The daemons are real processes on real pods,
their paths shaped by `tc netem` (pod 0 egress 80 ms, pod 1 20 ms, each ± 5 ms, pod 2 unshaped). A trial
cuts a settled central leader's egress and times the survivors' first leader from the cut's own
`/proc/uptime` (`docs/wip/kind-lane.md`, Piece 6).

| Daemon | Successor | Seconds to a successor, each trial |
|---|---|---|
| the lease fix and the round fix | the central survivor, 6 of 6 | 1.57, 1.98, 2.33, 3.08, 3.81, 4.96 (median 3.08) |
| the round fix only (`462b63d` + the fix) | the outranked pod 0, 6 of 6 | 4.97, 5.38, 6.08, 6.47, 7.21, 7.58 (median 6.47) |
| the lease fix only (`cf76129`) | pod 0, then pod 1 | 13.4, 25.4 |

The simulation of the same profile predicted the first two rows' successors exactly: the central survivor
200 of 200 with the lease fix (1,524 / 2,083 / 3,197 ms p50 / p90 / p99), the outranked pod 200 of 200
without (3,976 ms median). It runs the timed simulation's directional delays over 200 seeds, the "KIND
profile" line of `SLATES_LEADER_LOSS_SEEDS=200 cargo test -p slates-cluster --release --test priority --
--ignored --exact a_leader_loss_measured --nocapture`; the unfixed figure is the same tool on a `462b63d`
export. CI gates the successor's identity (`on_the_kind_profile_a_central_leader_passes_to_the_other_central_pod`). It does not model the round budget, so it could not see the
third row's defect. Real pods run about twice the simulation's time to a successor: 4 in 10 of the
successors' pre-elections drew no reply within their deadline.

**The campaign's session wait (2026-09-29).** New counters named that gap. A campaign's round could not ask
the only live voter, because its session was held out of its link by a discovery page. No late grant was
dropped. Ten trials each, the same profile, no other session's load on the machine:

| Daemon | Successor | Seconds to a successor, each trial | Voters not asked for a held session |
|---|---|---|---|
| `3f8733e` (counters only) | the central survivor, 10 of 10 | 1.58, 1.95, 1.95, 1.96, 2.35, 2.77, 3.45, 3.52, 3.91, 5.01 (median 2.77) | 5, in 4 trials |
| a campaign waits for a session out, within its round's base deadline | the central survivor, 10 of 10 | 1.54, 1.60, 1.69, 1.87, 1.87, 1.91, 1.93, 1.94, 1.95, 3.43 (median 1.91) | 0 |

The one slow trial after the fix drew a refusal by lease: the other survivor had heard the cut leader up to a
heartbeat later than the candidate. The retry won
(`docs/bugs/2026-09-29-a-campaign-asked-no-one-while-a-session-was-out.md`).

**The heal (2026-09-29, A-40).** Each trial then waits for the healed leader to rejoin: every pod holding all
three members with one leader. Over 19 trials it rejoined 4.95, 4.97, 4.97, 4.98, 4.98, 5.07, 5.09, 5.17,
6.08, 6.09, 6.11, 6.13, 6.15, 6.15, 6.16, 6.18, 6.19, 6.20 and 6.30 s after the heal. Before A-40 it had not
rejoined 180 s later. By the heal, 15 s into the cut, the reconnection schedule has reached its 6 s cap,
which bounds the wait.

### MLRaft: one to five logs across five regions (2026-09-29)

**Hardware:** as above. **Command:** `SLATES_MULTILOG_SEEDS=20 cargo test -p slates-cluster --release --test
multilog_timed -- --ignored --exact multi_log_on_the_failure_path --nocapture` (36 s).

**Setup:** the five published regions, 5 ms jitter, `n` logs led apart by priority, a keyed stream of 20
commands a second over 64 keys and a global stream of two a second, from 10 s for 60 s. The crash runs take
each log's preferred voter down from 30 s to 50 s in turn. Median over seeds of each seed's median / p99;
gaps are the longest time without an application.

| Logs | Steady keyed / global, ms | Messages | Log 0's leader crashed: keyed stream gap | Another log's leader crashed: keyed stream gap | A keyed command's expectation as a region is lost |
|---|---|---|---|---|---|
| 1 | 174/223 / 199/202 | 107,633 | 4,484 ms | — | 1,036 ms |
| 2 | 298/737 / 785/790 | 215,310 | 498 ms | 6,295 ms | 1,333 ms |
| 3 | 301/737 / 786/790 | 322,872 | 428 ms | 6,254–6,354 ms | 1,399 ms |
| 5 | 302/784 / 839/841 | 538,144 | 463 ms | 3,713–6,312 ms | 1,301 ms |

The expectation averages, over the logs, one fifth of the pause that log's crash leaves its own keyed commands
and four fifths of the steady median. Both groups keep one log (research record §3.6).

**The explorer** (`cargo test -p slates-cluster --release --test multilog -- --ignored --exact
the_multi_log_merges_alike_at_full_scale --nocapture`, 1.40 s): 200 seeds × 3,000 steps for three voters × three
logs and five × two, with no violation. It applied 99,499 and 62,858 keyed commands and 18,483 and 13,180
global ones, appended 3,398 and 1,142 barriers, and matched 46,551 and 20,077 restarted replays across 9,714
and 9,597 crash-restarts. The mutation that applies a global command without waiting for the other logs'
barriers is caught at seed 0, step 1,072.

### The prefix model, corrected: a slot goes only under a classic commit (2026-09-29)

**Hardware:** Apple M5 Max, 18 cores, 128 GB, macOS 26.4.1, rustc 1.98.0, release; no other session's build
alongside. State counts are exact. **Commands:** CI's step, `cargo test -p slates-cluster --release --test
prefix_model -- --ignored --test-threads=1 --exact the_design_keeps_agreement_at_full_scope
each_rejected_alternative_loses_a_committed_entry_or_resurrects_a_stale_one --nocapture`. One scope:
`SLATES_PREFIX_SCOPE=3,3,1,4 [SLATES_PREFIX_VARIANT=drop-at-sync|prune-at-fast-commit|drop-covered|
commit-from-windows|report-logs-too] [SLATES_PREFIX_CEILING_GB=24] cargo test -p slates-cluster --release
--test prefix_model -- --ignored --exact one_scope_from_the_environment --nocapture`.

The model now gives each node the classic commit index it knows, prunes a slot only under it, keeps a new
leader's window, and keeps a follower's committed prefix on an append as the code does. Log matching is Raft's
strict form.

| Variant | Scope (nodes, indices, values, terms) | Classes | Result | Wall | Peak |
|---|---|---|---|---|---|
| design | 3, 3, 1, 2 (the default suite; 6.9 s in debug) | 1,951,672 | no fault | 0.70 s | 299 MB |
| design | 3, 3, 1, 3 | 21,776,022 | no fault | 7.3 s | 2,622 MB |
| design | 4, 2, 2, 3 | 12,559,351 | no fault | 4.7 s | 1,918 MB |
| design | 3, 2, 2, 4 | 3,401,082 | no fault | 1.1 s | 561 MB |
| design, by hand (past CI's ceiling) | 3, 3, 2, 3 | 188,172,261 | no fault | 76.1 s | 14.0 GB |
| design, by hand (past CI's ceiling) | 3, 3, 1, 4 | 152,906,020 | no fault | 58.8 s | 11.7 GB |
| slots dropped at a sync (the first design) | 3, 3, 1, 4 | 15,379,817 (serial) | a chosen value lost after 18 steps | — | — |
| slots pruned under a commit that counts fast commits | 3, 3, 1, 3 | 860,982 (serial) | log matching broken after 12 steps; with the dialect's value-level matching instead, a chosen value lost at 3, 3, 1, 4 (43,450,155 classes) | 17.6 s (the latter) | 6.4 GB |
| commits counted from windows | 3, 3, 1, 3 | 382,952 (serial) | a committed entry lost after 12 steps | — | — |
| slots dropped once covered | 3, 3, 1, 3 | 2,015,576 (serial) | a committed entry lost after 16 steps | — | — |
| voters report their logs too | 3, 3, 1, 3 | 21,789,824 | no fault; 49,654 recoveries from a log | 6.8 s | 2,565 MB |

In every design scope, no path keeps a committed entry under an older term than a leader's (the design
asserts it), and slots are pruned under a commit 544,767 to 99,631,245 times. CI's step takes 13.1 s for the
design's scopes (2.6 GB peak) and 65 s for the rejected rules (2.9 GB peak; the serial searches run on one
core). The two by-hand scopes exceed the 4 GiB ceiling CI's smallest runner allows.

**Measured and rejected on the way:**
- Keeping a synced node's older slots until its commit index covered its synced leader's no-op, pruning under
  a commit index that counted fast commits: a chosen value lost at 39,450,280 classes (3, 3, 1, 4).
- The serial search keyed its visited set by whole keys, holding each 64-byte key twice (about 400 bytes a
  state at the peak): the 18-step history above outgrew the 4 GiB ceiling. It now holds 128-bit
  fingerprints, 131 accounted bytes a state, and fits.

### What a consensus retention publication costs as a group's log grows (2026-10-01)

**Command:** `SLATES_PUBLICATION_ENTRIES=1000,10000,50000 cargo test -p slates-cluster --release --test
publication_cost -- --ignored --nocapture`: a lone leader appends that many 64-byte commands, then five
publications are timed step by step: the clone of the retained Raft state (`RaftNode::saved`), its encoding
(`SavedRaft::to_bytes`), and the blake3 hash the publication carries. Apple M5 Max, 18 cores, 128 GiB; load
average 10–11 from other sessions; best of five shown, all five totals listed.

| Log | Record | Clone | Encode | Hash | Total (best) | All five totals |
|---|---|---|---|---|---|---|
| 1,000 entries | 77,082 B | 13.2 µs | 8.5 µs | 47.9 µs | 69.7 µs | 90.6, 73.4, 69.7, 74.0, 72.0 µs |
| 10,000 | 770,082 B | 85.4 µs | 52.5 µs | 475.8 µs | 613.7 µs | 833.5, 623.5, 618.5, 618.1, 613.7 µs |
| 50,000 | 3,850,082 B | 465.0 µs | 300.1 µs | 2.361 ms | 3.126 ms | 3.817, 3.128, 3.204, 3.126, 3.165 ms |

The cost is linear in the record's bytes, about 0.8 ns a byte, and the hash is three quarters of it. A
group's retained log is bounded by its compaction rule (thesis §5.1.2: a snapshot once the log exceeds the
last snapshot's size), so it stays within one to two times its encoded configuration: tens of kilobytes for
a region, where a publication costs tens of microseconds.

**Measured-and-rejected: incremental (delta) publication** (AUD-29-30). Publishing only the appended entries,
with periodic canonical checkpoints, would make each acknowledgement cost its delta instead of the whole
record. At the sizes compaction allows, the whole record costs tens of microseconds, so a delta format would
add a second recovery path (replaying deltas over a checkpoint, each step crash-tested) to save a cost the
compaction rule already bounds. Not built. What was built is admission: a proposal that would take a log past
its share of the record's region is refused before the log changes (`RaftNode::budget_refused`), so a
publication can no longer overflow after the protocol has moved. Revisit if a group's configuration grows
into the megabytes (root homes at fleet scale), where the 3.1 ms row starts to bind the acknowledgement path.

### A granted landing's slices against a probing client (2026-10-01)

`cargo test -p slates-server --test landing_fairness -- --nocapture` (debug build). The test lands 600 files,
written through the daemon's NFS transport, into a fresh directory in the build output. It runs on a
one-shard daemon while a second client probes `list` in a loop. Apple M5 Max, macOS 26.4.1, load average
4.7–7.0 from other sessions (not quiesced).

| Shape | Landing | Slices | Longest slice | Probes during it | Longest probe | Baseline probe |
|---|---|---|---|---|---|---|
| One unbounded slice (before AUD-29-25) | 831.5 ms | 1 | 810.6 ms | 3 | 831.0 ms | 168.9 µs |
| Sliced, run 1 | 1.80 s | 1,108 | 22.8 ms | 1,035 | 34.6 ms | 162.5 µs |
| Sliced, run 2 | 1.13 s | 1,093 | 31.7 ms | 1,021 | 36.9 ms | 98.4 µs |
| Sliced, run 3 (a stalled `fsync`) | 6.26 s | 1,170 | 205.4 ms | 1,096 | 205.6 ms | 166.8 µs |

**Without the probing client** (the same landing, sliced; three runs): 613, 612 and 596 ms, against
831 ms for the one call. Slicing costs no throughput here.

What it means: a slice ends within one unit of its budget, and a unit is one file's create, write and
`fsync`, so the longest wait a probe sees is that unit's disk latency. A large file is still one unit
(owed).

### The landing's percentile lane: slices and the shard's other work (2026-10-01)

`cargo test -p slates-server --test landing_fairness a_large_landing -- --nocapture` (debug build; AUD-29-25's
recorded lane). The same 600-file landing on a one-shard daemon, now probed by three kinds of work in turn — a
read (`list`), a provisioning (a 1 MiB volume created and destroyed) and a write lease (a write attach and its
detach on a second volume). The daemon records every slice in a log-linear histogram (`histogram.rs`: a quantile
reads back at most an eighth above the exact value; the maximum is exact) and counts a slice that ran past its
budget by more than its own last unit; the probes' quantiles are exact, from the samples. The budget is half the
shard's measured step quantum (the wake estimate's live value, so it moves between runs). Apple M5 Max, 18 cores,
128 GiB, macOS 26.4.1; load average 4.2–6.1 from other sessions (not quiesced). Three runs, all shown.

| Run | Landing | Slices | Budget | Slice p50 | p99 | p999 | max | Past budget | Shard's longest step |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 2.01 s | 1,206 | 2,019 ns | 41.0 µs | 1.57 ms | 5.24 ms | 5.62 ms | 0 | 13.8 ms |
| 2 | 1.96 s | 1,206 | 2,363 ns | 20.5 µs | 1.18 ms | 4.19 ms | 4.58 ms | 0 | 23.0 ms |
| 3 | 2.74 s | 1,206 | 2,402 ns | 26.6 µs | 18.9 ms | 25.2 ms | 25.3 ms | 0 | 25.5 ms |

| Run | Probe (226 each inside the landing) | p50 | p99 | p999 = max | Max before the landing |
|---|---|---|---|---|---|
| 1 | read | 870 µs | 2.87 ms | 13.9 ms | 135 µs |
| 1 | provision | 7.18 ms | 10.5 ms | 11.7 ms | 3.63 ms |
| 1 | lease | 267 µs | 3.14 ms | 6.54 ms | 305 µs |
| 2 | read | 816 µs | 2.33 ms | 13.9 ms | 107 µs |
| 2 | provision | 7.34 ms | 7.92 ms | 10.5 ms | 3.74 ms |
| 2 | lease | 227 µs | 2.41 ms | 5.93 ms | 236 µs |
| 3 | read | 148 µs | 21.5 ms | 28.7 ms | 3.42 ms |
| 3 | provision | 7.16 ms | 37.6 ms | 49.5 ms | 5.65 ms |
| 3 | lease | 392 µs | 31.3 ms | 39.8 ms | 1.80 ms |

What it means: the budget is a few microseconds, so every slice is exactly one unit (1,206 = two units per file
plus the sweep and syncs), and no slice ran past its budget by more than that unit in any run. The slice
distribution is the disk's: a unit is a create, a write or an `fsync`, and run 3 met a 25 ms one. Every kind of
work kept being served throughout — reads, provisioning and lease changes alike — each waiting at most one unit
plus its own service; a provisioning probe is two verbs, so it waits behind up to two. The shard's longest step
can exceed the longest slice (run 1: 13.8 ms against 5.6 ms) — it is the longest step of *any* task on the shard
over the whole test, the NFS writes and the probes included.

### An RPC-with-TLS connection built per accept, against a shared config (2026-10-01)

`cargo run --release -p slates-transport --example rpc_tls_bench`. It builds the network export's server connection
per accepted TCP connection (`rpc_tls_connection`: TLS 1.3, a client verifier over the operator's authority, ALPN
`sunrpc`), and compares that against two things. One is cloning one shared config into a connection, the shape R2
rejects when an alternative exists. The other is a mutual TLS 1.3 handshake in memory, which every connection
pays. The certificates are ECDSA P-256 from one test authority (rcgen). Apple M5 Max (18 cores), macOS 26.4.1,
load average 28–36 from other sessions (not quiesced). 2,000 builds and 200 handshakes per round, 7 rounds.

| Round | Build per connection | Shared config cloned | Mutual handshake | Build's share of an accept |
|---|---|---|---|---|
| 1 | 11.04 µs | 0.09 µs | 289.96 µs | 3.67% |
| 2 | 11.52 µs | 0.15 µs | 280.62 µs | 3.94% |
| 3 | 11.27 µs | 0.09 µs | 299.71 µs | 3.62% |
| 4 | 11.01 µs | 0.11 µs | 286.15 µs | 3.71% |
| 5 | 11.36 µs | 0.08 µs | 311.82 µs | 3.52% |
| 6 | 12.49 µs | 0.08 µs | 344.05 µs | 3.50% |
| 7 | 11.30 µs | 0.08 µs | 284.53 µs | 3.82% |
| **best** | **11.01 µs** | **0.08 µs** | **280.62 µs** | **3.78%** |

**Kept:** the per-connection build. Every `Arc` is the one rustls's constructor requires, with a single owner, in
the transport crate's D-8 exception module. Each connection verifies clients against the identity's trust anchors
as they stand when it is accepted. RFC 9289 §5.2.1 says a server SHOULD re-check clients when its anchors change.

**Measured and rejected:** one config shared by every connection. It saves at most 11 µs (3.8%) per accept, on a
path a kernel NFS client takes once per mount or reconnect, since it holds one TCP connection per server. The
cost would be slates-level shared ownership and trust anchors fixed at the listener's start.


### A scoped export's per-request check, and a scoped listing page (2026-10-03, AUD-29-76 follow-up)

`cargo run --release -p slates-vfs --example scope_diff_bench` measures the volume's check
(`Volume::within`: climb an object's parents to the scope). `cargo run --release -p slates-bridge-core --example
scoped_listing_bench` measures what a request pays through the seam every transport serves by (`ScopedBridge` over
`VolumeBridge`): one listing page of 1,024 files, and one lookup of a file, in a directory at each depth below the
scope. Apple M5 Max (18 cores), macOS 26.4, load average 7–18 from other sessions (not quiesced). Best of 7 rounds,
all shown in the commands' CSV output; tree at `bdfe8fb` plus the change below.

The volume's check, per call (best of 7, 20,000 calls a round):

| Depth below the scope | Directory inside | File inside | Directory outside (climbs to the root) |
|---|---|---|---|
| 1 | 40 ns | 63 ns | 81 ns |
| 8 | 257 ns | 274 ns | 344 ns |
| 64 | 2.50 µs | 2.55 µs | 2.68 µs |
| 512 | 25.2 µs | 26.1 µs | 26.6 µs |
| 4,096 | 241 µs | 247 µs | 250 µs |

About 40 ns a level. Each level resolves the parent's inode number to its current directory node through the inode
table (`Volume::parent_no` calls `current_dir`, then reads the node), because a directory node records its parent
by number (`DirNode::parent`). That attribution is from reading the code, not from a profile.

Through the bridge, before and after the change (best of 7):

| Depth | Listing page, before | Listing page, after | Lookup, before | Lookup, after |
|---|---|---|---|---|
| 1 | 123 µs | 74 µs | 0.32 µs | 0.22 µs |
| 8 | 486 µs | 78 µs | 0.89 µs | 0.46 µs |
| 64 | 2,973 µs | 76 µs | 6.16 µs | 2.74 µs |
| 512 | 24,004 µs | 97 µs | 47.7 µs | 23.9 µs |
| 4,096 | 229,436 µs | 297 µs | 465 µs | 227 µs |

**Kept:** a lookup's result and a listing's entries are checked against the directory the request already admitted
before the scope (`crates/bridge-core/src/scoped.rs` `within_admitted`). The subtree relation is transitive, so the
answer is the same, and an entry homed in the listed directory costs one step; only an alias homed elsewhere climbs
to the scope. A page now pays its depth once rather than once per entry, and a lookup climbs once rather than twice.

**Measured and rejected:** the per-entry climb to the scope. It cost 3 ms for a 1,024-entry page 64 levels down
and 229 ms at 4,096. Each request still pays one climb for the object it names, the 40 ns a level above.

### The snapshot diff over a large span (2026-10-03, AUD-29-76 follow-up)

`cargo run --release -p slates-vfs --example scope_diff_bench` (the diff rows). A volume of empty files, 256 to a
directory. It is snapshotted, given one-byte writes to some of its files spread evenly, and snapshotted again; then
`Volume::paths_changed_between` runs. The last row renames a 256-file directory into another. Same machine, load
average 9–11, best of 7.

| Files | Changes | Paths named | Diff |
|---|---|---|---|
| 10,000 | 1 | 1 | 1.2 µs |
| 10,000 | 100 | 100 | 74.6 µs |
| 10,000 | 10,000 | 10,000 | 6.47 ms |
| 10,000 | a moved directory | 516 | 57.0 µs |
| 100,000 | 1 | 1 | 1.1 µs |
| 100,000 | 100 | 100 | 83.0 µs |
| 100,000 | 10,000 | 10,000 | 7.84 ms |
| 100,000 | a moved directory | 516 | 58.3 µs |

The diff's cost follows the changes, about 0.65–0.8 µs a named path, and not the volume's size: one change costs
the same at ten times the files. The first run of this measurement found the diff naming 9,925 of 10,000 changed
files (`docs/bugs/2026-10-03-a-reverse-name-lookup-missed-every-entry-that-opens-a-leaf.md`); these numbers are
after that fix.

### What one barrier's recovery publication costs as a shard's content grows (2026-10-03)

**Command:** `cargo run --release -p slates-vfs --example publish_bench` (`crates/vfs/examples/publish_bench.rs`,
at `0e5a4b5` plus the bench): one volume holding one file of each size, written in 128 KiB calls; then five
publications, each timed step by step: the capture (`Volume::to_image`, which copies every file's bytes into the
image), the encoding (`ShardImage::to_content`) and the publication into double-buffered slots
(`ShardImage::write_to`: it reads back both slots' frames to learn their generations, checksums the new frame and
copies it in). The image read back must equal the one written. Apple M5 Max, 18 cores, 128 GiB; load average
3.4–5.3 from other sessions; best of five shown, all five publication-step figures listed.

| Content | Capture | Encode | Publish | Total (best) | Publish, all five |
|---|---|---|---|---|---|
| 1 MiB | 0.02 ms | 0.03 ms | 0.33 ms | 0.38 ms | 0.33, 0.37, 0.42, 0.45, 0.43 ms |
| 4 MiB | 0.11 ms | 0.06 ms | 1.12 ms | 1.28 ms | 1.33, 1.12, 1.66, 1.69, 1.53 ms |
| 16 MiB | 0.36 ms | 0.26 ms | 3.77 ms | 4.39 ms | 3.77, 3.98, 5.62, 5.68, 6.08 ms |
| 64 MiB | 1.45 ms | 0.96 ms | 15.99 ms | 18.39 ms | 15.99, 17.20, 23.28, 23.50, 23.24 ms |
| 256 MiB | 6.35 ms | 4.31 ms | 64.72 ms | 75.38 ms | 64.72, 67.36, 94.64, 93.97, 94.34 ms |

The cost is linear in the shard's content, about 0.29 ms a MiB, and it is paid at every barrier: an NFS `COMMIT`
or `FILE_SYNC` write, a FUSE `fsync`, and the `flush` every `close` sends. So a shard holding 256 MiB spends
75 ms on each close. The publication step is six sevenths of it: before writing, it re-verifies the CRC of both
slots (the committed image included) only to learn their generations, then checksums and copies the new frame.
This is the gap the in-place refinement closes (`docs/wip/recovery.md` §4; GAP-A9-6): content resident once in
anchor RAM, the image carrying references, so a barrier costs the shard's metadata, not its bytes.

**After A-64 (same day, same command, `publish_bench` now reporting µs and the image's size).** Three runs; load
average 3.5–8.8 from other sessions. Best of five per step:

| Content | Image | Capture | Encode | Publish | Total, three runs |
|---|---|---|---|---|---|
| 1 MiB | 1,212 B | 0.6–0.8 µs | 0.4–0.5 µs | 0.7–1.0 µs | 1.7, 2.2, 2.2 µs |
| 4 MiB | 3,852 B | 1.1–1.3 µs | 0.5–0.7 µs | 1.0–1.6 µs | 2.6, 3.4, 3.3 µs |
| 16 MiB | 14,412 B | 2.6–3.3 µs | 1.2–1.7 µs | 5.5–7.0 µs | 9.4, 11.9, 11.8 µs |
| 64 MiB | 56,652 B | 8.9–11.0 µs | 5.2–7.1 µs | 12.2–25.5 µs | 43.6, 27.5, 26.3 µs |
| 256 MiB | 225,612 B | 31.9–51.5 µs | 13.8–17.5 µs | 65.0–88.7 µs | 129.8, 157.7, 110.6 µs |

At 256 MiB that is 75.4 ms → 0.11–0.16 ms, about 500–680 times less, and the image is 0.08 % of the content. The
cost now follows the number of chunks (1,024 of 256 KiB here), about 220 bytes of image each. The publish step is
still the largest: it re-verifies both slots' CRCs to learn their generations before writing.

**The kept committed slot (same day).** A shard now keeps the committed slot its last publish returned
(`CommittedSlot`, `ShardImage::write_after`). Both slots are checked once, at the first publish after boot, and the
frame's CRC runs over the generation and the image without first copying them together. The FUSE write log's clear
takes the generation the publish returned instead of checking both slots again. Same command; load average 13.7–19.6
from other sessions; three runs, best of five per step:

| Content | Publish | Total, three runs |
|---|---|---|
| 1 MiB | 0.5 µs | 1.7, 1.7, 1.6 µs |
| 16 MiB | 2.6–2.7 µs | 6.6, 6.4, 6.6 µs |
| 64 MiB | 10.2–10.5 µs | 23.9, 24.2, 24.2 µs |
| 256 MiB | 40.8–41.5 µs | 91.5, 90.3, 91.1 µs |

The publish step at 256 MiB went from 65.0–88.7 µs to 40.8–41.5 µs, under a higher load than the runs before it. Every
recovery test passes. `a_publisher_that_keeps_its_committed_slot_publishes_in_one_pass_and_survives_a_torn_write`
proves a publish torn partway leaves the last image and the kept slot as they were.

### A shard-local global allocator against the system allocator (2026-10-03, measured, not landed)

The candidate: a `#[global_allocator]` in slates-server. Each shard thread allocates from a span of one reserved,
`NORESERVE` region (power-of-two classes, a bump pointer, owner free lists, a lock-free stack for remote frees); every
other thread uses the system allocator. It was built against the Linux startup timeouts, read at the time as shards
contending on the process's memory-map lock through the allocator.

Measured, Linux container on an M5 Max (Docker Desktop, 18 cores, rust 1.98.0):
- It removed the allocator's syscalls from shard threads: 847 → 25 `mprotect` calls per daemon start.
- It did not change the stall. Observations ending `Deadline` beside 108 burners: 13, 7, 10 with it; 2, 2, 13
  without (the CPU-time budget's first runs). The stall's cause was the arena lock's page population
  (`docs/bugs/2026-10-03-locking-an-arena-stalled-every-shard-on-the-memory-map-lock.md`), which no allocator touches.
- Provisioning (`provision_bench`, release, heap then system, two rounds each) is unreadable this day. The host load
  average was 35 from another session's KIND cluster, and the same binary's single-client spinning p99 read 174 µs
  in one round and 2.5 ms in the next:

| Row | heap 1 | system 1 | heap 2 | system 2 |
|---|---|---|---|---|
| spinning 1 p50 | 13,792 | 44,000 | 47,541 | 44,958 |
| spinning 1 p99 | 174,041 | 570,375 | 2,502,458 | 1,326,292 |
| parked 1 p99 | 327,250 | 557,333 | 3,664,500 | 3,621,750 |

Not landed: its one proven effect is not yet tied to a latency, and it costs eleven unsafe sites. To re-measure on a
quiesced host (load average under 1), build both arms and run them alternately:
```
cargo build --release -p slates-client --example provision_bench   # with, then without, the #[global_allocator]
target/release/examples/provision_bench
```
If it is re-taken, jemalloc (as ../vorpal uses it) is the comparison arm.

### The cryptographic library: AWS-LC against ring and RustCrypto (2026-10-03, A-66)

> The `seal_bench` example below was removed on 2026-10-04 with the transport's control-datagram seal (A-67 H-2c,
> replaced by hyper-datagram's sealed plane, which hyper-raft benchmarks against slates' seal: a tenth to a half of
> its cost a message). Its numbers stay on record; the commands below ran at `1ef18e4`.

Apple M5 Max (18 cores), macOS 26.4.1, release builds, load average 14.4–17.2 from other sessions (not quiesced).
"Before" is `240013e` (rustls on `ring`, the seal on RustCrypto's `aes-gcm`), extracted with `git archive` and built
in its own target directory. "After" is the tree with A-66. The two ran alternately, two runs each.

`cargo run --release -p slates-transport --example seal_bench`: a control datagram sealed and opened
(`encode_sealed`, `decode_sealed`), 100,000 per round, 7 rounds, nanoseconds per pair:

| Run | 64 B, each round | 1 KiB, each round | Best 64 B | Best 1 KiB |
|---|---|---|---|---|
| RustCrypto 1 | 2092, 2016, 2016, 2014, 2415, 2731, 3075 | 10068, 10079, 10064, 10064, 14330, 15175, 14583 | 2013.5 | 10063.7 |
| AWS-LC 1 | 247, 234, 223, 221, 226, 239, 250 | 541, 513, 537, 519, 514, 570, 545 | 221.1 | 512.8 |
| RustCrypto 2 | 2572, 2396, 2161, 2180, 2164, 2026, 2046 | 12584, 12412, 10432, 10347, 13709, 10216, 10105 | 2026.3 | 10105.4 |
| AWS-LC 2 | 204, 196, 198, 199, 200, 198, 199 | 457, 458, 464, 460, 459, 461, 463 | 196.0 | 457.3 |

`cargo run --release -p slates-transport --example rpc_tls_bench` (the 2026-10-01 harness, unchanged), best of 7:

| Run | Build per connection | Shared config cloned | Mutual handshake |
|---|---|---|---|
| ring 1 | 8.31 µs | 0.07 µs | 206.96 µs |
| AWS-LC 1 | 10.31 µs | 0.07 µs | 202.03 µs |
| ring 2 | 8.26 µs | 0.07 µs | 207.07 µs |
| AWS-LC 2 | 10.52 µs | 0.07 µs | 202.37 µs |

First random bytes of a fresh process through the provider (`seal_bench`'s first line), five processes each:

| Build | First random bytes |
|---|---|
| ring | 2.6, 3.5, 3.0, 5.0, 3.5 µs |
| AWS-LC, jitter entropy off (the build) | 16.6, 14.9, 12.8, 13.6, 12.4 µs |
| AWS-LC, jitter entropy on (`AWS_LC_SYS_NO_JITTER_ENTROPY=0`, its own target directory) | 17,453, 17,063, 16,994, 17,398, 17,198 µs |

Readings:
- The seal is 9–10× faster at 64 B and 20–22× at 1 KiB.
- The handshake is 2% faster.
- The per-connection build is 2 µs slower, paid once per accepted RPC-with-TLS connection (a mount or a reconnect).
- Jitter entropy would cost every new process 17 ms before its first handshake. The build leaves it out.

### The VFS on a busy machine (2026-10-04)

**Command:** `cargo build --release -p slates-cli --bin slates --example vfs_tails && target/release/examples/vfs_tails`
(`crates/cli/examples/vfs_tails.rs`). An in-process daemon (5 shards) serves a volume. The real `slates mount`
mounts it (macOS: `mount(2)` with the root handle, `actimeo=1`). Each operation is a direct syscall sequence on the
mount, timed per call. The sweep:
- 0, 1 and 2 background spinner threads per core, each sweeping 32 MiB (twice the largest L2);
- 1 caller, and 4 callers (one per four cores).

`VFS_TAILS_FOCUS=<seconds>:<load>` loops `rename(2)` alone and prints the daemon's own service times beside the
caller's (`Daemon::nfs_service_times`).

Machine: Apple M5 Max, 18 cores, 128 GiB. Other sessions kept the load average at 14–97 throughout, so "load 0"
is not an idle machine. Each figure is one run; all runs are shown.

**What one `rename(2)` costs on the wire.** From `nfsstat -c` deltas over 5,081 renames:
- 2.0 RENAMEs (the file's, then its AppleDouble `._` sidecar's, which macOS sends itself);
- 3.0 LOOKUPs and 1.0 GETATTR;
- 6 RPCs in all.

**Before the connection moved to its volume's owner (first sweep, p50 / p99 / p999 / max).**

| op | load/core | callers | p50 | p99 | p999 | max |
|---|---|---|---|---|---|---|
| create 4 KiB | 0 | 1 | 1.22 ms | 2.08 ms | 2.43 ms | 2.83 ms |
| rename | 0 | 1 | 0.39 ms | 0.79 ms | 1.00 ms | 1.25 ms |
| rename | 0 | 4 | 3.41 ms | 4.80 ms | 5.60 ms | 6.65 ms |
| rename | 1 | 1 | 2.44 ms | 188 ms | 266 ms | 321 ms |
| create 4 KiB | 1 | 1 | 8.85 ms | 88.5 ms | 172 ms | 184 ms |
| open+read 4 KiB | 1 | 1 | 0.20 ms | 1.61 ms | 73.2 ms | 80.8 ms |
| provision | 1 | 1 | 84 µs | 1.46 ms | 8.02 ms | 10.8 ms |

**Where a call's time went: the daemon's service time (focus loop, rename).** Every one of 185,745 calls was
forwarded from the accepting shard to the owner:
- at load 0: p50 31 µs, p99 115 µs, p999 197 µs;
- at load 1: p50 90 µs, p99 1.18 ms, p999 1.84 ms.

**After the move (`nfs.rs` `migrate`).** Only a connection's first call is forwarded.

| measured | load/core | p50 | p99 | p999 | max |
|---|---|---|---|---|---|
| daemon service | 0 | 2 µs | 29 µs | 45 µs | 0.1–2.4 ms |
| daemon service | 1 | 3 µs | 49–53 µs | 98–246 µs | 1.4–1.6 ms |
| caller rename | 0 | 320–340 µs | 568–596 µs | 870–883 µs | 7.5–11 ms |
| caller rename | 1 | 408–494 µs | 1.84–2.23 ms | 3.26–3.97 ms | 73–84 ms |

The full sweep after the move, at load average 40–97, still shows caller tails of 80–400 ms at one or two
spinners per core (rename p99 163 ms at load 1). The daemon's service time never exceeded 7.9 ms, so those tails
come from outside the serve:
- the calling thread's own wake after each of its six RPC replies;
- the kernel NFS client;
- the shard's wake from its driver, which the service timer does not include.

They are open (GAPS).

**Measured and rejected: thread QoS.** The shard threads at `QOS_CLASS_USER_INTERACTIVE`, then the caller, then
both. Three runs each of a 12-second rename loop at load 1. p99 / p999:

| arm | run 1 | run 2 | run 3 |
|---|---|---|---|
| none | 2.8 / 29 ms | 2.9 / 30 ms | 3.6 / 17 ms |
| caller | 2.6 / 28 ms | 4.6 / 57 ms | 3.5 / 44 ms |
| shards | 3.2 / 43 ms | 3.7 / 37 ms | 4.3 / 60 ms |
| both | 4.5 / 54 ms | 2.9 / 57 ms | 3.0 / 50 ms |

No arm beats the spread of the others; not landed.

**Where the daemon's own long serves go (same day, `served_local_off_cpu`).** A local serve is synchronous, so the
serve's wall time less the shard thread's CPU time (`CLOCK_THREAD_CPUTIME_ID`) is time the operating system held
the thread off a core. Raw RPC mode (`VFS_TAILS_RAW`, one RENAME per sample, no kernel client), load average 61–78
from other sessions:

| load/core | serve p99 | off-core p99 | serve max | off-core max | caller RPC p99 / p999 |
|---|---|---|---|---|---|
| 0 | 53 µs | 27 µs | 8.9 ms | 8.8 ms | 0.50 / 5.0 ms |
| 1 | 131 µs | 98 µs | 67.4 ms | 67.4 ms | 1.09 / 21.8 ms |
| 2 | 106 µs | 74 µs | 53.4 ms | 53.3 ms | 1.01 / 42.5 ms |

The daemon's long serves are preemption, not work: the CPU a serve takes is a few microseconds. **Shard QoS
re-measured on this metric and rejected again** (three interleaved pairs at load 1, off-core p99 / p999 / max):
- none: 106 µs / 655 µs / 42 ms; 20 µs / 295 µs / 26 ms; 74 µs / 786 µs / 73 ms;
- `QOS_CLASS_USER_INTERACTIVE`: 115 µs / 983 µs / 66 ms; 27 µs / 328 µs / 35 ms; 90 µs / 655 µs / 120 ms.

### A barrier publishes what changed (A-68, 2026-10-04)

**Command:** `bash e2e-host.sh 4` (a scratch script kept with this record's session): one anchor and `--shards 4`,
one bounded 2 GiB volume mounted by `slates mount`, four rounds into the same growing volume. Each round creates 2,000
small files with the shell, tars them onto the mount, and untars the archive into a new directory. Apple M5 Max, 18
cores, load average 2–6.

| round | create 2,000, before → after | untar, before → after |
|---|---|---|
| 1 | 4.9 → 1.07 s | 61 → 5.3 s |
| 2 | 17.6 → 1.03 s | 132 → 5.3 s |
| 3 | 34.9 → 1.12 s | 234 → 5.4 s |
| 4 | 50.2 → 1.32 s | 331 → 5.3 s |

The tar step's time varies from run to run on both binaries (1.7–16 s on the new one, 2.0–3.8 s on the old). A
per-call server log over two later rounds settled where the time goes:
- 306,927 calls in 33.5 s of wall time;
- the daemon's own service totals 2.93 s (CREATE 35 µs, COMMIT 12 µs, SETATTR 12 µs mean);
- idle gaps over 10 ms add to 0.86 s;
- the rest is the macOS kernel client's round trip, about 100 µs a call, times bsdtar's call count: about 150,000
  per round, most of them AppleDouble sidecars (14,007 CREATEs for 4,000 files) and attribute sets.

Interleaved tar A/B on the identical script, old then new: 2.39 / 1.69 s and 2.41 / 1.88 s.

### Content granule: small files take small blocks (A-69, 2026-10-04)

**Command:** `bash e2e-rss.sh` and `bash e2e-dd.sh` (scratch scripts kept with this record's session). Each starts one
anchor with `--shards 4`, creates one bounded 2 GiB volume and mounts it with `slates mount`. `e2e-rss.sh` reads the
daemon's RSS empty, after 4,000 tiny files and after 4,000 files of 4 KiB, all written through the mount.
`e2e-dd.sh` times `dd` writes of 1 MiB, 24 MiB and 256 MiB from `/dev/zero` (bs 32k). The old binary is HEAD
`6832c10`, built in a separate tree; the runs alternate old then new. Apple M5 Max, 18 cores; the machine is shared
with other sessions, load average 20–64.

| daemon RSS | HEAD (2 runs) | granule 4 KiB, zero sentinels (2 runs) |
|---|---|---|
| empty | 41.1 / 41.2 MB | 34.2 / 34.0 MB |
| 4,000 tiny files | 133.3 / 135.3 MB | 81.6 / 77.9 MB |
| 4,000 × 4 KiB | 294.3 / 294.1 MB | 114.7 / 123.6 MB |

256 MiB write, three alternating pairs: HEAD 0.372 / 0.378 / 0.374 s, new 0.374 / 0.367 / 0.372 s. The 1 MiB and
24 MiB writes are level too (0.021–0.024 s and 0.045–0.046 s on both).

Measured and rejected on the way:
- **Chunks cut at sixteen granules** (64 KiB on this host instead of 256 KiB). 256 MiB writes took 1.445 / 1.945 /
  1.901 s against HEAD's 0.624 / 0.554 / 0.767 s, three alternating pairs at load average 64. The cause is four times
  the chunks. Chunks stay sixteen host pages.
- **The granule alone, with the buddy's non-zero sentinels.** The empty daemon was 99.6 MB resident: the per-granule
  state, link and incarnation arrays (17 bytes a granule) were written whole at start-up. Zero sentinels make them
  lazily backed.

**Then the AppleDouble working copy (A-70).** Same `e2e-rss.sh` on the A-69 binary plus A-70, with the arena's
allocation read at each publication, load average 69–77:

| | A-69 | A-69 + A-70 |
|---|---|---|
| arena after both phases | 49.2 MB (4,096 B per tiny file, 8,192 B per 4 KiB file) | 16.4 MB (the file bytes) |
| RSS, 4,000 tiny files | 78–82 MB | 51.8 MB |
| RSS, + 4,000 × 4 KiB | 115–124 MB | 83.7 MB |

Attribution, from a `MallocStackLogging=1` memory graph (`leaks --forkCorpse --outputGraph`, then `malloc_history
-allBySize` grouped by the innermost slates frame) and `footprint --forkCorpse -v`. The shard's content mapping held
3,001 dirty 16 KiB pages (48 MB). The heap growth from 8,000 files was about 10 MB: the attribute inode each
provenance attribute takes (5 MB of inode slab), directory and trie slabs, and the op log's ring (5.6 MB, bounded by
its budget). The runtime's per-shard build (11 MB) is fixed, present in the empty daemon.

### Docker containers over a slates volume, and the partition log's resident pages (A-71, 2026-10-04)

**Command:** `bash e2e-docker-nfs.sh` (scratch, kept with this record's session). One anchor with `--shards 4`,
one bounded volume, `slates export ID`, then `docker volume create --driver local --opt type=nfs --opt
o=addr=host.docker.internal,vers=4.2,proto=tcp,port=PORT,hard --opt device=:EXPORT`: Docker Desktop's Linux VM
mounts the volume with its own kernel NFSv4.2 client. There is no host mount, no Docker Desktop file share and no
privilege on the Mac. Workload in `node:22`, over npm's own tree (2,524 entries, 18 MB): `cp -a`, `find`, read
and hash every file, `tar cf`, `rm -rf`, `tar xf`, `rm -rf`. Compared with a Docker Desktop host-directory bind
(APFS) and the container's own overlay. Apple M5 Max, Docker Desktop 29.3.1, load average 62–70 from other
sessions. Times in ms, three alternating rounds:

| step | slates NFSv4.2 volume | host-directory bind | container overlay |
|---|---|---|---|
| `cp -a` | 3,531 / 4,189 / 5,655 | 3,341 / 55,590 / 55,684 | 69 / 85 / 71 |
| `find` | 249 / 359 / 645 | 123 / 167 / 108 | 4 / 4 / 6 |
| read and hash all | 1,753 / 4,320 / 3,202 | 817 / 461 / 485 | 31 / 35 / 33 |
| `tar cf` | 1,278 / 2,690 / 2,066 | 4,448 / 14,925 / 7,874 | 12 / 13 / 13 |
| `rm -rf` | 918 / 1,696 / 1,520 | 3,283 / 13,914 / 10,197 | 15 / 14 / 15 |
| `tar xf` | 4,244 / 4,922 / 7,805 | 15,497 / 44,668 / 47,126 | 44 / 41 / 41 |
| `rm -rf` | 846 / 893 / 1,857 | 6,904 / 24,597 / 14,328 | 15 / 15 / 16 |

The same workload over the host's `slates mount` bound into the container (the OCI form, `-v MNT:/work`) fails at
`rm -rf`: "Directory not empty", 2,473 of 2,524 entries left as `.nfs.*` (Docker Desktop's share keeps every touched
file open on the host, so the macOS NFS client silly-renames each delete; `docs/wip/oci-handoff.md` §4). The NFSv4.2
volume leaves the volume empty after every round.

Daemon RSS over repeated rounds, 512 MiB volume:

| | round 3 | round 12 | round 24 |
|---|---|---|---|
| before A-71 | 90.8 MB | 107.1 MB | 128.9 MB |
| after A-71 | 87.0 MB | 88.0 MB | 89.1 MB |

Attribution before the fix: `footprint --forkCorpse -v` diffed between rounds 4 and 10 put all the growth in one
4,893,375-page region (434 → 1,078 dirty pages), the anchor segment (`SLATES_ANCHOR_LEN` 80,173,056,000 bytes). A
`mincore` count over the volume shard's slice of the content object stayed at 2,115 pages, and the heap grew 492 KiB.
On a 4 GiB volume the op log's budget (1% of the quota, 43 MB) also fills over the first rounds; that is charged
and bounded.

After A-72 (trie removal frees the nodes it empties; the op log's ring capped at its budget), same 24-round run:
85.7 MB at round 3, 86.0 MB at round 12, 86.3 MB at round 24 (+0.66 MB, against +2.1 MB after A-71 alone and
+38.0 MB before either).

### A Go build in a Docker container over NFSv4.2: where the time goes (A-74, A-75; 2026-10-04)

**Command:** `bash e2e-docker-go-ctr.sh` (scratch, with this record's session): one anchor with `--shards 4`, a 4 GiB
volume as a Docker `type=nfs` volume (NFSv4.2 from Docker Desktop's VM). In `golang:1.26`: `cp -a /usr/local/go`
(217 MB, about 12,800 files), `go build -a net/http`, then `go build net/http` again (cached). Client-side per-op
numbers are from `/proc/self/mountstats` (queue and RTT per call); server-side numbers are the daemon's `nfs.*`
signals. Apple M5 Max, load average 58–81 from other sessions.

| | 4 slots (before A-75) | 32 slots (A-75) | host-directory bind | container overlay |
|---|---|---|---|---|
| `cp -a` | 24.6–28.8 s | 23.1–23.6 s (56.2 s in one run at peak load) | 9.0 s | 0.28 s |
| `go build -a net/http` | 6.4–7.8 s | 5.8–6.1 s | 6.4 s | 3.3 s |
| cached rebuild | 0.78–1.10 s | 0.57–0.64 s | 0.23 s | 0.05 s |
| reopen: client queue | 1.05 ms | 0.08–0.13 ms | | |
| reopen: RTT | 0.44 ms | 0.78–0.88 ms | | |

- **Serve:** the daemon serves a call at a 2–3 µs median (p99 74–147 µs) and almost never finds two calls waiting
  on the connection (4 of 135,609 replies shared a write at 32 slots). So the remaining RTT is in transit, Docker
  Desktop's VM-to-host path (a plain TCP ping-pong over it: p50 132 µs, p90 1.84 ms, p99 4.0 ms at load 81).
- **One call in flight** (`go build -p 1`, `GOMAXPROCS=1`): reopen 0.18 ms, READ 0.17 ms, CLOSE 0.06 ms.
- **Correction:** an earlier reading of this run printed the queue column as RTT. The table above reads them
  correctly.

**Re-check after the shared machine's extra load ended** (another session's 24 CPU burners ran 17:55–19:37; the
table above was measured inside that window). Three alternating pairs at 19:42–19:45, load average 15–40: HEAD
`6832c10` (4 slots, one reply per call) against `3e45dca` (A-74, A-75).

| | `6832c10` | `3e45dca` |
|---|---|---|
| `go build -a net/http` | 5.95 / 6.70 / 6.16 s | 4.66 / 4.66 / 4.32 s |
| cached rebuild | 0.85 / 1.03 / 0.62 s | 0.56 / 0.58 / 0.53 s |
| reopen: queue + RTT | 1.03 / 1.27 / 0.98 ms | 0.56 / 0.49 / 0.53 ms |
| `cp -a` (one call in flight) | 30.2 / 21.2 / 20.8 s | 24.4 / 35.3 / 24.7 s |

The copy keeps one call in flight, so the slots cannot help it; its spread is the machine's.

**A-76, the session moves to the volume's shard** (same Go workload; the volume forced onto shard 3 of 4 by creating
volumes until one lands off the listener's shard; load average 15–25):

| | before A-76, volume on shard 3 | after A-76, volume on shard 3 | volume on the listener's shard |
|---|---|---|---|
| `go build -a net/http` | 8.0 s | 6.49 / 6.43 / 5.40 s | 5.97 / 6.04 / 5.40 s |
| reopen: queue + RTT | 2.20 ms | 0.80 / 0.78 / 0.62 ms | 0.77 / 0.77 / 0.64 ms |
| v3 calls forwarded | 334,133 | 2 (before the move) | 0 |

### A Rust build in a Docker container over NFSv4.2 (2026-10-04)

**Command:** `bash e2e-docker-rust.sh` (scratch, with this record's session). Same daemon and Docker `type=nfs` volume
as above. In `rust:1.98.0`, with the repository mounted read-only and the host's cargo registry read-only
(`CARGO_HOME` in the container, `CARGO_NET_OFFLINE`): copy slates' workspace (about 100 MB, 82 MB of it vendored
crates) onto the volume, `cargo build -p slates-vfs` with `CARGO_TARGET_DIR` on the volume, a no-op rebuild, and a
rebuild after touching `crates/vfs/src/lib.rs`. Apple M5 Max, load average 15–17, at `45361be`.

| step (ms) | slates NFSv4.2 | host-directory bind | container overlay |
|---|---|---|---|
| copy the workspace | 11,288 / 6,092 | 3,988 | 1,332 |
| `cargo build -p slates-vfs` | 10,435 / 8,507 | 11,973 | 6,883 |
| no-op rebuild | 419 / 223 | 10,852 | 105 |
| rebuild after a touch | 1,913 / 1,216 | 7,138 | 397 |

- **slates against the host bind:** every build step is faster on slates. Through Docker Desktop's share the no-op
  rebuild recompiles everything, consistent with cargo's mtime fingerprints not holding over that share.
- **Where the copy's time goes:** about ten round trips per file (`cp -a` sends 3.4 SETATTRs per file), each
  0.3–0.4 ms through Docker Desktop's VM-to-host path, against a 3–5 µs serve (`nfs.local_p50_ns`). Copy-only
  WRITE: 0.41 ms per call (queue 0.035, RTT 0.37).
- **Where the build's writes go:** 4,178 WRITEs carried 746 MB (about 180 KB each) at 2.99 ms per call. That is
  queue and RTT while the build saturates the VM's CPUs; the same path streams a 256 MiB `dd … conv=fsync` in
  197–216 ms (1.3 GB/s), against 1.6–2.7 s on the host bind.


### Delegations against the kernels (A-78, A-79; 2026-10-04)

Apple M5 Max, Docker Desktop's Linux kernel as the NFSv4.2 client, macOS's NFSv3 client; load average 40–62 (other
sessions' work, which is the norm on this machine). Scripts are in this record's session scratchpad.

**Cross-protocol consistency.** Command: `bash e2e-deleg-consistency.sh`, release build. A container holds a read
delegation on a settled file and reads it every 50 ms. The host writes it through `slates mount` (NFSv3).

| build | host write (ms) | reader sees new contents (ms after the write) | runs with a delegation |
|---|---|---|---|
| refused `NFS3ERR_JUKEBOX` (before the hold) | 4,033 | — | 1 of 1 |
| hold, probe not retrying `NFS4ERR_DELAY` | 41 / 52 / 67 | 67 / 114 / 195 | 0 of 3 (probe answered DELAY; nothing recalled) |
| hold + `NFS4ERR_DELAY` retried | 46 / 27 / 24 / 23 | 24 / 29 / 3 / 2 | 4 of 4 |

- Every delegated run counts `nfs.v3.held_for_recall: 1`, `nfs4.recall.sent: 1` and `nfs4.recall.answered: 1`. The
  reader never saw the new contents before the write: the recall comes first.
- The undelegated middle row is not a hold measurement. It is plain NFSv3 writes, and the reader polls its attribute
  cache. It is kept as the record of the bug (`docs/bugs/2026-10-04-a-callback-answered-delay-marked-the-back-channel-down.md`).

**Settled read-heavy workload.** Command: `bash e2e-docker-rust-settled.sh`. The Rust build of the record above, with
the workspace left to settle for one lease before the warm no-op rebuilds:

| warm no-op rebuild (ms) | without delegations | with read delegations |
|---|---|---|
| range over the runs | 186–217 | 106–148 |

- About 40% faster: the client opens, reads and closes settled files from its own cache.
- Measured and rejected: granting on any read-only open, with no settled rule. On the churning Go build it ran 3%
  slower, because the client returned the delegations itself. Hence the quiet period (A-78).
- Unexplained, seen once and not again: one `DELEGRETURN` stalled 84 ms. It is in GAPS.

### Write delegations in a Docker Rust build (A-80; 2026-10-05)

Command: `SLATES_BIN=<binary> timeout 400 bash e2e-docker-rust-slates.sh`, three alternating rounds. Base: `f3eb461`
(read delegations only) with the server-owner fix patched in, so both arms run under the same identity rules.
Apple M5 Max, Docker Desktop's 6.12 kernel, NFSv4.2; load average 18–23 from other sessions.

| step (ms) | base | write delegations |
|---|---|---|
| copy the workspace | 9,538 / 9,184 / 9,351 | 9,089 / 9,216 / 9,089 |
| `cargo build -p slates-vfs` | 9,122 / 8,863 / 8,829 | 7,861 / 7,878 / 8,402 |
| no-op rebuild | 395 / 373 / 390 | 213 / 195 / 197 |
| rebuild after a touch | 1,833 / 1,724 / 1,729 | 825 / 808 / 788 |

- The build is about 10% faster, the no-op rebuild about 48% and the rebuild after a touch about 54%; the copy is
  even.
- Per mountstats: CLOSE fell from about 6,700 to 4,580 per run, and READ from about 4,200 to 775 (served from the
  client's cache). OPEN_NOATTR (about 2,600) disappeared. 4,580 write delegations were granted.
- OPEN's round trip matched the base (0.545 ms against 0.534 ms) only after the gate stopped being rebuilt per grant.
  The first build measured 0.43–0.63 ms against 0.24–0.46 ms. The rebuild alone cost 238 µs at 4,500 delegations,
  measured in isolation (release build).
- DELEGRETURN: 1,700–1,760 per run, with a 74–87 ms client-side queue under this load; 275 with a 0.029 ms queue on
  a quieter run. The daemon answered no `NFS4ERR_DELAY` (`nfs4.delay.*` absent), so the queue is the client's.
  Unexplained, and not on the measured steps' path.

**Measured and rejected: holding replies across turns until the input drains** (Redis's rule). Each turn still ended
at the quantum, but replies were written only once the buffered calls were drained, the connection moved, or held
bytes reached the transfer ceiling. In the same workload WRITE's round trip doubled (2.1–2.2 ms against 0.95–1.1 ms)
and the build ran 8.8–10.1 s. A WRITE reply is tiny, so the byte bound never trips, while each buffered WRITE costs
tens of µs to serve: the first replies waited out the whole pipeline to save one write syscall each. A-74's rule
stands. A turn writes what it built once it passes the quantum (about one wake, about one syscall), so a reply waits
at most about a quantum. The pipelining test's flake came from its regime, not from the rule: in a debug build one
READ costs 46–68 µs against a 5.3 µs wake-estimate quantum. It now pins its quantum.

### MCP conformance: the official suite against `slates mcp` (A-87; 2026-10-05)

Commands (`docs/wip/conformance/mcp/`): `linux-build.sh` (a release build in `rust:1.98.0`), then
`docker run --rm -e CONF_SRC=1 -e "CONF_ARGS=--requirements 2026-07-28" -v slates-linux-target:/target:ro
-v <this dir>:/conf:ro -v <out>:/out node:22-trixie bash /conf/run.sh`, and `python3 classify.py <out>`.

`run.sh` starts an anchor, bootstraps it, serves `slates mcp --http 0`, and runs the suite (built from source at
`c37eec8`, 2026-10-01; the published 0.1.16 has no 2026-07-28 scenarios) through `proxy.js`. The proxy adds the
edge's bearer token, which the harness cannot send, and maps only its own address to the edge's, so hostile `Host`
and `Origin` values reach slates unchanged. The image must share the build's glibc (`node:22` bookworm cannot run a
`rust:1.98.0` binary).

| run | passed | failed | of the failures |
|---|---|---|---|
| `--requirements 2026-07-28` | 103 | 65 | 38 reference fixtures, 26 the optional tasks extension, 4 undeclared features (`completion/complete`; MRTR input requests no slates tool makes) |
| `--suite all` (legacy 2025-06-18/2025-11-25) | 7 | 24 | 19 reference fixtures, 5 undeclared capabilities (logging, completion, subscriptions), 2 legacy stateful-session checks |

- Every core check of what slates implements passes: `server-stateless` 21 of 21 testable (the other 4 need the
  suite's diagnostic tools), `caching` 8 of 8, `http-header-validation` 14 of 14, `sep-2164-resource-not-found` 3 of
  3, `dns-rebinding-protection` 2 of 2, `tools-list` 4 of 4, and the legacy `server-initialize` and `ping`.
- Defects the suite found, fixed with failing tests first:
  - legacy `ping` was unanswered;
  - a modern request (by header or by `clientCapabilities`) missing its version or capabilities was taken as
    legacy instead of refused `-32602`/400;
  - `initialize` as a modern request was answered instead of `-32601`/404;
  - a header naming a different version from the body met `-32022` before `-32020`.

### Tail latency under a hot-directory storm, against Tectonic's published tails (condition 12; 2026-10-05)

Command: `docs/wip/bench/hotdir/run.sh WORKERS FILES_PER_WORKER` (release build at `965b61f`). A slates volume is
exported over NFSv4.2 to a Docker container (Docker Desktop 6.12 kernel, `hard`), and `hotdir.py` runs `WORKERS`
threads in **one** shared directory, the non-ideal pattern Tectonic names (§6.3, metadata hotspots). It times every
operation: create + write 4 KiB + close, then random open + read + close, random stat, and a readdir every 100
operations. The container's own overlay is run as the reference. Apple M5 Max; load average 8–9 (other sessions).

| workers | op | slates p50 | slates p99 | slates p999 | overlay p50 | overlay p99 |
|---|---|---|---|---|---|---|
| 1 | create+write+close | 0.86–1.1 ms | 1.16–1.85 ms | 1.45–2.4 ms | 8 µs | 63 µs |
| 1 | open+read+close | 25–48 µs | 81–541 µs | 0.13–0.74 ms | 3 µs | 33 µs |
| 1 | stat | 3–104 µs | 0.24–0.28 ms | 0.30–0.34 ms | 0.8 µs | 5 µs |
| 4 | create+write+close | 2.5 ms | 4.4 ms | 7.0 ms | 49 µs | 227 µs |
| 16 | create+write+close | 11.5 ms | 18.9 ms | 20.4 ms | 0.58 ms | 2.4 ms |
| 16 | open+read+close | 1.2 ms | 4.3 ms | 7.2 ms | 0.33 ms | 2.9 ms |
| 16 | stat | 0.34 ms | 2.3 ms | 3.9 ms | 13 µs | 1.4 ms |

(Two single-worker runs, 8,000 and 2,000 files; ranges span both. Readdir at 16 workers is omitted: the reference
overlay's own 397 ms p50 shows Python's GIL, not the filesystem.)

- Where a create's time goes (mountstats, 1 worker): three round trips (OPEN with create 0.29 ms, WRITE 0.21 ms,
  CLOSE 0.17 ms) through Docker Desktop's VM-to-host path. The daemon serves an operation in 3 µs at p50 and
  0.49 ms at p99 (`nfs.local_p*_ns`).
- Concurrency in one directory multiplies the create's latency nearly linearly (1.1 → 2.5 → 11.5 ms for 1, 4, 16
  workers). The overlay shows the same shape at µs scale (8 → 49 → 583 µs): Linux holds the directory's lock across
  an `O_CREAT` open, so creates in one directory are serial on any filesystem.
- Reads are served from the client's cache under delegations: open+read+close at 25–48 µs, below one round trip
  (A-78, A-80). 8,000 write delegations were granted in the 16-worker run.
- **Against Tectonic** (FAST'21, Pan et al., Figure 3 and §6.3): blob-storage writes (partial-block quorum appends)
  have a median of about 30 ms with the CDF reaching 100% around 150–200 ms; blob reads a median of about 15 ms and
  a tail to about 100 ms; 72 MB warehouse block writes a p99 of about 1.5 s with hedging. A metadata shard serves
  at most 10 KQPS, and hot directories drive about 1% of name-layer shards to it, with retries after a backoff. Under
  the same hot-directory pattern, slates' p99 is 18.9 ms for creates at 16 writers and at most 4.3 ms for reads,
  and slates sets no per-shard metadata cap (a shard serves an operation in 3 µs). The tiers differ, and the
  comparison says so: Tectonic stores exabytes on disk across a datacenter, while slates serves a working set from
  RAM. The comparison is of what a caller waits for under the pattern, not of the media.
- Owed, measured: a write delegation's space limit is zero bytes, so the client flushes at every close. A
  reservation behind a non-zero limit would take the WRITE (0.21 of a create's 0.86 ms) off the close path. The
  same storm through Linux's own client on loopback is below; on a k8s node it is owed.

#### The same storm through Linux's own NFS client on loopback: the Nagle stall, before and after (2026-10-05)

Command: `docs/wip/bench/hotdir/native.sh WORKERS FILES_PER_WORKER` in a privileged `python:3.12-slim-trixie`
container (header of the script gives the `docker run`): the daemon and the kernel's NFSv4.2 client in one Linux
(Docker Desktop 6.12 kernel), over loopback, with `tmpfs` as the reference. Apple M5 Max, load average 6.8–8.2
(other sessions). One run per row.

| workers | op | before p99 | after p99 | after p50 | after p999 | tmpfs p99 |
|---|---|---|---|---|---|---|
| 1 | create+write+close | 43 ms | 0.86 ms | 0.47 ms | 1.25 ms | 3.8 µs |
| 1 | open+read+close | 42 ms | 0.21 ms | 29 µs | 0.27 ms | 4.1 µs |
| 1 | stat | — | 79 µs | 2.7 µs | 0.12 ms | 1.1 µs |
| 16 | create+write+close | 690 ms | 13.3 ms | 6.9 ms | 14.4 ms | 1.6 ms |
| 16 | open+read+close | — | 2.9 ms | 0.72 ms | 4.4 ms | 2.0 ms |
| 16 | stat | — | 1.5 ms | 0.13 ms | 2.2 ms | 1.1 ms |

- Before, the daemon served each operation at p50 2 µs and p99 0.59–0.72 ms, yet a round trip (mountstats OPEN)
  cost 1.46–6.3 ms and the tails sat at 40–50 ms: Nagle's algorithm on the server's sockets held a reply written
  while an earlier one was unacknowledged until the client's delayed ACK (Linux `TCP_DELACK_MIN`, 40 ms). The rt
  test `a_reply_in_two_writes_does_not_wait_for_the_peers_delayed_acknowledgement` reproduced it on Linux (median
  round 42 ms, every round after the first 40.7–51 ms) and passes on macOS, which acknowledges at once on loopback
  here. Fix: `TCP_NODELAY` on every runtime stream (docs/bugs/2026-10-05-nagle-delayed-ack.md).
- After, mountstats round trips at 1 worker: OPEN 0.32 ms, WRITE 31 µs, CLOSE 25 µs, GETATTR 26 µs. The OPEN with
  create is the create's remaining cost, and the daemon's local p99 (0.49 ms) is where it goes next.
- The Docker Desktop runs above never showed the 40 ms stall (p99 at most 1.85 ms); why that path escaped it was
  not measured.

After A-89 (a create no longer re-images its parent's entries; same command, release at the A-89 change, load
average 9.2–10.3):

| workers | op | p50 | p99 | p999 | before A-89 p99 |
|---|---|---|---|---|---|
| 1 | create+write+close | 0.21 ms | 0.38 ms | 1.23 ms | 0.86 ms |
| 1 | open+read+close | 26 µs | 0.17 ms | 0.27 ms | 0.21 ms |
| 1 | stat | 2.3 µs | 54 µs | 0.10 ms | 79 µs |
| 16 | create+write+close | 2.3 ms | 4.2 ms | 5.7 ms | 13.3 ms |
| 16 | open+read+close | 0.86 ms | 3.1 ms | 4.4 ms | 2.9 ms |
| 16 | stat | 0.15 ms | 1.7 ms | 2.5 ms | 1.5 ms |

- A `perf record -a -g` of the daemon during the run before (linux-perf in the container, 4999 Hz) put a fifth of
  the serving shard's samples in `Volume::dir_entries`' sort under `publish_shard` → `image_of_inode`; the machine
  was 93.5% idle. After: mountstats OPEN 41 µs (from 0.32 ms), WRITE 30 µs, CLOSE 23 µs; the daemon's local p99
  11 µs (from 0.49–0.59 ms). One-writer throughput 5,249 → 9,792 operations per second.
- Readdir of the 8,000-entry directory: p99 6.7–7.9 ms against tmpfs's 0.9 ms. Mountstats in a rerun: 9 READDIRs
  for 80 listings, mean round trip 0.556 ms. The client serves most listings from its cache, and the tail is a
  refetch after the directory changed.

#### Against Linux's own kernel nfsd (2026-10-05)

Command: `knfsd.sh WORKERS FILES` (scratch; the same storm against `nfs-kernel-server` exporting a tmpfs over loopback
NFSv4.2 in a privileged `python:3.12-slim-trixie` container, `nfsv4gracetime` 10 s and a warm-up create before
timing), beside `native.sh` on the same kernel. Load average 4.6–5.7. One run each.

| workers | op | knfsd p50 | knfsd p99 | slates p50 | slates p99 |
|---|---|---|---|---|---|
| 1 | create+write+close | 212 µs | 427 µs | 159 µs | 286 µs |
| 1 | open+read+close | 114 µs | 232 µs | 24 µs | 122 µs |
| 1 | stat | 2.5 µs | 10 µs | 2.2 µs | 41 µs |
| 1 | readdir | 0.52 ms | 4.2 ms | 0.35 ms | 6.5 ms |
| 16 | create+write+close | 2.0 ms | 3.6 ms | 1.6 ms | 3.2 ms |
| 16 | open+read+close | 1.0 ms | 3.6 ms | 0.85 ms | 3.2 ms |
| 16 | stat | 125 µs | 1.7 ms | 123 µs | 1.7 ms |

- Reads are faster under slates because it grants read delegations: the client opens without a round trip. knfsd
  returned 88 delegations in the run, slates 3,001.
- The stat tail is the Linux client's delegation watermark (`nfsv4.delegation_watermark` = 5000 on this kernel): past
  5,000 held delegations it returns them at close, then refetches the attributes. DELEGRETURN 3,001 and GETATTR 3,244
  against knfsd's 82.
- Readdir: slates sent 9 READDIRs where knfsd sent 5, each a quarter full (A-90). After A-90, a dedicated loop
  (`lsloop.py`: 8,000 files, then 300 rounds of create-one-and-list, 8,300 entries per listing) measures slates'
  listing p50 7.76 → 3.6–3.9 ms (p99 4.3–4.5 ms) against knfsd's 1.95–4.6 ms (p99 2.2–24 ms, one noisy run).
  The remaining gap was the v4 page passing through a v3 reply's encoding and decoding, closed by A-95:

| server (load average 6.9–8.9) | READDIRs per listing | bytes per READDIR | round trip | listing p50 | listing p99 |
|---|---|---|---|---|---|
| slates before A-95 (4 runs; 1 and 4 shards) | 4 | 128,812 | 0.71–0.74 ms | 4.05–4.28 ms | 4.81–5.08 ms |
| slates after A-95 (4 runs; 1 and 4 shards) | 4 | 128,812 | 0.38–0.41 ms | 2.65–2.89 ms | 3.13–3.81 ms |
| Linux knfsd over tmpfs (2 runs) | 4 | 125,874 | 0.34–0.35 ms | 2.38–2.47 ms | 3.20–3.31 ms |

  Command: `docs/wip/bench/listing/ls-ab.sh` / `knfsd-ls.sh` (the listing loop: 8,000 files, then 300 rounds of create-one-and-list
  under a fresh prefix, so every listing refetches; an earlier version re-created existing names, and its listings
  came from the client's cache at 0.35 ms). Per-READDIR figures are mountstats'.
  **Measured and rejected:** one shard against four, to rule out the routing hop: 0.71 against 0.74 ms per round trip.

### Remote pulls under delay, loss, a slow or silent holder, and many readers (condition 7; 2026-10-05)

Command: `cargo run --release -p slates-cluster --example fetch_bench` (the A-91 fetch; Apple M5 Max; simulated
network, virtual clock, so the numbers are deterministic and independent of the host's load). Readers fetch a 4 MiB
archive in 64 KiB chunks from its recorded holders over real endpoints (TLS 1.3, packet protection, the session
plane's congestion controller). Each holder sends through its own uplink, shared by every reader it serves, with a
queue of one bandwidth-delay product; each session's receive ceiling is twice its path's BDP. A warm-up fetch sets
the hedge at its chunks' p95, then the timed fetches start together. Completion includes the manifest's round trip.
Every reader's archive was rebuilt byte for byte.

| scenario | holders | readers | RTT | uplink | completion (min / median / max) | goodput / capacity | hedges | steals |
|---|---|---|---|---|---|---|---|---|
| lan | 1 | 1 | 1 ms | 1 Gbit/s | 38.9 ms | 863 / 1,000 Mbit/s | 0 | 0 |
| lan | 3 | 1 | 1 ms | 1 Gbit/s | 24.4 ms | 1,375 / 3,000 | 42 | 4 |
| wan | 1 | 1 | 80 ms | 100 Mbit/s | 653 ms | 51 / 100 | 0 | 0 |
| wan | 3 | 1 | 80 ms | 100 Mbit/s | 456 ms | 74 / 300 | 0 | 21 |
| wan, 1% loss | 1 | 1 | 80 ms | 100 Mbit/s | 717 ms | 47 / 100 | 0 | 0 |
| wan, 1% loss | 3 | 1 | 80 ms | 100 Mbit/s | 623 ms | 54 / 300 | 0 | 19 |
| wan, 5% loss | 3 | 1 | 80 ms | 100 Mbit/s | 690 ms | 49 / 300 | 0 | 13 |
| wan, one holder at 1/20 rate | 3 | 1 | 80 ms | 100 Mbit/s | 943 ms | 36 / 205 | 0 | 26 |
| wan, one holder silent | 3 | 1 | 80 ms | 100 Mbit/s | 981 ms | 34 / 300 | 0 | 26 |
| wan, 8 readers | 3 | 8 | 80 ms | 100 Mbit/s | 1,055 / 1,660 / 1,717 ms | 156 / 300 | 0 | 202 |
| wan, 8 readers, 1% loss | 3 | 8 | 80 ms | 100 Mbit/s | 1,406 / 1,506 / 1,751 ms | 153 / 300 | 0 | 202 |

- Before A-91 a fetch asked one holder: a silent one cost the takeover's whole period (the scenario cannot
  complete), and the first striped version without stealing took 2,177 ms with a holder at 1/20 rate (the p95 had
  learned the slow holder's latency as normal, so no hedge fired in time).
- **Measured and rejected:** a pulled per-holder window starting at two chunks and growing by one per answer (one
  holder at 80 ms: 1,118 ms against 653 ms; a second slow start on top of the transport's); that window sized from
  the reader's congestion window (12 s: the reader's window governs its requests, not the replies).
- Loss costs 10–50% at 80 ms. A 4 MiB object at 80 ms is dominated by the manifest's round trip and slow start, so
  goodput against capacity is low for one reader and rises with eight (52%); larger objects are owed in the grid.

### Real workloads through Linux's own NFS client, beside tmpfs (conditions 2 and 5; 2026-10-05)

Command: `docs/wip/bench/realworld_native.sh` in a privileged `rust:1.98.0` container (the header gives the `docker run`):
the daemon and the kernel's NFSv4.2 client in one Linux (Docker Desktop 6.12 kernel, 18 CPUs, 7.8 GB), a dynamic 3 GiB
volume, and a tmpfs of the same size as the reference. Apple M5 Max, load average 2.9–5.1. Release build after A-93.
Three runs; each cell is one run, in order.

| workload | tmpfs | slates |
|---|---|---|
| `git clone --depth 1` ripgrep | 0.84, 0.87, 0.89 s | 0.97, 0.98, 0.97 s |
| `git fsck --full` | 0.02, 0.01, 0.02 s | 0.02, 0.02, 0.02 s |
| `cargo build --release` (regex, serde, serde_json; registry and target on the volume) | 3.09, 3.28 s | 3.28, 3.34 s |
| `pip install requests flask` into a venv | 2.32, 2.09, 2.44 s | 2.15, 2.15, 2.16 s |

- Correctness: the clone's tree hash (SHA-256 over every tracked file) is identical on both, `git fsck --full` passes, the
  built binary runs and prints the same answer, and the file lists are identical except cargo's three lock files under
  `target/release` (`.cargo-lock`, `.cargo-build-lock`, `.cargo-artifact-lock`), which tmpfs has and slates does not.
  That matches cargo's own behaviour on NFS (from memory, to verify against cargo's `flock.rs`: it skips its file
  locks on an NFS mount, where `flock` can block forever, so it never creates them); no other file differs.
- **No write reaches a disk** (condition 3), traced: `docs/wip/bench/realworld_trace.sh` runs the same workloads with
  `strace -f` on the anchor and the daemon for every `open`/`openat`/`openat2`/`creat`, `mkdir`, `rename`, `unlink`,
  `link`, `symlink`, `truncate` and `memfd_create`. Through the clone, the build and the install (about 2,640 files written
  to the volume) the two processes made **no** opening with `O_CREAT`, `O_WRONLY`, `O_RDWR` or `O_TRUNC` and **no**
  namespace change; their only opens were kernel pseudo-files, read-only: `/proc/meminfo` 108 times (the memory
  pressure sampling, about seven a second) and `/proc/sys/vm/overcommit_memory` once.
- Cost: the clone is about 12% slower (0.97 s against 0.87 s), the build about 4%, and pip within noise. The first
  cargo build failed on both sides in an earlier run, a harness error (a second `[dependencies]` table), fixed before
  these numbers.

### The daemon SIGKILLed in the middle of real workloads (condition 11; 2026-10-05)

Command: `docs/wip/bench/realworld_chaos.sh`, run the same way as `realworld_native.sh` (same machine, same release
build, one run). The daemon (never the anchor) is SIGKILLed twice during each workload, at the delays shown. The
anchor restarts it over the RAM it holds, and the kernel's hard NFSv4.2 mount retries across the gap.

| workload | killed at | wall time (undisturbed, above) | result |
|---|---|---|---|
| `git clone --depth 1` ripgrep | +0.3 s, +0.6 s | 0.97 s (0.97 s) | exit 0; `git fsck --full` passes; tree hash `31a1f6f713d195f5`, the same as on tmpfs |
| `cargo build --release` (regex, serde, serde_json) | +1.0 s, +1.5 s | 3.58 s (3.28–3.34 s) | exit 0; the binary runs and prints `{"n":2}`, as on tmpfs |
| `pip install requests flask` into a venv | +0.5 s, +0.8 s | 3.09 s (2.15 s) | exit 0; `import flask, requests` succeeds (requests 2.34.2) |

- The anchor's account: generation 7 and 6 restarts, one per kill. No workload saw an error, and none was retried by
  the harness. The kernel resent each request lost in a gap, and the restarted daemon answered it over the same RAM.
- Cost of two restarts: about 0.3 s for the build and 0.9 s for pip, including the kernel's retransmit wait. The clone
  showed none within its run-to-run spread.
- The first attempt hung in the harness, not in slates: a bare `wait` also waited for the anchor, which never exits.
  The script now waits on the killer's pid.

### Linux FUSE mount: small creates, and a containerd/runc binding (conditions 2, 5, 12; 2026-10-05)

Command: `docs/wip/bench/fuse/fuse-perf.sh` (2,000 × open O_CREAT, write, close, stat through `slates mount` on Linux;
Docker Desktop 6.12 kernel, Apple M5 Max). Per-step p50 / p99; each row one run.

| build (load average) | 2,000 creates | open | write | close | stat |
|---|---|---|---|---|---|
| tmpfs | 0.01 s | 1 / 3 µs | 1 / 2 µs | 0 / 0 µs | 1 / 1 µs |
| before A-96 (≈ 8) | 0.64 s | 106 / 631 µs | 43 / 117 µs | 56 / 145 µs | 82 / 169 µs |
| delta references (9–10) | 0.44 s | 75 / 136 µs | 34 / 68 µs | 41 / 91 µs | 60 / 100 µs |
| delta references (9–10) | 0.36 s | 51 / 210 µs | 22 / 114 µs | 28 / 118 µs | 41 / 154 µs |
| + op log indexed (4–7) | 0.36 s | 63 / 173 µs | 28 / 92 µs | 35 / 93 µs | 49 / 121 µs |
| + op log indexed (4–7) | 0.45 s | 69 / 219 µs | 31 / 107 µs | 37 / 137 µs | 52 / 176 µs |

containerd: `docs/wip/bench/containerd/run.sh` runs containerd 1.7.24 and runc 1.1.15 (Debian trixie) and applies the
exact entry `slates attach --oci` returns (`{"type":"bind","options":["bind","rw","private","nosuid","nodev"]}`) through
`ctr run`, into alpine 3.20:
- 2,000 files, a hard link, a symlink, and a tar round trip: exit 0, 2,002 entries extracted, and the same tar hash
  read from the host side of the mount.
- The container sees `/work` as `rw,nosuid,nodev`.
- The read-only entry's write is refused with EROFS.
- Wall time, measured only under another session's load (average 33–87): 5.3–17 s against a tmpfs bind's 0.26–0.55 s.
  The per-file gap against Python's loop is owed (GAPS).

### Opening one sealed 4 KiB block of idle RAM (condition 9, A-92 piece 5; 2026-10-05)

Command: `cargo run --release -p slates-cluster --example seal_idle_bench` (hyper-seal at hyper-raft `f9a2c8e`, AWS-LC
AES-256-GCM; 100,000 opens per case, each timed alone). Apple M5 Max; load average 9.6–10.9 from other sessions.

| case | macOS p50 / p99 / p999 | Linux (Docker VM) p50 / p99 / p999 |
|---|---|---|
| timer alone | 0 / 42 / 84 ns | 0 / 42 / 42 ns |
| warm open, version key | 625 / 667 / 709 ns | 584 / 792 / 4,958 ns |
| warm open, file opener made | 625 / 833 / 1,167 ns | 583 / 750 / 2,417 ns |
| cold open (unwrap + commitment + open) | 1,375 / 1,459 / 2,000 ns | 1,417 / 1,875 / 13,833 ns |
| plaintext 4 KiB copy | 42 / 209 / 1,000 ns | 42 / 84 / 125 ns |

The warm open meets A-92's budget (at most about 1 µs p99 under load) on both systems, so idle RAM is sealed under a
version key kept warm per mounted volume; a cold open per access would not meet it.

### A-99 sealed read: idle content sealed in the arena, read through the chokepoint (condition 9; 2026-10-05)

Command: `cargo run --release -p slates-vfs --example sealed_read_bench` (64 MiB of full chunks sealed into one
`ChunkStore` under hyper-seal's `VersionKey`, the server's cipher, and into another in the clear; 200,000 random 4 KiB
reads at granule-aligned offsets through `read_extent_into`, each timed alone; then the whole 64 MiB read; three rounds
per run). Apple M5 Max, macOS 26.4.1; load average 16–18 from other sessions. The A/B binaries were run interleaved,
five runs each (`ab.log` in the session scratch).

| build | sealed 4 KiB p50 | sealed p99 (median of 15) | sealed p99 range | whole read | clear 4 KiB p50 / p99 |
|---|---|---|---|---|---|
| A: every segment opened in a stack scratch, then copied out | 792–875 ns | 1,083 ns | 1,000–1,333 ns | 5.66–6.42 GB/s | 209–291 / 417–625 ns |
| B: a segment the read covers whole opened in the caller's buffer | 667–750 ns | 916 ns | 875–1,250 ns | 6.92–7.69 GB/s | 209–250 / 417–458 ns |

B lands: every sealed row of B beats every row of A on p50 and throughput, and its median p99 is inside the 1 µs
budget (A-92). A is measured-and-rejected: it zeroed a 4 KiB stack buffer and copied each segment twice. The cost left
is AES-256-GCM itself (about 7 GB/s on one core, so about 580 ns per 4 KiB), against A-92 piece 5's warm open p99 of
667 ns for one segment alone. Same day, the tag store moved from one buddy pool built whole to a lazily grown slab of exact-length runs (for shard
start time, SLATES_DESIGN A-99). Read path, interleaved, three runs each at load average 8.8–9.0: buddy p50 625–667 ns,
p99 875–1,250 ns, 6.85–8.14 GB/s; slab p50 625–667 ns, p99 875–1,500 ns, 7.38–8.13 GB/s. Even: the change is not a
read-path change.

The bench's first run failed `Capacity`: it made a key before locking hyper-seal's key
region; it now calls `hyper_seal::lock_keys` first, as the daemon does at boot.

### Zero on free, and the dirty set's heap (A-99, A-68; 2026-10-05)

Command: `cargo run --release -p slates-vfs --example vfs_bench`, two binaries (the scrub on, the scrub a no-op), three
runs each, interleaved; Apple M5 Max, load average 8–11.

| row | scrub off (3 runs) | scrub on (3 runs) |
|---|---|---|
| write 4 KiB into a fresh window then truncate it away | 833–937 ns | 916–1,104 ns |
| write 4 KiB in place, same epoch | 458–500 ns | 437–479 ns |
| read 4 KiB | 130–145 ns | 122–135 ns |
| create burst of 190,000 files, per file | 1,516–1,583 ns | 1,523–1,644 ns |
| destroy per unit, one slice, 1M files | 22–23 ns | 22–25 ns |

The scrub costs one memset of a freed block (about 100 ns for a 4 KiB granule) on the paths that free one; it lands for
the at-rest guarantee. AC-1.5 at a million files, bisected over `git archive` builds: `cbde00a` 443,572,350 B,
`baced10` 443,572,350 B, `0799cc1` (A-68) 578,134,114 B, HEAD before the fix 586,356,834 B (over the 553 B/file budget),
after 451,795,070 B (within).

### The idle sweep on real installs through Docker (condition 9; 2026-10-05)

Command: `e2e-installs.sh 1` (session scratch): a release `slates anchor --quick --shards 4`, a 2 GiB bounded volume
mounted at a host path and bound into containers (`slates attach --oci-source`), then `npm install express lodash
typescript` (node:20-alpine), `pip install requests flask` in a venv (python:3.12-alpine) and a 2,000-file tar round
trip (alpine:3.20), each also on a container tmpfs. Apple M5 Max, Docker Desktop, load average 16–33.

| build | npm files | pip files | tar files | chunks sealed (`content.sealed`) | daemon RSS max |
|---|---|---|---|---|---|
| before the idle sweep (`7a75b32`) | 2,182 (tmpfs 2,182) | 1,390 (1,385) | 2,000 (2,000) | 109 | 117 MiB |
| with the idle sweep (`37e4e85`) | 2,182 (2,182) | 1,392 (1,385) | 2,000 (2,000) | 2,870 | 126 MiB |

Times (s, slates / tmpfs): npm 10.4 / 22.8, pip 16.8 / 9.7, tar 7.5 / 0.9 in the second run. The extra pip files are
NFSv3 silly renames held by Docker Desktop's virtiofs share (GAPS).

### Directory blocks against real trees (§4.5, AC-1.5, AC-1.8; 2026-10-05) — 1 KiB adopted (A-101), heap buffers rejected

Real trees' entries per directory (`dirhist.sh`, session scratch: `ls -A` counts under every directory):

| tree | dirs | entries | median | p90 | p99 | dirs of 3–56 entries (one 4 KiB block each) |
|---|---|---|---|---|---|---|
| npm (express, lodash, typescript; node:20-alpine) | 158 | 2,340 | 4 | 13 | 216 | 103 |
| pip venv (requests, flask; python:3.12-alpine) | 185 | 1,573 | 6 | 18 | 27 | 153 |
| this repository's `crates/` | 133 | 823 | 4 | 14 | 30 | 86 |

A directory of three or more entries holds a whole 4,112-byte block, so the pip venv's blocks alone are about 400 heap
bytes an entry (the bench's 36-entry directories of 49-byte names fill their blocks and hide it).

Tried: a block's bytes as a heap buffer, a power of two from 256 B to 4 KiB, doubling as it fills and shrinking at
compaction (the tree's split and merge rules unchanged). `vfs_bench`, three runs each against the previous build,
load average 19–24: the bench's AC-1.5 unchanged (352,335,998 → 352,721,118 B: its directories fill a page anyway),
lookups and readdir even, and AC-1.8's longest destroy slice 7–14 µs → 2.2–4.3 ms, all of it inside `dealloc` (the
allocator returning pages: the stall `dirtree`'s module doc recorded when it put blocks in slab slots). Rejected.
Then the block size itself, on the real trees (`cargo run --release -p slates-vfs --example tree_heap`: npm, pip and a
cargo `target/release`, 64 copies each, every heap byte counted), heap bytes an entry:

| block | npm | pip | cargo |
|---|---|---|---|
| 4 KiB (was) | 464 | 662 | 743 |
| 2 KiB | 374 | 465 | 527 |
| 1 KiB (adopted) | 332 | 368 | 418 |

`vfs_bench` two runs each, 4 KiB → 1 KiB: lookup in a tree directory of 128 265–276 → 224–244 ns, of 16 229–239 →
203–250, a 36-entry readdir 479–604 → 510–687, the create burst 1,663–1,697 → 1,765–1,818 ns a file, AC-1.8 destroy
slices within budget either way, AC-1.5 352,335,998 → 383,350,910 B (the bench's 36-entry directories). Still owed: owed instead: size-classed block slabs (256 B … 4 KiB buffers, each class a slab whose slots are never returned per
item), a block naming its buffer by class and handle: XFS's progression from in-inode to one block to leaf and node
forms (Sweeney et al., USENIX 1996) without the global allocator.

### An idle daemon's cost, and the real workloads again under a TCP-exhausted Docker VM (2026-10-05, afternoon)

Idle (`idle-cpu.sh 60`, session scratch): a release `anchor --quick --shards 4` with one mounted volume and no client
activity, after the idle sweep, zero on free and A-101: over 60 s the daemon used 0.04 CPU-seconds (about 0.07% of one
core) and the anchor 0.00; `top -pid` reported 0.0% CPU, 0 idle wakeups and 0.0 power. The reap tick (1 s) with its
sweep and the 10 Hz heartbeat cost nothing measurable at rest.

Again on 2026-10-07 at `5ef66e4e` (after the streamed checkpoint, the copy-aside seal and the counted publish paths;
`idle-cpu.sh`, 30 s windows, load average 6.4–8.0): idle with nothing mounted, daemon 0.07% of a core and anchor
0.03%; with a volume mounted and idle, daemon 0.03% and anchor 0.00%. Unchanged at rest.

Real workloads, rerun at 15:12–15:22 on HEAD (`realworld_chaos.sh`, `realworld_native.sh`): correct throughout (six
SIGKILLs, clone tree hash `31a1f6f713d195f5`, the build runs, pip imports), but slow: cargo build 115–134 s on slates
against 13.5 s on tmpfs, where the morning's record was 3.3 against 3.2. Bisected over Linux release builds of
`ad76b3b` (the morning's commit), `f989cc5` and `24b536a`: all three take 100–105 s on slates against 3.6–4.4 s on
tmpfs, so the slowdown is not in slates' code since then. `strace -c -w` on the build: `close` 71 ms a call, `openat`
34 ms, `statx` 18 ms on slates, microseconds on tmpfs, while the daemon served each request in p50 2.8 µs, p99 82 µs.
`TCP_NODELAY` is set on both sockets (`crates/rt/src/tcp.rs`). The Docker VM's `/proc/net/sockstat`: TCP `mem
189245` pages against `tcp_mem` `93192 124257 186384` — past the hard limit, 1,658 TCP sockets allocated, none of
them in the VM's host namespace (4 listening there, no NFS mounts). Every loopback RPC is throttled; numbers from this
VM are not comparable until it is reset (restarting Docker Desktop stops other sessions' containers).

### x86_64 Linux under emulation (condition 6; 2026-10-05)

`docker run --platform linux/amd64 rust:1.98.0` on the M5 Max (Docker Desktop's x86_64 emulation; `uname -m` x86_64),
HEAD after A-101, its own target and registry volumes, tests run as a non-root user; load average 18–60 from other
sessions:
- `cargo test -p slates-mem -p slates-vfs`: 316 passed, 0 failed (the content seal with its tag slab, the idle sweep,
  zero on free, the 1 KiB directory tree and its oracle, the boxed body variants).
- `slates-server --test seal`: 2/2; `--test daemon`: 19/19 (two failed in a parallel run on `Stalled` and passed alone).
- `slates-server --test recovery` in parallel: 8/26, every failure a client `Stalled` after 1 s or a wall-clock check;
  serially (`--test-threads=1`): 25/26, the sealed restart (AES-256-GCM through aws-lc's x86_64 code) among them. The
  last was a test defect: `two_daemons_in_one_process_measure_memory_pressure_from_their_own_start` injected the
  available memory but read the process's real resident memory, which the hold subtracts since `0db78cb`; it now pins
  both (3/3 natively, 3/3 under emulation).

### Remote pulls of 64 MiB, and three per-packet passes over every open stream removed (condition 7; 2026-10-05)

Command: `cargo run --release -p slates-cluster --example fetch_bench wan 1024` (the A-91 grid at 1,024 chunks of 64
KiB; simulated network, virtual clock; every reader's archive rebuilt byte for byte). Apple M5 Max.

| scenario | completion (min / median / max) | goodput / capacity | steals |
|---|---|---|---|
| wan 1 holder | 5,907 ms | 90.9 / 100 Mbit/s | 0 |
| wan 3 holders | 2,315 ms | 231.9 / 300 | 351 |
| wan 1 holder, 1% loss | 6,102 ms | 88.0 / 100 | 0 |
| wan 3 holders, 1% loss | 2,501 ms | 214.7 / 300 | 352 |
| wan 3 holders, 5% loss | 2,627 ms | 204.4 / 300 | 347 |
| wan 3 holders, one at 1/20 rate | 3,675 ms | 146.1 / 205 | 465 |
| wan 3 holders, one silent | 3,802 ms | 141.2 / 300 | 468 |
| wan 3 holders, 8 readers | 12,991 / 17,030 / 17,407 ms | 246.7 / 300 | 2,613 |
| wan 3 holders, 8 readers, 1% loss | 8,895 / 18,201 / 18,860 ms | 227.7 / 300 | 2,856 |

Getting there found three costs that grew with the open streams, each paid per packet or per poll (a 64 MiB pull did
not finish its first scenario in 9 minutes of CPU; sampled):
- `Endpoint::drain` read every open receive stream (68% of samples): now only those a stream frame reached
  (`Connection::take_readable`). The first 64 MiB scenarios: >9 min → 36 s of CPU, virtual results unchanged.
- `poll_transmit`'s credit checks compared every open stream's advertised credit with its ceiling: now a set of
  streams whose credit may have moved (opened, read, a peer's `StreamDataBlocked`; all of them when the window grows).
  36 → 11 s, virtual results identical. A first cut missed `StreamDataBlocked` and three loss oracles stalled.
- The fetch worker probed every open exchange (`take_reply`, which drains) each poll: now only the exchanges whose
  replies completed (`Endpoint::take_completed`). The 4 MiB grid 21 → 6.3 CPU-seconds.
The harness had three faults of its own, all fixed: names `f{at:03}` stopped sorting past 999 chunks, chunk contents
repeated every 256 chunks (a 64 MiB archive held 16 MiB of distinct chunks, so goodput read 230 Mbit/s on a 100 Mbit/s
link), and the warm-up hedged at the deadline where the daemon hedges at 100 ms with no readings (a silent holder
ranked first for the manifest froze it). Some rows varied run to run (5% loss, 8 readers with loss): the bench's self-signed ECDSA
certificates sign with a random nonce, so a signature's DER length varies (70-72 bytes) and with it the handshake's
packet sizes and every virtual timing after. With Ed25519 (always 64 bytes) three runs of the grid are identical to the
tenth of a millisecond.

### Codemode against list-and-read on a real agent task (condition 13; 2026-10-05)

Command: `cargo run --release -p slates-mcp --example codemode_tokens` (an in-process daemon, 2 shards; an overlay
volume over this repository's `crates/`; the MCP server driven as an agent drives it). Apple M5 Max, load average
about 6. The task: which Rust files mention `unsafe`.

| path | tool calls | JSON-RPC reply bytes | tokens (bytes ÷ 4, a heuristic) | wall time |
|---|---|---|---|---|
| `slates.fs.list` + `slates.fs.read` of every `.rs`, filtered by the agent | 734 | 28,808,365 | about 7.2 M | 143 ms |
| one `slates.query` (`FROM files(..) WHERE ext = "rs" AND content CONTAINS "unsafe" SELECT path`) | 1 | 4,150 | about 1,040 | 98 ms |

- The same 73 files either way (the example fails if the sets differ).
- 6,941× fewer bytes into the agent's context and 734× fewer calls. For a real agent the calls dominate: each one is
  a model turn.
- The reply bytes are about twice the 15 MB of source. Each result carries its structured content and, as the MCP
  specification recommends for compatibility, the same JSON as text.

### An overlay of a real tree: read, change, plan (condition 5; 2026-10-05)

Command: `bash <scratch>/overlay-diff.sh` (release build; `volume create --dynamic 4GiB --base crates/`, `slates mount`,
then the host tools through the mount; each row timed by a Python `subprocess` wrapper, the CLI process included).
Apple M5 Max, macOS's own NFS client, 2 shards, load average about 6.

| step | slates | host |
|---|---|---|
| create the overlay volume | 4 ms | — |
| list 688 files | 9 ms (cold and warm) | 10 ms |
| read every byte, 15.4 MB | 109 ms cold, 36 ms warm | 25 ms |
| change set after 10 appends, 5 creates, 3 removals | `land` plan 4 ms, exactly 10 replace / 5 create / 3 delete | `diff -rq` 29 ms, the same 18 |

- The writes became possible only with the same day's owner fix: before it, every base entry was root's through the
  mount and refused the user's writes (`docs/bugs/2026-10-05-base-entries-reported-root-as-owner.md`).
- The cold read, about 158 µs per file through the NFS client, is the cost to work on next.
- **Where the cold read goes:** 2,331 RPCs for 688 files (one ACCESS and one GETATTR per open, the macOS client's
  close-to-open checks, and 940 READs), about 42 µs each end to end, against a daemon-side service p50 of 3 µs. The
  client's per-call cost dominates, and the server cannot remove those calls under NFSv3.
- **Transfer size, 2026-10-05.** The macOS mount asked no `rsize`/`wsize`, so the client used 32 KiB. Swept in random
  order, four rounds each, a fresh daemon and mount per round (`<scratch>/ab-transfer.sh`):

  | transfer | 688-file tree, cold read | 64 MiB write + fsync | 64 MiB cold read |
  |---|---|---|---|
  | 32 KiB | 110–122 ms | 79–83 ms | 59–61 ms |
  | 64 KiB | 108–123 ms | 50–53 ms | 37–41 ms |
  | **128 KiB (chosen)** | 109–123 ms | 35–39 ms | 25–29 ms |
  | 256 KiB | 127–143 ms | 30–31 ms | 19–21 ms |

  - 128 KiB is the knee: large writes run 2.2× faster (about 1.8 GB/s) and large reads 2.3× faster (about 2.4 GB/s),
    and the small-file walk does not slow. At 256 KiB the client's own per-request cost slowed the walk by about 17%.
  - With it, a READ is sized to the bytes the file has past its offset, not to the asked count, and the bridge reads
    into the reply's buffer directly (no zeroed buffer of the asked size, no second copy). At 256 KiB, five rounds in
    random order: a median of 119 ms against 124 ms for the walk.

### An overlay of a real tree on Linux: read, change, plan (condition 5; 2026-10-08)

Command: `docker run --rm --privileged -e SLATES=/target/esc/release/slates -v slates-linux-target:/target:ro -v
"$PWD/docs/wip/bench/overlay":/o:ro rust:1.98.0 bash /o/overlay_diff.sh /usr/lib 3` (the Linux release build at HEAD
plus A-121). The tree is copied into tmpfs, then each round runs on a fresh volume and fresh mounts. Each round
creates the overlay (`--base`), then walks, cold-reads and warm-reads it through slates, through the kernel's own NFS
server exporting the same tmpfs (the same NFSv4.2 client, its floor) and on the host. It then makes 10 appends,
5 creates and 3 removals, and times `slates land` (the plan, presented, not granted) against `diff -rq` over a host copy
given the same changes. Docker Desktop 6.12 kernel, Apple M5 Max, load average 13.7 from other sessions.
/usr/lib: 5,863 files, 7,113 entries, 540.7 MB.

| step | slates | kernel NFS server | host |
|---|---|---|---|
| create the overlay volume | 2 ms | — | — |
| walk every entry | 157–268 ms | 135–158 ms | 4–5 ms |
| cold read, every byte | 1,761–1,806 ms | 2,066–2,199 ms | 91–92 ms |
| warm read | 672–734 ms | 1,344–1,580 ms | 90–99 ms |
| change set after 18 changes | `land` plan 2–3 ms, exactly 10 replace / 5 create / 3 delete | — | `diff -rq` 371–389 ms: 11 differ, 5 + 3 only-in, 8 dangling links |

- Before A-121 the same run (load 14) planned `symlink=657` besides the 18, one per untouched base symlink
  (`docs/bugs/2026-10-08-base-symlinks-were-planned-as-changes.md`). Reads in that run: cold 1,398–1,544 ms against
  1,438–1,666 ms for the kernel's server, warm 571–650 against 988–1,548.
- Where the warm read goes: slates hands out read delegations, so its warm pass sends 1,560 opens against 5,863 for the
  kernel's server. The client returns the delegations it cannot keep (`DELEGRETURN` 1,560), and no data READ is sent
  while the page cache holds the tree. At load 27 (2026-10-08, one round) the page cache did not hold it, and both
  servers re-read about 5,500 READs.
- `diff -rq` is the less exact baseline: it follows a symlink alias to an appended file (11 differ for 10 appends)
  and cannot compare a dangling link. The plan compares entries, not what they point to.
- **Outsider edits, 2026-10-08** (the bench's outsider step; Linux: `overlay_diff.sh /usr/lib/python3.13 1`, 1,252
  files; macOS: `overlay_mac.sh <release> crates/vfs <scratch>`, 79 files over macOS's NFSv3 client). An outsider
  appends to a file the volume replaced and to one it never touched, after the client has read it:

  | | Linux NFSv4.2 (delegations held) | macOS NFSv3 |
  |---|---|---|
  | untouched file's new bytes through the mount | 157 ms | 377–520 ms |
  | replaced file | `(Modified)` drift; plan presents `ModifyModify` | same |
  | recalls for the one untouched edit | asked 1, sent 1, answered 1 (A-122, A-123) | — (no delegations in v3) |

  Before the day's fixes, the Linux edit was not seen after 5.2 s: 153 recalls asked, 152 unanswered.
  On macOS over a tree with symlinks (`overlay_mac.sh <release at 07a50710> /usr/lib <scratch>`, 498 files, 29
  symlinks), A-121 holds through macOS's own client:
  - the plan was `create=5 delete=3 replace=9`, with no `symlink` entries; one pick was read-only, and its append
    was refused alike through the mount and on the host copy;
  - `diff -rq` printed 40 lines for the same changes;
  - the outsider's edit was seen after 875 ms, and the replaced file was presented `ModifyModify`.
  At `/usr/lib` scale (`overlay_diff.sh /usr/lib 3` at `0e4f378b`, 5,863 files, load spikes from other sessions):
  - over the run, 13,244 delegations granted;
  - the outsider step asked 1 recall, sent 1 and had it answered, and the new bytes read through the mount after
    121 ms;
  - the replaced file was presented `ModifyModify`;
  - every round's plan was exact in 1–3 ms (`diff -rq` 333–479 ms);
  - cold reads: slates 1.7–2.8 s, the kernel's server 1.7–6.5 s.

### The daemon under a container memory cap, and large writes over Linux's NFS client on loopback (conditions 11, 12; 2026-10-05)

Commands: `docs/wip/bench/pressure/memory-cap.sh` (privileged `rust:1.98.0`, `--memory 1g --memory-swap 1g`, the Linux
release build at /target, an output directory at /out), `fsync-trace.sh` and `knfsd-fsync.sh` (the same shape, the
last in `python:3.12-slim-trixie`). Docker Desktop 6.12 kernel, Apple M5 Max, load average 5–9.

- **Under a 1 GiB cgroup cap** (`memory.max` 1073741824), 1 MiB files written with `fsync` through the kernel's NFSv4.2
  client until refused:
  - the 58th was refused `ENOSPC`, typed;
  - all 57 written files read back with their SHA-256 intact;
  - after deleting half, an 8 MiB write succeeded;
  - the daemon was alive, the anchor recorded 0 restarts and 0 panics, and `memory.events` showed `oom_kill 0`;
  - status answered throughout.
  One volume (on one shard) held 57 MiB of a 1 GiB container. **Wrong in the first version of this entry:** the
  cap did not size the reserve at 128 MiB. The reserve was 170.7 MiB (1 GiB ÷ 2 shards ÷ 3 classes), and the buddy
  arena used only its largest power-of-two part, 128 MiB, as one region. A volume's ceiling being its owner shard's
  reserve is owed in GAPS (A-98).
- **The reserve's whole length, 2026-10-05** (the same command, `FILES=200`, load average 8.4): the arena range is now
  cut into power-of-two regions on the mapping granule, largest first (`daemon.rs` `arena_parts`), so the whole
  reserve is allocatable. Region 0 keeps its old base and length, so an image written before still names its blocks.
  - One volume now holds **124 MiB** before the 125th 1 MiB file is refused `ENOSPC`, against 57 MiB before.
  - The first 124 files read back with their SHA-256 intact; after deleting half, an 8 MiB write succeeded.
  - Daemon alive, 0 restarts, 0 panics, `oom_kill 0`.
  - The gain is more than the 42.7 MiB tail. The operation headroom is capped by the arena's capacity, so with a
    smaller capacity the earlier run lost a larger share of it; the split between the two is not measured.
  - Test: `a_volume_past_the_reserves_power_of_two_part_fills_and_survives_a_restart` (recovery.rs).
- **The shared extent pool (A-98), 2026-10-05** (the same command, `FILES=600`, load average 5.3–6.6, commit
  `f989cc5`): one volume now draws on both shards' slices.
  - One volume holds **264 MiB** before the 265th 1 MiB file is refused `ENOSPC`, against 124 MiB with a shard's
    own slice and 57 MiB this morning.
  - The first 200 files read back with their SHA-256 intact; after deleting half, an 8 MiB write succeeded. Daemon
    alive, 0 restarts, 0 panics, `oom_kill 0` under the 1 GiB `memory.max`.
  - The two slices hold about 341 MiB. What takes the other 77 MiB (the operation headroom, the control shard's own
    claims, a slice's tail under one chunk) is not yet measured.
  - Writes ran at the fresh-mount stall's pace (WRITE `avg_exe` 398 ms over 1,092 ops), the owed item below, so the
    run took 166 s.
- **The pressure hold no longer counts the daemon's own growth, 2026-10-05** (the same command, `FILES=600`):
  - One volume holds **338 MiB**, with 355.2 MB committed of the 357.8 MB pool (the rest is the operation headroom),
    against 218–264 MiB before.
  - The first 200 files' SHA-256 intact; after deleting half, an 8 MiB write succeeded. Daemon alive, 0 restarts,
    0 panics, `oom_kill 0`.
  - The cause of the missing 77 MiB, found with diagnostic lines at the refusal: a hold of 111 MB per shard for memory
    the daemon had itself filled (`docs/bugs/2026-10-05-pressure-hold-counted-the-daemons-own-growth.md`). A 48 MiB reserve
    admits a 36 MiB volume and holds 34 MiB of files across a restart, byte for byte, on macOS and Linux. Before the
    change, the create was refused `BudgetExceeded { available: 22 MiB }`.
- **Large writes on a fresh mount stall** at 200 ms steps, for slates and for Linux's own knfsd alike:

  | server | 1 MiB write + `fsync` | `dd` 100 MiB, `conv=fsync` |
  |---|---|---|
  | slates, fresh NFSv4.2 mount | 621–627 ms | 1.2–8.3 MB/s |
  | slates, the same mount after other mounts moved 200 MB | 0.6–0.9 ms | 241–471 MB/s |
  | Linux knfsd over tmpfs, fresh NFSv4.2 mount | 206–208 ms | 4.9 MB/s |

  - The kernel's tracepoints and `ss` show the mechanism. The client sends four 256 KiB WRITEs at once (slots 0–3);
    slates answers them about 207 ms apart, knfsd with one such step. The daemon reads everything available and
    meets `EAGAIN`, then nothing arrives for about 205 ms (`strace`).
  - The client's socket has a 4,608-byte send buffer, a congestion window collapsed to 2, and retransmissions on a
    201 ms timeout. The namespace counts `TCPRcvQDrop`, `TCPZeroWindowDrop`, `TCPOFODrop` and `TCPDelayedACKLost`,
    and the daemon's socket its own drops (`d4`–`d5`) with an autotuned 2.2 MB receive buffer and nothing queued.
  - So this is Linux's NFS client over loopback in this VM, which knfsd meets too; slates meets it three times per
    flush where knfsd meets it once. Why three is owed.
  - **The VM was over its TCP memory limit (found later the same day; these numbers are not evidence about slates).**
    Inside a fresh container, `/proc/net/sockstat` read `TCP: mem 189277` pages (739 MB). The VM's `tcp_mem` is
    `93192 124257 186384`, so the allocation was above the hard maximum, and the kernel holds every TCP socket in the
    VM near its minimum receive allowance. That fits what was seen: the daemon's socket dropped segments while its
    queue was empty and its buffer autotuned to 2.2 MB, and over three flushes the counters were `TCPRcvQDrop` 9,
    `PruneCalled` 137 and `TCPFromZeroWindowAdv` 137, deterministically.
    - What holds the memory is not certain. No process-visible namespace held more than a trivial queue, and two of
      this session's containers (`5a5e7b603590`, `ae3a4ede408f`) have been wedged in `do_exit` on their own `hard`
      NFS mounts since 01:36 and 02:30. The memory note on NFS wedges records them as recoverable only by a Docker
      Desktop restart, which would also stop other sessions' containers, so it is Ada's call.
    - knfsd's one stall was measured in the same VM, so the slates/knfsd comparison is void too. The large-write
      numbers stand only once a clean kernel is measured.
  - **A clean kernel, the same day: macOS's own NFS client** (release build, `slates mount` of a fresh dynamic volume,
    2 shards, Apple M5 Max, load average about 6). Each 1 MiB `write` plus `fsync` took 1.2–1.3 ms over five rounds,
    and 64 MiB plus `fsync` took 81 ms (789 MB/s). There was no stall: slates' write path shows none when the kernel's
    TCP memory is healthy. Linux still owes its own clean measurement.
  - **Measured and rejected 2026-10-05: draining the socket before serving.** All queued bytes were read into the
    connection's buffer (bounded by the session offer, `max_request × max_requests`) before any call was served.
    The result was unchanged: 621/623/625 ms against 621/623/624 ms, and the same 9 drops and 137 prunes. The daemon
    already keeps its queue empty, so the receive side is not where the drops come from.
  - **Measured and rejected:**
    - `SO_RCVBUF` raised on accepted streams: `fsync` still 622–626 ms and `dd` 1.2 MB/s (the option caps the buffer
      at `rmem_max` and turns autotuning off);
    - `TCP_QUICKACK` after every read: `fsync` 621–626 ms, `dd` 1.2 MB/s.
    Neither is in the tree.


## Session-plane ready sets: a fresh frame no longer walks idle streams (2026-10-06)

**What changed.** The connection's fresh-frame scheduler (strict priority by class, round-robin within one)
found its next stream by scanning every send stream of the class from a cursor. A send stream stays in that
list after its data is framed, until its last frame is acknowledged, so every drained-but-unacknowledged
exchange (and a server's requests whose replies are not yet written) was stepped over for each frame. The
scheduler now keeps, per class, an ordered set of the streams that can frame now, keyed by install order. A
stream joins when it becomes sendable (installed, granted stream credit, admitted by the peer's
`MaxStreams`) and leaves when framing empties it or it is forgotten. A frame costs O(log n) in the ready
streams, whatever the idle ones.

**Work witness** (`SendStops::examined`, test
`a_fresh_frame_costs_the_same_beside_any_number_of_idle_streams`): one bulk frame examined 1 stream beside
no idle streams and 25 beside 24. Now 1 in both. Stream completion also no longer does an O(n) `retain` of
the class list.

**Service order, A/B on the class grid.** Command: `cargo run --release -p slates-transport --example
class_latency_bench`, `SEEDS` set to 1–20 in a scratch copy only (260 runs a build). Virtual time; Apple M5
Max, load 80, which does not change virtual-time numbers. Per-scenario medians, geomean of new/old:

| Build | Control p99 | Metadata p99 | Bulk goodput | Worst scenario |
|---|---|---|---|---|
| Pure rotation (rejected) | ×0.940 | ×1.015 | ×0.993 | 100 Mbit/s, 20 ms, 1 % loss: control ×1.37, metadata ×1.56, bulk ×0.84 |
| Next after last served, oldest after a completion (kept) | ×0.989 | ×0.915 | ×1.024 | 100 Mbit/s, 100 ms, 1 % loss: control ×1.06, bulk ×1.11 |

**Noise floor.** Two runs of the old build agree on 258 of 260 rows. The grid was not reproducible at all
before this change: its self-signed ECDSA certificates sign with a random nonce, the DER signature varies
from 70 to 72 bytes, and that moves the handshake's packet sizes and every virtual timing after. Two old
runs agreed on 25 of 39 rows. The class and congestion grids now use Ed25519 (always 64 bytes), as
`fetch_bench` does since 2026-10-05. Two rows still differ run to run, so one more entropy source remains.
It is not yet found.

**Measured and rejected.**
- **A pure rotation** (served from the front, back to the back). It is processor sharing within a class.
  The scan restarted at the oldest stream whenever a stream completed, which approximates first-come service.
  For completion time among similar sizes, first-come beats processor sharing (Harchol-Balter, *Performance
  Modeling and Design of Computer Systems*, ch. 30). The rotation lost the 100 Mbit/s, 20 ms, 1 % loss
  scenario by ×1.37 on control p99.
- **The scan's exact order** was not kept either. Its cursor was an index that wrapped when it served the
  class's last stream, so a stream installed afterwards waited a full cycle. Serving the first ready stream
  after the one served last fixes that, and measured ×0.989 / ×0.915 / ×1.024 above.

**Oracle.** Two test-only checks:
- *Order.* The service rule ("first ready stream after the last served, in install order, wrapping; the
  oldest after a completion") is computed by a linear walk and compared with the ready sets' choice for each
  frame. Zero differences over the connection unit tests, and zero over the whole class grid with the check
  compiled in.
- *Membership.* `ready_is_exact` holds that the sets are exactly the sendable streams on every poll of the
  transfer tests. Mutations caught: a credit grant that does not requeue fails 7 tests; a raised `MaxStreams`
  that admits nothing fails the concurrent-exchanges integration test.

## Linux FUSE mounts: fewer kernel round trips per request (2026-10-06)

**Workload.** `python3 -m venv v && v/bin/pip install requests flask` on a slates FUSE mount, against the same in the
container's own filesystem. Linux 6.12 (Docker Desktop's VM, 18 vCPUs), the release binary, the daemon and the mount
as an ordinary user, host load 48–70 (the machine is shared). Both installs produce the same 1,385 files with
identical bytes outside the files that embed their own path.

**Wall time is not usable at this load.** Over five alternating rounds of HEAD and the change, each build spans about
2.5× on its own (HEAD 12.1–29.5 s, the change 10.0–27.2 s), against 2.4–3.7 s in the container's filesystem. Each
FUSE request is two cross-thread handoffs through the kernel. On a host about 3× oversubscribed, a handoff costs
hundreds of microseconds and varies with whatever else runs, so the measure taken is what the workload asks of
slates.

**Wall time on a quiet host (later the same day, load 9).** One container, one daemon, alternating, six rounds;
the release binary; Linux 6.12 under Docker Desktop, 18 vCPUs. The script is `pip-time-inner.sh`: `python3 -m venv
v && v/bin/pip install requests flask`, timed with `EPOCHREALTIME`.

| | slates FUSE mount | the container's own filesystem |
|---|---|---|
| install, round 1 (cold) | 2.83 s | 1.63 s |
| install, rounds 2–6 | 1.95–2.02 s | 1.59–1.61 s |
| `import flask, requests` | 0.10 s | 0.08 s |

- **The gap is the request count.** Steady state is 1.24×. The 0.4 s gap is close to 19,222 requests at about 20 µs per
  kernel round trip, so what remains is the crossing itself, not slates' work.
- **Correctness:** 1,382 files on each side, and identical bytes outside `RECORD`, `pyvenv.cfg` and `.pyc`.
- **One earlier run** had a 7.67 s round that six later rounds did not repeat (a host-load spike).

**The same workload on macOS** (macOS 26.4, M5 Max, load 4–5, the release binary). It ran through `slates mount`,
the NFS loopback mount with no privilege, against APFS, alternating, three rounds. The script is `mac-pip.sh`; Python
3.9.6 is the system interpreter.

| | slates mount | APFS |
|---|---|---|
| install (rounds 1–3) | 3.95, 3.61, 3.51 s | 2.95, 2.86, 2.87 s |
| `import flask, requests` (warm, rounds 2–3) | 0.16, 0.17 s | 0.59, 0.59 s |

- **Installs** run 1.22–1.34× APFS.
- **Warm imports** run 3.5× faster than APFS. An inference, not measured: the NFS client's attribute cache saves the
  metadata work APFS repeats per file.

**Requests slates receives**, counted by opcode at the bridge (deterministic):

| | HEAD | Change |
|---|---|---|
| pip install, all requests | 21,709 | 19,222 (−11.5%) |
| `LOOKUP` | 5,025 | 3,616 (−28%) |
| `FLUSH` | 2,361 | 1,395 (−41%) |
| `GETATTR` | 4,250 | 4,217 |
| `import flask, requests` after it | 1,074 | 864 (−20%) |
| re-reading 200 cached files: requests per file | 5 (`OPEN`, `GETATTR`, `READ`, `FLUSH`, `RELEASE`) | 2 (`OPEN`, `RELEASE`) |
| 200 `stat`s of cached files | 1 each | 0 |

**What changed:**
- **A lookup miss is a negative entry** (node id 0), cached for its directory's lifetime: forever for the volume's
  own directories, whose every name change through another attachment is invalidated, and bounded for a live base.
  A virtio-fs guest, with no invalidation channel, gets lifetime 0.
  **Reversed on 2026-10-19 (A-137), and this row's share of the −28% with it.** The premise — "whose every name
  change through another attachment is invalidated" — is false on Linux 6.19, which refuses
  `FUSE_NOTIFY_INVAL_ENTRY` for a negative dentry and keeps answering from it, so a name a neighbour creates is
  never seen. A miss is `ENOENT` again. Re-measured on its own, which this table never did: five interpreter
  starts over a mount cost **273** daemon reads with the negative entries and **310** without (+14%; two
  interleaved rounds, 268/278 against 298/323, non-overlapping, Fedora 6.19). The other two mechanisms in this
  table — kept pages and kept listings — are untouched, and so are their rows.
- **Open flags.** `FOPEN_KEEP_CACHE` for the volume's own files, and `FOPEN_CACHE_DIR` for its directories:
  the kernel keeps pages and listings across opens. `FOPEN_NOFLUSH` for a read-only handle: no `FLUSH`, and no
  barrier, at its close.
- **Activity.** A served FUSE request counts as activity (`note_activity`), as an NFS call does: the shard spins
  between a burst's requests instead of parking. An uncached lookup's median went 23 → 12 µs in the first rounds.
  Its p50 is about 40 µs under load afterwards, where it was about 60 µs before, and its minimum fell 25 → 9 µs.
- **Read-only mounts are the kernel's `ro`.** A file read and then stat'ed costs no `GETATTR`, and a write is refused
  `EROFS` with no request.

**What remains, and why:**
- **The `GETATTR` per open on a writable mount is the kernel's own** (Linux 6.12 source): every `READ` marks atime
  stale (`fuse_invalidate_atime`, unless the mount is read-only), and every `WRITE` marks size, mtime and ctime stale
  (`fuse_write_update_attr`, unless writeback caching owns them). Writeback caching is declined on the recorded
  grounds that the kernel's ownership of the size defeats invalidation of a change made through another
  attachment.
- **1,898 `FUSE_IOCTL` requests per install** are Python's `isatty()` on every `open()` (`TCGETS`). The kernel
  forwards every ioctl on a FUSE file and never remembers an `ENOSYS` (`fuse_send_ioctl`, `ioctl.c`), so every FUSE
  filesystem pays them. slates answers each with one fixed reply.

**Correctness.**
- The real-kernel suites pass: the bridge (`owner_turn`, coherence across attachments), virtio-fs, and the server's
  FUSE mounts.
- A new coherence phase proves a name created in the root through another attachment is seen by a mount that had
  cached it absent. It found the root-invalidation defect
  (`docs/bugs/2026-10-06-fuse-invalidations-of-the-root-named-an-inode-the-kernel-does-not-know.md`).

## Measured and rejected: kicking only a parked shard on the pair rings (2026-10-06)

**The candidate.** `ShardContext::send_to` (`crates/rt/src/shard.rs`) kicks its target after every pair-ring push
(an eventfd write, or a kevent), whatever the target is doing. The foreign path kicks only a parked target
(`Parking::kick_if_parked`). The candidate gave the pair path the same rule, under the same fence and the target's
park-time re-check. It is correct:
- the loom model of the parking protocol passes (3 of 3);
- the runtime's Miri suites pass;
- a use-level test saw 200 of 200 wakes to a busy shard arrive with every kick saved.

**The measurement.** Command: `cargo run --release -p slates-rt --example rt_bench`, alternating HEAD and the
candidate, three rounds each. Cross-shard wake round trip, median [bootstrap interval]:

| | HEAD (always kick) | Candidate (kick a parked shard) |
|---|---|---|
| macOS 26.4, M5 Max, load 45–55, both shards parking | 3,583–3,834 ns | 3,625–4,000 ns |
| macOS, both shards spinning | 4,750–5,250 ns | 12,292–13,292 ns (p99 26–40 µs) |
| Linux 6.12 (Docker Desktop, 18 vCPUs), both shards parking | 1,583–1,584 ns | 1,375–1,458 ns |
| Linux, both shards spinning | 1,958–2,000 ns | 2,291–2,416 ns |

Spinning after activity is the daemon's mode under load, and there the candidate is 20% slower on Linux and 2.5×
slower on macOS. **Rejected.**

**What the timeline showed.** On macOS, a temporary per-shard event log (step, spin, park, send) found:
- No lost wakes: the spin never hit, and every park either found its message waiting or was kicked.
- 1,973 of about 2,000 kicks were skipped: the target had not yet announced a park when its wake arrived.
- The sending shard is slower to reach its next event without its own kick: from the send to its next event, p50
  3 → 5 µs and p90 5 → 13 µs, with no event logged in the gap.

The mechanism is not yet identified. It sits in the OS's scheduling of the shard threads, not in the runtime's
protocol: nothing in the step after a poll depends on the kick. The macOS clock reads in 1 µs steps there; a Linux
timeline at nanosecond resolution is the next measurement.

**Consequence beyond slates.** `hyper-rt`'s design (§3.2) routes every cross-thread wake through kick-only-if-parked,
so this measurement is owed to its §12 rows: told to the focal session the same day.

**Addendum, the same day: a Linux timeline at nanosecond resolution.** Each build was probed (a timestamped event per
step, spin, park and send), and the probe's own overhead reversed the result: always-kick 2,458 ns, the candidate
1,917 ns, against 1,958 and 2,375 ns unprobed. So the difference is a phase effect, not a fixed cost.
- **The mechanism, from the timeline.** About 2,250 of 8,000 kicks reached a target that was mid-step. Each left a
  pending event, so the target's next park returned at once: an extra, accidental poll of its inbox. A wake landing
  in that poll needs no kernel wake. A wake landing after it needs the kernel's, which is about 10 µs on this loaded
  macOS host.
- **Why the extra poll matters at all.** In every run, both builds and both systems, the idle spin never hit (0 hits,
  one miss per round trip). Its window is the 2-competitive spin-then-park threshold (spin for the expected cost of
  parking; Karlin, Manasse, McGeoch and Owicki 1991), and that threshold took the startup profile's `wake.mean`
  (1.7 µs on macOS), well below the wakes actually paid under load.
- **Measured in the daemon's configuration** (macOS, load 14–40). The spin window was ×100 (`IDLE_WINDOW_RATIO`)
  with the online estimate on, alternating, three rounds: always-kick 4,875–5,917 ns, the candidate 12,167–12,791 ns.
  Still rejected.
- **The window is not the cause.** The shards' counters show 1–2 real driver waits per 2,000 round trips: nearly
  every park found its message already waiting, so no kernel wake is in the candidate's extra 7 µs. What remains is
  the 10–13 µs in which a shard thread that makes no system call logs nothing (the macOS timeline above), which is
  the OS scheduling two spinning threads. The kick's system call is what keeps the peer promptly scheduled. That
  mechanism is inferred, not measured, and no change to the spin derivation is justified by it.

## Per-operation latency on a Linux FUSE mount, and a close that no longer publishes (2026-10-06)

**Setup.** Linux 6.12 under Docker Desktop (18 vCPUs), load 7–9, the release binary, 2 shards. Each operation was run
5,000 times by one Python process with `perf_counter_ns` (the script is `p99-inner.sh`), against the container's
own filesystem and tmpfs.

| | slates FUSE p50 / p99 / p999 | container fs p50 / p99 | tmpfs p50 / p99 |
|---|---|---|---|
| create + write 4 KiB + close | 105 / 171 / 794 µs → **79 / 177 / 783 µs** | 7.3 / 23 µs | 1.8 / 4.0 µs |
| stat | 13 / 23 / 51 µs | 0.8 / 1.1 µs | 0.5 / 0.8 µs |
| open + read + close | 15 / 25 / 54 µs | 1.7 / 2.6 µs | 1.3 / 1.8 µs |
| unlink | 44 / 64 / 255 µs | 3.2 / 4.8 µs | 0.8 / 1.6 µs |

- **The kernel round trip sets the floor:** a FUSE request costs about 13 µs, so reads and stats sit at it.
- **Namespace changes pay a publication on top** (§4.8 barrier: a create, an unlink).
- **The close's `flush` published too, before.** It now publishes nothing while the write log holds every write since
  the last publication (A-63). Create + write + close went 105 → 79 µs at p50 (−25%); p99 unchanged within noise.
- **A real install doesn't feel it:** pip, six rounds, 1.93–2.01 s against 1.95–2.02 s before. The flush is a small
  share of the whole.
- **Proven safe:** `a_closed_files_writes_survive_a_kill_from_the_write_log_alone` writes and closes a file, kills the
  daemon, and reads it back. With logging disabled the file comes back empty, the mutation check.
- **The next lever is the create's own publication.** An intent log for namespace changes, as ZFS's ZIL does, would
  let a create reply after one append. It is a design change to §4.8's barrier, recorded in GAPS, not built.


### 2026-10-06: the no-panic sweep costs nothing measurable on the hot paths

The sweep turned every `[]` index and slice in shipped code into a `get`, among them the ART's child arrays, the
directory tree's slots and the inode trie.

**Setup.**
- Apple M5 Max, 128 GiB. Load average 9–13 from other sessions' containers throughout, so rows move by ±10%
  between runs of one binary.
- A/B, interleaved: HEAD (`732ca43`, built from a detached worktree) against this tree. Each binary was built
  release into its own target directory.
- Commands: `db_bench` and `vfs_bench` from those directories (`cargo run --release -p slates-db --example db_bench`
  and `... -p slates-vfs --example vfs_bench`).

**db_bench, three runs each (medians):**

| Row | New | HEAD |
|---|---|---|
| ART insert at 10⁴ keys | 22 / 26 / 22 ns | 23 / 24 / 31 ns |
| ART lookup at 10⁴ keys | 8 / 11 / 8 ns | 8 / 9 / 10 ns |
| ART insert at 10⁵ keys | 41 / 41 / 43 ns | 44 / 42 / 47 ns |
| ART lookup at 10⁵ keys | 17 / 16 / 20 ns | 16 / 14 / 21 ns |
| Recover 10⁴ volumes from 10⁶ records | 404 / 450 / 415 ms | 447 / 409 / 416 ms |

**vfs_bench, first pass.** Every row sat within ±5% except the small directory-tree lookups.
- Lookup in a 2-entry tree was 166–182 ns, against 161–171 ns at HEAD.
- Directory inserts got 4–9% faster (a 128-entry tree: 182 against 192–197 ns).
- Two causes were fixed:
  - The searches decoded a whole slot to compare its hash. They now read the hash word alone (`hash_at`) and decode
    a slot only when the hashes match.
  - The word read took a bounds-checked range and then a chunk. It is now one `get(at..)` and `first_chunk`.

**vfs_bench after those, four interleaved runs each:**

| Lookup | New | HEAD |
|---|---|---|
| 2-entry tree | 177 / 177 / 171 / 203 ns | 177 / 177 / 171 / 171 ns |
| 16-entry tree | 213 / 213 / 208 / 244 ns | 218 / 218 / 203 / 218 ns |
| Inline directory of 2 | 106 / 104 / 101 / 119 ns | 109 / 106 / 111 / 101 ns |

The fourth new run was high on every row at once, a load spike.

**Verdict:** parity, so the sweep lands.

### 2026-10-06: denying overflow-capable arithmetic costs nothing measurable on the hot paths

Every shipped crate's arithmetic is now saturating or checked; GAPS has the no-panic sweep's arithmetic half. A/B against
HEAD (`60460a8`, built from a detached worktree), interleaved, on the same Apple M5 Max (128 GiB).

**First pass: load 10–13, with a Miri run holding a core.** Three pairs. The median ratios ran 1.00–1.13 with
overlapping ranges, which cannot be read.

**Second pass: load 6, Miri finished.** Five pairs. Command: `vfs_bench` from each target directory
(`cargo run --release -p slates-vfs --example vfs_bench`). Medians, new against HEAD:

| Row | New | HEAD |
|---|---|---|
| Lookup, 2-entry inline directory | 99 ns | 106 ns |
| Lookup, 2-entry tree | 161 ns | 166 ns |
| Lookup, 128-entry tree | 229 ns | 234 ns |
| Insert and remove, 128-entry tree | 187 ns | 182 ns |
| Read 4 KiB | 127 ns | 138 ns |
| Write 4 KiB in place | 437 ns | 458 ns |
| Resolve a 3-component path, 10⁵ files | 406 ns | 385 ns |

Every row's five values overlap the other build's. The geometric mean of the 21 row ratios is 0.984.

**Verdict:** parity. The sweep lands as a correctness change, not an optimization.

### 2026-10-06: giving free content pages back to the OS (A-105)

Linux (Docker Desktop VM on an Apple M5 Max, Linux 6.12, 4 KiB pages, load 13–15 from other sessions' containers).

**`mem_bench`, the free path** (`cargo run --release -p slates-mem --example mem_bench`, three runs). Each iteration
allocates a 64 KiB block, touches each page and frees it:

| Variant | Median per cycle |
|---|---|
| Zeroed in place | 317–322 ns |
| Given back and faulted in again | 6,667–6,834 ns |

That is why the give-back is an idle purge, not a step of every free.

**FUSE A/B against HEAD** (`7208e42`, a detached worktree; scratchpad `discard-ab-inner.sh`, five interleaved
rounds, both mounted as an ordinary user). Each round:
1. `create+write+close+unlink` churn, 4,000 × 4 KiB and 2,000 × 64 KiB;
2. a 64 MiB write;
3. its delete;
4. the content memfd's allocation 5 s later.

| Measure | HEAD | New |
|---|---|---|
| 4 KiB churn p50 | 119–222 µs, median 163.0 | 50–169 µs, median 163.2 |
| 4 KiB churn p99 | 259–4,604 µs, median 266 | 147–301 µs, median 278 |
| 64 KiB churn p50 | 129–235 µs, median 172 | 134–197 µs, median 185 |
| 64 KiB churn p99 | 255–3,841 µs, median 354 | 269–354 µs, median 299 |
| Allocated 5 s after the 64 MiB delete | 128 MiB, every round | 0 MiB, every round |

Latency is at parity: every row's ranges overlap. The memory goes back.

**Measured and rejected the same day: give-back at every free.** Three rounds; the measurement phase overlapped the
first two rounds' builds and a local cross-lint, so its latencies are not comparable. The give-back itself worked (0
MiB after the delete, every round). The p99 medians came out at 578 µs (4 KiB) and 592 µs (64 KiB) against HEAD's 325
and 318, at load around 18. Both sides' ranges ran up to several milliseconds, so that run could not attribute the gap.
The in-process measurement above did: a given-back block costs 6.8 µs more per reuse. That decided the idle purge.

## `fallocate` through the Linux FUSE mount, and a write's charge over the windows it touches (A-108; 2026-10-06)

All runs are in Docker Desktop's Linux VM on this Mac (aarch64), image `rust:1.98.0`, release builds. The daemon runs
`--quick --shards 1`, so both volumes share one shard, and is mounted by an ordinary user. Host load average was 10–16
throughout (other sessions' containers), so the numbers are taken under load, as the target conditions are. The
scripts are scratchpad `a108-inner.sh` and `wc-inner.sh`.

**The allocation and the shard beside it.** For each variant:
- `fallocate -l 64M` into a 128 MiB volume, timed;
- then a 96 MiB `fallocate`, while a Python loop on a second volume of the same shard times `open`+`close` (each one
  reaches the daemon) for one second, against one second with no allocation running.

| Variant | 64 MiB `fallocate` | `open`+`close` during it: p50 / p99 / max | Same, no allocation: p50 / p99 / max |
|---|---|---|---|
| One call, unsliced (`slice_bytes = u64::MAX`) | 27 ms | 12 µs / 56 µs / 37,626 µs | 19 µs / 47 µs / 935 µs |
| Sliced at 37 KB, whole-file charge map (first cut) | 171 ms | 13–14 µs / 145–166 µs / 543–594 µs (2 quiet runs of 3) | 13 µs / 26–66 µs / 243–250 µs |
| Sliced at whole windows, whole-file charge map | 53–62 ms | 16–39 µs / 113–585 µs / 918–6,694 µs | 16–25 µs / 54–409 µs / 914–1,953 µs |
| **Sliced at whole windows, charge over the touched windows (shipped)** | **33–36 ms** | **15–20 µs / 67–78 µs / 416–2,329 µs** | **16–19 µs / 36–45 µs / 242–2,112 µs** |

- The unsliced call held the shard for 37.6 ms, so another volume's request waited that long.
- Sliced, the worst wait during the allocation is within the run's own no-allocation maximum.
- The first cut's 37 KB slices ended inside 64 KiB windows, so each slice grew the window's block again. Every slice
  also rebuilt the whole file's window map twice. Both are fixed, and the allocation is now within 1.3× of the
  unsliced call.

**A write's charge (`Volume::write_charge`).** It built a map of every chunk window in the file on every write, so a
1 MiB write at the end of a large file paid for all of its windows. It now maps only the windows the write touches.
The test is `dd if=/dev/zero bs=1M count=768` into a 1 GiB volume, three interleaved rounds. "Before" is the build
with the whole-file map; the builds differ in nothing else on the write path.

| Build | Round 1 | Round 2 | Round 3 |
|---|---|---|---|
| Whole-file map | 296 MB/s | 305 MB/s | 303 MB/s |
| Touched windows | 811 MB/s | 854 MB/s | 818 MB/s |

That is 2.7× on a 768 MiB sequential write. The charge oracle (`crates/vfs/tests/charge_oracle.rs`) and the full vfs
suite pass unchanged, so the charge is the same number, computed over fewer windows.

## macOS: giving free pages back as reusable pages (A-110; 2026-10-06)

This Mac (M5 Max, Darwin 25.4), release builds; scratchpad `macshm/` (the madvise probe) and `mac-a110.sh` (the
daemon). The footprint is `proc_pid_rusage`'s `ri_phys_footprint`, or `/usr/bin/footprint -p`.

**The advice, on a 64 MiB POSIX shared memory object mapped twice:**

| Advice | Footprint | Reads after it |
|---|---|---|
| `MADV_DONTNEED` | 65 → 65 MiB | the old bytes |
| `MADV_FREE` | 65 → 65 MiB | the old bytes |
| `MADV_FREE_REUSABLE` | 65 → 1 MiB | the old bytes (until the kernel takes the page) |
| `MADV_ZERO` (11) | 65 → 65 MiB | zeros, in both mappings |

**The reuse protocol** (`MADV_FREE_REUSABLE`, then write all 64 MiB):
- with no `MADV_FREE_REUSE`: the footprint stayed at 1 MiB, so the written pages were still marked;
- with `MADV_FREE_REUSE` first: 65 MiB.

**The cost of `MADV_FREE_REUSE`, five runs:**

| Range | Time |
|---|---|
| 2 GiB, 64 MiB marked | 1,726–1,936 µs |
| 2 GiB, 1 GiB marked | 31,265–37,109 µs |
| One 64 KiB block | 3–5 µs |

That decided per-block clearing on first use over one call at mapping time.

**The daemon** (one shard, a 512 MiB volume through the NFS mount):

| Step | Daemon footprint |
|---|---|
| Start | 24 MB |
| 256 MiB written | 293 MB |
| Deleted, 6 s later | 36 MB |
| 128 MiB written again | 165 MB |

Before A-110, no page went back on macOS: the recovery test's purge count was 0 of 64 MiB. The anchor's footprint was
275 MB throughout, unrelated to content.

**The cost of the whole-region record on a fresh daemon** (512 MiB `dd` through the NFS mount, a fresh daemon per run,
load average 10–16). "Empty" is a build whose regions start with no record, which leans on the fault clearing the mark:

| Variant | Rounds | Whole record | Empty record |
|---|---|---|---|
| One reuse call per 64 KiB block | 3 | 964–978 MB/s | 1,197–1,240 MB/s |
| A bitmap word per call, run end searched to the region's end | 6 | 881–993 MB/s | 978–1,173 MB/s |
| **A bitmap word per call, search bounded by the span (shipped)** | **6** | **1,069–1,191 MB/s** | **1,067–1,216 MB/s** |

The first fix did not move the cost because the cost was not the calls: `MADV_FREE_REUSE` over 512 MiB in 1 MiB spans
took 150 µs in all, and faulting the pages in afterwards was as fast as without it (12.6–13.9 GB/s). A sample of the
writing daemon found `Region::prepare` itself, scanning the bit set to the end of the region's one run on every
allocation.

## The idle anchor's footprint (2026-10-06)

Release anchor, `--quick --shards 1`, nothing mounted, `/usr/bin/footprint -p` on this Mac (M5 Max, Darwin 25.4).

| Build | Anchor footprint | `MALLOC_LARGE` |
|---|---|---|
| Before | 275 MB | 3 freed regions still dirty: 128 + 128 + 16 MiB |
| Probe buffers mapped | 2.3 MB | none |

The profile it measures did not move, three runs per build:

| memcpy size | Before (MB/s) | After (MB/s) |
|---|---|---|
| 64 KiB | 82,852–116,612 | 89,898–116,612 |
| 1 MiB | 71,089–82,241 | 78,152–80,921 |
| 128 MiB | 21,855–22,733 | 20,404–26,368 |

## A barrier no longer walks every possible region (2026-10-06)

Linux 6.12 under Docker Desktop, the release binary, `--quick` (one partition per core, 18), one Python process per
operation as in "Per-operation latency on a Linux FUSE mount" (scratchpad `ops-inner.sh`, `abops-inner.sh`).

**Found.** A FUSE create or unlink waits a publication (§4.8 barrier). Timed inside `publish_shard`, on a 1 GiB arena:

| Step | Time |
|---|---|
| `capture` | 1.4 µs |
| The delta publication proper | 1.1 µs |
| `commit_capture` | 4 µs |
| `release_idle` | 8–9.8 µs, returning nothing |

The arena kept its regions in a vector indexed by extent id, `partition × 64 + part`. An 18-partition daemon had up
to 1,152 entries, mostly empty and each a few hundred bytes, and every barrier walked them in its capture, its
commit, its capacity sum and its idle release. The regions are now dense, with an id-to-position index, so a walk
visits only the regions held.

**A/B, HEAD against the change, three interleaved rounds, load average 7–8 (1,000 operations each, p50 / p99):**

| Round | create + close, HEAD | create + close, new | unlink, HEAD | unlink, new |
|---|---|---|---|---|
| 1 | 125 / 478 µs | 87 / 279 µs | 55 / 238 µs | 32 / 115 µs |
| 2 | 141 / 484 µs | 117 / 464 µs | 78 / 261 µs | 57 / 293 µs |
| 3 | 118 / 249 µs | 81 / 159 µs | 72 / 166 µs | 31 / 104 µs |

**A developer workload on the mount** (the 700 files and 16 MB of `crates/` copied in, then `git add`,
`git commit`, `find`, `grep -r`, `tar`, `rm -rf`; scratchpad `work-inner.sh`). The two runs were taken at different
loads; tmpfs moved too (its copy took 133–168 ms, then 80–101):

| Step | Before | After | tmpfs, after |
|---|---|---|---|
| `cp -a` | 374–390 ms | 191–302 ms | 80–101 ms |
| `git add -A` | 573–602 ms | 357–383 ms | 142–145 ms |
| `git commit` | 120–124 ms | 61–69 ms | 6 ms |
| `find` | 29–30 ms | 14–16 ms | 1 ms |
| `rm -rf` | 137–172 ms | 82–85 ms | 3 ms |

The remaining distance to tmpfs on namespace changes is the publication per change, which an intent log would take
off the reply path (GAPS, "a FUSE namespace change waits a whole publication").

**Then the capture and commit themselves (same day).** They walked every word of the region's live and deferred
bitmaps. Now a summary bit per word marks what changed since the last capture, and which words hold deferred blocks,
so each walks only those. `capture` plus `commit_capture`, one change per publication (a create's barrier), in a
release unit probe:

| Region | Before | After |
|---|---|---|
| 64 MiB | 42 ns | 42 ns |
| 1 GiB | 584 ns | 42 ns |
| 16 GiB | 10,667 ns | 458 ns |

On the Linux FUSE mount (1 GiB arena, three interleaved rounds against `aad23ee`, load average 3.7–6), create + close
went from 57–59 / 162–176 µs to 52–55 / 143–162 µs (p50 / p99), and unlink p99 from 82–98 to 63–95 µs. The saving
grows with the shard's arena, which is the point: a 16 GiB arena paid about 10 µs per barrier.

The deferral model (`no_block_an_image_names_is_ever_handed_out_again`) now predicts each deferral from the design's
rule, rather than reading it back. Dropping the change note on allocation fails it ("deferred exactly when an image
names it"); before the sharpening, the model would have followed the implementation.

## macOS NFS: what one file create costs, RPC by RPC (2026-10-06, diagnosis)

This Mac (M5 Max, Darwin 25.4), the release daemon (`--quick --shards 1`), the NFSv3 mount `slates mount` makes. 500
files each go through create+close, stat, open+close and unlink, from Python. Client RPCs counted with `nfsstat -c`
before and after (scratchpad `mac-rpc.sh`).

| Operation | p50 | p99 |
|---|---|---|
| create + close | 621 µs | 1,036 µs |
| stat | 1 µs | 10 µs |
| open + close | 45 µs | 99 µs |
| unlink | 253 µs | 516 µs |

RPCs for the 500 files: Create 1,000, Lookup 3,788, Getattr 1,983, Setattr 500, Write 500, Commit 500, Access 500,
Remove 500.

Every file the process created carries a `com.apple.provenance` extended attribute (`ls -la@`). NFSv3 has no extended
attributes, so the macOS client writes it into an AppleDouble `._name` sidecar, which slates serves as the file's own
attribute view. That costs:
- a second CREATE;
- a WRITE and a COMMIT, the COMMIT waiting a whole publication;
- a SETATTR;
- several LOOKUPs and GETATTRs.

About 13 RPCs per created file at roughly 50 µs each, which is the 621 µs.

The levers, each needing its own A/B:
- the sidecar's COMMIT without a publication, with a durability argument for extended attributes;
- fewer LOOKUP and GETATTR round trips through the attribute and negative-name caching the replies grant;
- a transport with extended attributes the macOS client speaks (its NFS client does v3 and v4.0; slates' v4 front end
  is 4.1/4.2).

**Measured and rejected, the same day: answering the sidecar's WRITE `FILE_SYNC`.** A WRITE to an AppleDouble view
was made stable at once and answered `FILE_SYNC` (RFC 1813 §3.3.7), so the macOS client sent no COMMIT. COMMITs went
from 500 to 0 for 500 files, as intended. But create+close stayed at 626 and 705 µs p50 in two runs, against 621 µs
before. The COMMIT had cost the server 3 µs, its round trip was not on the create's wait, and the publication moved to
the WRITE. A dead-even A/B does not land: the change was reverted. The levers left are the LOOKUP and GETATTR round
trips, which the trace puts at about 6 per file.

## SIGKILL under write load on a Linux FUSE mount (conditions 11 and 3; 2026-10-06)

Docker, Linux 6.12, the release binary under its anchor (`--quick`, 17 shards), a 2 GiB volume mounted by an ordinary
user (scratchpad `crash-inner.sh`).
- **The writer** (Python), for 30 s: creates files of 100 B to 300 KB, writes them whole (short writes looped), and
  fsyncs each. It records each file's SHA-256 outside the volume only after the fsync returns. Past 2,000 files it
  deletes an old one every other step.
- **The killer**, meanwhile: SIGKILLs the daemon 15 times at random intervals of 0.4–1.6 s. The anchor restarts the
  daemon and holds the mount across each death (A-61).
- **The check** afterwards: every recorded file is read back and hashed, and every deleted name must be absent.

| Kills | Files checked | Intact | Corrupt | Deleted | Resurrected | Worst operation |
|---|---|---|---|---|---|---|
| 15 | 22,068 | 22,068 | 0 | 20,029 | 0 | 238 ms |
| 15 | 21,698 | 21,698 | 0 | 19,677 | 0 | 217 ms |
| 0 | 21,706 | 21,706 | 0 | 19,688 | 0 | 29 ms |

- No panic in any run.
- Restarted daemons replayed the writes their predecessor had logged (A-63; "replayed 3 logged writes").
- The worst operation is the restart's pause: about 200 ms against 29 ms with no kills.
- **The ENOSPC every run reached is correct.** Deleting every other file kept about 21,700 files live, about 2 GB, and
  `volume stat` reported `referenced_bytes` 2,147,483,588 of the 2 GiB quota.
- **Harness lessons:** the first versions ignored `os.write`'s short return near the quota and leaked a descriptor on
  a failed write. Both read as slates faults (a "mismatched" file, a wall of `EMFILE`), and neither was one.

## macOS NFS: a daemon restart under the held connection (A-113, 2026-10-06)

This Mac (M5 Max, Darwin 25.4), release binaries, the anchor with `--quick --shards 1` (A/B) or `--shards 2` (kills).

**The restart, before and after.** Before, through the kernel mount: each `SIGKILL` of the daemon stalled the writer
1,003–1,015 ms (six kills); four kills in quick succession stalled one call 4,006 ms. After, through one userspace NFS
connection (`crates/cli/tests/nfs_held.rs`, `SLATES_TEST_CLI=1`, six kills inside bursts of 12 pipelined FILE_SYNC
WRITEs): the longest time from a kill until the next request was answered was 126.1, 122.7 and 123.9 ms over three
runs. That is the anchor's restart of the daemon; nothing reconnected.

**Every request's new cost, A/B.** The held connection peeks each request, consumes it after its reply, and sends whole
records. `8a18d90` (base) against the change (held), interleaved base, held, base, held, four rounds each, through the
kernel mount `slates mount` makes (scratchpad `ab.sh`, `ab-work.py`: 500 files of create+write 4 KiB+close, open+read
4 KiB+close, unlink; 200 × 256 KiB write+fsync). Load average 4.3–6.6.

| Operation | base p50 | held p50 | base p99 | held p99 |
|---|---|---|---|---|
| create+write+close | 433, 442, 452, 446 µs | 438, 442, 457, 432 µs | 897, 840, 895, 887 µs | 849, 877, 1,010, 899 µs |
| open+read+close | 31, 31, 31, 32 µs | 31, 32, 33, 33 µs | 54, 41, 86, 60 µs | 41, 42, 56, 56 µs |
| unlink | 136, 131, 156, 141 µs | 135, 126, 143, 154 µs | 309, 290, 334, 285 µs | 641, 297, 316, 274 µs |
| 256 KiB write+fsync | 528, 572, 597, 572 µs | 545, 542, 575, 550 µs | 909, 1,856, 1,673, 1,016 µs | 845, 854, 954, 956 µs |

Medians are even (open+read+close perhaps 1 µs dearer: the discard is one more call). The 256 KiB write's p99 is
tighter with the change, likely because each connection's buffers now hold two whole records where the default was
128 KiB. The held unlink p99 of 641 µs did not repeat in three more rounds.

**The anchor waits on the daemon's exit (same day).** The anchor's loop, since A-113, polled only its hold channel
with a 100 ms pause as the timeout, so it learned of a dead daemon up to a whole pause late. It now polls the
daemon's exit too: a pidfd on Linux, a kqueue `NOTE_EXIT` on macOS (`slates_ipc::exit_watch`).
- macOS, `nfs_held` (six kills inside write bursts), the longest kill-to-answer: 122.7–128.7 ms before, 54.7–55.3 ms
  after, in three runs each.
- Linux, Docker, the npm install with kills (scratchpad `install.sh`; lodash, express, typescript, webpack,
  @babel/core, eslint; 7,419 files): 4.5 s with no kills; 8.3 s with 12 kills before (about 320 ms a kill); 4.7 s with
  7 kills after (about 30 ms a kill). Every run: `npm ls` clean, and pip's RECORD hashes for 3,606 files intact under
  10–11 kills.

**A client connecting during a restart (A-114, same day).** `slates status` started at the moment the daemon was
killed: before, exit 3 after 1,013.8 and 1,016.3 ms (the claim wait); after, exit 0 in 14.5 ms on Linux and
15.6–17.0 ms on macOS (`crates/cli/tests/held_rendezvous.rs`, six kills a run, the longest; a debug CLI). A trace
of the restart, release build, macOS: the anchor restarts the daemon at +2 ms, the control loop runs at +6.6 ms, and
the pending claim is accepted at +6.7 ms. For scale, a `slates` invocation costs 3.4 ms whether or not it reaches the
daemon (`--help` 3.4 ms, `status` 3.5 ms, p50 of 20).

## A restart's volume rebuild against the file count (2026-10-06)

This Mac (M5 Max, Darwin 25.4). `cargo run --release -p slates-vfs --example recover_bench`: one volume of N 100-byte
files in directories of 1,000, imaged, then claimed and rebuilt into a fresh store, best of 5 (all five shown by the
bench). The daemon-level figure is `restart-big.sh` (scratchpad): a 50,000-file volume made through the macOS mount,
unmounted, then the daemon killed three times under its anchor (`--quick --shards 2`), the recovering shard's start
from its log.

| Step | ns a file (10k / 50k / 200k) | Daemon, 50,000 files |
|---|---|---|
| Before | 366 / 400 / 392 | shard start 70–74 ms (rebuild 61 ms) |
| The rebuild reuses the handles placing returned (no second trie walk) and a flat kind table | 325 / 326 / 344 | — |
| Inodes placed through a leaf cursor (15 of 16 numbers share the last leaf; each set walked all 12 levels twice) | 191 / 204 / 223 | — |
| Entries inserted in the tree's key order, and an append past the rightmost block leaves the full block full | 144 / 164 / 181 | shard start 45.8–46.4 ms |

Directory blocks after the rebuild: 301 / 1,503 / 6,007, against 551 / 2,753 / 11,011 for the same volume built by
creates in name order. That is 45% fewer page-sized blocks, about 20 MB less for 200,000 files. The split rule is
PostgreSQL nbtree's rightmost-page split and SQLite's `balance_quick`. `set` also stopped allocating a path vector it
never read, which every create paid.

The daemon's remaining 46 ms is mostly the image decode and the block claims, which come before the rebuild; that is
the next lever.

**Then the image decode (same day).** A recovery found the committed checkpoint by checking both slots' CRCs, then
found and read it again (`read_from`): four passes over the image, a zeroed span buffer each, and a copy. It now reads
the frame each slot claims without its CRC, takes the newest, and copies and checks it once, falling back to the
other only when that fails (the same slot checking both would choose). And a replayed delta binary-searches its
directory's entries, each comparison stepping two folded-name iterators a character at a time: an exact name now
compares as bytes and two ASCII names under folding as lowercased bytes (a property test holds `compare` and `same`
to the folded forms on generated names, Unicode included; the dirtree's lookups take the same path).

| `recover_bench`, 10k / 50k / 200k files | Before | After |
|---|---|---|
| checkpoint decode, ns a file | 102 / 105 / 111 | 64 / 64 / 64 |
| journal replay as the daemon publishes (a delta a create), µs | 2,791 / 14,115 / 39,949 | 1,761 / 8,869 / 20,248 |

Daemon, the 50,000-file macOS volume (100,101 inodes: each file's provenance attribute is an AppleDouble sidecar
inode), load average 11: decode 20.9 → 11.6–12.9 ms, claims 2.6–3.1 ms, rebuild 19–22 ms (214 ns an inode, the
bench's rate); the recovering shard starts in 38.6–44.3 ms, from 70–74 ms this morning.

## macOS NFS: the 13 round trips of one create are the client's (2026-10-06, diagnosis)

The same Mac, the release daemon (`--quick --shards 1`), the mount `slates mount` makes; 200 creates of a 100-byte
file, client counts from `nfsstat -c` (scratchpad `rpc-count.sh`), and the server's own sequence for three creates
(procedure, target handle, reply length; a temporary trace, not kept).

Per create: GETATTR 4, LOOKUP 2, CREATE 2, WRITE 2, COMMIT 2, SETATTR 1. The order:

| # | Call | Target | Why it is sent |
|---|---|---|---|
| 1 | LOOKUP `f` → NOENT | directory | `open(O_CREAT)` |
| 2 | CREATE `f` | directory | |
| 3 | LOOKUP `._f` → NOENT | directory | the provenance attribute's AppleDouble sidecar |
| 4 | GETATTR | directory | revalidated before the sidecar's create |
| 5 | CREATE `._f` | directory | |
| 6 | GETATTR | sidecar | close-to-open, at its open |
| 7–8 | WRITE, COMMIT | sidecar | the attribute's bytes, flushed at its close |
| 9 | GETATTR | sidecar | close-to-open, at its close |
| 10 | SETATTR | file | |
| 11–12 | WRITE, COMMIT | file | the data, flushed at close |
| 13 | GETATTR | file | close-to-open, at close |

Every reply already carries the attributes a client could use instead: the failed LOOKUP the directory's (92 bytes),
CREATE the handle, the object's attributes and the directory's before and after (276), WRITE before and after (136),
COMMIT after (104). The GETATTRs are close-to-open revalidations, and macOS offers no way to turn them off: its mount
flags (`<nfs/nfs.h>` `NFS_MFLAG_*`) have no `nocto`, and `mount_nfs(8)` only tunes the attribute-cache timeouts,
which slates already sets to the finest nonzero value. The sidecar is how the macOS NFSv3 client stores extended
attributes (NFSv3 has none). The server cannot remove a round trip of this create, only make each one cheaper. The
remaining levers are outside the v3 server: a transport with extended attributes and delegations that the macOS
client speaks.

## The daemon's memory against a volume's file count: checkpoints streamed (2026-10-06)

This Mac (M5 Max, Darwin 25.4). Scratchpad `mem-50k.sh`: the release daemon under its anchor (`--quick --shards 2`), a
1 GiB volume mounted through the macOS NFS mount, 50,000 files of 100 bytes made in directories of 1,000 (100,101
inodes with the AppleDouble sidecars), `footprint -p` of each process before and after.

| | Daemon before files | Daemon after 50,000 files | Anchor |
|---|---|---|---|
| Before | 20 MB | 270, 307 MB | 2.4 MB |
| Checkpoints streamed | 20 MB | 163 MB | 2.4 MB |
| … and the buffer reserved at the expected size | 20 MB | 139, 141 MB | 2.4–2.5 MB |
| … and streamed straight into the slot (no kept buffer) | 20 MB | 122, 122 MB | 2.4 MB |

How it was found: `footprint -v` put the growth in `MALLOC_LARGE` (188 of 307 MB dirty, none reclaimable), and
`malloc_zone_pressure_relief` returned nothing, so it was not freed-and-cached. A probe that built the same volume in
one process grew 47 MB without publishing and 144 MB publishing as the daemon does, rising in steps at each
checkpoint: +20, +28 and +43 MB at the checkpoints after 18,401, 29,157 and 46,199 files. Each checkpoint built the
whole `VolumeImage` (every inode image, every directory's entries) and then its encoding, and the allocator kept each
such high-water mark.

The change: `Volume::encode_image` writes exactly the bytes `to_image().encode()` did, encoding each head inode as it
is captured and dropping it; the server streams the shard image (`ShardImage::encode_start` … `encode_finish`) into
one buffer the shard keeps, reserved at `Journal::expected_checkpoint_bytes`, and frames it with
`Journal::checkpoint_encoded`. Two oracle tests hold the streamed bytes equal to the old encoding on volumes with a
snapshot, a clone, an attribute, a hard link and an open orphan, and the streamed checkpoint recovers to the same shard
image.

What remained above the volume's own 47 MB was then mostly the kept buffer (one image's worth, reused) and the
per-create delta path.

**Streamed into the slot (later the same day).** `Journal::begin_checkpoint` returns a `StreamedImage` that writes the
image into the free slot as it is encoded, through a 64 KiB stage the shard keeps (`STREAM_STAGE_BYTES`): the slot's
old header is zeroed first and the new one written last, so a refused, crashed or abandoned stream never commits and
the committed slot is never touched. Counts are written before their items (a trie visit counts the head's inodes; a
volume that cannot be captured restarts the checkpoint without it, at most once per volume), so nothing is patched
after it may already be in the slot. The release daemon (load average 8.7–9.5, the machine shared): 122 MB in both
rounds. `cargo run --release -p slates-vfs --example create_heap` (50,000 files, one process, the allocator counting):

| | Retained heap per file | Allocations per file | Reallocations per file |
|---|---|---|---|
| Before this day's allocation work | 1,897 B | 39.9 | 9.0 |
| Dirty sets as sorted `Vec`s, the delta encoded in a kept scratch, one exact buffer per journalled path | 1,420 B | 30.9 | 0.006 |
| The checkpoint streamed into its slot | 1,024 B | 30.9 | 0.003 |
| Inode images and publications encoded from the store, never built | 1,024 B | 13.05 | 0.003 |
| Attribute table sized to its entries; a write's epoch view inline; a create in a current directory records nothing | 936 B | 8.05 | 0.003 |
| `BaseBody`'s never-read `descriptor` removed: `Inode` 184 → 168 B | 904 B | 8.05 | 0.003 |

**Encoded from the store (later again).** A checkpoint built an `InodeImage` per inode and a delta one per changed
inode, plus a `String` per entry, each copying the inline body, the attribute names and the entry names only to encode
and drop them. `Volume::encode_inode` and `Volume::encode_publication` write the same bytes from the store (the inline
and directory bodies and the attribute table field by field, the rarer bodies through their `BodyImage`; a directory's
entries are gathered by reference and sorted, one allocation a directory). The derive now emits each enum variant's
discriminant (`BodyImage::WIRE_TAG_INLINE`, `VolumeRecord::WIRE_TAG_DELTA`, …), so a field-by-field encoder cannot
disagree with its variant order. Oracles: the model test's every generated step compares `encode_publication` with
`publication().encode()` byte for byte (a mutation writing every entry as absent failed it at the first create), the
streamed-checkpoint tests compare the image bytes, and `a_streamed_shard_delta_equals_the_shard_delta_and_recovers`
the shard delta's. Checkpoint allocations per file 8.87 → 0.01, delta 9.00 → 0; the daemon after 50,000 macOS files
122, 123 MB (unchanged: these were transient allocations, not retained bytes; load average 8.8–11.5).

**Sized to what is held (later still).** `XattrTable` is a boxed slice with one slot per attribute and a non-zero
word for the working copy: 24 bytes, not 40, and its entries 24 bytes for one attribute where a `Vec`'s first growth
reserved four (96). `content_by_epoch` returns an inline view for a body of one part (`EpochContent`), where a `Vec`
for every body was two allocations per write, the attribute's value write included. `need_current_dir` records only
a directory it counts, so a create in a current directory allocates nothing there. Per phase: create 430.6 B and 2.0
allocations, the write 128.7 B and 1.0 (its inline body), the attribute 373.8 B (was 461.8) and 5.0 (was 7.0).

`BaseBody.descriptor` was written `None` at every construction and never read (held descriptors live in the base
plane's `descriptors` map), yet its `Option<u64>` made every inode's `Body` 64 bytes: removed, `Body` 48 and `Inode`
168. `tree_heap` (real trees, heap bytes per entry): npm 316 → 300, pip 352 → 336, cargo 402 → 386. Measured and not
taken: `Body` reaches 32 bytes only with `lost` packed into `base_len`'s top bit (a rustc layout probe: 40 with a
lazily boxed `pinned` alone), which every length write would then have to refuse at 2^63; 8 bytes an inode does not
buy that risk to the overlay's sizes.

Per phase after the streaming: create 430.6 B and 3.0 allocations, the 100-byte write 128.7 B and 3.0, the provenance attribute
461.8 B and 7.0, a delta 0 B and 9.0, a checkpoint 2.6 B (was 398.9) and 8.9. Tests:
`a_streamed_shard_checkpoint_equals_the_shard_image_and_recovers` (an image several stages long, framed byte for byte),
`a_refused_or_abandoned_streamed_checkpoint_leaves_the_committed_one`, and the server's
`a_streamed_checkpoint_restarts_without_a_volume_it_cannot_capture` (a 2,000-file volume streamed into the slot before
an overlay without its host refuses; the committed image holds the first volume whole).

## A read touches only the extents it overlaps (2026-10-06)

This Mac (M5 Max, Darwin 25.4), release build. A throwaway A/B example (one volume, one file written in 1 MiB
writes, 200,000 random 4 KiB reads, best of 5, all 5 shown) run in this tree and in a worktree at `5cfed78b`. The
store's chunk is 64 KiB, so a 512 MiB file has 8,192 extents.

| File | Before (ns per read, rounds) | After (ns per read, rounds) |
|---|---|---|
| 16 MiB | 953 (953 976 990 961 975) | 223 (254 241 241 223 269) |
| 128 MiB | 5,624 (5881 5738 5786 6093 5624) | 383 (387 383 399 488 469) |
| 512 MiB | 22,225 (22325 25068 22737 22245 22225) | 514 (553 514 546 584 571) |

`read_body` and `read_in` resolved every extent's chunk and only then tested the overlap. `overlapping`
binary-searches the extents, which every body keeps ascending and non-overlapping. Found while fixing
`docs/bugs/2026-10-06-a-truncate-refused-partway-left-its-file-reading-zeros.md`: one unresolvable chunk anywhere
in a file refused every read of it.

## A write reconciles only the windows it can change (2026-10-06)

Same machine and method as the read row above. A random 4 KiB overwrite recounted the file's every extent twice for
the epoch histogram (`content_by_epoch` before and after), so it grew with the file. Before: 16.8 µs in a 16 MiB
file, 39.2 µs at 128 MiB, 74.4 µs at 512 MiB. A write on a chunked body now reconciles only the windows it covers
and the window of the extent open before it (`touched_windows`). Every extent begins at a window boundary and a
window holds one, so nothing outside those windows changes. After, best of 5: 16.6 µs, 14.5 µs, 16.8 µs.

The recorded command is `cargo run --release -p slates-vfs --example large_file_bench`. Its first run, at load
average ~11:

| File (extents) | Sequential µs/MiB | Read ns (rounds) | Overwrite ns (rounds) |
|---|---|---|---|
| 16 MiB (256) | 70 | 249 (255 283 261 249 268) | 24,554 (24754 25442 25420 24554 25085) |
| 128 MiB (2,048) | 69 | 384 (396 384 421 464 437) | 11,998 (25218 25243 23412 11998 12926) |
| 512 MiB (8,192) | 83 | 457 (528 507 457 463 491) | 18,345 (22277 26886 29905 29157 18345) |

Oracle: `a_writes_accounting_equals_a_recount_after_every_step`. A seeded history of 300 overwrites and truncates
runs on a six-window file; after every step, the live volume's referenced and unique bytes must equal those of a
rebuild, which recounts from the bodies. A mutation that dropped the written windows failed it. Dropping only the
open extent's window passes, because sealing keeps the extent's epoch and block when there is no cipher. That window
is kept anyway: a seal under a cipher may move the block.

## The KIND lane on the streamed-checkpoint build (2026-10-07)

This Mac (M5 Max), Docker Desktop, kind, `cargo xtask kind all` at `3ba73241` (load average 8–10): the image built
from the repository's Dockerfile, smoked, then on a three-pod chart install:

| Step | Result |
|---|---|
| Install and formation | 3 replicas installed and formed in 7.5 s |
| Placement | A volume's snapshot placed at `f + 1` across pods in 7.5 s |
| Takeover | The owner's pod deleted (SIGKILL): `slates-0` took the volume over and served it placed 3.5 s after the delete, every byte of its 4 files |
| Rejoin | The replacement pod rejoined with a fresh identity 3.7 s after deletion |
| `wan` (80 ± 20 ms) | 18 samples over 183 s, 0 leader changes |
| `wan-loss` (80 ± 20 ms, 1 % loss) | 18 samples over 183 s, 0 leader changes |
| `ceiling` (350 ms) | formed in 4.8 s; 18 samples over 183 s, 0 leader changes |

`cargo xtask kind export` (kubelet mounting a published volume as an `nfs` PersistentVolume over RPC-with-TLS)
skipped loudly here: Docker Desktop's kernel has no TLS record layer (`CONFIG_TLS`). That leg runs in CI with
`--require-kernel-tls`.

#### The storm again on the streamed-checkpoint build (2026-10-07)

Same command (`native.sh`), Linux release build at `462be77e`, load average 7.7–10.6:

| workers | op | p50 | p99 | p999 | after A-89 p99 |
|---|---|---|---|---|---|
| 1 | create+write+close | 0.18 ms | 0.35 ms | 0.49 ms | 0.38 ms |
| 1 | open+read+close | 24 µs | 80 µs | 117 µs | 0.17 ms |
| 1 | stat | 30 µs | 84 µs | 179 µs | 54 µs |
| 16 | create+write+close | 1.9 ms | 3.37 ms | 4.65 ms | 4.2 ms |
| 16 | open+read+close | 0.79 ms | 3.39 ms | 20.3 ms | 3.1 ms |
| 16 | stat | 0.12 ms | 1.63 ms | 3.21 ms | — |

The daemon's own operation time (`nfs.local_p*_ns`, shard 0): p50 1.8–1.9 µs, p99 9.2–10.2 µs. The 2026-10-05 record
had 0.49 ms.

Open: one stat at one worker is now a round trip (p50 30 µs; the A-89 record's 2.3 µs was the client's attribute
cache). mountstats counts exactly 1,117 GETATTRs for 2,000 stats on every run. A/B at one worker, two runs each,
against the build before this day's changes (`43826f76`): stat p50 30.0 / 30.0 µs there and 30.1 / 30.1 µs here, the
same 1,117 GETATTRs. So the change predates this day's work. Bisected the same day, one worker, same command:

| Build | stat p50 | GETATTRs per 2,000 stats |
|---|---|---|
| A-89 itself (`76520d22`) | 34.1 µs | 1,117 |
| `e3423db8~1` | 29.5 µs | 1,117 |
| `e3423db8` (the v4 READDIR rewrite) | 33.0 µs | 1,117 |
| `43826f76` | 30.0 µs | 1,117 |
| `462be77e` | 30.1 µs | 1,117 |

The commit that recorded 2.3 µs gives 34 µs today. So no slates change moved it: the environment did (Docker
Desktop's LinuxKit kernel is 6.12.76-linuxkit now; its NFS client decides when a stat revalidates). 2,000 random
draws over 2,000 files touch about 1,264 distinct files, near the 1,117 GETATTRs, which suggests the client
revalidates a file's first stat after its close and serves later ones from its cache. The 2.3 µs row is a reading
of the older client, not a floor this build lost.

### Remote pulls under horrific networks: delay jitter collapses Copa (condition 7; 2026-10-07)

`fetch_bench` gained harsh rows (reordering jitter via `SimPath::reordering`, in-order jitter, 10% loss) and two
holder counters per row: spurious losses (declared lost, then acknowledged) and the smallest end congestion
window. Release build, this Mac, simulated network and virtual clock.

At 1,024 chunks (64 MiB):

| Scenario | Result |
|---|---|
| 3 holders, 8 readers, 10% loss (80 ms) | completes, 15.2–21.5 s, 199.6 of 300 Mbit/s |
| 250 ms, reordering ±60 ms, 5% loss, 3 holders | **not complete within 600 s** |
| thin 10 Mbit/s, 200 ms, reordering ±40 ms, 2% loss | **not complete within 600 s** |

The thin link's parts apart, at 16 chunks (1 MiB):

| Thin link, 200 ms | Goodput | End window | Spurious losses |
|---|---|---|---|
| no impairment (`wan`, for scale, 80 ms 100 Mbit/s) | 20.9 Mbit/s | 749 KB | 0 |
| 2% loss, in order | 5.1 Mbit/s | 361 KB | 0 |
| jitter ±40 ms, kept in order, no loss | 1.5 Mbit/s | 24 KB | 0 |
| jitter ±40 ms, reordering, no loss | 0.8 Mbit/s | 19.6 KB | 9 |
| jitter ±40 ms, reordering, 2% loss | 0.6 Mbit/s | 17.1 KB | 15 |

The cause is delay jitter, not loss and not mainly reordering:

- Loss alone keeps a 361 KB window.
- Jitter alone, with no reordering and no spurious loss, collapses it to 24 KB.
- Copa in its default mode ignores loss (`on_loss` acts only in the competitive mode or on persistent congestion),
  so an Eifel-style undo of spurious loss responses (RFC 4015) would not help; it was considered and is not the
  fix.
- Copa sizes its window from `standing RTT − minimum RTT`. With ±40 ms jitter the minimum locks onto the fast
  tail while the standing RTT (a minimum over half a round trip) sees few samples once the window is small. The
  spread is read as queue and the window shrinks further: a feedback loop.
- Reordering on top doubles the cost through spurious retransmissions (the adaptive tolerance, `reorder.rs`,
  bounds those).

Owed: a jitter-robust delay signal, chosen from the literature and A/B'd on these rows plus the 57-scenario
bake-off (which had no heavy jitter). Until then the two reordering rows fail at 64 MiB.

#### The two-network topology's crossing on `congestion_bench` (2026-10-07)

`cargo run --release -p slates-transport --example congestion_bench "wan jitter"` runs two rows added for the Docker
topology's link (`docs/wip/bench/multiregion/run.sh`): 200 ms, ±40 ms reordering jitter, 3 % loss. Simulated
network, virtual clock, seeds 1–3.

| Row | Goodput (Mbit/s) | Capacity share (mean) | Steady ping p99 (mean) |
|---|---|---|---|
| 10 Mbit/s | 4.24, 3.57, 2.58 | 0.346 | 522 ms |
| 100 Mbit/s | 5.21, 4.20, 4.08 | 0.045 | 513 ms |

These are long-lived flows, after a 20-round-trip warm-up. A cross-region read on the real topology is short
request/response exchanges, and there the session delivered 65–100 KB/s (`docs/bugs/2026-10-07-a-cross-region-read-took-one-wan-round-trip-per-window.md`).
That matches `fetch_bench`'s short transfers above, not this steady state.

#### Measured and rejected: a sample-count floor on Copa's standing RTT (2026-10-07)

The idea: Copa's standing RTT spans `srtt/2`; let it span the last N samples when that is longer (N = RFC 9002's
initial window, 10), so a small window's minimum is not of a handful of jittered samples (order statistics: N
samples over a jitter width W have an expected minimum W/(N+1) above the floor).

`fetch_bench`, thin link (10 Mbit/s, 200 ms):

| Scenario | Before | With the floor |
|---|---|---|
| jitter ±40 ms in order, 1 MiB | 1.5 Mbit/s | 2.7 Mbit/s |
| reordering + 2% loss, 1 MiB | 0.6 Mbit/s | 2.3 Mbit/s |
| jitter ±40 ms in order, 8 MiB | — | 28.2 s at N = 10; 20.4 s at N = 32 |
| reordering + 2% loss, 8 MiB | — | 55.8 s at N = 10; 23.5 s at N = 32 |

`congestion_bench`, 56 scenarios against `HEAD`, deterministic:

| Variant | Capacity share (geomean) | Ping p99 (geomean) | Wins | Losses |
|---|---|---|---|---|
| Floor alone | +0.45% | +1.2% | jitter row 0.534 → 0.706 share; 64 kbit/s at 100 ms | 64 kbit/s at 300 ms: p99 585 → 1,052 ms, share 0.912 → 0.847 |
| Floor capped at one RTT | −0.2% | +2.9% | jitter row 0.534 → 0.694 | 64 kbit/s at 300 ms: share 0.912 → 0.751; 1 Mbit/s 1% loss: p99 137 → 192 ms |

The floor swaps jitter tolerance for slow-link behaviour. On a slow link, N samples span seconds, the filter keeps a
stale low minimum, under-reads the queue and over-sends. So it does not land. The owed design is the one the
starvation result prescribes (Arun, Alizadeh, Balakrishnan, SIGCOMM 2022, §6): estimate the path's non-congestive
jitter D and hold at least D of delay before reading it as queue. That would separate jitter from queue rather than
widening a time window, and it must clear this grid with no loss before it lands.

#### Measured and rejected: a delivered-BDP floor under Copa's window (2026-10-07)

The idea (BBR's model, Cardwell et al., ACM Queue 2016): Copa's window never drops below `max delivery rate over 10
round trips × RTTmin`, so jitter read as queue cannot shrink it below the path's bandwidth-delay product.

`fetch_bench`, thin link (10 Mbit/s, 200 ms), 8 MiB:

| Scenario | Completion |
|---|---|
| jitter ±40 ms in order | 11.7 s (5.7 Mbit/s; the sample floor's best was 20.4 s) |
| 2% loss, in order | 7.9 s (unchanged) |
| reordering ±40 ms, no loss | 67.9 s, 18 spurious losses |
| reordering ±40 ms, 2% loss | 91.0 s, 123 spurious losses, end window 14.9 KB |

`congestion_bench` against `HEAD`: capacity share +0.9% and ping p99 +0.8% by geomean.

- Wins: 100 Mbit/s at 300 ms share 0.487 → 0.622; 100 Mbit/s at 20 ms with 5% loss, p99 81.9 → 61.6 ms.
- Losses: the capacity step 10 → 2 → 10 Mbit/s, ping p99 160 → 500 ms (the floor remembers the old rate for 10
  rounds and holds a queue, BBRv1's stickiness that BBRv2/v3 bound with loss-driven `inflight_hi`); 10 Mbit/s at
  300 ms with 0.1% loss, p99 409 → 509 ms.
- Under reordering the floor never forms: spurious retransmissions depress the measured delivery rate.

It does not land. What the two experiments establish for the owed design:

1. In-order jitter is solved by a bandwidth-model floor.
2. The floor must give way on a capacity drop within a round trip or two, not ten (an `inflight_hi`-style bound,
   BBRv3 draft-ietf-ccwg-bbr §5.5).
3. Reordering needs its own fix: the spurious retransmissions themselves, which the adaptive reordering tolerance
   (`reorder.rs`) still lets through at ±40 ms. That is RACK's reordering window growing too slowly for this
   jitter.

#### Measured and rejected: the floor with a two-round filter and a loss reset (2026-10-07)

The variant: the delivered-BDP floor with BBRv3's two-round maximum (`MaxBwFilterLen`) instead of ten, and a loss
while the floor binds restarting the filter from the last round's rate (BBRv3's loss-driven `inflight_hi`, in
spirit).

`congestion_bench` against `HEAD`: capacity share +0.9% and ping p99 −4.4% by geomean.

- The step regression is gone: 10 → 2 → 10 Mbit/s ping p99 160 → 132 ms.
- Burst loss: 326 → 119 ms. 100 Mbit/s at 300 ms: share 0.487 → 0.621.
- Losses: 1 Mbit/s at 100 ms with 5% loss, p99 418 → 489 ms; 1 Mbit/s at 300 ms with 0.1% loss, 436 → 499 ms;
  two-flow fairness share 0.510 → 0.481 (above the Jain floor).

`fetch_bench`, thin link (10 Mbit/s, 200 ms), 8 MiB, against `HEAD` measured the same day:

| Scenario | `HEAD` | Ten-round floor | Two-round floor + loss reset |
|---|---|---|---|
| jitter ±40 ms in order | 18.7 s | 11.7 s | 26.3 s |
| reordering ±40 ms, no loss | 68.2 s | 67.9 s | 90.5 s |
| reordering ±40 ms, 2% loss | 103.7 s | 91.0 s | 86.6 s |
| 2% loss, in order | 7.9 s | 7.9 s | 7.9 s |

It does not land. It improves the general grid but is slower than `HEAD` on two of the three jitter rows, the case it
was for.

The three experiments together:
- A long memory of the path's rate gives jitter tolerance and holds stale queues on a capacity drop.
- A short memory fixes the drop and gives the tolerance back.
- The owed design must tell a capacity drop from jitter by a signal other than how long it remembers. Candidates
  from the literature: a sustained RTT rise beyond the measured jitter band while the delivery rate falls (a drop),
  as against RTT spread with a steady delivery rate (jitter).
- The 1 MiB runs earlier in this record exaggerated the jitter collapse, since slow start dominates them. 8 MiB is
  the honest size for these rows.

#### The reordering rows are independent jitter (2026-10-07)

`fetch_bench` now also reports the holders' persistent-congestion collapses (`Endpoint::persistent_collapses`). On
the thin link with ±40 ms reordering and no loss, 8 MiB, there were 0 collapses, 15 spurious losses and an end window
of 18.5 KB. So neither persistent congestion nor retransmission explains the 68 s.

Likely reading (an interpretation of `sim.rs`'s path model, not separately measured): the simulator's in-order
jitter clamps each datagram to arrive no earlier than the one before. Its delays therefore bunch together, and Copa
sees little variance; with reordering each datagram's delay is drawn independently over the full band. The
reordering rows are the true independent-jitter case, and their collapse is the delay ambiguity the starvation
result proves, not a reordering defect: retransmissions are few. The owed design brief stands, sharpened: the delay
signal must separate independent non-congestive jitter of tens of milliseconds from queueing, without a long memory
of the path's rate, which the capacity-step row punishes.

#### Measured and rejected: the ten-round floor with the loss reset (2026-10-07)

The fourth combination, to separate the two effects variant three bundled: the ten-round filter (best for jitter)
with the loss reset (credited with fixing the capacity step).

| | Result |
|---|---|
| `congestion_bench` (geomean) | share +0.9%, ping p99 +5.0% |
| Burst loss, p99 | 326 → 2,845 ms |
| The 10 → 2 → 10 Mbit/s step, p99 | 160 → 500 ms, unchanged from the plain ten-round floor |
| `fetch_bench` thin link, 8 MiB: jitter in order | 11.7 s (as the plain floor) |
| reordering, no loss | 67.9 s |
| reordering + 2% loss | 99.2 s |

So the loss reset did not fix the step. Variant three's fix came from its two-round memory, and its reset rarely
fires when the losses come, since the floor is not the binding limit then. The reset also leaves the window high
through bursts.

The map for the owed design, from the four:

| Variant | Jitter in order | Capacity step | Grid p99 (geomean) |
|---|---|---|---|
| Sample floor | helps | regresses slow links | +1.2% |
| Ten-round floor | 11.7 s, best | 500 ms | +0.8% |
| Two-round floor + loss reset | 26.3 s, worse than `HEAD`'s 18.7 | 132 ms, fixed | −4.4% |
| Ten-round floor + loss reset | 11.7 s | 500 ms | +5.0% |

No floor fixes the independent-jitter (reordering) rows: 68–104 s across all of them. That case needs its own
mechanism.

#### Landed: Copa reads independent jitter out of its queue estimate (2026-10-07)

The mechanism the independent-jitter rows needed. Copa's queue estimate is `d_q = RTTstanding − RTTmin`.
`RTTstanding` is the least of the `n` samples in its `srtt/2`; with an empty queue and jitter of width `W`
independent per sample it sits `W/(n+1)` above the path's floor (order statistics of `n` uniform draws). A small
window has few samples, reads a large `d_q`, shrinks, and has fewer samples still: the collapse the starvation result
predicts (Arun, Alizadeh & Balakrishnan, SIGCOMM 2022, §6: a delay-bounding law must keep non-congestive jitter out
of its delay signal). The law now estimates `W` from each `srtt/2` epoch's samples and subtracts `W/(n+1)`, telling
jitter from queue by the samples' shape: a queue moves the RTT monotonically within half a round trip, so its path
length (the sum of the successive differences) is its net change; jitter zigzags. `W ≈ 3·(path − |last − first|)/(n−1)`
(two independent uniform draws differ by `W/3` on average), from the last whole epoch (`copa.rs`, `Epoch`).

Commands: `cargo run --release -p slates-transport --example congestion_bench` (the 58-scenario grid, three seeds
each), and `FETCH_BENCH_SEED=<1..8> cargo run --release -p slates-cluster --example fetch_bench thin 128` (8 MiB on the
thin link; the seed override and the handshake retry added to the bench the same day). Apple M5 Max, simulated
network and virtual clock.

`fetch_bench`, median of 8 seeds (all eight shown in the scratch record; the in-order row is noisy: `HEAD` alone spans
8.0–32.0 s):

| Thin link, 10 Mbit/s, 200 ms, 8 MiB | `HEAD` | landed |
|---|---|---|
| reordering ±40 ms, no loss | 79.0 s | 24.3 s |
| reordering ±40 ms, 2% loss | 101.9 s | 32.8 s |
| jitter ±40 ms in order | 18.1 s (8.0–32.0) | 24.1 s (7.9–30.1) |
| 2% loss, in order | 8.1 s | 8.0 s |

`congestion_bench` against `HEAD`: capacity share +1.6%, steady ping p99 −1.9% by geomean. Wins over 10%: burst loss
p99 326 → 148 ms; 10 Mbit/s 100 ms with 1% loss p99 220 → 179 ms, with 5% loss 456 → 312 ms; the reordering row's
share 0.534 → 0.857; the two-network crossing's share 0.346 → 0.423 at 10 Mbit/s and 0.045 → 0.064 at 100 Mbit/s.
Losses over 10%: 10 Mbit/s 20 ms 1% loss p99 47 → 52 ms; 100 Mbit/s 20 ms 5% loss p99 82 → 92 ms.

The two-network Docker topology (`docs/wip/bench/multiregion/reads.sh`, 100 ms ± 40 ms one way, 3% loss), cross-region
reads of 8 MiB, three readers × three rounds: 85.0, 100.9, 103.5, 120.1, 120.6, 121.1, 121.8, 126.7 s byte-identical
(median about 120 s), against 124.9–144.9 s (median about 135 s) before. One read refused after 54.2 s, its forward
unanswered within its stall budget (load average 10 from other sessions; recorded in `GAPS.md`). The gain on the real
crossing is a tenth, not the simulator's threefold: the real path's jitter is not the simulator's uniform draw, and
the read was bound by more than the window (open).

Measured and rejected on the way, each against the same `HEAD` (fetch medians where eight seeds were run, else seed
1; grid geomeans):

| Candidate | Reordering, no loss | Reordering + 2% loss | Grid share / p99 | Why it lost |
|---|---|---|---|---|
| `range/(n−1)`, `n` from the window in packets | 60.1 s | 64.4 s | — | `n` counted packets; samples come one an acknowledgement |
| `range/(n−1)`, `n` the samples counted | 14.6 s | 15.7 s | +1.3% / +4.2% | a slow link's queue read as jitter: 64 kbit/s 300 ms p99 585 → 1,199 ms, share 0.912 → 0.750 |
| range less the net change | 56.4 s | 59.3 s | — | for few samples a jitter's net change is most of its range |
| `range/(n−1)` × the last epoch's zigzag share | 18.9 s | 31.6 s | +1.4% / +2.0% | burst loss p99 326 → 1,322 ms |
| the same × the current epoch's share | 61.0 s | 76.6 s | — | the current epoch has too few samples early on |
| the current window's range × the last epoch's share | 27.7 s | 35.3 s | — | in-order jitter 34.4 s |
| `range/(n−1)` less one packet's serialization | 26.2 s | 39.7 s | +1.2% / +2.0% | 1 Mbit/s 20 ms 5% loss p99 686 → 1,393 ms |

The second candidate is the strongest on the jitter rows alone (5.4–6.5×), and is kept on record for a design that
can tell a slow link's queue from jitter without giving those gains back. The landed one keeps 3.1–3.3× with a clean
grid.

### Landed: a settled roster's discovery checks back off to the membership horizon (battery; 2026-10-08)

An idle fleet node sent one discovery exchange to every peer every heartbeat (`GAPS.md`, same day). A record link now
doubles the wait between pages while its sweep stays complete at one roster generation, from one heartbeat up to the
membership horizon (`lease::horizon_ns`, 900 ms: the longest the detector takes to declare a death). So a roster
change is learned no later than a death would be. Any change, new session or sweep under way returns to one
heartbeat (`fleet::next_discovery_wait`). The non-vacuity counter is `fleet.discovery.pages`.

**Commands:**
- the two-network Docker topology (`docs/wip/bench/multiregion/run.sh`), six nodes with no impairment;
- bootstrapped, then idle for 40 s;
- pages counted on a1 and b1 over 20 s, CPU from `docker stats` (3 samples of 6 nodes);
- arms alternated A, B, A, B;
- load average 12–15 from other sessions.

| Arm | Pages per node in 20 s | Mean CPU per idle node |
|---|---|---|
| A, a page every heartbeat | 974–1,000 | 1.55%, 1.37% |
| B, backed off to the horizon | 110–111 | 1.04%, 1.24% |

Pages fell 9×, and mean idle CPU about 20% (1.46% → 1.14%); the rest is probes and Raft heartbeats. The fleet suite
passed 79/79 in 272 s (252–299 s before), so membership convergence did not slow.

**Follow-up, 2026-10-09: an unbootstrapped fleet's configuration fetches.** Before any `bootstrap`, a root and council
learner fetched every period (their empty configurations make every live peer look like a divergence). They now back
off from a heartbeat to the horizon while the group stays uninitialized. Recovery and a stale-configuration refusal
still fetch at once (`fleet::next_learner_wait`; non-vacuity counter `fleet.learner.fetch_paced`, fetches counted in
`fleet.learner.fetches`). A/B by the in-process test
`an_unbootstrapped_fleet_paces_its_configuration_fetches_to_the_horizon` (two nodes, never bootstrapped, 6 s; A is HEAD
with only the counter added, built in its own target directory):

| Arm | Fetches per node in 6 s | Periods paced |
|---|---|---|
| A, a fetch every period | 96 | 0 (the test fails) |
| B, backed off to the horizon | 12 | 102 |

The fleet suite passed 80/80 in 276 s, so formation and bootstrap learning did not slow.

**Build trap, recorded so it is not repeated:** the first two B images ran A's binary. The Dockerfile shares one cached
`target/` across builds, and BuildKit reuses an unchanged `COPY . .` layer with its old file times, so cargo kept the
other tree's compiled crates. Touching the sources did not help. Each arm is now built in its own target volume and
copied into a distroless image, and the binaries' hashes must differ (A `d6b21905979c`, B `2f469a521c60`).

### Measured and rejected: RFC 9754 delegated timestamps for the `stat` after a delegated write (condition 12; 2026-10-08)

**Why it looked right.** A probe on the Linux 6.12 client (write, close, `stat`, 200 cycles on a write-delegated
file) made one GETATTR per cycle, a 37.4 µs `stat`, against 0.7 µs for repeated `stat`s of an unchanged file. RFC 9754
lets the holder of a write delegation keep the file's access and modification times. The 6.12 client defines
`NFS_CAP_DELEGTIME`, and sets it when the server lists `time_deleg_access`, `time_deleg_modify` and the
`open_arguments` want for delegated timestamps (`_nfs4_server_capabilities`, v6.12).

**The arm.** It advertised the three attributes and returned `OPEN_DELEGATE_WRITE_ATTRS_DELEG` when asked, accepting
the holder's times by SETATTR clamped to the wall clock.

**What it showed.** The client asked for and got the attribute delegation; the OPEN reply decoded by tshark carried
type 5. The probe still made 200 GETATTRs in 200 cycles (51–59 µs per `stat`). Each GETATTR asked only for Type, FSID,
FileId, NumLinks, Space_Used and Mounted_on_FileId: no times, size or change. So the round trip is the space used
(and link count) after a write. Delegated timestamps do not cover it. The likely reason, inferred and not traced in
the client: a write without a delegation carries a GETATTR that keeps them fresh, and one under a delegation does not.

Rejected as dead even on this client; the patch is kept in the session scratchpad.

### Payload copies out of the NFSv4 READ and WRITE paths (condition 12; 2026-10-08)

**Found by profiling and counting.** In the daemon's large-write profile `memcpy` was about 24% of self time. A
counting allocator around `serve_compound` (`cargo run --release -p slates-bridge-nfs --example v4_io_bench`, new: a
session, one file, 2,000 compounds of 256 KiB each per round, five rounds; `V4_IO_TRACE=1` prints the call stack of
every allocation of 200 KB or more) found the full payload copied into a fresh buffer 5 times per WRITE and 9 times per
READ before the change:
- WRITE: `write_args` grown by doubling, again when `state_io` appended its 24-byte suffix to an exactly sized vector,
  and `Export::write_as` copying the decoded data into a `Vec` before the volume's write copied it into the arena.
- READ: `state_io` copying the reply body out, `v3call::read` copying the data out, the v4 body grown by doubling, and
  the session's reply cache keeping every reply, though RFC 8881 §2.10.6.1.3 has the client ask per request
  (`sa_cachethis`) and Linux asks only for state-changing operations, never a READ or a WRITE.

**The fix.** `write_args` sized once with the suffix's room; the decoded data lent to the volume's write; `state_io`'s
reply kept whole with its body lent past the status word; the read data lent from it; the READ body sized once; and a
reply kept only when its SEQUENCE asked (a retry of one that did not is `NFS4ERR_RETRY_UNCACHED_REP`, which a client
answers by resending).

**Measured** (macOS aarch64, A = `1e461047`, B = the fix, built in separate target directories, interleaved, load
16–37):

| Per 256 KiB operation | A | B |
|---|---|---|
| WRITE, bytes the server allocates | 1,314,026 | 526,970 (−60%) |
| READ, bytes the server allocates | 2,361,554 | 1,574,869 (−33%) |
| WRITE, median of 8 best-of-5 runs | 56.1 µs | 54.5 µs (even) |
| READ, median of 8 best-of-5 runs | 61.0 µs | 41.1 µs (−33%) |

- Through Linux's NFS client (`docs/wip/bench/nfs/large_io.sh 3`, the new byte comparison): every 512 MiB file read
  back identical. Throughput there moved within the shared machine's noise (load 25–57).
- Same day, the compound's reply is encoded in place: its header (status, tag, operation count) is written before the
  results and its words filled in at the end, so assembling the reply no longer copies every result into a second
  buffer. A READ allocates 1,574,869 → 1,312,741 bytes (one transfer less); a WRITE is unchanged (526,966).
- ~~Still copied per READ: the volume's read out of the arena, the v3 reply's encoding, the state wrapper, the v4 body
  and the record framing~~; per WRITE, the v3 re-encoding itself. Each is a further step.
- **READ at two buffers, 2026-10-08.** A READ was copied five times (a trace of every allocation of 256 KiB or more,
  `V4_IO_TRACE=1`): the bridge's read buffer, the v3 reply's opaque, the state wrapper behind its status word, the v4
  body and the compound reply. Now:
  - the bridge reads straight into the v3 reply after its fixed words, which are filled in after (`read_into`,
    `XdrWriter::opaque_appended`);
  - `state_io` writes that reply behind its status word in place;
  - the v4 operation's body is the owner's reply lent from its eof word on (`Body { bytes, from }`, `v3call::read_tail`):
    READ3resok's tail is READ4resok byte for byte, an equality a unit test checks for every padding case.

  | Per 256 KiB READ (`v4_io_bench`, macOS aarch64) | before | after |
  |---|---|---|
  | bytes the server allocates | 1,312,741 | 526,154 (two buffers: the owner's reply and the compound's) |
  | best of five rounds, three runs | 38.3 µs | 16.6–19.1 µs |

  Through Linux's NFS client (`large_io.sh 3`, cold reads by a fresh mount): every 512 MiB file read back identical;
  reads at 1.5–2.2 GB/s with 250–360 ms of daemon CPU per 512 MiB, writes 0.7–1.4 GB/s (load 8–12). The script's
  global `drop_caches` was replaced by the remount the same day: it walks every superblock in the VM and hung for good
  behind the wedged one (the first attempt's shell is still in D state).

### A write's retention estimate and window lookup by binary search (condition 12; 2026-10-08)

**Found by profiling.** `perf record -g` of the daemon during 512 MiB `dd` writes through Linux NFS (release build with
line tables, `docs/wip/bench` style container, 2026-10-08): the self time was `memcpy` about 24%, AES-GCM sealing at
rest 14.5%, the kernel's `copy_to_user` 11.2%, first-touch page faults on the content object about 8%, then
`Volume::retention_of_write` 4.5% and `Volume::open_window` 2.1%: both walked every extent of the file on each write
(the retention estimate tested each piece; the window lookup was a linear `position`), so a write cost O(file
windows), the shape the read path and `write_charge` lost on 2026-10-06.

**The fix.** The estimate visits only the extents its operation can touch (`overlapping`, two binary searches: a
write's windows, a truncate's or an edit's tail), and the window is found by `binary_search_by_key` on the ascending
offsets.

**Measured.** `cargo run --release -p slates-vfs --example large_file_bench` (macOS aarch64, A = `aa725a57`'s tree in
a worktree, B = the fix, interleaved, load 17–37 from other sessions; best of 5 rounds per run):

| File | A: random 4 KiB overwrite | B | A: sequential µs per MiB | B |
|---|---|---|---|---|
| 512 MiB (8,192 extents) | 33.8, 35.3, 37.0, 37.2 µs | 15.7, 18.3, 24.3, 15.6 µs | 126, 119, 100, 106 | 295 (load), 81, 72, 79 |
| 128 MiB | 13.5, 14.4, 13.3, 17.1 µs | 12.1, 14.8, 13.6, 13.5 µs | 111, 126, 75, 80 | 194 (load), 74, 71, 68 |
| 16 MiB, 10 runs | 7.8–14.5 µs, median about 10.3 | 9.4–17.1 µs (one 23.5 load spike), median about 11.2 | | |

- The 512 MiB overwrite halves and stops growing with the file; the 16 MiB row is within this load's noise (the change
  only removes work).

### A real tree copied: slates against the kernel's nfsd (condition 12; 2026-10-08)

Command: `docs/wip/bench/mixed/copy_tree.sh /usr 3` (privileged `rust:1.98.0`, the release build of `6d2d5d91`, Linux
6.12 aarch64): `cp -a /usr` (28,502 entries, 1,565,931,853 bytes) through one Linux NFSv4.2 client into slates and into
nfsd exporting tmpfs, `sync` and `async`, with tmpfs as the floor; each copy removed, untimed, before the next. Load
10–80 from other sessions, falling through the run.

| Round | slates | nfsd `sync` | nfsd `async` | tmpfs |
|---|---|---|---|---|
| 1 | 21.2 s | 22.2 s | 24.5 s | 2.7 s |
| 2 | 15.8 s | 15.6 s | 24.4 s | 2.8 s |
| 3 | 16.2 s | 15.3 s | 18.3 s | 2.3 s |

- Parity with nfsd on a tree copy, within about 5% each round; nothing here is a win to claim.
- The first version of this bench measured nfsd's NFSv4 grace period: a fresh nfsd refuses new opens for 90 s, and
  the first copy sat it out (93–105 s against 24–29 s past it). It now shortens the grace period and waits it out, as
  `pressure/knfsd-fsync.sh` did; `linux_vs_nfsd.sh` was given the same (its earlier runs carried the stall on one
  operation of 400, a maximum, not a recorded p50 or p99).
- Holding three copies at once outgrew the 7.6 GiB Docker VM and hung Docker; each copy is now removed before the next.

### Memory per file on a real tree, against tmpfs (condition 12; 2026-10-08)

**The commands.** Linux 6.12 (Docker Desktop, aarch64, 4 KiB pages), release build of `1e854b56`, privileged
`rust:1.98.0`, load 7–8 from other sessions:
- `docs/wip/bench/mem/per_file.sh /usr`: copies `/usr` (28,393 entries, 1,567,942,094 content bytes) into a slates
  volume over the kernel's NFSv4.2 client, then into tmpfs. slates' side is the anchor's and the daemon's
  proportional set size (`smaps_rollup` Pss); tmpfs's is the kernel's Shmem plus slab, after dropping clean caches.
- `docs/wip/bench/mem/per_entry.sh 100000`: 100,000 empty files in 100 directories, the same two sides.
- `CREATE_HEAP_SIZES=<the tree's sizes> cargo run --release -p slates-vfs --example create_heap`: the volume alone,
  every allocation counted.

| | slates | tmpfs |
|---|---|---|
| Empty file, per entry | 843 B (daemon heap and tables; anchor 0) | 742 B (inode, dentry) |
| `/usr`, beyond content, per entry | 6,202–6,370 B (6 runs) | 2,083–2,392 B |
| `/usr`, beyond content, share | 11.2% | 4.3% |
| `/usr` copy | 13.0–22.0 s | 1.7–6.5 s |

**Where slates' `/usr` overhead is** (178 MB in all):
- **Block rounding, 88.6 MB (5.6%)**: the volume references 1,656,542,072 bytes for 1,567,942,094 of content. A
  window's block is the smallest power-of-two number of granules that holds its bytes, up to a 64 KiB chunk (§4.2
  "allocator rounding"). Modeled on the tree's real sizes:

  | Tail rule | Waste |
  |---|---|
  | Power of two, 4 KiB granule (today) | 6.26% |
  | Power of two, 512 B granule | 4.75% |
  | Page multiple, 4 KiB (tmpfs) | 3.34% |
  | Four classes per doubling, 4 KiB (jemalloc's spacing) | 3.56% |
  | Four classes per doubling, 512 B | 1.16% |

- **Heap, about 62 MB**, attributed by heaptrack at a `SIGKILL` after the copy (live allocations by stack):
  - the volume's op log, 23.1 MB (`vfs/src/journal.rs:197`, bounded by its derived share of the volume);
  - the arena's per-region tables, 10.6 MB over 59 region additions;
  - the volume's content and inode records, about 26 MB: 801 B per file, as `create_heap` measures with the tree's
    sizes.
  - The NFS layer adds nothing per entry: the empty-file run's heap growth (496 B per file) equals the volume's
    create cost (498 B).
- **30.4 MB more in the content object** than the volume's committed blocks (`/memfd:slates-con-mem` 1,686,921,216 B
  against 1,656,542,072 committed, 15 s idle after the unmount): the shard's recovery memory, A-68's two checkpoint
  slots and its delta log in the same object, all three resident once written. A-117 gives back the stale slot and
  the log past its tail when the shard is idle: 12–19 MB of it on `/usr`, and an empty file 844 → 695 B (below
  tmpfs's 740–754 B). The A/B is A-117's table.

**Hypotheses measured and rejected on the way:**
- Client file state (opens, write delegations): the unmount, which returns them, freed nothing (Pss rose 16 MB).
- glibc keeping freed 256 KiB write buffers: with `GLIBC_TUNABLES=glibc.malloc.mmap_threshold=131072` the heap was
  62.9 MB against 64.0 MB.
- The op log's share: a 2 GiB maximum instead of 8 GiB left the heap at 64.4 MB.
- `committed_versions` (594,237): a reservation ledger, not live versions.

**Measured and rejected: an idle purge that returns runs below the discard size.** The idle purge (A-105) keeps a
dirty run shorter than the derived discard size (16 KiB here) resident, and drops it from the dirty set. Returning
every run instead returned 0.5 MB more of the 1.7 GB (`content.purged_bytes` 348,622,848 against 348,119,040, the
content object 1,646,690 KiB against 1,648,206 KiB): dead even, so A-105's rule stands. The patch and its test are
kept out of the tree.

**Owed:**
- Tail rounding: done by A-118 (a sealed chunk's block trimmed to its pages): block rounding 88.6 → 44.3 MB on
  `/usr`, the daemon 34–39 MB smaller at idle, copy time unchanged (the A/B is A-118's table).
- Windows: the content object gives no page back there (`SparseObject::discard`), so A-117 does nothing on that host;
  a measured decommit is owed. macOS marks the pages reusable since 2026-10-08 (A-117's macOS note).

### Measured and rejected: no write delegation for an open that creates (condition 12; 2026-10-08)

**The idea.** The `stat`, unlink and rename costs of write delegations come from files delegated at their create;
the reopen speed comes from delegations of existing files. So the arm (E) granted no write delegation to an
`OPEN4_CREATE`, and still granted one to an open of an existing file.

**The run.** Same command as the four-arm A/B, A and E interleaved three times each, load 4–14. Grants per run: A 2,462
(one per created file), E 1,123 (reopens only).

| Arm | p50 geomean vs A | p99 geomean vs A |
|---|---|---|
| E: no write delegation on create | ×1.05 | ×1.31 |

- E wins `stat` (34 → 6 µs at 1 worker, 58 → 11 µs at 16).
- E loses the reopens: overwrite 82 → 173 µs and read 63 → 119 µs at 1 worker. The first reopen of each new file now
  pays the OPEN round trip a create-time delegation saved.
- Rejected.

At this load A, today's policy, beats every arm measured, B included (×1.27 p50 and ×1.52 p99, from B's earlier runs
at higher load). So the policy stands: write delegations as today. The `stat` after a delegated write is the client
revalidating the space used, measured in the entry above.

### NFSv4 delegation policy, four arms (condition 12; 2026-10-08)

Command: `docs/wip/bench/mixed/linux_vs_nfsd.sh 400 64` (slates' rows). Each arm's Linux release binary was built in
its own target volume (A `56a4bb36f5ba`, B `e6639415e8f3`, C `e446e78a8291`, D `950fa1940fbd`), and the runs were
interleaved. Load average 9–28 from other sessions; C and D's second runs fell at the high end. Grants from the
daemon's status (`nfs4.delegation.granted`): A and C 2,462 a run (a write delegation per created file), B 0, D 1,491
(read only).

| Arm | Runs | p50 geomean vs A | p99 geomean vs A |
|---|---|---|---|
| A: write delegations, read delegations once a file has settled (today) | 5 | ×1.00 | ×1.00 |
| B: no write delegations | 5 | ×0.85 | ×1.13 |
| C: A, and read delegations without the settled rule | 2 | ×1.41 | ×2.31 |
| D: B, and read delegations without the settled rule | 2 | ×1.37 | ×2.94 |

Where B moves, by median p50:
- **`stat`:** 46–67 → 7–9 µs.
- **unlink:** 150 → 76 µs at 1 worker, 309 → 115 µs at 4.
- **create:** 535 → 359 µs at 4 workers.
- **reopens (overwrite, append, read):** 1.4–3× slower; 141 → 249 µs at 1 worker, 209 → 682 µs at 16.

The daemon's calls explain the reopens: without a delegation the client revalidates every open, with GETATTR
4,913 → 23,123 and ACCESS 2,713 → 10,975 a run. There were no recalls in any arm. So the cost of write delegations
sits on the client: it returns the delegation before an unlink or rename, and asks the server on `stat`.

C and D rejected: read delegations without the settled rule put a recall on every later change and sped no read
(D's read p50 230 µs against B's 196 µs). A against B is a split on performance. B also removes the one correctness
failure write delegations cause: Linux 6.17's truncate under a delegation, refused by mode bits (`GAPS.md`
2026-10-07). The choice was put to Ada with B recommended.

### The incumbent baseline: slates against the kernel's own NFS server, one Linux client (condition 12; 2026-10-08)

Command: `docs/wip/bench/mixed/linux_vs_nfsd.sh 400 64` in a privileged `rust:1.98.0` container (the header gives the
`docker run`). The Linux release build is the day's last code change (`cf35aba6`), and the Docker VM's kernel is
6.12.76-linuxkit. One Linux NFSv4.2 client over TCP mounts:
- slates (4 shards);
- the kernel's nfsd (8 threads) exporting a tmpfs as the v4 root;
- plain tmpfs, the floor.

`mixed.py` runs 400 operations a worker in one shared directory, then a 64 MiB file is written, fsynced and read
back. Every byte was identical. Load average 8–11 from other sessions.

| workers | op | slates p50 / p99 | kernel nfsd p50 / p99 | tmpfs p50 / p99 |
|---|---|---|---|---|
| 1 | create | 217 µs / 436 µs | 366 µs / 925 µs | 2 µs / 10 µs |
| 1 | overwrite | 110 µs / 242 µs | 370 µs / 725 µs | 1 µs / 3 µs |
| 1 | read | 76 µs / 166 µs | 175 µs / 492 µs | 3 µs / 4 µs |
| 1 | stat | 37 µs / 88 µs | 8 µs / 128 µs | 1 µs / 2 µs |
| 16 | create | 1.87 ms / 2.97 ms | 2.37 ms / 3.87 ms | 466 µs / 2.46 ms |
| 16 | overwrite | 136 µs / 1.68 ms | 477 µs / 1.24 ms | 403 µs / 2.14 ms |
| 16 | read | 99 µs / 2.62 ms | 272 µs / 962 µs | 1.30 ms / 3.89 ms |
| 16 | stat | 43 µs / 456 µs | 4 µs / 244 µs | 25 µs / 1.24 ms |
| 16 | rename | 1.62 ms / 2.67 ms | 2.11 ms / 3.81 ms | 96 µs / 1.56 ms |
| 16 | unlink | 1.74 ms / 2.62 ms | 2.06 ms / 3.50 ms | 154 µs / 1.38 ms |

- **Where slates wins:** the medians of every write-side operation at every concurrency, and the 16-worker tails
  of create, rename and unlink.
- **Where slates loses:**
  - the 16-worker read p99 (2.62 against 0.96 ms) and the overwrite and append p99s (1.68 against 1.24 ms);
  - `stat` medians (37–44 µs against 4–8 µs). The client keeps answering nfsd's from its cache and asks slates.
    That is a hypothesis, untraced: nfsd hands out read delegations, and slates' write-delegation policy is Ada's
    open decision.
- **Large files:** slates writes 546–817 MB/s against nfsd's 967–1,524 MB/s and the floor's 1.7–2.1 GB/s; slates
  reads 2.3–2.7 GB/s, about nfsd's.

**Measured and rejected: a 1 MiB transfer size** (Linux's `NFS_MAX_FILE_IO_SIZE`, what nfsd offers, against slates'
256 KiB). The client mounted `rsize=wsize=1048576`. The large write did not improve: 461–631 against 546–817 MB/s at
1, 4 and 16 workers. Reads rose a little (2.67–2.71 against 2.31–2.65 GB/s), too little to land on one run. So the
write gap to nfsd is in slates' own write path, owed a profile. Two tests encode the 256 KiB size as a literal and
would need deriving from the constant first (`rtmax is one arena chunk`; a listing sized to need two pages).

### Mixed read-write tails on macOS's own NFS client, and a shard QoS class measured (condition 12; 2026-10-07)

Command: `bash docs/wip/bench/mixed/mixed_mac.sh <release slates> <out dir> OPS LARGE_MIB`. It runs a release anchor
with 4 shards, a 2 GiB bounded volume mounted through `slates mount` (NFSv3 loopback, `mount_nfs`, no sudo), and
`mixed.py` in one shared directory at 1, 4 and 16 workers. Each worker's operations are weighted: create (open,
write 4 KiB, close) 30, overwrite 15, append 10, read 25, stat 10, rename 5, unlink 5. Then a large file is written
sequentially, fsynced and read back by checksum. The scratch APFS directory, which is on disk, is the reference. The
mount is always removed before the anchor stops (`macos-nfs-kill-test-panics-the-mac`). Apple M5 Max. **Load average
57 to 77 from other sessions throughout**: this is the busy machine, not a quiet baseline. Every byte read back was
identical.

One run at 400 operations a worker, load 67 to 74:

| workers | op | slates p50 | slates p99 | APFS p50 | APFS p99 |
|---|---|---|---|---|---|
| 1 | create | 4.1 ms | 137.7 ms | 64 µs | 3.6 ms |
| 1 | read | 73 µs | 2.8 ms | 21 µs | 77 µs |
| 1 | unlink | 1.3 ms | 108.8 ms | 35 µs | 50 µs |
| 16 | create | 30.8 ms | 456.2 ms | 265 µs | 54.2 ms |
| 16 | read | 398 µs | 28.3 ms | 44 µs | 24.5 ms |
| 16 | unlink | 41.5 ms | 557.8 ms | 98 µs | 31.2 ms |

Large file (64 MiB): slates 199–243 MiB/s write, 475–1,613 MiB/s read; APFS 545–1,489 MiB/s write.

- The daemon itself served each call in 3 µs at p50 and 53 µs at p99 (`nfs.local_p*_ns`) over the whole run. Nearly
  all of the client's time is outside the daemon's service: the macOS NFS client, the loopback socket, and each of the
  several cross-process hops an operation makes waiting for a core at load 70. The server sets `TCP_NODELAY` on every
  accepted connection (`slates-rt` `tcp.rs`).
- **RPCs per create, counted by the daemon.** Status now reports each shard's NFS calls by procedure
  (`ShardReport::nfs_calls`). Run with `bash docs/wip/bench/mixed/rpc_per_op.sh <slates> <out> 200`, at load 86:
  - 14.04 RPCs per create (open `O_CREAT`, write 4 KiB, close): getattr 5.03, lookup 2.0, create 2.0, write 2.0,
    commit 2.0, setattr 1.0;
  - p50 16.1 ms and p99 52.5 ms per create.
  - **Half of it is a file nobody asked for.** macOS attaches `com.apple.provenance` to every new file, and NFSv3
    carries no extended attributes, so the macOS client writes them into an AppleDouble sidecar (`._name`), which it
    hides from its own listing. That second file costs its own lookup, create, write and commit, so every create
    through the macOS v3 mount is two. Shown by `docs/wip/bench/mixed` (`xattr -l` on the new file names
    `com.apple.provenance`).
  - The levers sit on the client's side of the protocol: NFSv4 named attributes (macOS's v4.0 client supports them;
    slates serves 4.1 and 4.2), or another macOS transport. Each is a design question, owed with its A/B.
- **Each step's calls, cold attribute cache** (`bash docs/wip/bench/mixed/steps.sh <slates> <out>`, one shard):

  | Step | Daemon-counted calls |
  |---|---|
  | stat | getattr 2 |
  | open, read, close | getattr 2, access 1 |
  | overwrite 4 KiB at 0 | getattr 4, read 1, write 1, commit 1 |
  | create, close | getattr 4, setattr 1, lookup 2, create 2, write 1, commit 1 |
  | create, write 4 KiB, close | getattr 5, setattr 1, lookup 2, create 2, write 2, commit 2 |
  | unlink | getattr 3, lookup 3, access 1, remove 1 |

  - The overwrite's read is the client filling a partial page: Apple Silicon's page is 16 KiB, and a 4 KiB write
    covers a quarter of it.
  - An empty create still writes and commits once: the sidecar's provenance attribute.
  - **The getattrs are the client's, not a missing attribute.** CREATE returns the new file's attributes, and WRITE,
    COMMIT and SETATTR each return the file's `wcc` with its attributes after. Every reply carries what the client
    needs, so the getattrs are the macOS client's close-to-open revalidation at open and after the flush at close,
    and no server change removes them. Over NFSv3 on macOS a create costs about 7 calls at least, and about 7 more
    for the provenance sidecar.
- Against Tectonic's published tails (150–200 ms writes, about 100 ms reads; FAST'21): at 16 writers under load 70,
  slates' create and unlink p99 (456 and 558 ms) are worse, and its read p99 (28 ms) is better. The busy-host tail of
  the macOS loopback path is open work for condition 12.

**Measured, inconclusive: a `QOS_CLASS_USER_INITIATED` class on the shard threads.** One experiment binary, run with
and without `SLATES_EXPERIMENT_QOS`, three interleaved rounds a side at 300 operations a worker, load 57 to 62. The
experiment code was never committed. p50 and p99 in ms, round by round:

| workers | op | default | `USER_INITIATED` |
|---|---|---|---|
| 1 | create p99 | 118 / 163 / 179 | 70 / 87 / 146 |
| 1 | unlink p99 | 20 / 33 / 12 | 47 / 93 / 53 |
| 4 | create p50 | 7.1 / 27.7 / 16.6 | 4.8 / 2.5 / 4.5 |
| 4 | create p99 | 74 / 166 / 114 | 83 / 92 / 36 |
| 4 | unlink p99 | 56 / 109 / 164 | 44 / 19 / 25 |
| 16 | create p99 | 327 / 465 / 303 | 353 / 442 / 278 |
| 16 | unlink p99 | 318 / 460 / 256 | 365 / 443 / 266 |

Four writers improve clearly. One writer improves on creates and worsens on unlinks, and sixteen are even. A mixed
A/B does not land. The class also moves work onto performance cores, which costs battery, and it outranks other
processes. Owed: more rounds, a quiet-machine arm, and the energy cost (`powermetrics`) before any decision.

**Rerun 2026-10-08 on `bcc02163` at load 5–6** (same command, 400 operations a worker, 64 MiB large file; every byte
identical):

| workers | op | slates p50 | slates p99 | APFS p50 | APFS p99 |
|---|---|---|---|---|---|
| 1 | create | 507 µs | 966 µs | 58 µs | 81 µs |
| 1 | read | 38 µs | 85 µs | 19 µs | 40 µs |
| 16 | create | 6.2 ms | 15.9 ms | 549 µs | 1.7 ms |
| 16 | unlink | 6.4 ms | 16.3 ms | 202 µs | 802 µs |
| 16 | rename | 4.5 ms | 12.3 ms | 234 µs | 1.2 ms |
| 16 | read | 318 µs | 1.2 ms | 721 µs | 2.1 ms |

- At load 5 rather than 70, the 16-worker create p99 is 15.9 ms rather than 456 ms: most of the old tail was other
  sessions' load.
- The daemon's own service time per NFS call stayed p50 3 µs, p99 45 µs (`nfs.local_*`), so the remaining gap to
  APFS is in macOS's NFS client. It serializes directory changes in one shared directory, and spends about 14 RPCs
  a create, half of them on AppleDouble sidecars for extended attributes that NFSv3 cannot carry. The macOS
  extended-attribute transport (NFSv4 named attributes or another) is Ada's decision, owed.
- Large streams: 1.16–1.40 GB/s written, 2.24–2.54 GB/s read.

### The real crossing's window, read from the owner's sessions (condition 7; 2026-10-08)

Command: `IMAGE=slates:mr sh docs/wip/bench/multiregion/reads.sh <scratch> <8 MiB payload> 3` at `401e9ad3` (the
two-network topology, 100 ms ± 40 ms one way, 3% loss each way). The script now prints the owner's record sessions
(`fleet.sessions` in status). Load average 3–9.

Reads: a0 (same region) 0.15–0.16 s; across regions 91.4, 106.4, 111.9, 113.2, 114.9, 118.8, 119.8, 121.7 s
byte-identical; one refused after 55.9 s (`b1`, round 2), as in the run before.

The owner's (`a1`) sessions after the reads:

| To | Congestion window | Smoothed RTT | PTO | Spurious losses | Collapses |
|---|---|---|---|---|---|
| region-1 reader | 9,184 B | 237.3 ms | 364.7 ms | 0 | 0 |
| region-1 reader | 7,655 B | 230.0 ms | 450.8 ms | 0 | 0 |
| region-1 reader | 9,736 B | 247.2 ms | 588.4 ms | 0 | 0 |
| region-0 peer | 9,400 B | 0.15 ms | 1.15 ms | 0 | 0 |
| region-0 peer | 9,412 B | 0.18 ms | 1.18 ms | 0 | 0 |

- **The window, not loss, bounds the crossing.** About 9 KB per 240 ms round trip is about 40 KB/s a session. That
  matches 8 MiB in about 110 s with read-ahead's few windows in flight. No spurious loss and no persistent collapse
  were declared.
- **It is Copa's delay signal.** Copa's target rate is `1/(δ·d_q)`. With ±40 ms jitter each way, the standing RTT over
  `srtt/2` sits about 80 ms above the minimum, so δ = 0.5 gives about 25 packets/s, or about 6 packets (about 8 KB)
  in flight at 240 ms. That is the measured window. Candidate 4's correction (`W/(n+1)`, landed 2026-10-07) removes
  too little here: a 6-packet window takes few samples per `srtt/2` epoch.
- Owed: the controller change, A/B'd on `fetch_bench`'s thin and grid scenarios and on this crossing. Candidate 2
  read 5.4–6.5× on the jitter rows and failed only on slow links' queues. Before then, the reading needs an epoch
  that spans enough samples.

### Replication to the mirror region across a lossy crossing (condition 10; 2026-10-09)

Command: `IMAGE=slates:mr sh docs/wip/bench/multiregion/mirror.sh <scratch> <64 KiB payload> N` (two networks, a
router shaping the crossing; each round writes the file, snapshots it, and times its placement at home, then in the
mirror region by `volume placed --mirror`). Load average 8–24.

| Build | Crossing | Snapshots placed in the mirror | Mirror placement |
|---|---|---|---|
| `d26b01b0`, unshaped | none | 1 of 1 | 1.01 s |
| `d26b01b0` | 100 ms ± 40 ms, 3% loss | 0 of 6 (8 MiB and 64 KiB) | never in 120–300 s |
| `d4d8d81f` (mirror rounds paced from their path) | same | varies 0–5 of 5 a run | 2–5 s when placed |
| `aefcc6da` (the home record keeps the mirror placement) | same | 10 of 10 in two runs, then 3 of 15 | 2.1–4.8 s |
| `962d4a9e` (delivered pair keys recorded apart) | same | 23 of 24 over five runs | 2.0–6.2 s |
| `962d4a9e` | 150 ms ± 60 ms, 10% loss | 5 of 5 | 6.9, 8.1, 9.6, 17.8, 75.7 s |
| `962d4a9e`, every node limited to half a CPU (`CPUS=0.5`) | 100 ms ± 40 ms, 3% loss | 5 of 5 | 2.3–6.9 s |
| `962d4a9e`, half a CPU each | 150 ms ± 60 ms, 10% loss | 5 of 5 | 4.7, 6.6, 8.0, 13.1, 13.9 s |

- The four faults each stopped every placement in their own way: a mirror round judged stalled at its offer; a late
  shipment acknowledgement discarded; a healer's home re-record wiping the mirror placement, so a second job shipped
  other holders at the same position; and a node's own pair key and a delivered one sharing one owner on partition
  0, so each side refused the other's (GAPS, 2026-10-09).
- At 10% loss, 71 content rounds and 39 shipments found no free session to their targets: one record session a peer,
  lent elsewhere. That is the likely cost of the 75.7 s outlier, not yet measured apart.
- The one snapshot unplaced in the 3% runs was followed by a `NoSpace` write refusal on the owner (open in GAPS).

### Cross-region reads at HEAD: none refused after an unanswered forward is resent (condition 7; 2026-10-09)

Command: `IMAGE=slates:mr sh docs/wip/bench/multiregion/reads.sh <scratch> <8 MiB payload> 3` (the two-network
topology, 100 ms ± 40 ms one way, 3% loss each way; the image built from the tree under test). Load average 9–19.

| Build | Cross-region reads of 8 MiB (s), byte-identical | Refused |
|---|---|---|
| `c264b71c` | 30.2, 24.3, 28.2, 19.4, 34.3, 23.5, 18.3 | 2 of 9 (`HomedElsewhere`) |
| `cf1162ed`, the script's exit status fixed | 24.8, 25.4, 24.8, 26.1, 20.0, 29.3, 19.4, 19.3 | 1 of 9 |
| resend to an `alive()` owner | 22.8, 27.7, 31.9, 29.8, 31.4, 31.4, 26.3, 24.2 | 1 of 9 |
| resend unless confirmed dead, run 1 | 29.0, 32.0, 23.0, 20.8, 19.0, 22.0, 24.8, 21.9, 21.9 | 0 of 9 (`fleet.forward.retried` 1 on b2) |
| resend unless confirmed dead, run 2 | 23.5, 24.7, 28.2, 26.3, 33.9, 21.4, 33.6, 27.8, 25.6 | 0 of 9 (`retried` 1 on b0, 1 on b2) |

**Contested, 2026-10-09** (`CONCURRENT=1 IMAGE=slates:mr sh docs/wip/bench/multiregion/reads.sh <scratch> <8 MiB
payload> 3` at `22e16df3`: each round starts the three region-1 readers at once, all reading the same file across the
crossing). Rounds: 29.9 / 28.7 / 24.1 s, 22.8 / 24.9 / 27.3 s, 22.5 / 23.6 / 20.3 s. All nine byte-identical, none
refused. Each contested read took about as long as a read alone, so the crossing carried about three times the bytes
in the same time. Load average 12–22.

**Contested on a harsher crossing, 2026-10-09** (`CONCURRENT=1 DELAY=150ms JITTER=60ms LOSS=10% IMAGE=slates:mr sh
docs/wip/bench/multiregion/reads.sh <scratch> <8 MiB payload> 3`, three region-1 readers at once, 10% loss each way):

| Build | Contested reads served (of 9) | Refusals |
|---|---|---|
| `22e16df3` (resend only an unanswered forward) | 4 | 5 × `HomedElsewhere`, within 1.1–1.5 s: forwards never sent (`forward_unsent`, the session held), and one location round unavailable |
| + resend an unsent forward to a live owner | 7 | `LeaseUnconfirmed` (the owner's lease not yet confirmed) and `OwnerUnreachable` (no owner located), both in round 1 |
| + the client sends a read page again after either refusal | 9 | none; reads 34.7–65.5 s, all byte-identical |

The median read (about 25 s) is about 1.8× faster than the 44 s of 2026-10-08's landed build, from the day's other
changes or the load; it was not A/B'd.

### The read-ahead batch's rate cap removed: cross-region reads 2.7x faster on the real crossing (condition 7; 2026-10-08)

The owner's sessions (above) showed windows of about 9 KB on the crossing. The question was whether Copa or the read
path held them there. The read path's batch was capped at what the path's measured delivery rate moves in one
liveness budget. Every batch is app-limited by construction, since it asks for exactly its windows, so the cap fed
on its own small batches: a low rate held the next batch to one window, which measured a low rate again.
Delivery-rate estimation as BBR uses it never lets an app-limited sample lower the estimate
(draft-cheng-iccrg-delivery-rate-estimation §3.3).

Command: `IMAGE=<image> sh docs/wip/bench/multiregion/reads.sh <scratch> <8 MiB payload> 3` on the two-network topology
(100 ms ± 40 ms one way, 3% loss each way). Three readers in region 1 × three rounds, all N shown. Load average 4–8.

| Image | Cross-region reads of 8 MiB (s) | Forwards | Windows joined |
|---|---|---|---|
| `4f627c6e` (the rate cap) | 97.4, 108.4, 111.3, 117.8, 119.4, 119.7, 124.5, 128.6, 131.1 | 354 | 39 |
| experiment: the cap removed (uncommitted) | 28.5, 33.9, 34.6, 38.7, 41.3, 42.7, 47.3, 49.2, 51.7 | 24 | 369 |
| as landed (the cap and its rate removed) | 26.5, 33.1, 39.6, 41.3, 44.1, 47.9, 47.9, 49.8, 52.2 | 24 | 369 |

Every read in all three arms was byte-identical; none was refused. The median fell from about 119 s to about 44 s.
Held-session waits fell (`fleet.forward.session_out` 13 → 3).

**Correction (same day): the record session's window is not shown to be the next bound.** The owner's record sessions
to the three readers, sampled every 2 s during one round, held 10.8–11.6 KB windows, slowly shrinking, at 199–267 ms
RTT. The reads in that round took 31.8–46.2 s, about 180–260 KB/s each, but an 11 KB window at 240 ms allows about
46 KB/s. So either the reads' bytes do not all cross that one window, or the reported window is not what limits the
send. The read's real path (which sessions carry a batch's windows) must be identified before any controller change;
the "9 KB bounds the crossing" reading above came from idle snapshots.

The simulator did not show this. `fetch_bench`'s fetch keeps the pipe full, so it never runs app-limited. A
`crossing` scenario matching this link (1 Gbit/s, 200 ms, ±40 ms reordering each way, 3% loss each way) reads 8 MiB in
35.4–59.7 s with 31–43 KB windows and 196–353 spurious losses, while the real crossing showed 0 spurious losses. The
simulator's jitter and the real netem's differ, so a controller change is A/B'd on both.

### The crossing's carrying connections, read at last: windows grow, and reordering is read as loss (condition 7; 2026-10-08)

The correction above was right to doubt the 11 KB windows, and the reason is now shown. A forward rides the
*reader's* dialed record session, whose sender state lives in the owner's serve loop. Status reported only the
sessions the owner dialed. `302ea768` adds `served_sessions`. Sampled every ~6 s during a crossing round (100 ms ±
40 ms, 3% loss each way):

| Reader's connection at the owner | Window during its read | Smoothed RTT | Spurious losses |
|---|---|---|---|
| first | 25.8 → 28.4 → 41.5 → 50.0 KB, then 37.4 KB | 150–221 ms | 5 → 7 → 21 → 52 → 112 |
| second | 11.6 → 30.6 → 32.0 → 35.9 → 71.2 KB | 156–218 ms | 0 → 6 → 9 → 22 → 53 |
| third (not yet reading) | 11.4–11.6 KB | 188–216 ms | 0 |

- **Windows grow during a read, to 50–71 KB.** The 9–11 KB readings, here and in the first table above, were idle
  connections or the wrong ones.
- **The real crossing reorders, and the reordering is read as loss.** The carrying connections declared dozens to
  over a hundred spurious losses per read. The first connection's window fell from 50.0 to 37.4 KB as its spurious
  losses went from 52 to 112.
- **So the simulator's crossing scenario is a fair proxy after all:** 31–43 KB end windows, 196–353 spurious losses,
  35.4–59.7 s for 8 MiB, against about 44 s for the real reads. The "0 spurious losses" in the earlier tables came
  from the wrong connections.
- **Next bound, owed:** reordering mistaken for loss. RACK widens its reordering window per spurious retransmission
  (RFC 8985 §6.2). Each candidate is A/B'd on `fetch_bench crossing` first, then on this crossing.

### Measured-and-rejected: undoing Copa's loss reaction when its losses prove spurious (condition 7; 2026-10-08)

The idea was RFC 4015's Eifel response, as Linux applies it on DSACK. In the competitive mode, Copa halves `1/δ` on a
loss, at most once per round trip. The arm recorded `1/δ` before each halving and the packets declared lost behind
it. It restored `1/δ` once every one of those packets had been acknowledged late (`reorder::on_acknowledged`'s
spurious list). Unit-tested: restored after all losses proved spurious, kept behind any real loss. Counters:
reactions and undone reactions.

Command, both arms built in release at `7a8748c1` (HEAD in its own worktree and target directory; the arm on top),
run back to back on this Mac (Apple M5 Max, 2026-10-08):
`FETCH_BENCH_SEED=$n fetch_bench crossing` for n = 1..8, the default 64 chunks (4 MiB), one reader, one holder.

| Seed | HEAD (ms) | Undo arm (ms) | Reactions undone / taken |
|---|---|---|---|
| 1 | 16,576 | 16,576 | 0 / 4 |
| 2 | 21,013 | 21,013 | 1 / 19 |
| 3 | 32,431 | 29,546 | 5 / 32 |
| 4 | 18,549 | 22,230 | 4 / 30 |
| 5 | 15,010 | 20,206 | 8 / 34 |
| 6 | 22,647 | 22,647 | 2 / 19 |
| 7 | 16,577 | 18,385 | 4 / 23 |
| 8 | 22,632 | 21,808 | 4 / 22 |

The mean was 20.7 s on HEAD and 21.6 s with the arm: worse in three seeds, better in two, equal in three. Rejected;
the code was removed, not kept behind a switch.

What it showed:
- **The competitive mode does engage on the crossing** (4–34 loss reactions per 4 MiB read). The ±40 ms reordering
  keeps the queue from ever looking empty.
- **Most reactions follow a real loss.** With 3% real loss each way, only 0–8 of 4–34 had every loss behind them prove
  spurious. So the spurious losses are not what holds the window through `1/δ`.
- The window's remaining suspect is Copa's delay signal itself: reordering jitter read as queueing delay
  (`RTTstanding − RTTmin`) shrinks the target rate whether or not any loss is declared. That is the next candidate,
  A/B'd the same way.

### Landed: Copa reads summed jitter from its two least samples (condition 7; 2026-10-08)

**Why.** A trace of Copa's inputs on `fetch_bench crossing` (seed 1, 427 epoch samples) showed the read sitting at Copa's
own equilibrium on an empty 1 Gbit/s bottleneck:
- the queue estimate had a median of 6 ms (p90 25 ms) with no queue at all;
- `RTTstanding` sat 14 ms above `RTTmin` at the median, of which the jitter estimate removed 7 ms;
- the window was 30–56 KB, and `cwnd/RTTstanding` was about 385 KB/s, which is Copa's target `2/d_q` at 6 ms;
- the competitive mode was on in only 8 of 427 samples.

The landed jitter estimate (2026-10-07, above) modelled each sample's jitter as uniform: the least of `n` samples
`W/(n+1)` above the floor. A round trip's jitter is the sum of the two directions'. Near its floor the density of a
sum rises from zero (`F(x) ∝ x²`), so the least of 20 samples sits about `a·√(π/2n)` above it. Simulated over 4,000
epochs of 20 samples, with two one-way jitters uniform over 0–80 ms:

| | Mean excess over the floor |
|---|---|
| true (least sample less floor) | 22.1 ms |
| uniform model | 5.0 ms |
| landed estimate | 21.0 ms |

**What.** For a lower tail `F(x) ∝ x^α`, the least sample's expected excess is `α` times the expected gap to the
second least (extreme-value order statistics; Hill, Annals of Statistics 1975). With `α = 2` for summed jitter, the
epoch's excess is the larger of the uniform estimate and `2·gap`. Two guards keep a queue out of it, each A/B'd:
- **S:** the gap first loses the sender's own interval per packet, `smss·RTTstanding/cwnd`, because a queue spaces
  its samples by at least a service time;
- **T:** the gap counts in proportion to the epoch's turning points against the `2(n−2)/3` independent samples show
  (the turning-point test; Brockwell & Davis §1.6), because a filling or draining queue turns rarely.

**Commands** (Apple M5 Max; simulated network, virtual clock, deterministic per seed):
- `FETCH_BENCH_SEED=<1..8> fetch_bench crossing` (4 MiB, 1 Gbit/s, 200 ms, ±40 ms reordering and 3% loss each way);
- `FETCH_BENCH_SEED=<1..8> fetch_bench thin 128` (8 MiB on the 10 Mbit/s, 200 ms thin link);
- `congestion_bench` (the 58-scenario grid, three seeds each).

Both arms were built in release from the same tree.

Crossing, 4 MiB, mean of 8 seeds:

| Arm | Mean | Range |
|---|---|---|
| `HEAD` | 20.7 s | 15.0–32.4 s |
| `2·gap`, zigzag share only | 8.5 s | 5.9–10.1 s |
| T | 6.9 s | 5.6–8.0 s |
| S | 10.0 s | 7.6–15.6 s |
| **S and T (landed)** | **9.6 s** | **7.1–14.0 s** |

Grid against `HEAD`, by geomean, with the rows that moved more than 10%:

| Arm | Share | Steady ping p99 | Rows worse > 10% | Rows better > 10% |
|---|---|---|---|---|
| zigzag share only | ×1.001 | ×1.025 | 8, incl. burst loss 148 → 254 ms and 1 Mbit/s 100 ms no loss 135 → 183 ms | 3 |
| T | ×1.001 | ×1.008 | 7, incl. 64 kbit/s 300 ms 0.1% loss 586 → 794 ms | 4 |
| S | ×1.002 | ×1.003 | 3, incl. 10 Mbit/s 300 ms 1% loss 569 → 766 ms | 1 |
| **S and T** | **×0.999** | **×1.001** | **2: the reordering row 32.1 → 36.7 ms (its share 0.857 → 0.914); 1 Mbit/s 100 ms 5% loss 402 → 507 ms (one seed 383 → 635 ms, the others +14% and +2%)** | **2: 10 and 100 Mbit/s, 20 ms, 5% loss** |

Thin link, 8 MiB, median of 8 seeds (S and T against `HEAD`):

| Row | `HEAD` | Landed |
|---|---|---|
| reordering ±40 ms, no loss | 24.3 s (15.9–34.2) | 12.6 s (9.7–15.0) |
| reordering ±40 ms, 2% loss | 32.8 s (22.0–49.3) | 13.3 s (11.7–16.9) |
| jitter ±40 ms in order | 24.1 s (7.9–30.1) | 21.1 s (11.5–37.5) |
| 2% loss, in order | 8.0 s | 8.0 s |

The in-order jitter row's median improved but its spread widened (max 30.1 → 37.5 s).

**The one grid row that worsened, widened to 16 seeds** (`CONGESTION_BENCH_SEEDS=1,…,16 congestion_bench
"rate=1000000 rtt=100ms loss=5%"`, the commit before and the commit that landed it, same day): steady ping p99 median
396 → 418 ms (+5.6%), mean 418 → 442 ms (+5.7%), worst seed 557 → 635 ms; share 0.884 → 0.883. A small, real cost on
a 1 Mbit/s lossy link, not the one seed it looked like at three seeds; recorded against the crossing's 2×.

Measured and rejected the same day: the turning-point test applied as a test (no tail term when the epoch has fewer
turns than independence allows, beyond two standard deviations of variance `(16n−29)/90`) rather than in proportion.
The worsened row got worse again (16 seeds, mean 442 → 462 ms, worst 635 → 709 ms), and the crossing slowed on seed 1
(7.9 → 13.8 s, the other seven equal). An epoch that is part trend and part jitter loses its tail evidence whole.

**The real crossing.** Command: `IMAGE=slates:mr-<arm> sh docs/wip/bench/multiregion/reads.sh <dir> <8 MiB payload> 3`.
This is the two-network Docker topology, 100 ms ± 40 ms one way with 3% loss. Each run reads 8 MiB from b0, b1 and b2
in the other region, three rounds each. Both images were built from the same commit (`HEAD` from a worktree, the
estimator from the tree), and the runs alternated: `HEAD`, estimator, `HEAD`, estimator. Load average 4–7 from other
sessions.

| Arm | Reads | Median | Range | All byte-identical |
|---|---|---|---|---|
| `HEAD` | 18 | 49.0 s | 31.0–62.6 s | yes |
| estimator | 18 | 24.9 s | 18.9–33.0 s | yes |

The owner's served sessions ended with windows of 85–127 KB, against 22–44 KB at `HEAD`, still with hundreds of
spurious losses each (455–676). Reordering read as loss no longer costs window, because Copa's default mode ignores
loss. The window had been bound by the delay signal.

Cleaned up: the arms' code is the landed code only; the trace used to find this was removed.

### Landed: a write that covers a sealed window whole opens it without reading it (conditions 5 and 12; 2026-10-09)

Found by profiling a sustained overwrite over the macOS NFS mount (`sample` of the daemon, 2 s mid-write). Of the busy
shard's active samples, 665 were the socket receive, 290 memmove, 226 AES-GCM encrypt and 201 AES-GCM decrypt. A
pure write stream should decrypt nothing. Each WRITE reopened its window's sealed extent (`ChunkStore::reopen`):
- it decrypted the old chunk (A-99);
- it copied the old bytes twice;
- then the write overwrote them.

Now a write from a window's start at least as long as the window's sealed extent opens the window fresh
(`ChunkStore::replace`, non-vacuity counter `replaced_unread`). The old chunk is released by the same epoch rule, so a
snapshot keeps it. The fresh block holds the write's first window whole, so the write cannot stop partway with the old
bytes gone.

**Where it applies.** The window is 16 pages:
- on Linux, 64 KiB, and the kernel client writes 256 KiB (`wsize=262144`), so every aligned WRITE covers whole
  windows;
- on macOS, 256 KiB, and the mount asks for 128 KiB transfers (`MACOS_TRANSFER_BYTES`), so no WRITE covers one.

The macOS A/B was therefore even, as expected: 923–1,243 MiB/s and 6.8–7.6 s of daemon CPU in both arms, client-bound.

**Command:** `docs/wip/bench/overwrite/linux.sh` in a privileged `python:3.12-slim-trixie` container.
- A Linux release build of each arm, from its own target volume and source tree.
- A 512 MiB random file is overwritten six times per round in 1 MiB writes, with an fsync per pass, through the kernel's
  NFSv4.2 client on loopback; three rounds per container.
- Arms run new, base, new, base after base, new, base, new: twelve rounds each.
- Apple M5 Max, Docker Desktop 6.12 kernel, load average 10–17 from other sessions.

| arm | daemon CPU per 3 GiB, median (range) | throughput, median (range) |
|---|---|---|
| base (`53e182e6`) | 4.55 s (3.91–7.84) | 644 MiB/s (365–759) |
| replace unread | 3.64 s (3.07–4.86) | 760 MiB/s (599–912) |

Daemon CPU per byte fell 20% and throughput rose 18%. The daemon is CPU-bound on this path (CPU about equal to wall
time), so the CPU it saves is throughput. Linux's own nfsd measured 967–1,524 MB/s on the same storm (above), so the
remaining gap is still owed a profile.

### Landed: a written file's delta carries its written windows (A-126; conditions 5 and 12; 2026-10-09)

Found by profiling the daemon (`perf record` in the privileged container, 2 s mid-write) after `acb68d89`:
- `extent_image` was 10.6% of samples, under `encode_publication` and `publish_shard`;
- encoding the extent list was 3.0%, and iterating it 2.6%.

Every barrier re-imaged every extent of a written file. The kernel client sent 1,781 COMMITs in 18 passes, about
one per 5 MiB written, and each imaged all 8,192 of the 512 MiB file's 64 KiB windows. A file changed only by writes
to its chunked body is now published as its written windows: spans of whole windows, ascending and coalesced (A-126).

**Sequential overwrite** (`docs/wip/bench/overwrite/linux.sh`; Linux release builds, each from its own target volume):
- arms alternated `acb68d89`, patch, `acb68d89`, patch, then the reverse: twelve rounds each, three per container;
- Apple M5 Max, Docker Desktop 6.12 kernel, load average 7.5–11.4 from other sessions.

| arm | daemon CPU per 3 GiB, median (range) | throughput, median (range) |
|---|---|---|
| `acb68d89` | 3.92 s (3.21–5.05) | 753 MiB/s (592–886) |
| written windows | 2.56 s (2.13–3.06) | 1,143 MiB/s (945–1,358) |

That is 35% less daemon CPU per byte and 52% more throughput. Linux's own nfsd measured 967–1,524 MB/s on the same
container shape (2026-10-05), so this path now writes at kernel-server speed. After the change, publication is about 5%
of the daemon's samples (`extent_image` 2.5%, CRC 2.3%). What remains:
- memcpy, about 31%: RPC record reassembly 6.3%, the arena write 4.5% (the one a write needs), `open_window` 3.3%,
  `Call::parse` 3.1%, the v4-to-v3 WRITE re-encode 2.6%, seal 1.8%;
- AES-GCM at rest, 19.7% (condition 9);
- the kernel's receive copy, 12.8%.

**Random small writes, a database's pattern** (`docs/wip/bench/overwrite/random.sh`): the 512 MiB file is written once,
then each round makes 4,000 random page-aligned 4 KiB writes with an fsync every 8.
- Arms rotated base, hull, spans, then spans, hull, base: six rounds each.
- "Hull" is a first cut that recorded one range per file, the hull of its writes.

| arm | fsyncs per second, median (range) | daemon CPU per round |
|---|---|---|
| `acb68d89`, the whole file per COMMIT | 390 (240–527) | 0.96–2.05 s |
| one hull per file | 451 (385–486) | 1.03–1.29 s |
| spans (landed) | 1,645 (1,456–2,136) | 0.22–0.35 s |

**Measured and rejected: one hull per file.** A transaction's random pages span nearly the whole file, so the hull was
no better than the whole image: 451 against 390 fsyncs/s. Spans carry only the windows written: 4.2× the commit rate
at about a fifth of the daemon CPU.

**The same random pattern's fsync latency** (`random.sh` now times every fsync; 2026-10-09):
- arms base (`acb68d89`) and A-126 (`5fd6d3af`) alternated twice, three rounds each, 500 fsyncs a round;
- load average 15–16 from other sessions.

| arm | fsync p50 | p99 | p999 | max |
|---|---|---|---|---|
| base, the whole file per COMMIT | 2.83–3.23 ms | 3.52–6.22 ms | 5.29–12.94 ms | 12.94 ms |
| A-126, written windows | 0.48–0.74 ms | 0.75–1.37 ms | 2.81–4.36 ms | 4.36 ms |

A database's commit through the kernel client now waits under 1.4 ms at p99 on a busy machine, against 3.5–6.2 ms
before. Against Tectonic's published tails (blob writes reaching 150–200 ms; FAST'21, Figure 3), the comparison is of
what a caller waits for under the pattern, not of the media: slates commits to anchor RAM, Tectonic to disks across a
datacenter.

### Landed: NFS replies built as their own records and sent without gathering (2026-10-09)

Found by profiling the daemon during O_DIRECT reads through Linux's NFSv4.2 client (`docs/wip/bench/overwrite/` with a
read script; reads at 1.6–1.9 GB/s with the daemon CPU-bound). Each 256 KiB READ reply was copied four times in user
space before the kernel's own copy:

| copy | share of samples |
|---|---|
| into the compound's buffer (`compound::record`) | 5.3% |
| into the RPC reply (`reply_bytes`) | 4.1% |
| into a framed record (`write_record`) | 4.8% |
| into the turn's send run (`serve_connection`) | 3.4% |

The reply is now built as its own record (`rpc::reply_record`: the marker reserved, the header and the results written
once). A turn's records go out as slices in one `writev` (`TcpStream::send_records_vectored`), so a run is never
copied into one buffer, and the whole-send guarantee of A-113 still holds (XNU's `sosend` judges the whole request).
The arguments of each call are kept in place in their record body (`Call::args`), not copied out.

**Measured in process** (`reply_record` against `reply_bytes` then `write_record` then the run copy, a 256 KiB result,
20,000 replies, three passes, release build on Apple M5 Max at load 14–28): 15.1–17.0 µs per reply before,
3.77–3.85 µs after.

**In the daemon's profile:**
- memcpy fell from 21.9% to 16.8% of samples during reads;
- `reply_bytes`, `write_record` and the run's copy are gone;
- `reply_record` is the one copy of the results (5.6%).

**Through the kernel client the throughput A/B was within noise:**
- the `Call` change alone against the full change: 1,819 against 1,844 MiB/s median over nine rounds each;
- against the storm-fixed base, four alternations at load 21–52 spanned 351–2,051 MiB/s in both arms.

The change is landed on the in-process and profile measurements, not on that A/B. The compound's copy of the READ
data (5.3%) and AES-GCM decryption (29%, A-99's per-read open) remain.

### Kills during random writes against A-126's written-window deltas (condition 11; 2026-10-09)

**Command:** `docs/wip/bench/kill_writes.sh` (new) in a privileged `python:3.12-slim-trixie` container, on a Linux
release build of `faad1562`.
- A writer makes random 4 KiB writes into a 64 MiB file on an NFSv4.2 hard mount, with an fsync every 8, keeping its own
  copy of the file.
- Meanwhile the daemon is SIGKILLed `KILLS` times, `GAP` seconds apart, and the anchor restarts it. Every restart replays
  the journal, whose deltas carry written windows (A-126).
- After the writer's last fsync the volume is remounted, so the client's cache is cold, and the file is compared with
  the writer's copy.
- Load average 9–15 from other sessions.

| run | writes / fsyncs | kills | restarts | writer time | longest fsync | file after remount |
|---|---|---|---|---|---|---|
| `KILLS=10 GAP=1.5` | 200,000 / 25,000 | 10 | 10 | 25.7 s | 0.07 s | identical |
| `KILLS=10 GAP=1.5` | 200,000 / 25,000 | 10 | 10 | 15.6 s | 0.03 s | identical |
| `KILLS=30 GAP=0.3` | 200,000 / 25,000 | 30 | 30 | 14.8 s | 0.03 s | identical |
| `KILLS=30 GAP=0.3` | 200,000 / 25,000 | 30 | 30 | 13.6 s | 0.03 s | identical |

Every byte the writer wrote survived 80 SIGKILLs in all, each recovery replaying windowed patches. No fsync waited
longer than 70 ms across a kill: the anchor restarts the daemon from its RAM in milliseconds.

### The hot-directory storm after the probe-storm fix: delegations granted again (condition 12; 2026-10-09)

Command: `docs/wip/bench/hotdir/native.sh WORKERS 500` (Linux's own NFSv4.2 client against a daemon in the same
privileged container). Arms:
- spans (`5fd6d3af`-era, before `9c0d5057`, its back channel caught in the probe storm);
- HEAD (`faad1562`).

Alternated twice at 1 and 16 workers; load average 4.7–8.2.

| workers | op | before p50 / p99 | after p50 / p99 |
|---|---|---|---|
| 1 | create+write+close | 450–466 / 758–841 µs | 178–232 / 359–508 µs |
| 1 | open+read+close | 294–301 / 524–643 µs | 27–35 / 83–135 µs |
| 1 | ops/s | 3,582–3,748 | 9,485–12,684 |
| 16 | create+write+close | 4,019–4,149 / 6,075–6,593 µs | 1,698–1,740 / 3,186–3,390 µs |
| 16 | open+read+close | 1,362–1,431 / 4,566–4,799 µs | 948–981 / 3,458–3,933 µs |
| 16 | stat | 45–58 / 1,952–1,993 µs | 131–137 / 1,896–1,994 µs |

- With the back channel up, the client holds read delegations and serves an open of a file it delegated from its
  cache. A single writer's open+read fell about 9×, and CLOSEs halved (1,000 → 500).
- At 16 workers the elapsed time stays about 7 s: tmpfs takes the same, so Python's GIL bounds it, not the file system.
- Against Tectonic's published tails (writes reaching 150–200 ms, reads about 100 ms; FAST'21): sixteen writers in one
  directory now see create p99 3.4 ms and read p99 3.9 ms.
- Owed: at 16 workers `stat`'s p50 rose from 45–58 to 131–137 µs (its p99 is unchanged), with GETATTRs at 91–697 against
  81. The client revalidates more under delegations; not yet traced.

**Follow-up, same day: NFSv4 replies framed in place.** The compound now encodes its results after the record marker
and RPC header reserved by the server (`compound::serve_into`, `rpc::success_record_head`), which fills them in last
(`rpc::finish_success_record`). The results are no longer copied to be framed:
- `reply_record`'s 5.6% is gone;
- memcpy fell from 16.8% to 10.1% of the daemon's samples on the O_DIRECT reads;
- the remaining copies are the decrypted chunk's into the read buffer (6.1%, the one a read needs) and the compound's
  `record` of the READ result (3.7%).

The throughput A/B (three alternations at load 21–28) was swamped by load spikes that landed on the in-place arm's
rounds: 578, 612 and 908 MiB/s, against 1,714–1,993 MiB/s in its unloaded rounds and 1,576–2,026 MiB/s for HEAD. It is
landed on the profile and on the copy removed, measured above at about 3.8 µs per 256 KiB reply. A wire test holds the
in-place framing byte for byte to `reply_record` over 2,000 cases.

### Measured and rejected: a deeper macOS NFS read-ahead (2026-10-09)

The macOS mount leaves `NFS_MATTR_READAHEAD` at the kernel's default of 16 READs ahead. An experiment binary set it
from the environment (attribute 6 in `MountArgs::encode`, never committed).
- Each run: 3 GiB read six times through `slates mount` with `F_NOCACHE`, from a cold client cache. The file was written,
  then the volume unmounted and mounted again.
- Arms alternated twice; Apple M5 Max at load 23–30.

| read-ahead | 3 GiB read rate | daemon CPU per run |
|---|---|---|
| default (16) | 570–1,247 MiB/s | 4.7–5.7 s |
| 32 | 775–1,388 MiB/s | 4.5–5.6 s |
| 64 | 526–834 MiB/s | 5.3–5.9 s |
| 128 | 303–1,280 MiB/s | 4.8–5.9 s |

Dead even within the load's noise, so it does not land.

A first sweep had read the cached copy at about 10 GB/s with 0.5 s of daemon CPU: `F_NOCACHE` stops caching but
does not evict what the write cached. Any macOS read benchmark must remount after writing.

On these cold reads (428–473 MiB/s on HEAD at load 20) the daemon's samples are mostly waits in `kevent`. Under that
load the macOS `sample` tool also counts preempted threads at whatever user function they were in, so small functions'
counts are not their CPU time.

### Exactly-once across daemon kills: the mixed kill bench, before and after A-127 (condition 11; 2026-10-09)

**Command:** `docs/wip/bench/kill_mixed.sh` (new) in a privileged `python:3.12-slim-trixie` container, with
`SEED=<seed> OPS=30000 KILLS=30 GAP=0.4`, on Linux release builds.
- A Linux NFSv4.2 client makes 30,000 random operations over at most 16 files: creates, writes inside and past the end,
  truncates shorter and longer, appends, unlinks and renames. Each opens, acts and closes its file.
- Meanwhile the daemon is SIGKILLed 30 times, 0.4 s apart, and the anchor restarts it.
- The workload keeps a model of every file and records any operation that fails, with whether its file exists afterwards.
- After the storm the volume is remounted, so the client's cache is cold, and the tree must equal the model name for name
  and byte for byte.
- Since A-127 the bench also reads each daemon life's `nfs4.replay.*` counters just before killing it, and sums them.
- Load average 14–40 from other sessions.

Before (`e494963d`-era build, `linux-inplace`):

| seed | result |
|---|---|
| 101 (first attempt, 20 kills) | `unlink` raised `ENOENT` for a file the model held |
| 101 (5 reruns) | identical |
| 8 | `unlink("r1668")` raised `ENOENT`; the file was gone after |
| 9 | `rename("f23670", "r23676")` raised `ENOENT`; the tree held `r23676`, the model did not |
| 7, 10, 11 | identical |

After (A-127), three builds as the change was refined:

| build | seeds | kills | result |
|---|---|---|---|
| first cut | 8, 9, 101, 7, 10, 11 | 180 | identical; the divergence counter fired 2–14 times a life, false positives fixed in the next build |
| divergence fix | 11, 8, 9 | 90 | identical; no divergence |
| final | 8, 9, 101, 7, 10, 11 | 180 | identical; no divergence. Per seed, 29–49 records restored over the killed lives; seed 11's lives answered one retry from its record |

- 450 kills across 15 seeds without a failed operation, against 3 of 13 seeds failing before.
- The window the bug needs, between a barrier and its reply's send, is narrow: the final 180 kills hit it once, and that
  retry was answered from its record instead of being run again.

### The cost of call records: a single writer's create storm, before and after A-127 (2026-10-09)

**Command:** `docs/wip/bench/hotdir/creates.sh` (new), alternating a build without A-127 (`e494963d`-era) and the final
A-127 build, six rounds each, in privileged containers on this machine. Every create opens with a compound whose reply
the client asks to be kept, and closes with another, so both are recorded.

| round | load (1 min) | before: creates/s, p50, p99 | after: creates/s, p50, p99 |
|---|---|---|---|
| 1 | 40 / 34 | 4,675, 187.6 µs, 438.5 µs | 4,708, 191.8 µs, 401.1 µs |
| 2 | 28 / 25 | 4,618, 195.9 µs, 410.0 µs | 4,434, 202.2 µs, 420.3 µs |
| 3 | 17 / 15 | 4,549, 195.4 µs, 426.6 µs | 4,385, 206.4 µs, 423.3 µs |
| 4 | 15 / 13 | 4,674, 192.5 µs, 408.0 µs | 3,098, 239.5 µs, 2,814 µs |
| 5 | 17 / 18 | 1,500, 241.6 µs, 7,523 µs | 3,595, 228.8 µs, 1,707 µs |
| 6 | 20 / 18 | 1,851, 214.4 µs, 5,303 µs | 4,333, 199.8 µs, 459.5 µs |

- Rounds 1–3 are undisturbed in both arms: +4, +6 and +11 µs on a p50 near 195 µs (2–5%), and −4% to +1% in creates per
  second. Rounds 4–6 each have one arm hit by a load spike, in both directions.
- In one daemon, the same OPEN+CLOSE pair alternately kept and not kept, 4,000 pairs each in interleaved blocks of 250
  (a scratch harness over the restart suite's NFSv4 client, macOS release, three runs): p50 57/57/58 µs not kept against
  59/62/59 µs kept. That difference includes the slot cache's copy of a kept reply, which predates A-127.
- Measured and rejected, in the first cut, at 192 → 216 µs median p50 and 4,519 → 4,061 creates/s (five alternated
  rounds):
  - a record that rode every publication once durable, re-carried for each read after its change;
  - a record that rode publications for file-state changes too;
  - BLAKE3 as the argument digest, 0.47% of the daemon's samples, on its portable path.
  The replacements: a record rides a publication only for a volume change, a file-state change's record rides its own
  transaction, the digest is FxHash's word mix, and answering skips the lookup while no restored record is held.
- Owed: a state transaction carries the slot's whole record, the entries a publication already carried included. A record
  split between the image's prefix and the transaction's suffix, merged at restore, would trim those entries.

### NFSv3 mutations on a held connection under kills, before and after A-128 (condition 11; 2026-10-09)

**Command:** `SLATES_TEST_CLI=1 cargo test -p slates-cli --test nfs_held a_mutation_applied_before -- --nocapture`
(macOS 26, debug build, the real `slates anchor`).
- One held loopback connection pipelines bursts of 24 MKDIRs of fresh names.
- Each burst's daemon is `kill -9`ed partway in: half the kills follow the burst's first reply, half come at a fixed
  delay.
- A burst's requests stay in the socket until the turn that answers them has sent its replies (A-113).
- Load average 70–85 from other sessions.

| build | runs | kills | MKDIRs first answered `NFS3ERR_EXIST` | answered from held replies |
|---|---|---|---|---|
| before (A-127, no held replies) | 1 | 20 | 10 of 480 | (counter absent) |
| A-128 | 8 | about 160 | 0 | 1–38 per run |

- Before, the run-again requests came in runs of one turn (`d03-00`…`d03-03`, `d06-09`…`d06-12`).
- After, a successor restored 7–23 held replies per death that printed. Most were replies whose requests had already
  been consumed, and those are never asked for.
- A-113's WRITE-burst flow at the same load: longest kill-to-answer about 1.2 s in some runs and 190–270 ms in others,
  in both the base build and this one, three alternations each. The 1.2 s runs are the anchor's crash-loop backoff
  (A-119): its log says "restarting it in 1000 ms" when kills come faster than the restart bound derived from the start
  p99, which load raises. A-113's 123–126 ms was recorded at lower load.

### Landed: one pressure sampler a daemon, holds sent only when they change (battery; 2026-10-09)

**Command:** `idle_csw.sh <binary> <instance> 60 [shards]` (a scratch script, its whole body below). It starts `slates
anchor` from a release build, waits 15 s after the daemon appears, and reads the daemon's context switches from
`top -l 1 -pid <daemon> -stats csw` before and after 60 s. Arms alternated: the base `5dd8f3dd` and the change.
macOS 26 on this 18-core machine, load average 75–109 from other sessions.

```sh
"$BIN" --instance "$INSTANCE" anchor ${4:+--shards $4} > /dev/null 2>&1 &
# … wait for `pgrep -f "instance $INSTANCE daemon"`, sleep 15 …
A=$(top -l 1 -pid "$D" -stats csw | tail -1 | tr -dc '0-9'); sleep 60
B=$(top -l 1 -pid "$D" -stats csw | tail -1 | tr -dc '0-9')
echo "csw_per_s=$(( (B - A) / 60 ))"
```

| shards | base: context switches/s (per 60 s run) | one sampler, sent on change |
|---|---|---|
| 5 (this machine's default) | 36, 36, 39 | 27, 27, 27 |
| 16 | 113, 142 | 43, 39 |

- **Cause:** every shard runs a reap loop, and each loop sampled the host's memory and sent its hold to every shard
  every second. N shards sent N² messages a second at rest, and a message to a parked shard wakes it.
- **Change:** only the control shard samples, and a hold goes to a shard only when it differs from the last one sent. A
  daemon at rest sends none.
- The saving grows with the square of the shards: 25–30% of an idle 5-shard daemon's switches, 65–72% of a 16-shard
  one's.
- `top`'s idle-wakeup column read 0 for every process at this load, since no core was idle. Context switches are the
  measurable stand-in here; a quiet machine's `powermetrics` run is owed.

### Landed: the daemon process's thread waits on the heartbeat instead of polling (battery; 2026-10-10)

**Command:** the same `idle_csw.sh` as the one-sampler entry above (release builds, a 15 s settle, then `top`'s context
switches over 30 s), alternating `8d2343a6` and the change, one shard each. macOS 26 on this 18-core machine, load
average 18–30.

| build | context switches/s at rest, one shard |
|---|---|
| `8d2343a6` | 34, 32 |
| the change | 24, 23 |

- **Found by fitting idle switches against the shard count** at `8d2343a6`: 38, 36, 40 and 51 a second at 1, 2, 4 and 8
  shards. That is about 35 a second per daemon and about 2 per shard. `sample` of an idle one-shard daemon showed
  three threads:
  - the shard, in `kevent`;
  - the doorbell thread, in a one-second `os_sync_wait_on_address_with_timeout`;
  - the process's main thread, in `park_timeout` every heartbeat (100 ms), asking whether a signal arrived, whether the
    anchor asked for a stop, and whether the anchor still lives. Each ask of the anchor's stop word also allocated: it
    rebuilds the segment's whole region list to find one region.
- **Change:** the control shard's heartbeat, which beats at that cadence anyway, asks the same three questions
  (`Daemon::wait_for_stop`), and the process's thread blocks on a channel until one holds. Stop latency is unchanged:
  one heartbeat.
- **Proven by use:** the CLI's anchor-death flow ("the anchor is gone; leaving"), the SIGTERM drain flow ("stop
  requested; drained", leadership handed off before exit) and the restart flows, all with `SLATES_TEST_CLI=1`.
- **Left as it is:** the anchor's own 11 switches a second at rest (it reads the daemon's heartbeat each beat, so a
  wedged daemon is caught within one beat of the liveness budget; crashes reach it through its exit watch), and the
  doorbell thread's one-second bound.

### Landed: an anchor region found by arithmetic, not by building the layout (CPU; 2026-10-10)

**Command:** a scratch test, not kept, over a real `AnchorSegment` (release, this machine, load 20–45). It reads every
pool owner word (`pool_owner`) of a fresh segment 200 times and reports the mean per read and per whole-pool scan:
5 and 18 partitions, 64 owner words each. Arms: `ce01b29b`, the cached-table cut, and the final arithmetic lookup.

| build | per owner word, 5 / 18 partitions | one whole-pool scan, 5 / 18 partitions |
|---|---|---|
| `ce01b29b` (each lookup builds the region list) | 189 ns / 340 ns | 60 µs / 391 µs |
| final: `Geometry::region` by arithmetic (three runs) | 11–15 ns / 9–14 ns | 3–4 µs / 10–16 µs |

- **Cause:** every word of the anchor segment is read through its region, and `Geometry::region` built the whole list,
  about 65 entries at 18 partitions, to find one. It cost 128–233 ns a lookup (a second scratch timing over
  `Geometry` alone).
- **Where it fell:**
  - a log append does about seven lookups;
  - a pool scan does partitions × 64. One scan runs on every `Store::admittable` (a dynamic volume's `statfs`, every
    refusal), every admission that grows from the pool, and every pressure refresh.
- **Measured and rejected:**
  - a region table built once per attachment and indexed: 6–8 ns a word, but it adds a field to `AnchorSegment`, which
    every shard's state carries;
  - keeping the arithmetic lookup out of line (`#[inline(never)]`): 8–15 ns against 9–15 ns inlined, dead even.
- **Observed, not a cost:** `provision_bench`'s release binary grew from 5.9 to 8.1 MB of machine code with any of the
  cuts, about 2,250 more symbols, nearly all fleet and TLS code. Under the whole-program LTO, the base build had dropped
  code this in-process bench never reaches. The shipped `slates` binary is the same size either way:
  11,752,832 bytes before against 11,751,200 after, machine code within 648 bytes.
- `every_region_is_found_at_its_place_in_the_layout_and_no_other` holds the arithmetic to the layout's own list for
  four shapes of geometry, and refuses kinds past it.

### Landed: the anchor looks at a running daemon when its heartbeat could lapse, not every beat (battery; 2026-10-10)

**Command:** `anchor_csw.sh <binary> <instance> 30`, a scratch script. It starts a one-shard `slates anchor`, settles
15 s, and reads `top`'s context switches for the anchor and the daemon over 30 s. Arms alternated: the build with the
daemon's stop watch, and the change. Load average 26–30.

| build | anchor, context switches/s | daemon |
|---|---|---|
| before | 11, 11 | 26, 25 |
| the change | 2, 2 | 26, 25 |

- **Cause:** the anchor's observation loop polled every heartbeat (100 ms) for a lapse, though exits and held devices
  already wake it through descriptors.
- **Change:** a running daemon is looked at when its heartbeat could first be found lapsed, the last beat plus the
  liveness budget, and at least every half budget. The half keeps the loop's own gap under the budget, which it would
  otherwise read as the host having slept (`lapse_wait`).
  - A starting daemon is still looked at every beat, since its first beat times its start.
  - A silent daemon is now found lapsed at the budget's end, not up to a beat later.
- **Found on the way:** once a lapse's kill was sent, the loop judged the same stale heartbeat again each turn and
  killed and logged again until the daemon was reaped. With the old 100 ms pause that was 21 log lines for one freeze;
  with the new zero wait it spun (102). A kill is now sent once and the loop waits on the exit.
- **Proven by use:** `a_frozen_daemon_is_killed_at_its_heartbeat_lapse_and_restarted` (new: `SIGSTOP` the daemon; it is
  replaced after the budget less one beat, one kill logged), 3 of 3. The anchor-death, restart, signal-drain and
  `nfs_held` flows pass.
- At rest, one shard's daemon and anchor together: 48 context switches a second this morning (37 + 11), 27 now
  (25 + 2).

### Landed: a landing's files reach the disk in batches, one durability barrier each (A-130; 2026-10-10)

**Commands:**
- The batch-size sweep: a scratch A/B (`crates/land/examples/sync_ab.rs`, not kept), release, 2,000 files of 4 KiB in a
  fresh directory per arm and three rounds. Per file: create a temporary and write it, then write it out (macOS
  `fsync`; Linux nothing). Per batch of `n`: one barrier (macOS `F_BARRIERFSYNC` on the batch's last file; Linux
  `syncfs`), then rename each file. "Per file" is the engine before A-130: Linux `fdatasync`, macOS `F_BARRIERFSYNC`
  on each file. The busy rounds run the same while a second thread rewrites a 256 MiB region of a file on the same
  filesystem in 1 MiB writes, never syncing.
- Concurrent `fdatasync` (`sync_ab concurrent`): `d` threads each write and `fdatasync` their share, then every file
  is renamed.
- macOS: this machine (Apple Silicon, APFS), load 13–16. Linux: Docker Desktop's VM (kernel 6.12.76-linuxkit, ext4
  on the VM's virtio disk), a container capped at 2 GiB and 2 build jobs, run as a non-root user; load on the host
  23–26.

**macOS APFS, µs a file** (longest single barrier in parentheses, µs):

| round | per file | n=1 | n=2 | n=4 | n=8 | n=16 | n=32 | n=64 | n=256 | n=2000 |
|---|---|---|---|---|---|---|---|---|---|---|
| quiet 0 | 953.9 | 882.7 (2,518) | 471.2 | 274.7 | 203.9 | 205.8 (2,485) | 173.0 | 161.9 | 151.6 | 169.5 (11,403) |
| quiet 1 | 604.2 | 477.7 (7,345) | 338.3 | 309.6 | 228.3 | 191.9 (1,460) | 177.2 | 169.7 | 187.0 | 168.3 (11,175) |
| quiet 2 | 869.3 | 646.2 (16,308) | 535.2 | 329.3 | 251.6 | 210.9 (3,542) | 174.8 | 170.2 | 133.1 | 139.4 (5,488) |
| busy 0 | 860.2 | 1,382.8 | 583.5 | 481.3 | 261.5 | 171.3 (1,311) | 148.3 | 167.9 | 209.8 | 178.6 (5,286) |
| busy 1 | 590.0 | 558.9 | 329.7 | 277.3 | 536.5 | 467.9 (38,989) | 199.5 | 164.4 | 154.5 | 166.7 (4,032) |
| busy 2 | 676.3 | 586.1 | 452.7 | 339.2 | 256.0 | 219.0 (2,809) | 228.5 | 193.0 | 180.7 | 181.2 (5,046) |

The macOS busy writer rewrote at memory speed (372,226 MiB in 25.9 s): APFS absorbed it in the page cache, so it loaded CPU
and memory more than the disk.

**Linux ext4, µs a file:**

| round | per file (`fdatasync`) | n=1 | n=4 | n=16 | n=64 | n=256 | n=2000 |
|---|---|---|---|---|---|---|---|
| quiet 0 | 948.2 | 1,753.4 | 629.0 | 107.6 | 70.4 | 50.1 | 24.6 |
| quiet 1 | 685.0 | 1,283.9 | 316.8 | 146.2 | 43.0 | 23.5 | 18.6 |
| quiet 2 | 403.2 | 1,509.1 | 436.6 | 111.1 | 45.3 | 27.0 | 20.2 |
| busy 0 | 648.5 | 76,054.6 | 23,901.6 | 4,045.1 | 1,361.4 | 439.1 | 96.9 |
| busy 1 | 776.7 | 92,060.3 | 27,518.1 | 7,847.4 | 2,443.5 | 412.9 | 255.6 |
| busy 2 | 794.5 | 71,951.7 | 37,585.6 | 6,432.9 | 2,513.7 | 733.1 | 163.3 |

Longest single `syncfs` beside the busy writer: 604 ms, 3.72 s and 2.80 s (rounds 0–2).

**Linux concurrent `fdatasync`, µs a file:**

| round | d=1 | d=2 | d=4 | d=8 | d=16 |
|---|---|---|---|---|---|
| quiet 0 | 1,690.0 | 1,410.2 | 1,073.1 | 598.6 | 565.5 |
| quiet 1 | 4,765.1 | 3,066.3 | 1,776.8 | 1,454.6 | 630.8 |
| quiet 2 | 2,518.9 | 2,417.3 | 1,676.1 | 276.2 | 255.1 |
| busy 0 | 1,665.5 | 553.8 | 374.5 | 204.6 | 176.8 |
| busy 1 | 755.8 | 660.7 | 368.0 | 219.4 | 148.8 |
| busy 2 | 844.9 | 488.0 | 333.5 | 173.5 | 142.4 |

**Through the engine, macOS APFS** (`land_bench flat`, release; the per-file barrier is HEAD `85958800` built in a
scratch worktree, interleaved with the change; µs a new file, median of 5 runs; load 24–37 from other sessions):

| round | arm | 1,000 | 2,000 | 4,000 | 8,000 | 16,000 |
|---|---|---|---|---|---|---|
| 1 | barrier a file | 916 | 1,068 | 1,072 | 961 | 1,137 |
| 1 | batched (my test builds overlapped it) | 449 | 602 | 713 | 641 | 591 |
| 2 | barrier a file | 1,764 | 1,538 | 1,366 | 1,361 | 1,199 |
| 2 | batched | 423 | 429 | 446 | 461 | 525 |

Batched is 1.5–4.2× faster, and it held its cost while the load rose between rounds; the per-file barrier did not.
Over the simulated host the arms are within noise, 4.4–5.3 against 4.2–5.6 µs an entry. A third arm with the
simulated host's new bookkeeping stubbed ran 4.9–5.4, so the oracle adds no measurable cost at this load. The gated
`land.land_per_entry_sim_1000_of_100000_engine_only` row ran 13.6–14.1 µs (HEAD) against 14.0–15.8 µs (medians, three
rounds), with intervals overlapping. Both arms are about twice the row's 7,016 ns ceiling at this load, so the
ratchet would fail them alike until the machine quiets.

**The landing's percentile lane, before and after** (`landing_fairness a_large_landing -- --nocapture`, debug; HEAD's
binary from the same scratch worktree, interleaved; 600 files on a one-shard daemon probed by reads, provisions and
leases; load 27–29):

| arm | landing | slices | slice p99 | slice p999 | slice max | read p50 | provision p50 | lease p50 |
|---|---|---|---|---|---|---|---|---|
| barrier a file | 1.24 s | 642 | 5.2 ms | 10.4 ms | 10.4 ms | 1.49 ms | 2.98 ms | 2.78 ms |
| barrier a file | 1.97 s | 1,206 | 7.3 ms | 21.0 ms | 28.0 ms | 0.96 ms | 2.18 ms | 1.27 ms |
| barrier a file | 1.26 s | 1,172 | 3.1 ms | 8.4 ms | 8.7 ms | 1.03 ms | 2.67 ms | 2.22 ms |
| batched | 0.86 s | 1,790 | 1.2 ms | 4.2 ms | 4.7 ms | 0.36 ms | 0.98 ms | 0.73 ms |
| batched | 0.88 s | 1,844 | 1.0 ms | 3.7 ms | 4.6 ms | 0.38 ms | 0.91 ms | 0.71 ms |
| batched | 1.93 s | 1,663 | 3.1 ms | 11.5 ms | 21.7 ms | 0.64 ms | 1.42 ms | 1.13 ms |

No slice ran past its budget in either arm. The work a landing's shard serves meanwhile waits 2–3× less at the
median, because fewer units block it on a sync.

**Seen in both arms, not caused by the change:** one shard step of 126–185 ms per run, and one probe per run waiting
behind it. The 2026-10-01 runs, at load 4–6, saw longest steps of 13.8–25.5 ms. Open (GAPS, the same day).

- **macOS:** batches of 16 take 89–94% of the gain to 2,000 (the copy window's bound, A-130); 64 take another 15–20%,
  not taken until the daemon accounts a descriptor share.
- **Linux, `syncfs` rejected:** fastest when the disk is quiet (19–25 µs a file at 2,000) and worst beside any other
  writer, because it writes back that writer's dirty data too. At 16 files a batch it costs 5–12 times the per-file
  `fdatasync`, with single barriers of seconds: one stalls the shard that runs it. Rejected for files and for §4.15's
  directory sync.
- **Linux, owed:** concurrent `fdatasync` holds up beside the busy writer (142–177 µs a file at 16, against 756–1,666
  at one), because journal group commit merges in-flight fsyncs without touching other writers' data. It is Linux's
  batch barrier, built next with its own A/B through the engine. The quiet rounds were noisy (the d=1 baseline ran
  1,690–4,765 µs, against 403–948 in the sweep an hour earlier).

### Landed: Linux's batch barrier, every file's `fdatasync` in flight at once (A-130; 2026-10-10)

**Commands:**
- **The syscall A/B:** a scratch (`sync_ab.rs`, not kept) in the engine's shape. Batches of `n` files of 4 KiB: per
  file a temporary created and written, optionally its writeback started (`sync_file_range(SYNC_FILE_RANGE_WRITE)`);
  per batch the barrier; then each file renamed. 2,000 files per arm, three rounds, quiet and beside the busy writer
  of the entry above.
- **The engine A/B:** `land_bench flat` for `20b302aa` (one `fdatasync` a file) and the change, each built in its own
  target directory in one container and interleaved.
- **Where:** Docker Desktop's VM, kernel 6.12.76-linuxkit, ext4, 2 build jobs, a 3 GiB cap, non-root. The VM was far
  busier than at noon (load 24–49 inside it, from other sessions' containers): the same per-file `fdatasync` that cost
  403–948 µs then costs 2,670–7,003 µs now. The arms ran interleaved, so they saw the same VM.

**The syscall A/B, µs a file** (longest barrier in parentheses, ms):

| round | sequential, one a file | sequential, n=16 | writeback first, then sequential, n=16 | concurrent, n=16 | writeback first, then concurrent, n=16 | concurrent, n=8 |
|---|---|---|---|---|---|---|
| quiet 0 | 7,003 | 9,554 (527) | 2,198 | 1,846 (213) | 1,034 (142) | 2,327 |
| quiet 1 | 5,685 | 7,699 (615) | 1,968 | 890 (68) | 636 (31) | 861 |
| quiet 2 | 3,152 | 2,232 (142) | 429 | 644 (55) | 507 (45) | 598 |
| busy 0 | 2,670 | 4,591 (474) | 1,726 | 624 (75) | 689 (81) | 1,377 |
| busy 1 | 3,795 | 3,477 (259) | 822 | 446 (27) | 316 (13) | 1,249 |
| busy 2 | 4,142 | 5,558 (509) | 816 | 1,122 (129) | 618 (46) | 1,300 |

**Through the engine, µs a new file** (median of 5 runs; load inside the VM at each arm's start):

| round | arm | load | 1,000 | 2,000 | 4,000 | 8,000 | 16,000 |
|---|---|---|---|---|---|---|---|
| 1 | one `fdatasync` a file | 39 | 4,800 | 5,331 | 11,091 | 4,846 | 3,426 |
| 1 | concurrent barrier | 35 | 1,618 | 1,198 | 1,501 | 1,435 | 965 |
| 2 | one `fdatasync` a file | 49 | 1,793 | 2,395 | 2,193 | 3,185 | 781 |
| 2 | concurrent barrier | 24 | 208 | 182 | 219 | 218 | 206 |

- **Round 1, at like load:** the concurrent barrier is 3.0–7.4× faster. Round 2's arms met different loads, so its
  3.8–14.6× is not quoted as the gain. Round 2's 182–219 µs is what the change costs when the VM quiets.
- **Why it works:** a batch of `fdatasync`s in flight together waits on one journal commit and one device flush in
  ext4 (jbd2 joins every sync waiting on the running transaction to its commit). One at a time, each forces its own.
  Each file is durable when its own call returns, so the guarantee is exactly the per-file one.
- **Writeback first** (`sync_file_range` at write time) helped the concurrent barrier in five of six rounds, by 21–45%,
  and lost 10% in the sixth (busy 0). It needs an `unsafe` libc call, since rustix wraps no `sync_file_range`. Not taken: its gain is
  inside this VM's noise, so it waits for a quieter measurement.
- **Sequential barriers in a batch are no better than one a file** (2,232–9,554 µs): every `fdatasync` still forces
  its own commit. Sixteen at once beat eight at once in four of six rounds, by up to 2.8× beside the busy writer; eight
  was 3–7% faster in two quiet rounds.
- **Threads:** a barrier starts one scoped thread per file but the caller's (up to 15), and joins them. A one-file
  batch starts none.

### Landed: a published arena's regions keep their live heads from creation, so no publication scans one (A-64; 2026-10-10)

**Commands:**
- **Release:** a scratch example (not kept). Build `Buddy::new(4096, 23)` (a 32 GiB region of 4 KiB granules) or
  `Buddy::new_imaged(4096, 23)`, allocate 64 blocks, and time the first `capture`. Three rounds, this machine at load
  about 25.
- **Debug:** a scratch print in the first-capture branch during `landing_fairness a_large_landing`.

| round | deferred (before) | started imaged (after) |
|---|---|---|
| 0 | 3,442 µs, 8,388,608 granules scanned | 3 µs, none |
| 1 | 2,865 µs, 8,388,608 | 1 µs, none |
| 2 | 2,904 µs, 8,388,608 | 1 µs, none |

- **In the daemon (debug):** one first capture per run before, 8,388,608 granules in 64.6 ms inside one verb's shard
  step. After, none in two runs with the print restored.
- **The fairness lane's longest step after the fix:** 20–147 ms over twelve runs, nine of them 97–147 ms. That is
  another long step, open (GAPS, the same day).
- **Cost elsewhere:** none. Allocations and claims in a published arena keep the bitmap, as they did after a first
  capture, and an allocator nothing publishes from keeps the deferral's one flag test.

### Landed: a named temporary is placed by one rename that replaces nothing (A-131; 2026-10-10)

**Commands:**
- **The syscall A/B:** a scratch (not kept), release, this machine at load about 12–16. 2,000 fresh named temporaries
  per arm, three rounds: `linkat` to the name then `unlinkat` of the hidden name, against one
  `renameatx_np(RENAME_EXCL)`.
- **The engine A/B:** `land_bench flat`, HEAD `56d309dc` built in a scratch worktree and interleaved with the change,
  two rounds, onto APFS.

| round | `linkat` + `unlinkat`, µs a file | `renameatx_np(RENAME_EXCL)` |
|---|---|---|
| 0 | 259.1 | 56.5 |
| 1 | 208.8 | 46.2 |
| 2 | 197.5 | 45.9 |

**Through the engine, µs a new file** (median of 5 runs):

| round | arm | load | 1,000 | 2,000 | 4,000 | 8,000 | 16,000 |
|---|---|---|---|---|---|---|---|
| 1 | link and unlink (`56d309dc`) | 17 | 393 | 480 | 634 | 592 | 750 |
| 1 | one rename | 26 | 240 | 237 | 271 | 359 | 247 |
| 2 | link and unlink | 26 | 581 | 611 | 517 | 467 | 477 |
| 2 | one rename | 15 | 165 | 168 | 177 | 198 | 241 |

- **The change ran under the higher load in round 1 and still won 1.6–3.0×.** Round 2, with the loads reversed, shows
  2.0–3.6×.
- **Where the time had gone:** sampled on APFS (debug, the fairness lane), the landing spent 286 samples in `linkat`
  and 84 in `unlinkat` of 653, against the per-file `fsync`'s 42 and the temporary's `openat` 134.
- **The day's three changes on macOS, end to end:** about 700–900 µs a file this morning; 165–359 µs now.

### Real use: the slates tree landed through the CLI, before and after A-130/A-131 (macOS; 2026-10-10)

**Command:** `land_e2e.sh BIN SRC WORK`, a scratch script (not kept). Per run:
- a fresh `slates anchor`, an explicit `bootstrap root`, and a dynamic 2 GiB volume;
- the volume mounted over the loopback NFS bridge (`slates mount`) and the tree copied in with `cp -R`, then the mount
  removed (always before the anchor stops);
- `slates land` presented, `slates grant` issued with the anchor's exported variables, and `slates land --grant` timed
  into an empty directory;
- then `diff -r` of the source against the target.

**Inputs and arms:**
- The tree: `git archive` of the slates repository at `f9f0c081`: 3,685 files and 110 MB, 4,172 landed entries with
  the directories.
- The arms: release binaries of `85958800` (before A-130) and of the day's head, interleaved over five rounds.
- This machine at load 12–16.

| round | before (`85958800`) | after (the day's head) |
|---|---|---|
| 1 | 4.221 s | 1.447 s |
| 2 | 3.824 s | 2.995 s |
| 3 | 5.477 s | 1.486 s |
| 4 | 4.626 s | 1.544 s |
| 5 | 5.632 s | 1.723 s |

- **Medians:** 4.63 s before and 1.54 s after, 3.0× faster; pair by pair 1.3–3.7×.
- **Every run:** `state: done`, 4,172 entries written, and `diff -r` silent. The day's head took 502 barriers for the
  3,685 files: files larger than a batch's window take a barrier each, and the small ones share them.
- **The mount:** checked absent after every run.

### Landed: Linux's write-out starts each file's writeback before the batch's barrier (A-130; 2026-10-10)

**Commands:**
- **Syscall rerun:** the A/B of the entry above, on a quieter VM (another session's container at about two cores),
  three arms, five rounds quiet and five beside the busy writer. The two deciding arms' order alternates round by round.
- **Engine A/B:** `land_bench flat` for `f9f0c081` (the concurrent barrier alone) and the change, each built in its
  own target directory, three interleaved rounds.
- **Where:** ext4 in Docker's VM, non-root, a 3 GiB cap.

**Syscall, µs a file:**

| round | one `fdatasync` a file | concurrent, n=16 | writeback first, then concurrent, n=16 |
|---|---|---|---|
| quiet 0 | 789.5 | 129.2 | 76.2 |
| quiet 1 | 385.7 | 157.4 | 64.0 |
| quiet 2 | 510.0 | 142.4 | 74.5 |
| quiet 3 | 483.3 | 112.2 | 76.0 |
| quiet 4 | 376.4 | 197.4 | 128.7 |
| busy 0 | 12,959.5 | 6,119.8 | 2,323.4 |
| busy 1 | 19,538.9 | 6,034.0 | 3,688.2 |
| busy 2 | 8,882.9 | 3,867.6 | 2,206.7 |
| busy 3 | 602.9 | 225.6 | 123.5 |
| busy 4 | 620.9 | 211.1 | 114.8 |

Writeback first won every round: 1.5–2.5× quiet and 1.6–2.6× beside the busy writer.

**Through the engine, µs a new file** (median of 5 runs; load inside the VM at each arm's start):

| round | arm | load | 1,000 | 2,000 | 4,000 | 8,000 | 16,000 |
|---|---|---|---|---|---|---|---|
| 1 | barrier alone | 13 | 321 | 245 | 266 | 230 | 265 |
| 1 | writeback first | 17 | 186 | 310 | 179 | 120 | 161 |
| 2 | barrier alone | 16 | 177 | 178 | 192 | 213 | 184 |
| 2 | writeback first | 15 | 128 | 99 | 90 | 105 | 127 |
| 3 | barrier alone | 12 | 216 | 208 | 191 | 212 | 231 |
| 3 | writeback first | 14 | 77 | 71 | 76 | 74 | 82 |

- **Rounds 2 and 3, at like loads:** 1.4–2.1× and 2.6–2.9× faster. Round 1 was mixed: slower at 2,000 only, with the
  change at the higher load.
- **Cost:** one `unsafe` libc call (rustix wraps no `sync_file_range`); slates-land's unsafe budget went from 3 to 4.
- **Since the morning on Linux:** one `fdatasync` a file cost 573–913 µs on a quiet VM; the head runs 71–127 µs.

### Landed: Linux's batch barrier on ⌈√n⌉ streams, not one thread a file (A-130; 2026-10-10)

**Commands:**
- **Engine A/B:** `land_bench flat`, 1,000 to 16,000 new files. Arm A was HEAD, with every file's `fdatasync` on a
  thread of its own (sixteen threads a full batch). Arm B was the change: ⌈√n⌉ streams, four a full batch, each syncing
  its share in turn. Each arm was built in its own target directory.
- **Order:** rounds interleaved, the arms' order alternating round by round.
- **CPU throttling:** the container's `cpu.stat` `throttled_usec` was read before and after each arm. It sums every
  CPU's throttled time, so it can exceed the wall time.
- **Where:** ext4 in Docker's VM, three containers:
  - `--cpus=1 --memory=256m`, no swap;
  - `--cpus=2 --memory=256m`, no swap;
  - all of the VM's CPUs and 3 GB.

**µs a new file:**

| container | round | arm | 1,000 | 2,000 | 4,000 | 8,000 | 16,000 | throttled |
|---|---|---|---|---|---|---|---|---|
| 1 CPU, 256 MB | 1 | sixteen threads | 485 | 430 | 296 | 397 | 289 | 188.8 s |
| 1 CPU, 256 MB | 1 | √n streams | 172 | 105 | 109 | 114 | 150 | 0.4 s |
| 1 CPU, 256 MB | 2 | √n streams | 97 | 99 | 95 | 108 | 1,075 | 0.2 s |
| 1 CPU, 256 MB | 2 | sixteen threads | 1,987 | 2,414 | 395 | 778 | 357 | 79.8 s |
| 1 CPU, 256 MB | 3 | sixteen threads | 2,247 | 2,029 | 2,100 | 1,392 | 694 | 43.5 s |
| 1 CPU, 256 MB | 3 | √n streams | 207 | 168 | 114 | 112 | 105 | 0.3 s |
| 2 CPUs, 256 MB | 1 | sixteen threads | 772 | 786 | 771 | 1,242 | 178 | 3.7 s |
| 2 CPUs, 256 MB | 1 | √n streams | 329 | 1,337 | 2,086 | 155 | 136 | 0 |
| 2 CPUs, 256 MB | 2 | √n streams | 548 | 847 | 175 | 180 | 246 | 0 |
| all, 3 GB | 1 | sixteen threads | 129 | 163 | 99 | 99 | 107 | 0 |
| all, 3 GB | 1 | √n streams | 86 | 83 | 83 | 86 | 81 | 0 |
| all, 3 GB | 2 | √n streams | 118 | 108 | 93 | 117 | 103 | 0 |
| all, 3 GB | 2 | sixteen threads | 109 | 164 | 340 | 139 | 123 | 0 |
| all, 3 GB | 3 | sixteen threads | 1,393 | 1,148 | 617 | 914 | 679 | 0 |
| all, 3 GB | 3 | √n streams | 746 | 498 | 653 | 1,903 | 1,921 | 0 |

**What it shows:**
- **At one CPU, the decision.**
  - Sixteen threads started at once spend the container's CPU quota in a burst. The cgroup is then throttled until the
    next period: 43.5–188.8 s of throttled time over a bench, against 0.2–0.4 s for four streams.
  - Four streams were 1.9–24× faster at every size but one. The exception is a 16,000-file outlier (1,075 µs) of the
    kind seen at 256 MB before; the hypothesis there, unshown, is the cgroup's dirty page cache.
  - The throttling falls on everything in the cgroup, so it would stall every shard of a daemon, not only the landing.
- **At two CPUs:** throttled 3.7 s against none. The sizes are mixed: four streams were faster at 1,000, 8,000 and
  16,000 files, and slower at 2,000 and 4,000. Round 2's sixteen-thread arm was lost when Docker Desktop restarted, which
  also killed the container.
- **With every CPU:**
  - Quiet (rounds 1 and 2): four streams were 1.15–3.7× faster at every size but one, 1,000 files in round 2 (118
    against 109).
  - Round 3 ran while the VM was contended by other sessions' containers, and both arms slowed 5–20×. Four streams were
    1.9–2.3× faster at 1,000 and 2,000 files, about even at 4,000 (653 against 617), and **2.1–2.8× slower at 8,000
    and 16,000**.
  - With CPU to spare and slow commits, sixteen syncs in flight merge into fewer journal commits than four streams of
    four.
- **No count wins everywhere** under the cost model k·a + (n/k)·b:
  - CPU contention raises a, the cost of starting a thread, and favours fewer streams.
  - I/O contention raises b, a round of commits, and favours more.
  - ⌈√n⌉ is the balance where the two are equal. It is chosen because the CPU burst throttles the whole cgroup, while
    the extra commits slow only the landing.
- **Open, to measure:** threads kept for the landing rather than started per barrier would cut a toward zero. Every
  file's sync could then be in flight with no burst, which might win in all three containers.

### Landed: a landing's durability waits run off the shard (A-132; 2026-10-10)

**Commands:**
- **The lane:** the landing-fairness lane (`landing_fairness`, release): 600 files landed while a second client probes
  the same shard in turn with reads, provisions (a volume created and destroyed) and lease takes.
- **The arms:** A is `078bb5e2`, with the barrier on the shard and macOS's `fsync` in the write-out. B is A-132. Each
  was built in its own target directory.
- **Linux:** ext4 in Docker's VM. One fresh `--cpus=2 --memory=256m` container a run, no swap. Three conditions, three
  interleaved rounds each, the arms' order alternating:
  - `quiet`: the VM as it was;
  - `syncwriter`: a sibling container rewriting a 512 MiB file with `fsync` after each pass;
  - `hog2`: two spinning processes in the test's own cgroup.
- **macOS:** APFS on this Mac, six interleaved rounds at load 22–24 from other sessions.
- **Each run** reports the landing's time, its slices' maximum (its own longest hold on the shard), the shard's longest
  step (anyone's), and each probe kind's p99 and maximum inside the landing.

**Linux** (ms; probes give the worst kind's p99 / max):

| condition | round | arm | landing | slice max | longest step | probe p99 | probe max |
|---|---|---|---|---|---|---|---|
| quiet | 1 | A | 237 | 18.7 | 18.7 | 18.94 | 18.94 |
| quiet | 1 | B | 58 | 0.3 | 10.5 | 0.13 | 1.63 |
| quiet | 2 | B | 59 | 0.4 | 1.3 | 0.12 | 1.28 |
| quiet | 2 | A | 53 | 3.0 | 3.0 | 1.26 | 3.26 |
| quiet | 3 | A | 1,328 | 64.4 | 64.4 | 63.56 | 64.47 |
| quiet | 3 | B | 87 | 0.3 | 1.8 | 0.10 | 2.12 |
| syncwriter | 1 | A | 1,080 | 90.6 | 90.6 | 108.39 | 108.39 |
| syncwriter | 1 | B | 351 | 0.7 | 5.5 | 0.08 | 9.27 |
| syncwriter | 2 | B | 283 | 0.6 | 1.9 | 0.21 | 6.10 |
| syncwriter | 2 | A | 139 | 6.7 | 6.7 | 6.75 | 6.75 |
| syncwriter | 3 | A | 1,081 | 79.7 | 79.7 | 126.72 | 126.72 |
| syncwriter | 3 | B | 2,067 | 8.5 | 112.4 | 0.13 | 112.54 |
| hog2 | 1 | A | 389 | 35.9 | 35.9 | 72.67 | 72.67 |
| hog2 | 1 | B | 607 | 2.0 | 19.2 | 0.28 | 51.08 |
| hog2 | 2 | B | 1,667 | 4.5 | 49.9 | 0.07 | 60.59 |
| hog2 | 2 | A | 667 | 59.9 | 59.9 | 85.42 | 85.42 |
| hog2 | 3 | A | 170 | 18.9 | 18.9 | 19.11 | 19.11 |
| hog2 | 3 | B | 571 | 1.0 | 47.1 | 0.09 | 54.04 |

- **The shard's tail, the change's purpose.**
  - With B, the probes' worst p99 was 0.07–0.28 ms in every run, against 1.26–126.72 ms with A.
  - The landing's own longest hold was at most 8.5 ms, against 3.0–90.6 ms.
  - With A, the longest step was always a landing slice: a barrier.
- **What B leaves:** probe maxima of 51–113 ms in contended runs, from steps outside the landing's slices.
  - Under `hog2`, instrumented runs attribute them to the cgroup's CPU quota, not to blocking calls:
    - a poll held off the CPU 49.8 ms (`Preempted`);
    - the landing's finish, about 1 ms of CPU at 600 files and unsliced, stretched to 52 ms by throttling;
    - two unattributed polls up to 34.7 ms.
  - The 112 ms step beside the syncing writer did not recur in three instrumented runs: the longest step was 1.9–4.2 ms
    there, and the finish 1.4–1.6 ms.
- **The landing's own time is mixed:**
  - quiet: about as fast or faster;
  - `syncwriter`: faster once (351 against 1,080 ms), slower twice (283 against 139, 2,067 against 1,081);
  - `hog2`: 1.6–3.4× slower.
  - The prober now gets the shard during each barrier where A blocked it, so the landing shares the shard. Each barrier
    also starts its thread and its stream threads inside the throttled cgroup.

**macOS** (ms; rounds at load 22–24):

| round | arm | landing | slice max | longest step | probe p99 (worst) | probe max |
|---|---|---|---|---|---|---|
| 1 | A | 1,807 | 15.6 | 148.9 | 20.98 | 151.49 |
| 1 | B | 2,898 | 17.6 | 144.2 | 18.16 | 165.07 |
| 2 | B | 208 | 4.1 | 4.1 | 1.26 | 4.26 |
| 2 | A | 210 | 5.0 | 5.0 | 3.95 | 5.30 |
| 3 | A | 222 | 4.3 | 4.3 | 3.86 | 4.54 |
| 3 | B | 240 | 4.2 | 4.2 | 0.52 | 4.51 |
| 4 | B | 265 | 4.6 | 4.6 | 1.44 | 5.07 |
| 4 | A | 242 | 4.4 | 4.4 | 0.91 | 4.64 |
| 5 | A | 249 | 4.8 | 4.8 | 3.66 | 4.98 |
| 5 | B | 225 | 4.0 | 5.2 | 0.52 | 7.55 |
| 6 | B | 235 | 4.4 | 4.4 | 0.63 | 4.45 |
| 6 | A | 251 | 4.2 | 4.2 | 0.96 | 8.40 |

- **Five quieter rounds:** both arms held the shard about 4–5 ms at most. B's worst p99 was lower in four of five rounds,
  0.52–1.44 against 0.91–3.95 ms.
- **The first round was contended:** the landing ran 1.8–2.9 s, and both arms took a ~145 ms step outside the landing's
  slices. It is not a durability wait, since B moved those off the shard and A's slices stayed under 16 ms. The open
  hypothesis is the shard thread descheduled on an oversubscribed machine (the shard-QoS question in GAPS).
- **Before this A/B,** an instrumented run on this Mac caught the per-file `fsync` holding a write unit 206 ms. B has no
  such unit.

**macOS throughput** (`land_bench flat`, the one-call landing, so every barrier runs in place; µs a new file):

| round | arm | load | 1,000 | 2,000 | 4,000 | 8,000 | 16,000 |
|---|---|---|---|---|---|---|---|
| 1 | A | 39.7 | 313 | 335 | 258 | 259 | 340 |
| 1 | B, ⌈√n⌉ streams | 40.6 | 434 | 426 | 467 | 420 | 429 |
| 1 | B, one stream | 56.5 | 416 | 465 | 402 | 389 | 348 |
| 2 | B, ⌈√n⌉ streams | 55.0 | 214 | 225 | 245 | 268 | 271 |
| 2 | B, one stream | 58.5 | 267 | 344 | 318 | 377 | 380 |
| 2 | A | 80.7 | 336 | 378 | 367 | 399 | 326 |
| 3 | B, one stream | 74.5 | 202 | 245 | 249 | 261 | 279 |
| 3 | A | 53.1 | 231 | 250 | 248 | 287 | 285 |
| 3 | B, ⌈√n⌉ streams | 48.9 | 256 | 227 | 233 | 270 | 261 |

- No arm won consistently at loads of 40–81; each round's winner differed.
- Moving macOS's `fsync` from the write-out into the barrier costs no measurable throughput here, and neither stream
  count beat the other. Recorded as inconclusive, not as a win: macOS keeps Linux's derived rule, one rule for every
  platform.

### Landed: a landing's finish is sliced; its advance is bounded, not O(entries) (A-133; 2026-10-11)

**What changed.** The landing's finish advanced the overlay past what reached the disk (forget each landed entry or
rebase it, then invalidate the listings) in **one synchronous shard step**, O(entries). A-133 slices it one entry a unit
under the step budget, driven by the server between yields, the report's figures accumulated over the loop so nothing
O(entries) runs in one unit; the completion's records still commit as one atom.

**Command.** `land_bench` scratch mode (not kept): for each entry count, an overlay of N flat files over the in-memory
host is stepped to its finish, then the advance is driven by `finish_step(budget)` with a 125 µs budget, every step
timed. Reported: steps, and the step-time p50/p99/max. The in-memory host isolates the advance from disk; the per-entry
cost on a real volume is ~1.7× higher (the 600-entry server measurement).

**Before — the one-call advance (`land_advance`), µs an entry, 2 runs:**

| entries | advance (one step) | per entry |
|---|---|---|
| 1,000 | 0.95–0.99 ms | 0.95 µs |
| 10,000 | 10.0–11.0 ms | 1.05 µs |
| 25,000 | 25.2–29.1 ms | 1.08 µs |
| 50,000 | 52.3–53.2 ms | 1.05 µs |

Linear at ~1.0 µs an entry → ~105 ms at 100k, ~1 s at 1M, all in one shard step.

**After — the sliced finish's step time (125 µs budget):**

| entries | steps | p50 | p99 | max |
|---|---|---|---|---|
| 1,000 | 7 | 125 µs | 130 µs | 130 µs |
| 10,000 | 82 | 126 µs | 752 µs | 752 µs |
| 50,000 | 436 | 126 µs | 235 µs | 3.1 ms |
| 100,000 | 793 | 126 µs | 135 µs | 6.4 ms |

- **p50 and p99 are flat at the budget (≈126 µs) across 1k→100k** — the advance is bounded per step, independent of the
  entry count.
- **The max is a tail outlier,** not an O(n) step: it varied run to run (9.1 → 7.2 → 6.5 → 6.4 ms at 100k) and grows with
  the number of steps (more samples hit the shared VM's scheduling/allocator tail), while p50/p99 stay flat. Per-unit
  instrumentation found no O(n) unit — one 556 µs unit across 100k entries, the rest under 500 µs.
- **The fold mattered:** before folding `advanced()` (a manifest-keyed `BTreeMap` + `Vec`) and the count passes into the
  slice, the longest step was 25 ms at 100k (the begin/terminal O(n·log n) in one unit); after, p50/p99 are at budget.

**Correctness.** The sliced-vs-one-call oracle holds (`crates/land` 62 tests); reads return the landed content at every
slice and the base is clean after (`a_sliced_advance_reads_consistently_and_leaves_the_base_clean`); the server landing
suites and vfs suite are green.

### Condition 12: slates' measured p99s, and an honest comparison to TectonicFS (2026-10-11)

**The competitor, read first.** TectonicFS (Pan et al., "Facebook's Tectonic Filesystem", FAST '21) is an
**exabyte-scale warehouse/blob filesystem** — 1,590 PB, 10.7 billion files, 15 billion blocks, 4,208 storage nodes
(Table 2). It reports **throughput and scale, not small-operation p99 latency**: metadata shards serve up to **10 KQPS**,
and ~1% of Name-layer shards hit that limit and retry after backoff (§6.3, Fig 4c). On latency it is explicit (§6.5,
"The impact of higher metadata latency"): *"HDFS metadata operations are in-memory and all metadata for a namespace is
stored on a single disk. In contrast, Tectonic stores its metadata in a sharded key-value store and disaggregates
metadata layers. This means Tectonic metadata operations may require one or more network calls (e.g., a file open
operation will interact with the Name and File layers)… compute engines parallelize this step to hide the extra latency
of individual Tectonic rename operations."* So Tectonic deliberately trades higher per-operation metadata latency for
exabyte scale, and hides it with parallelism; it publishes no small-op p99.

**Why the classes differ.** slates is an **in-memory copy-on-write VFS** — exactly the in-memory metadata class
(HDFS-like) that Tectonic contrasts itself against as the lower-latency design. A slates metadata or read operation on
the local path touches RAM, with no key-value-store hop and no network call; a cross-region one is bounded by the
measured RTT (§4.8), not by added store hops. So a single-operation p99 comparison favours the in-memory design by
construction — the honest framing is that slates occupies the low-latency end Tectonic gave up for scale, while Tectonic
occupies a scale point slates does not claim (EB warehouse blobs).

**slates' measured p99s (this session and prior, in-memory host / loopback NFS / fleet):**
- **Landing finish, sliced (A-133):** step p50 126 µs, p99 130–752 µs, flat from 1k to 100k entries (125 µs budget).
- **Other clients during a landing (A-132), 2-CPU/256 MB, quiet + a syncing writer + CPU hogs:** worst p99 **0.07–0.28 ms**
  (was 1.26–126.72 ms before the barriers moved off the shard).
- **Local NFS op p99 5.7 µs** (`overlay_diff.sh`, `nfs.local_p99_ns`).
- **Overlay real-tree reads at the NFS client's floor:** cold 1.58–1.78 s for 5,863 files / 540 MB — **faster than the
  Linux kernel's own NFS server** on the same client (1.77–2.03 s), the apt live same-machine competitor.
- **Change tracking:** an exact diff plan in 2–3 ms against `diff -rq`'s 406–461 ms (~150×).

**Verdict.** For single-operation latency under non-ideal (contended) access, slates' p99s are microsecond-scale on the
local path and bounded by measured RTT on the fleet path — the low-latency end of the design space Tectonic documents
itself as trading away for exabyte throughput. The directly runnable competitor on this hardware, the Linux kernel's NFS
server, slates beats on read latency. A live Tectonic A/B is not possible (it is Facebook-internal and not open source);
this comparison is against its published figures, with the workload-class caveat stated.

### FUSE max_pages: the advertised 256 KiB write was unreachable, and a large write cost twice the round trips (2026-10-19)

Following the re-measurement below — which puts a create at one round trip and almost nothing else — the obvious
question is how many round trips a *large write* pays. Measured through a real mount (Docker kernel 6.12,
release): sequential write throughput **plateaued at a 128 KiB buffer**, 1,204 → 2,082 → 2,188 → 1,985 MiB/s at
64/128/256/1,024 KiB, with per-call p50 47.9 / 57.7 / 110.9 / 482.4 µs — whole round-trip steps, one request,
one, two, eight.

**Cause.** `FUSE_MAX_PAGES` was never negotiated. Without it the kernel splits every request at
`FUSE_DEFAULT_MAX_PAGES_PER_REQ` = 32 pages, which is 128 KiB on a 4 KiB page, whatever `max_write` says — so
the 256 KiB slates advertises at INIT could not be reached and every write ≥ 256 KiB paid double.

**Fix.** Ask for the flag and send `max_pages` = `MAX_WRITE / page_size`, bounded by the kernel's own
`FUSE_MAX_MAX_PAGES` (256). The page size is read at run time: a 4 KiB host wants 64 and a 16 KiB arm64 host
wants 16, and hard-coding either would silently halve or quadruple the request size on the other. This raises
nothing — it makes the number already advertised reachable.

**Proof, by count rather than by clock.** Each FUSE request is one `read()` from `/dev/fuse`, so the daemon's
`syscr` across a known write is load-independent. Writing 256 MiB:

| buffer | daemon `read()` calls | requests per `write()` | request size |
|---|---|---|---|
| 128 KiB | 4,101 | 1 | 128 KiB |
| 256 KiB | **2,058** | **1** | **256 KiB** |
| 1,024 KiB | **2,051** | **4** | **256 KiB** |

The 128 KiB row pins the scale at 2.0 reads per request (2,048 one-request calls → 4,101 reads). On that scale
1,024 calls of 256 KiB cost 2,058 reads — one request each, not two — and 256 calls of 1 MiB cost 2,051, four
each. Before the change all three rows would read ~4,100. **Half the round trips for every write ≥ 256 KiB.**

A timing A/B was attempted first and is **not** reported as evidence: the host's load changed between arms
(tmpfs itself fell 14,776 → 7,691 MiB/s), so the two runs were not comparable. The count is immune to that,
which is why it is the measurement here.

**x86_64 confirmed, on a quiet host, by an A/B with its own control (2026-10-19).** The counts above are Linux
**aarch64** (Docker, 6.12). The first x86_64 attempt is recorded as **not** evidence: the scratch tree on the bench
box sat at an old commit, dropping today's `init.rs`/`abi.rs` into it failed to compile (29 errors), so the probe
that ran used the **pre-change** binary and its numbers (7,953 / 6,064 / 4,563 reads) say nothing about this fix.
The files were restored from that tree's own HEAD and it builds again.

The leg is now taken properly on a second x86_64 host that was idle — `fedora.attlocal.net`, 32 cores, kernel
6.19.10-300.fc44, page size 4,096, load1 1.60–2.15 throughout, release build with the repo's pinned rustc 1.98.0.
Two arms from one tree: **A** is HEAD, **B** is HEAD with the single line `| flags::MAX_PAGES` deleted from
`wanted()`; the binaries were checked to differ by hash before either ran (`cf63ef8db1c9` vs `4a12b6161c5a`). Each
point is a fresh daemon, volume and mount; 64 MiB is written at one buffer size and `fsync`ed before the daemon's
`syscr` is sampled; the arms are interleaved A, B, A, B.

| buffer (64 MiB) | A `read()`s (2 rounds) | B `read()`s (2 rounds) | A requests | B requests |
|---|---|---|---|---|
| 128 KiB | 2,025 / 1,982 | 2,061 / 2,062 | 512 | 512 |
| 256 KiB | **1,038 / 1,037** | 1,550 / 1,546 | **256** | 512 |
| 1,024 KiB | **654 / 654** | 1,166 / 1,166 | **256** | 512 |

**The 128 KiB row is the control**: 128 KiB is under both arms' caps, so both must issue one request per `write()`,
and both do — the arms agree within 2% there and separate only where the cap bites. Reading the table needs one
constant, `reads = 2 x requests + 2 x write() calls`, which fits all six points within 3%; the two reads per
request are the same scale the aarch64 run pinned. The fit is not load-bearing, because the two rows that matter
need no model at all: at a 1 MiB buffer both arms make exactly 64 `write()` calls, so their per-write overhead is
identical, and the measured difference is 1,166 - 654 = 512 reads = exactly 2 x 256 extra requests. Subtracting
the 512 and 1,024 request-reads leaves an overhead of **142 in both arms**. At 256 KiB the same subtraction leaves
525 and 524. The arms differ in one thing and the arithmetic closes on it.

**So on x86_64 with 4 KiB pages the request is 256 KiB with the flag and 128 KiB without** — the requests for any
write of 256 KiB or more are halved, which is what the aarch64 counts said, now on the other architecture and the
other page size. In syscalls the daemon does 33% fewer reads at a 256 KiB buffer and 44% fewer at 1 MiB, and
nothing changes at 128 KiB. What already carried across architectures stands: `max_pages` is derived from the
run-time page size, the ABI test asserts `max_pages x page_size == MAX_WRITE` on every platform CI builds, and the
kernel path that honours the field is architecture-independent.

Re-take it with (the sweep is one command; the `sed` makes the B arm and the tree is disposable):

```
cargo build --release -p slates-cli --bin slates && cp target/release/slates /tmp/slates-A
sed -i '/^    | flags::MAX_PAGES$/d' crates/bridge-fuse/src/init.rs
cargo build --release -p slates-cli --bin slates && cp target/release/slates /tmp/slates-B
# per point: anchor --quick --shards 2; volume create --dynamic 8GiB; mount; write 64 MiB; fsync;
# delta of /proc/<daemon>/io syscr.  Interleave the arms.
```

**The read side does not move, and the reason is measured.** The same count over a 256 MiB read-back is 4,100
daemon reads whatever the caller's buffer — 128 KiB per READ request, unchanged by `max_pages`:

| read buffer | daemon `read()` calls | request size |
|---|---|---|
| 128 KiB | 4,101 | 128 KiB |
| 256 KiB | 4,100 | 128 KiB |
| 1,024 KiB | 4,100 | 128 KiB |

Reads are driven by the kernel's **readahead window**, not by the caller's buffer or by `max_pages`, and the
mount's window was read directly rather than inferred: `/sys/class/bdi/0:424/read_ahead_kb` is **128** for the
slates mount (and for every other FUSE bdi in that kernel). slates advertises `max_readahead` = 256 KiB at INIT
and the window is still 128 kB, so a read never exceeds it.

Raising it is a **deployment** knob, not a negotiation: `read_ahead_kb` is a root-owned sysfs file, and R10
says slates requires no privilege, so the daemon cannot set it for its own mount. It belongs with L1's hugetlb
zones on the list of things a host can do for slates rather than things slates does. Worth stating because the
write fix above might otherwise be assumed to help reads; it does not, and the ceiling is someone else's.

### Condition 12 re-measured (2026-10-19): the table below is stale by ~10×, and the round trip is the cost

**Withdrawing three of my own amendments from 2026-10-18.** They reasoned from the 276.9 µs create in the
2026-10-11 table to conclude that the transport was ~5% of a create and ~94% was work inside the daemon. The
premise does not reproduce, so the conclusion is wrong, and the lever list this file carried *before* those
amendments — "reduce the FUSE/daemon round-trip" — was right.

Re-run of `docs/wip/bench/fuse/fuse-perf.sh` plus an overlay variant, **release** build at `818623fa`, Docker
Desktop kernel **6.12.76-linuxkit** (the same 6.12 Docker environment the 2026-10-11 row records), aarch64,
host load 25–28, 2,000 creates per leg with `creates.py`:

| op, p50 | tmpfs | slates, plain volume | slates, `--shared` over a 500-file base | the 2026-10-11 row |
|---|---|---|---|---|
| create (`open` O_CREAT) | 1 µs | **27 µs** | **30 µs** | 276.9 µs |
| write | 1 µs | 11 µs | 14 µs | 471.1 µs (rewrite) |
| close | 0 µs | 10 µs | 10 µs | — |
| stat | 0 µs | 22 µs | 26 µs | 13.9 µs |

2,000 creates take 0.15 s plain and 0.17 s on the overlay (the old A-96 row: 0.26 s). The overlay's 500 base
entries are visible through the mount, so it is the real shape, and it costs **3 µs** per create — not the
~250 µs the gap between the two tables would have needed.

**Why the old row is stale.** It is dated the same day as **A-134**, which fixed exactly this: overlay volumes
republished their *whole image* per barrier, "O(volume) per op, O(N²) to fill a directory — overlay create p50
grew 196 → 714 µs over 4,000 creates" (`GAPS.md`). A pre-A-134 create of a few hundred microseconds is precisely
what the row records, and nobody re-ran the table after the fix landed (`6625b6f2`).

**Where a create's 27 µs goes, measured three ways and now consistent.** A stat — one round trip and a trivial
lookup — is 22 µs, so a create is about **five microseconds above the round-trip floor**. In-process the whole
bridge path (FUSE codec + dispatch + the VFS create) is **0.5 µs** (`shards_fs_bench`), and the VFS create alone
~2 µs (`vfs_bench`). `perf record` on the daemon through 2,000 creates attributes its CPU accordingly:

| symbol | share |
|---|---|
| `fuse_request_end` (kernel) | 20.1% |
| `el0_svc` (kernel syscall entry) | 19.5% |
| `slates_rt::epoll::EpollDriver::wait` | 18.9% |
| `__kernel_clock_gettime` | 4.9% |
| `fdget` + `invoke_syscall` + `do_el0_svc` (kernel) | 8.5% |
| slates' own (`reference_for`, `publish_shard`, `any_ring_ready`, `Ring::depth`, …) | ~1.2% each |

About half the daemon's CPU is the kernel's FUSE and syscall paths and a fifth is the runtime's epoll wait; its
own filesystem work barely registers. 164 samples, so the shape is sound and the digits are not.

**The three-way table, re-run like-for-like (2026-10-19).** Same container, same run, same 500-file base, 2,000
creates each with `creates.py`; overlayfs `lowerdir`/`upperdir`/`workdir` on a tmpfs, slates `--shared` over the
same tree:

| op, p50 (p99) | tmpfs | overlayfs | slates (FUSE) | slates vs overlayfs |
|---|---|---|---|---|
| create | 1 µs (3) | 2 µs (4) | 31 µs (95) | **16×** |
| write | 1 µs (2) | 1 µs (2) | 14 µs (36) | **14×** |
| close | 0 µs (0) | 0 µs (0) | 13 µs (25) | — |
| stat | 0 µs (1) | 1 µs (1) | 27 µs (51) | **27×** |

**The multiples are now uniform, and that is the finding.** The superseded row reported 12–325× with the write
path as a wild outlier (create 55×, rewrite 147×, create+fsync 325×). Re-measured, every operation sits in a
14–27× band — because every operation pays the same ~25 µs round trip and almost nothing else. There is no
"write path problem"; there is one transport cost, charged once per message, and the kernel's in-kernel
filesystems pay 1–2 µs for the same work because they are in the kernel.

And the `create+fsync` arm, the old row's biggest outlier at 325×, driven the same way (1,000 files,
`open`+`write`+`fsync`+`close` timed as one span):

| op, p50 (p99) | tmpfs | overlayfs | slates (FUSE) | slates vs overlayfs | the old row |
|---|---|---|---|---|---|
| create + fsync | 2 µs (4) | 3 µs (8) | 87 µs (185) | **29×** | 325× |

87 µs is four operations — create 31, write 14, close 13 and the fsync itself about thirty, which is one more
round trip and nothing else. slates' `fsync` is a success no-op by design (the anchor segment is already the
source of truth, R1), so what it costs is the message, exactly as the rest of the table says.

**Every multiple in the re-run is 14–29×**: stat 27×, write 14×, create 16×, create+fsync 29×. The superseded
row's 12–325× spread, and the "write path dominates" reading built on it, were artifacts of the pre-A-134
whole-image republish.

Conditions: Docker Desktop (kernel 6.12.76-linuxkit) on an 18-core aarch64 Mac, host load 25–27, release build,
one run per arm rather than best-of-N, p50/p99 over 2,000 files (1,000 for the fsync arm); the superseded row
used 3,000 and reported p999/max as well.

**x86_64, bare metal (2026-10-19), and it is better again.** Same workload on the Linux box — 32-core x86_64,
kernel 7.2.9-cachyos, no VM — release build of 2026-10-18 (which postdates A-134; checked by ancestry). The box
was carrying **load 61 from another session's builds** throughout, which is what the p99 column is showing:

| op, p50 (p99) | tmpfs | overlayfs | slates (FUSE) | slates vs overlayfs |
|---|---|---|---|---|
| create | 2 µs (11) | 6 µs (26) | **16 µs (595)** | **2.7×** |
| write | 1 µs (3) | 2 µs (4) | 8 µs (492) | 4× |
| close | 0 µs (0) | 1 µs (1) | 3 µs (246) | 3× |
| stat | 1 µs (2) | 1 µs (3) | 5 µs (424) | 5× |
| create + fsync | 4 µs (18) | 7 µs (23) | **37 µs (888)** | **5.3×** |

On bare metal the round trip is ~5 µs rather than the ~25 µs the Docker-on-macOS VM imposes, and slates lands
**within 3–5× of the kernel's own overlayfs at p50** — against the superseded row's 12–325×. The p99s (246–888 µs)
are the host's load, not slates: all three arms ran in the same contended minute, and tmpfs's own p99 rose to
11–18 µs in it.

This mount was **not** `--shared` (the box has no `user_allow_other`, and changing a machine's `/etc/fuse.conf`
is not mine to do); the Mac run's `--shared` arm cost 3 µs over the plain one, so the comparison is unaffected.
Both runs used a base-backed volume over the same 500-file tree.

**The honest statement of condition 12 is now:** on bare-metal Linux, slates over FUSE answers a create in 16 µs
against the in-kernel overlayfs's 6 µs, and the gap is one userspace round trip. Not "12–325× slower, dominated
by the write path".

**The harness is committed:** `docs/wip/bench/fsbench/` — `run.sh` (the container invocation), `inner.sh` (the
three arms in one run over one base) and `fsync.py`. The superseded row cited this path and it did not exist;
every number in the two tables above now comes from a command in the tree. It needs a **release** build in the
`slates-linux-target` volume; the build line is in `run.sh`'s header.

**One caveat the harness itself exposed, and it is a property worth knowing.** A verification run of the same
script while a `cargo build` ran beside it in the same Docker VM read slates create p50 **130 µs** and stat
98 µs, against 31 µs and 27 µs in the uncontended run — while tmpfs and overlayfs barely moved (1–3 µs either
way). An in-kernel filesystem serves the caller in the caller's own timeslice; slates needs its daemon
scheduled, so its per-operation latency is far more sensitive to CPU contention than theirs. That is inherent to
being out of kernel and is the honest companion to the ratios above: they are ratios on a *quiet-ish* host, and
the gap widens on a loaded one. (That run also used 500 creates rather than 2,000, so warm-up weighs more
heavily in its p50.)

**So the lever is the round trip, and it is ~22 µs.** Against tmpfs's 1 µs create slates is ~27×, not the ~55×
the stale row implies against overlayfs. The in-design levers that attack the round trip — FUSE writeback
caching and a larger `max_write` to batch, and virtio-fs + DAX to replace the round trip with a page-table
access — are the ones that matter; the daemon-side work they would not touch is already half a microsecond.
Owed: re-run the overlayfs leg so the three-way table is like-for-like again (this run had no overlayfs arm),
and the same on x86_64.

### Condition 12, like-for-like: slates (FUSE) vs the kernel's overlayfs vs tmpfs (2026-10-11) — superseded above

A direct, runnable competitor comparison — the Stop-hook asked for like-for-like, and overlayfs is slates' closest class
(the kernel's own copy-on-write overlay that container runtimes mount by default). Same workload on all three, over the
same 500-file base on a tmpfs: 3,000 small files created, stat'd, read, rewritten (a copy-up for an overlay), and 1,000
created with `fsync`, each op timed; per-op-type p50/p99/p999/max. `docs/wip/bench/fsbench/` (Docker, kernel 6.12,
privileged, `--device /dev/fuse`), slates mounted `--shared` (FUSE), overlayfs `lowerdir`/`upperdir`/`workdir` on the
tmpfs.

| op (µs) | overlayfs p50/p99 | tmpfs p50/p99 | slates FUSE p50/p99 | slates vs overlayfs |
|---|---|---|---|---|
| stat | 1.0 / 1.8 | 0.8 / 1.5 | 13.9 / 30.2 | ~14× |
| read | 3.5 / 5.4 | 2.9 / 3.8 | 43.3 / 79.0 | ~12× |
| create | 5.0 / 16.0 | 3.1 / 6.0 | 276.9 / 553.5 | ~55× |
| rewrite | 3.2 / 4.3 | 2.7 / 3.7 | 471.1 / 669.7 | ~147× |
| create+fsync | 3.8 / 14.4 | 2.1 / 5.0 | 1233.9 / 1777.0 | ~325× |

Slates was also measured over **NFSv4.2** (kernel-side caching/readahead) on the same volume, same run: it is *worse*
than FUSE for writes (create p50 1,872.9 µs, rewrite 1,927.6 µs — synchronous RPC an op) and slightly better for reads
(read p50 32.1 µs vs FUSE 45.5 µs). Neither mount path is within 10× of overlayfs.

**Should slates write its own (kernel) overlayfs? No** — decided against (2026-10-11, Ada's question). overlayfs is fast
only because it is **in-kernel** (no userspace round trip an op). An in-kernel slates would break R10 (unprivileged, no
kernel extension), condition 6 (a Linux module is not cross-OS), and condition 3 (in-kernel, backing-store-coupled
overlays are what slates exists not to be). The overlay *logic* is already slates' (lower+upper+whiteouts+exact change
tracking); what is slow is the **kernel↔daemon transport**, not the overlay semantics. Every userspace/distributed peer
pays this same premium — JuiceFS, Alluxio, CernVM-FS (FUSE, distributed) and gVisor/sandboxfs (FUSE, hermetic) are all
10–100× slower per op than in-kernel overlayfs; they trade local latency for distribution/hermeticity/portability, as
slates does, while overlayfs/EROFS buy speed with privilege, locality and OS-specificity slates refuses. The in-design
levers to narrow the gap, in order: (1) profile one FUSE create end to end to attribute the 277 µs (round trip vs shard
hop vs copy-up vs merge tracking) before building; (2) io_uring FUSE (`FUSE_OVER_IO_URING`, Linux 6.14+, 2–4× FUSE gains,
unprivileged); (3) virtio-fs + DAX for the microVM case (shared memory, near-native; `crates/bridge-virtiofs` exists);
(4) FUSE writeback caching + larger `max_write` to batch write round trips.

**Honest verdict — correcting the earlier "far above industry-leading" framing.** Against the **in-kernel** overlayfs,
slates is **12–325× slower per operation**, dominated by the **write path** (create/rewrite/fsync). This is the
out-of-kernel protocol hop: a FUSE (or NFS) round trip plus the daemon's per-op processing and merge tracking, which the
in-kernel overlayfs does not pay. So slates does **not** have "far above industry-leading" p99s against the kernel's own
local overlay; the prior comparison (knfsd, Tectonic) was against *networked/distributed* filesystems that also pay
network/store hops, where slates wins — but that is the wrong peer for a local overlay.

**What slates buys for the premium** (overlayfs offers none of these): hermetic RAM with no disk escape except a granted
merge, remote/replicated volumes, post-quantum encryption in transit and at rest, cross-OS mount, and exact change
tracking. The honest position is that slates trades local per-op latency for those properties — the inverse of
Tectonic's "trade latency for scale."

> **Superseded 2026-10-19** (see "Condition 12 re-measured" above): the in-process figures in this section
> stand, but the 276.9 µs they are set against does not reproduce — a create is 27–31 µs. The *conclusion* drawn
> below, that most of a create is unaccounted for, is wrong; there was never a missing 259 µs.

**Measured (2026-10-18), second cut: the bridge is not the missing time either — dispatch is 0.5 µs.** The
decomposition below put ~94% of a create "between" the transport and the VFS. Driving the same FUSE opcodes
in-process through the bridge narrows that further, and the answer is that the between-layer is not the bridge.

`cargo run --release -p slates-bridge-fuse --example shards_fs_bench`, macOS aarch64, host load 19, 2026-10-18 —
a framed FUSE request in, dispatched through `VolumeBridge`, the framed reply out, over an in-memory duplex:

| op | in-process | the same op through a Linux FUSE mount | ratio |
|---|---|---|---|
| GETATTR / stat | **291 ns** | 13,900 ns | 48× |
| READ 4 KiB | **541 ns** | 43,300 ns | 80× |
| LOOKUP | **541 ns** | — | — |
| CREATE | **500 ns** p50 (3.7 µs p99, 36 µs max) | 276,900 ns | **554×** |

So the FUSE codec, the bridge dispatch and the VFS together answer a create in half a microsecond, and a real
mount takes 277. Three layers are now measured and none of them is the cost.

**Caveats, and they matter here.** The in-process volume is a plain bounded volume built directly
(`VolumeBridge::new`); the mounted one was `--shared`, an overlay over a 500-file tmpfs base, and published. So
this measurement deliberately excludes overlay copy-up, base-plane lookups and publish/change-tracking — any of
which a real create pays and this one does not. It also serves over an in-memory duplex rather than the
production `/dev/fuse` channel and its serve loop. Different host and architecture again, as above.

**What is left to attribute, with four candidates already eliminated.** Measured away: the bridge dispatch
(0.5 µs), the VFS create (~2 µs), and the transport floor (13.9 µs by the stat row). Read away, and worth
recording so they are not re-chased:

- **Cache revalidation is not it.** A classic FUSE cause of multiplied round trips is a zero entry/attr lifetime,
  forcing the kernel to revalidate on every path operation. slates already answers `CacheLifetime::Forever` for a
  volume's own objects (`bridge.rs::valid_for`), bounded only for a *live base* entry an outsider may change.
  `fuse-perf.sh`'s volume has no base, so every lifetime there is Forever.
- **Self-invalidation is not it.** A mutation does not make the mount notify its own kernel: a transport takes the
  invalidation cursor *after* serving its own request, "so its own changes are never invalidated in its own
  kernel" (`bridge-core/src/lib.rs::seen`). With one mount and no other writer, the notify path owes nothing, so
  the per-batch notify thread is not being spawned per create.

The ~130 µs that a *plain*-volume create costs (A-96: 2,000 creates in 0.26 s) and the further ~147 µs the
`--shared` overlay adds therefore sit in the daemon's serve loop around dispatch and in the kernel's own CREATE
path — and in the overlay's base-plane work for the `--shared` half. That a GETATTR costs 13.9 µs and a CREATE
130 µs through the same loop, when both are a 0.5 µs dispatch in-process, is the shape to explain.
`docs/wip/bench/fuse/fuse-perf.sh` names it by symbol — it `perf record`s the daemon through 2,000 creates.
Owed; it needs a Linux host quiet enough to profile, and both of mine are loaded (the box at 120–283 from another
session all evening).

> **Superseded 2026-10-19.** The share arithmetic here divides by a stale 276.9 µs. Re-measured, the transport
> is most of a create, not 5% of it. The VFS and bridge measurements are sound; the percentages are not.

**Measured (2026-10-18): the VFS primitive is 1% of a FUSE create, and the transport 5%. The other ~94% is the
path between them.** The amendment below reasoned from the stat row that the round trip could not be most of a
create; the in-process half is now measured rather than inferred.

`cargo run --release --example vfs_bench -p slates-vfs`, macOS aarch64 (M-series, 18 cores), host load 25,
2026-10-18 — the namespace row, which creates and unlinks one file in a populated tree:

| in-process VFS | median | p99 |
|---|---|---|
| create **and** unlink, 1,000-file tree | **3,875 ns** | 9,375 ns |
| create **and** unlink, 100,000-file tree | **3,958 ns** | 4,750 ns |
| look one name up in a 36-entry dir | 343 ns | 348 ns |

So the VFS's own create is **~2 µs** (the row is two namespace operations) and flat as the tree grows 100×.
Set against the FUSE create's 276.9 µs p50:

| where the 277 µs is | µs | share |
|---|---|---|
| FUSE transport (the stat row: one message, a trivial lookup, a reply) | ~14 | ~5% |
| the VFS create itself | ~2–4 | ~1% |
| **everything between them** — bridge dispatch, the client/daemon IPC, the shard hop, per-op bookkeeping | **~259** | **~94%** |

**Caveats, stated.** The two legs are on different hosts and architectures (in-process macOS aarch64; the FUSE
figures Linux x86_64 under Docker, kernel 6.12) and the harnesses differ, so the shares are order-of-magnitude,
not a ledger. What they cannot be is reversed: a 100× platform difference would be needed to make the VFS or the
transport the dominant term, and the two hosts are within a small factor of each other on every other row.

**What this means for the lever.** The cost is neither the mount protocol nor the copy-on-write filesystem — both
are already microsecond-scale. It is the per-operation path the daemon runs between receiving a FUSE message and
calling the VFS. That is cross-platform work, it is the same path every transport shares (FUSE, NFS, virtio-fs),
and it is where a profile should go next: `docs/wip/bench/fuse/fuse-perf.sh` runs 2,000 creates with `perf record`
on the daemon and will name the symbols inside that 259 µs. Owed; it needs a Linux host quiet enough to profile.

> **Superseded 2026-10-19 — and backwards.** It *is* most of it. This amendment reasoned from the stale row to
> demote the round trip; the re-measurement promotes it back, which is where the lever list had it all along.

**Amendment (2026-10-18): the table already bounds how much of this is the round trip, and it is not most of it.**
The order of levers below leads with the FUSE/daemon round trip, and the same table prices that round trip: a **stat
is 13.9 µs p50** — one FUSE message to the daemon, a trivial lookup, and the reply. A create is 276.9 µs. Even
allowing a create two or three messages where a stat takes one, the transport accounts for roughly 14–42 µs of it,
**5–15%**; the other 85–95% is work the daemon does on the create path. Rewrite (471 µs) and create+fsync (1,234 µs)
are further still from the floor.

So the largest lever is **not** the transport: it is what a create costs inside the daemon, which is cross-platform
VFS/server work rather than FUSE work, and is measurable in-process without a mount (`crates/vfs/examples/vfs_bench.rs`
times a create+unlink in a populated tree). Lever (2) in the list below is in any case closed — io_uring was removed
from the runtime on 2026-10-17 (A-136, Ada's A/B), so `FUSE_OVER_IO_URING` would be a second driver for one job.

This is an inference from the recorded table, not a new measurement: it says where to point the profiler, not what
the profiler will find. The owed step is unchanged and now has a sharper question — run
`docs/wip/bench/fuse/fuse-perf.sh` (2,000 creates through the mount with `perf record` on the daemon) and attribute
the ~263 µs *inside* the daemon: dispatch, shard hop, inode allocation, directory insert, merge/change tracking,
publish bookkeeping.

**Harness note.** The three-way table above cites `docs/wip/bench/fsbench/`, which **is not in the tree and never has
been** (`git log --diff-filter=A -- '*fsbench*'` is empty). The commands are supposed to be the contract; this one
cannot be re-run as cited. `docs/wip/bench/fuse/fuse-perf.sh` is the committed harness for the slates-FUSE leg.

**The optimization target, now quantified.** The write path dominates: create 277 µs, rewrite 471 µs, create+fsync
1,234 µs at p50, against overlayfs's 3–5 µs. Reducing the FUSE/daemon round-trip and the copy-up cost on writes is the
single largest p99 lever for condition 12 — far larger than any landing-side gain. Reads (43 µs) and stats (14 µs) are
closer but still ~12–14×. Next: profile one FUSE write end to end (client → kernel FUSE → daemon shard → overlay
copy-up → reply) to attribute the 277 µs, and A/B against the NFSv4.2 mount path (kernel-side caching/readahead may beat
FUSE).

### Condition 12/2: the overlay write path is O(volume) per op — whole-image publish, not a delta (2026-10-11)

A fresh Linux aarch64 release build (in Docker, kernel 6.12) re-validated the write path and attributed the earlier
"277 µs create." The daemon's **recovery barrier** (`publish_shard`, which every mount mutation waits on) is a
**delta** for a plain volume — measured ~3 µs on the shard, create ~42 µs end-to-end over FUSE, **flat** as the volume
grows — refuting the earlier working hypothesis that the barrier dominates the write path. The `perf` split of a plain
FUSE create shows the daemon's own logic at single digits of CPU (`fuse::serve` 1.7 %, `fuse::reply` 0.4 %,
`encode_publication` 0.85 %); the cost is the **FUSE kernel transport** (`fuse_request_end` 18 %, syscall entry/exit
~25 %, `fuse_copy_one`, `fuse_dev_write`) — the intrinsic userspace-FS premium, consistent with the ~13 µs FUSE floor
already recorded (2026-10-06).

**But overlay (base-backed) volumes do not take the delta.** `publishes_whole()` forces the whole image for any volume
with a base, a snapshot or a clone, so every mutation re-encodes the entire volume. Measured (`plain_vs_overlay_scale.sh`,
4000 creates in one directory, p50 by window):

| volume | [0,500) | [1500,2000) | [3500,4000) |
|---|---|---|---|
| plain (`--dynamic`) | 99 µs | 68 µs | 97 µs — **flat, O(1) delta** |
| overlay (`--dynamic --base`) | 196 µs | 714 µs | 490 µs — **grows, O(dir) whole-image** |

`perf` of overlay creates: `publish_shard` is **70 %** of the shard's CPU (vs 0.85 % plain), in `encode_inode_within`
(51 %) and `encode_directory` → quicksort by `NameEquivalence::compare` (33 %+21 %). The cost scales with the **upper**
directory, not the base (base 100→8000 files held create p50 at 128–159 µs for a fixed depth). So the b8fb3d9b "277 µs
create" was the overlay whole-image re-encode of a ~1500-entry directory — not a copy-up (134 µs, cheaper) and not VM
load. This is the OCI/container write path (condition 2) and a bounded-work (R4) concern: filling a directory is O(N²).
Root cause and the oracle-gated fix: `docs/bugs/2026-10-11-overlay-volumes-republish-whole-image-per-operation.md`.

**Real-container re-validation at this build (conditions 2, 3, 4, 11; Docker, privileged):**
- **Condition 4 (escape fuzz):** `battery.py`, 5 seeds × 300 hostile ops (path traversal `../`×8, device-node `mknod`,
  cross-user, write-through to `/etc`, setuid, hardlink/symlink escapes) through a FUSE mount — **no invariant violated**;
  the daemon's `/proc/<pid>/io` write_bytes moved **0 bytes** (condition 3: zero disk escape) and the whole outside-tree
  hash was unchanged (condition 4).
- **Condition 2 (containerd 1.7.24 + runc 1.1.15):** `slates attach --oci` → `{dst:/work, options:[bind,rw,private,nosuid,nodev]}`,
  `ctr run` ran a 2000-file workload inside a real container (exit 0, 2002 entries, 0.97 s), the setuid-root probe ran as
  **uid 65534 (nobody), not root** (A-94 under the OCI mount), the read-only attach refused writes, and the host side saw
  all 2002 entries with an identical tar sha.
- **Condition 11 (chaos):** git clone / cargo build / pip install on a kernel NFSv4.2 mount, daemon **SIGKILLed twice**
  per workload — anchor restarted it **6 times over held RAM** (generation 7, restarts 6), every workload finished
  byte-correct (git `fsck` clean, the built binary ran, `import flask 2.34.2` worked).

Commands (committed lanes): `docs/wip/bench/fuse/fuse-perf.sh`, `docs/wip/bench/escape/{inner.sh,battery.py}`,
`docs/wip/bench/containerd/run.sh`, `docs/wip/bench/realworld_chaos.sh`; the overlay A/B and perf lanes are scratch
(`plain_vs_overlay_scale.sh`, `overlay_perf.sh`).

### Condition 2/12: overlay volumes publish deltas, not their whole image (A-134; 2026-10-11)

The overlay O(volume)-per-op write path (previous entry) is **fixed**. An overlay (base-backed) volume — every
OCI/container volume — now publishes a **delta** per barrier while its base plane is unchanged, instead of re-imaging
the whole volume. Two coupled gates both had to move: `Dirty::recording()` (which had skipped recording changes for any
base/snapshot/clone volume, so the delta was empty) now records for a base-only volume, and `publishes_whole()` takes
the delta unless the base plane changed since the last publish (`BasePlane::durable_change_token` = the four versioned
tables' own write counts — bumped in `Versioned::set`, so an in-place head overwrite counts, which `version_count`
misses — plus a drift counter). A copy-up/whiteout/rename/drift still publishes the whole image (the only record that
carries the base plane); recovery applies the delta over the checkpoint, and `VolumeImage::apply` preserves `self.base`.

**Proof (load-independent — allocation-counted, valid under any machine load).**
`crates/vfs/tests/delta_cost.rs::an_overlay_create_publishes_a_delta_costing_the_same_in_a_large_upper_as_in_a_small_one`:
one create in an overlay now publishes a `VolumeRecord::Delta` (asserted, was `Full`), and its publication + replay
allocate **identically** in a 4,096-entry upper directory and a 64-entry one — i.e. O(delta), not O(volume). Before the
fix the publication was the whole image, allocating with the directory (O(N²) to fill one). Recovery byte-correctness is
proven by `crates/server/tests/recovery.rs` 39/0 (the overlay restart oracle and the crash-at-every-durable-step
oracle). `cargo clippy --workspace --all-targets -- -D warnings` and `cargo xtask check` clean.

A wall-clock p99 A/B (overlay create flat vs the earlier 196 → 714 µs growth) is owed on a quiet machine — this session's
host was at load 48 (other sessions), where timing is unreliable; the allocation-count proof above is load-independent
and establishes the asymptotic win now. Snapshot/clone volumes still publish whole (owed, a separate delta refinement).
**Superseded the same day** — see "own-snapshot volumes publish deltas too — all three shapes now O(delta)" below;
this sentence records what was true at this measurement, not what is true now.

**Wall-clock confirmation (A-134; Linux FUSE in Docker, kernel 6.12, 3 runs at load 6–15, 2026-10-11).** Overlay create
p50 by 500-op window in a single growing directory, against a plain volume on the same build:

| window | plain (no base) | overlay (empty base), 3 runs |
|---|---|---|
| [0, 500) | 67–95 µs | 144 / 184 / 185 µs |
| [1500, 2000) | 72–96 µs | 71 / 693 / 670 µs |
| [3500, 4000) | 65–96 µs | 69 / 100 / 98 µs |

At depth (`[3500,4000)`) the overlay create is **flat at ~70–100 µs, ≈ the plain volume** — the pre-fix value at that
depth was ~490 µs (previous entry), a **5–7× win**, confirming the O(delta) allocation proof wall-clock. The elevated
first window (~144–185 µs) is the first checkpoint plus base-plane setup. The `[1500,2000)` spike to ~680 µs appeared in
two of three runs but was 71 µs in the cleanest (lowest-load) run at the **same** window — concurrent-session load
noise, not algorithmic: a residual O(upper) cost would be monotonic (highest at `[3500,4000)`, not a mid-window spike
that vanishes under lower load). The asymptotic win is established deterministically and confirmed here wall-clock.

**vs TectonicFS (condition 12).** The OCI write path — overlay create — is now **~70–100 µs p50 on the local path**,
the µs-scale low-latency class. There is no direct p99-vs-p99 A/B to run: TectonicFS (FAST '21, Pan et al.) is
closed-source and, as a throughput-oriented exabyte warehouse store (≈10 KQPS/shard, a metadata op is "one or more
network calls"), **publishes no small-op p99** — it explicitly trades metadata latency for scale. The honest comparison
is the class one in the "Condition 12 … honest comparison to TectonicFS" entry above: slates holds the low-latency end
Tectonic gives up, and beats the runnable same-machine competitor (the kernel's NFS server) on read latency. The
remaining refinement to the slates-side number is a **quiet-machine** p99 histogram (vs this session's load-6–15
figures); the competitor side is a published-figure comparison by necessity.

### Condition 12: overlay p99 histogram and A-134 confirmed in practice (2026-10-11, host load 3.5→VM 6.7)

A quiet-ish-machine p99 histogram of the OCI write path (overlay volume, Linux FUSE, A-134), 10k creates + 5k
create+fsync, with the publish path instrumented (`PUBDBG`, reverted):

| op | p50 | p90 | p99 | p999 | max | stalls >1 ms |
|---|---|---|---|---|---|---|
| overlay create | 102 µs | 220 µs | 3,129 µs | 7,006 µs | 13,437 µs | 798 / 10,000 |
| overlay create+fsync | 103 µs | 138 µs | **232 µs** | 610 µs | 2,489 µs | **3 / 5,000** |
| plain create (same workload) | 76 µs | 137 µs | 221 µs | 437 µs | 1,516 µs | 4 / 10,000 |

**A-134 confirmed in practice, not just asymptotically.** The instrumented publish path showed the overlay publishing
**small deltas of 471–757 bytes** (not whole images), with checkpoints rare (~1 per 2,000 publishes, same as a plain
volume). So the O(volume)-per-op publish is genuinely gone on the real daemon under load — the delta bytes are constant
and tiny, matching the deterministic allocation proof.

**The durability-critical path is clean:** overlay create+**fsync** is p99 232 µs with only 3 stalls in 5,000 — on par
with a plain volume (p99 221 µs). The tail appears only in overlay create **without** fsync (798 stalls, p99 3.1 ms).
Since the same volume and base with fsync is clean, the tail is not the publish or a base-plane read (those run on the
fsync path too) — it is consistent with the kernel's FUSE async write-back bursting under VM load on the unsynced path,
not a daemon cost A-134 left behind. A dedicated isolation of that unsynced-writeback tail (and a bare-metal, not-VM
run) is the remaining condition-12 refinement; the durability path and the publish cost are both established.

### Condition 2: clones publish deltas for their own new files (A-134 extended; 2026-10-11)

A clone (an agent's work volume) used to publish its whole image at every barrier. It now records and publishes a
**delta** for its own new files, falling back to the whole image only on a barrier that **diverges** an origin inode
(copies up an inode born at or before the origin epoch — the move out of `origin_shared` a delta cannot carry, since
recovery refuses an inode in both `inodes` and `origin_shared`). Proof (load-independent, allocation-counted):
`crates/vfs/tests/delta_cost.rs::a_clone_create_publishes_a_delta_costing_the_same_in_a_large_clone_as_in_a_small_one`
— one create in a published clone publishes a `VolumeRecord::Delta` whose publication+replay allocate the same in a
4,096-file clone and a 64-file one (O(delta), not O(volume)). Recovery byte-correctness incl. divergence:
`crates/server/tests/recovery.rs::a_clone_recovers_over_its_recovered_origin...`, now extended to diverge an inherited
file in the clone, restart, and confirm the clone reads its diverged bytes while the origin's copy is untouched.
Snapshot (own-snapshot) volumes still publish whole — a separate snapshot-image delta (`trie::changed`) is owed.
**Superseded by the next entry, the same day:** all three shapes are O(delta); this records the state at this
measurement.

### Condition 2: own-snapshot volumes publish deltas too — all three shapes now O(delta) (A-134 generalized; 2026-10-11)

The delta publish now covers the last shape: a volume holding its **own snapshot** (e.g. a green merge target written
while snapshotted) publishes a delta for its own new files, falling back to the whole image only on a barrier that
**diverges a snapshot-pinned inode** (its old version moves into the snapshot's private records — a shared→private move
a delta cannot carry). This unified the gate across overlay/clone/snapshot: `recording()` records for every published
volume; `publishes_whole()` takes the whole image only on a shape change, `base_plane_changed`, or `shared_diverged`
(set in `make_current_inode` — the chokepoint for writes, nlink changes and deletes — when it copies up a clone-origin
or own-snapshot-pinned inode). Proof (load-independent, allocation-counted):
`crates/vfs/tests/delta_cost.rs::a_snapshotted_create_publishes_a_delta_costing_the_same_in_a_large_volume_as_in_a_small_one`
— one create in a snapshotted volume publishes a `Delta` whose publication+replay allocate the same in a 4,096-file and
a 64-file volume. End-to-end recovery: `crates/server/tests/recovery.rs::acknowledged_content_and_its_snapshot_survive_a_restart`
now also creates a file after the snapshot and confirms it (a delta) survives the restart alongside the diverge-the-head
whole-image case. With overlay (A-134) and clone deltas, **all three volume shapes are now O(delta) per op**, whole-image
only on a shared→private divergence. Condition 2's write-path delta optimization is complete.

### Measured-and-rejected: draining many pages per period does not speed takeover recovery (2026-10-11)

Hypothesis: `crates/server/src/takeover.rs::list_page` fixes each recovery page at `page_budget()` (the initial
receive window, ~55 objects) and `drive_host_takeover` asks one page per member per 100 ms coordinator period, so a
large retired host recovers in O(N) periods — "minutes for a very large host." Implemented a bounded drain loop
(`drive_host_takeover` pulls pages until the host is drained or one period elapses, via an extracted
`next_page_requests` that advances the round without re-promising local copies; each page stays one-flight-sized).

**Rejected — the A/B is dead-even.** Periods-to-recover (in-process 3-node fleet, kill the owner, measure
`Daemon::fleet_progress` periods to the last-id object placed; `takeover_periods_scale`, this M-series box):

| retired host | before (one page/period) | after (drain loop) |
|---|---|---|
| 1,024 volumes | 20 periods | 18 |
| 4,096 volumes | 34 periods | 33 |

A fixed ~1–2-period saving that **shrinks** with N — so recovery is **not paging-bound**. The ~18-period floor is
the required safety horizons (AUD-08 owner lease + SWIM detection, each ~900 ms ≈ 9 periods, correctness-required),
and the part that grows with N is the **O(N) required adopt + materialize** per object, not the paging. The
"slow paging" premise was wrong; the loop was reverted (the recovery oracle stayed 39/0 and the servable/`NotFound`
gate read 0 `NotFound` throughout, so the loop was *correct* — just not a win). Recovery of realistic hosts is ~2 s
(1,024 volumes), dominated by the horizons and per-object work, neither a cheap lever.

### Landed: a checkpoint marks its volumes by set, not a scan — O(V²) → O(V log V) in a shard's volumes (§4.8; 2026-10-12)

**What changed.** A barrier's common path is `publish_delta` (its cost is its changes, A-68); `publish_checkpoint`
(§4.8, the periodic amortized compaction when the journal `wants_checkpoint`) re-images the whole shard, then sweeps
every hosted volume to mark it published or not. That sweep asked `published.volumes.contains(&slot.id)` — a linear
scan of the committed list — once per volume, so the mark was **O(V²)** in the volumes a shard holds. The committed ids
are now built once into a `BTreeSet` (the volumes' own key type, matching the server's other membership sets), so the
sweep is **O(V log V)**. The slates case is a shard holding thousands of tiny agent volumes (provisioned in <50 µs, many
per host); the checkpoint stalls the shard's single thread, so a quadratic term there is a p99 tail under real load.

**Command.** `checkpoint_cost_scales_with_the_hosted_volume_count` (a `#[test] #[ignore]` measurement in
`crates/server/src/verbs.rs`): on one shard create V empty bounded volumes, then time `publish_checkpoint`, best of 5.
The volumes are empty, so the image's streaming is metadata only and the mark sweep stands out.
`cargo test --release -p slates-server --lib checkpoint_cost_scales_with_the_hosted_volume_count -- --ignored --nocapture`.
macOS, Darwin 25.4, 18 cores, host load ~13.9 (a shared machine — wall-clock, so the *shape* across V is the reading,
not the absolute points); best of 5.

**Before — the linear-scan mark (`published.volumes.contains`), best of 5:**

| volumes | checkpoint (best) | ns/volume |
|---|---|---|
| 512 | 208 µs | 406 |
| 1,024 | 483 µs | 472 |
| 2,048 | 1.30 ms | 634 |
| 4,096 | 3.71 ms | 906 |
| 8,192 | 11.83 ms | 1,444 |

ns/volume climbs 406 → 1,444 (super-linear). Fitting `a·V + b·V²`: a ≈ 337 ns/vol (streaming/sort, linear),
b ≈ 0.135 ns/vol² (the scan). At 8,192 the quadratic is ~9 of the 11.8 ms (77%); extrapolated it is ~145 ms at 32,768
volumes and ~0.4 s at 55,000 (a million volumes over eighteen shards) — a pure-waste single-thread stall per checkpoint.

**After — the set-membership mark (`BTreeSet::contains`), best of 5:**

| volumes | checkpoint (best) | ns/volume |
|---|---|---|
| 512 | 275 µs | 537 |
| 1,024 | 458 µs | 447 |
| 2,048 | 1.11 ms | 544 |
| 4,096 | 2.00 ms | 488 |
| 8,192 | 3.36 ms | 410 |

- **ns/volume is flat (~410–540 across 512 → 8,192)** — the quadratic term is gone; the cost is the linear streaming/sort.
- **8,192 volumes: 11.83 ms → 3.36 ms (3.5×),** and the gap widens with V (the removed term is quadratic): ~12× at
  32,768, ~18× at 55,000.
- At V=512 the after is a shade higher (275 vs 208 µs) — the set build's fixed cost, where the quadratic it removes is
  still negligible; the win is at the scale that matters.

**Correctness.** Same membership (a duplicate in the Vec, which the set folds, never changed a membership answer), so
behaviour is identical. The publish-path arbiter is green: `cargo test --release -p slates-server --test recovery` —
the two checkpoint-recovery oracles pass (`an_overlays_copied_up_and_created_files_survive_a_restart_over_their_base`,
`a_clone_recovers_over_its_recovered_origin...`), 38/39 with one pre-existing pressure-hold timing flake
(`a_hold_finer_than_the_pools_free_extents_...`, 3/3 on isolated re-run; it reads the shared machine's real memory
swings under load, unrelated to this change).

**Sibling (reported, not fixed here).** `stream_checkpoint`'s `left_out` closure scans `skipped` linearly per volume
(O(V × skipped)). `skipped` is empty in the normal path — every volume captured — so it did not enter this measurement
and bites only in the degenerate many-skips failure mode; a measurement-first fix awaits a scenario that forces skips
at scale.

### Measured: a transient false condemnation is harmless and its recovery is size-independent — so no detection-cost margin is owed (condition 11; 2026-10-12)

**Why.** slates can weight its SWIM detector's surety by a condemnation cost (`Detector::set_condemnation_cost`): the more a false condemnation would cost, the surer — and so slower — the detector is before it fires. The natural cost looks like the condemned host's whole takeover (N × ~1.5 µs, up to ~1.5 s at 1 M objects), which would slow detection for large hosts (the LAN-3 least-detection bound grows none→9 s as the cost grows none→1 s; Part 6, `fleet-under-load.md`). This measures the **real** cost of a transient false positive, to set the margin from evidence, not the worst case.

**Command.** `transient_false_condemnation_cost_is_flat_in_the_owners_object_count` (a `#[test] #[ignore]` fleet measurement in `crates/server/tests/fleet.rs`): a three-node mesh, N scratch volumes on A, both survivors made to falsely observe A dead (the fold A's own detector performs when it ages a peer to death), and while A refutes and is re-admitted, A's service of one of its objects is sampled and the re-admission window counted in load-independent periods. `cargo test --release -p slates-server --test fleet transient_false_condemnation -- --ignored --nocapture`. macOS, Darwin 25.4, 18 cores, host load ~14.

| owner's objects | re-admitted (after a real retirement) | periods to re-admit, per survivor | A's reads refused / sampled |
|---|---|---|---|
| 256 | yes | [1, 1] | 0 / 9 |
| 4,096 | yes | [1, 1] | 0 / 10 |

- **Zero unavailability.** The wrongly-retired *live* owner kept serving its own objects throughout (0 refusals), at both sizes — the owner lease (AUD-08) holds while the owner refutes.
- **Re-admission is O(fleet), size-independent.** One period to re-admit, flat across a 16× object range — membership refutation never enumerates objects (D-12).
- **So the condemnation cost is ~0** (the measured unavailability), not N × 1.5 µs. `set_condemnation_cost` is now set explicitly to zero in `crates/cluster/src/member_plane.rs` with this derivation, so the detector detects at its least bound (~0.9–1.84 s), fast for every host size, and a change to the vendored default cannot silently slow it. slates' safety against a false positive is the owner lease, not a slower detector. F = 10 s (the failover SLO, `FAILOVER_SLO_NS`) stands unchanged.

### Writable+readable DAX: the per-access win that closes the FUSE write/read gap (§4.6 A-9; 2026-10-13)

Environment: Apple M5 Max, macOS 26.4.1, Rust 1.98.0, release profile; branch `writable-dax`. Command:
`cargo run --release -p slates-vfs --example dax_bench`. Best-of-5, each the mean of 100,000 inner iterations;
all runs shown. The DAX access and setup numbers are **measured here** on the real content arena block the gate
pins and exposes (`Volume::dax_map_*`, `ChunkArena::bytes`); the mediated per-op costs are the **recorded** FUSE
and NFS figures from the Condition-12 sections above (2026-10-08/11), the round-trip DAX removes.

**Why.** slates over FUSE is 12–325× slower per op than in-kernel overlayfs, and the gap is the kernel↔daemon
round-trip, not the store (the arena is already RAM). Writable+readable DAX maps a file's content block into the
guest's DAX window, so a read or write is a memory access through the guest's own page table — no daemon call —
after a one-time `FUSE_SETUPMAPPING`.

| op (4 KiB) | DAX, measured | runs (ns) | mediated, recorded |
|---|---|---|---|
| read (memcpy out of the arena block) | **37 ns** | [45, 40, 37, 38, 38] | FUSE 43,000 / NFS 5,700 |
| write (memcpy into the arena block) | **33 ns** | [34, 54, 33, 38, 38] | FUSE 277,000 |
| `dax_map_read` setup (pin + register) | 82 ns | [85, 82, 82, 83, 84] | — |
| `dax_map_write` setup (copy-up + pin + register) | 573 ns | [622, 612, 627, 573, 573] | — |

**Amortised** — a file touched N times, mediated N×round-trip vs DAX (one SETUPMAPPING round-trip + the measured
mint, then N memory accesses):

| N | read vs FUSE | write vs FUSE | read vs NFS | write vs NFS |
|---|---|---|---|---|
| 1 | 1.0× | 1.0× | 1.0× | 0.9× |
| 16 | 15.8× | 15.9× | 14.3× | 13.4× |
| 256 | 209× | 248× | 96× | 99× |
| 4,096 | 905× | **2,749×** | 148× | 165× |

**Reading it honestly.** A single access does not benefit (DAX pays one round-trip to set the mapping up); the
win is the repeated access that is the whole reason to map a file, and it is 15× by N=16 and two to three orders
of magnitude by N=4,096. Per-access DAX (33–37 ns, pure memory) is faster than even in-kernel overlayfs
(read 3.5 µs, Condition-12 above) — once mapped, the kernel VFS layer is out of the path. The numbers measure
**warm** access (the arena block resident); a guest's first touch of each window page takes a one-time page
fault, folded into the mapping's setup, not per access.

**Scope — what is measured vs owed.** This quantifies the mechanism's read+write win on the real arena + gate,
with the mediated side from the recorded baseline. The wired device window (the `MAP_FIXED` of the content
arena's anchor shared-object fd into a VMM cache region, `crates/bridge-virtiofs`) and a live virtio-fs DAX guest
A/B against overlayfs are the owed end-to-end validation; the daemon-side seam (ABI, `namespace_inode` gate,
`Bridge::dax_map`/`dax_unmap`) is wired and tested (this branch). This is a projection grounded in measured
components, not a guest-kernel A/B — stated so it is not mistaken for one.

### Real-world workloads, slates vs tmpfs vs JuiceFS — does it hold up under real use (2026-10-14)

Environment: Docker `rust:1.98.0`, privileged, on the Mac's Linux VM (no `/dev/kvm`, so the usable transport is
measured, not a DAX guest); slates release built in-container (134 s). Command:
`docker run --rm --privileged -v <repo>:/work:ro -v <out>:/out -v slates-cc:/ccache rust:1.98.0 bash
/work/docs/wip/bench/realworld_compete.sh`. Real developer workloads on a slates **NFSv4.2** volume through
Linux's own kernel client, beside a tmpfs and **JuiceFS** 1.4.1 (the userspace/distributed competitor: FUSE,
SQLite metadata, a local file object store): a shallow `git clone` + `git fsck --full` + a tree hash, a
`cargo build --release` with real crates.io dependencies, and a `pip install` into a venv.

| workload | tmpfs | slates (NFSv4.2) | JuiceFS | slates vs JuiceFS |
|---|---|---|---|---|
| git clone (ripgrep, depth 1) | 1.32 s | **1.17 s** | 1.67 s | 1.4× faster |
| git fsck --full | 0.02 s | 0.02 s | 0.05 s | 2.5× |
| cargo build (regex/serde/serde_json) | 8.32 s | **8.18 s** | 46.31 s | **5.7× faster** |
| pip install (requests + flask) | 2.58 s | 2.84 s | 5.01 s | 1.8× |

**Correctness (holds up):** every workload exits 0 on every arm; the ripgrep clone is **byte-identical** — 236
files under `./rg/` on all three, tree hash `31a1f6f713d195f5` on all three; the cargo-built binary runs and
prints the same `{"n":2}`; the only slates-vs-tmpfs file-list difference is three transient cargo
`.cargo-*-lock` files (a `find`-vs-cargo-cleanup timing artifact, not divergence); `slates status` reports 0
refusals of any kind.

**Reading it.** Against tmpfs, slates-NFS is **at tmpfs level** for real workloads — the per-op FUSE/NFS
microsecond (the 12–325× the Condition-12 micro-op table shows) is amortised away by the kernel's page cache,
readahead and the workload's own compute, so the micro-op gap is not what real use pays. Against the apt
userspace/distributed peer, JuiceFS, slates is **1.4–5.7× faster**, widest on `cargo build` because a build
touches thousands of small files and JuiceFS pays a SQLite-metadata + object-store round-trip on each where
slates serves from RAM. This is the honest real-use picture: slates holds up (byte-identical, zero refusals) and
beats the directly-comparable competitor, while the micro-op gap vs *in-kernel* overlayfs is real but mostly
invisible to real workloads — and is what the DAX data path (measured separately above) removes entirely once
its device window is wired behind a KVM-capable host.

## DAX sealed-chunk backing: view versus reopen, and the learned hold (2026-10-17)

A sealed chunk (ciphertext at rest, A-92/A-99) cannot be mapped into a guest's DAX window as it stands — the guest's
MMU would read ciphertext, and refusing the SETUPMAPPING SIGBUSes a `dax=always` mmap. Two arms were built in full,
each on its own branch, and measured head to head (§4.6):

- **view** (`writable-dax-view`): back the chunk with a decrypted read-only throwaway the mapping owns; the canonical
  chunk stays ciphertext and the copy is freed on revoke.
- **reopen** (`writable-dax-reopen`): transition the chunk's own representation sealed→open — decrypt it in place, map
  its own block, and re-encrypt it once the window is gone.

Both are correct: each carries a live crosvm guest's mmap of a 192 KiB multi-block file byte-for-byte
(`GUEST-MMAP-OK`), with the N-map loop backing all three chunks of the one SETUPMAPPING.

Environment (in-process arms): Apple M5 Max, 128 GiB, macOS 26.4.1, Rust 1.98.0, release. **Load average 16.4–16.9
throughout, another session's VM at 451% CPU** — not a quiet machine, so the two arms were built to separate binaries
up front and run **alternately**, and each data point gets a fresh store, volume and sealed file.
Command: `cargo run --release -p slates-vfs --example dax_backing_ab` on each branch, best of 3, all runs shown.

### The memory relationship (contention-proof: a count, not a timing)

Streaming a 64 MiB sealed file while varying the simultaneously-mapped bytes:

| simultaneously mapped | view extra RAM | reopen extra RAM |
|---|---|---|
| 0.25 MiB | 0.25 MiB | 0.00 |
| 1 MiB | 1.00 MiB | 0.00 |
| 2 MiB | 2.00 MiB | 0.00 |
| 8 MiB | 8.00 MiB | 0.00 |
| 32 MiB | 32.00 MiB | 0.00 |
| 64 MiB | 64.00 MiB | 0.00 |

Slope exactly 1.0 over 256×: **view costs one byte of RAM per byte simultaneously mapped**; reopen costs none.

### What a real guest actually maps, and how soon it comes back

Environment: bare-metal x86_64, kernel 7.2.9-cachyos, crosvm, 512 MB guest, 4 KiB pages. **Load average 34** (shared
box). Command: `crosvm_dax_ab` harness with the guest writing a 32 MiB file — four times its DAX cache, so the guest
kernel must reclaim mapping ranges — then reading every byte twice through one mmap.

| advertised DAX cache | live-mapped high-water | remaps observed | revoke→remap p99 |
|---|---|---|---|
| 8 MiB | **8.000 MiB** | 522 | **67 ms** |
| 4 MiB | **4.000 MiB** | 541 | **537 ms** |

Two readings, two conclusions. First, the guest fills its cache and holds it full, so **view's extra RAM is the DAX
cache slates advertises** — at the 256 MiB cache the harness shipped with, +256 MiB of RAM in a RAM-only system.
Second, the interval after which a guest asks for a reclaimed chunk again moved **8×** for one workload merely by
changing the cache, so no fixed re-seal delay is right: it is learned per volume (§4.6).

### Timings (interleaved, isolated, load recorded) — quiet machine, with the learned hold in place

Environment: bare-metal x86_64, kernel 7.2.9-cachyos, 4 KiB pages, release. **Load average 2.98** (quiet), both arms
built to separate binaries and run alternately, two rounds, each point best of 3 on a fresh store; the two rounds
agree to within a millisecond throughout. This supersedes an earlier set taken on a loaded Mac (load 16–17) *before
the learned hold existed*; those are withdrawn.

Streaming a 64 MiB sealed file, varying the simultaneously-mapped bytes:

| live-mapped | reopen | view | view extra RAM |
|---|---|---|---|
| 0.25 MiB | 0.011 s (5,960 MiB/s) | 0.007 s (8,860 MiB/s) | 0.25 MiB |
| 1 MiB | 0.011 s (5,648) | 0.008 s (7,896) | 1.00 MiB |
| 2 MiB | 0.012 s (5,488) | 0.008 s (8,117) | 2.00 MiB |
| 8 MiB | 0.012 s (5,470) | 0.008 s (7,544) | 8.00 MiB |
| 32 MiB | 0.012 s (5,245) | 0.009 s (6,909) | 32.00 MiB |
| 64 MiB | 0.012 s (5,322) | 0.011 s (5,814) | 64.00 MiB |

Re-reading a 2 MiB working set twenty times:

| re-read | reopen | view |
|---|---|---|
| guest returns before the sweep | **0.001 s** (41,356 MiB/s) | 0.005 s (8,724 MiB/s) |
| sweeps run between the re-reads | **0.001 s** (34,270 MiB/s) | 0.005 s (8,762 MiB/s) |

Two readings. View leads on a one-shot stream, but **its lead shrinks as the live set grows** — 1.57× at 0.25 MiB,
1.09× at 64 MiB — because the throwaway's allocate-and-copy scales with the mapped set while reopen's in-place
decrypt does not; at a realistically full cache the two are within 9%. And the bottom row is **the learned hold
measured in a workload**: before it existed, sweeps between re-reads re-encrypted the chunk and reopen *lost* that
row by 1.1–1.7×; with the hold learned the sweeps leave the plaintext alone and reopen wins it by 3.9×. The hold is
what removes reopen's only real penalty, and it is removed by learning, not by a chosen delay.

**Measured and rejected: the decrypt-view throwaway.** It leads only on a one-shot stream, by 1.57× at a small live
set falling to 1.09× at a full one, and it loses every re-read row by ~4×. Against that it needs one byte of RAM per
byte simultaneously mapped with no amortisation, and the live guest shows that means the whole advertised DAX cache
(256 MiB as shipped). Reopen holds one representation, costs no mapped-set RAM, and its only penalty — the re-encrypt
— is removed by a hold learned from the guest's own behaviour, measured above.

**Correction on record.** The first numbers taken for this comparison were wrong and were withdrawn: all workloads
shared one store (later measurements inherited earlier arena churn), load was not recorded, and the arms were not
interleaved. That overstated view's streaming win by about 2× and understated reopen's returning-guest win by about
2×. The tables above are the re-measurement under the stated discipline.

## io_uring against epoll on Linux: the measurement D-9 never had (2026-10-18)

D-9 chose io_uring on Linux citing `DEFER_TASKRUN` + `SINGLE_ISSUER` "for jitter" at evidence tier B
(the `io_uring_setup(2)` man page). No measurement in this project had ever compared the two drivers,
so the default rested on a citation rather than a number. Ada asked why we use io_uring at all, given
its security record; this is the number that was missing from the answer.

Environment: AMD, 32 cores, Linux 7.2.9-2-cachyos, Rust release profile, **a shared box under another
session's work — load average 13 → 29 over the run, recorded per block**. Two binaries differing in
one line: the epoll arm has `rt::uring::probe` forced to `false`, so `os_driver` selects
`EpollDriver`; everything else identical.
Command: `cargo build --release -p slates-client --example provision_bench` per arm, then the arms
interleaved `uring, epoll` three rounds, each invocation its own 3 runs × (2,000 spinning + 500
parked) samples at 1, 8 and 64 clients.

Arms verified to be the drivers they claim, by descriptor rather than by trust: the io_uring arm
holds 31 `anon_inode:[io_uring]` descriptors and 0 `eventpoll`; the epoll arm holds 0 and 31.

| Row (median of 3 invocations, ns) | io_uring | epoll | epoll / io_uring |
|---|---|---|---|
| `parked_1_p50` | 65,402 | 73,855 | **1.13× (io_uring better)** |
| `parked_1_p99` | 1,490,024 | 1,624,044 | 1.09× |
| `parked_1_p999` | 2,022,083 | 2,122,540 | 1.05× |
| `spinning_1_p50` | 19,496 | 19,353 | 0.99× |
| `spinning_1_p99` | 1,657,573 | 1,577,323 | 0.95× (epoll better) |
| `spinning_8_p50` | 20,047 | 17,356 | 0.87× (epoll better) |
| `spinning_8_p99` | 2,024,321 | 1,556,771 | **0.77× (epoll better)** |
| `spinning_8_p999` | 3,185,257 | 2,366,245 | 0.74× |
| `spinning_64_p99` | 4,117,519 | 3,897,188 | 0.95× |

What it means. io_uring's advantage is real but small and **confined to the parked wake path** — 8.5 µs
on a 65 µs operation at p50, shrinking to 5% by p999. In the **spinning** path, which is the one R9's
sub-50 µs provisioning gate actually measures, epoll is equal or better, by 23% at the 8-client p99.
So D-9's jitter rationale holds only where the gate does not look, and costs nothing where it does.

Honesty about this reading: the box was loaded and the load climbed through the run, so every p99 and
p999 here (1.5–4 ms) is dominated by scheduling, not by the driver — those rows are nearly noise. The
**p50 rows are the trustworthy ones**, and they say the same thing: parked favours io_uring by 13%,
spinning favours epoll. Within each round the epoll arm runs second, which biases *against* epoll, so
its spinning win is if anything understated. A quiesced re-run would sharpen the parked figure; it is
no longer owed for the decision, which D-9's amendment settled (see the outcome note below), and the
arms it would need are gone from the tree with io_uring.

What it does not measure: security. io_uring is ~60% of Google's kernel VRP submissions, is disabled
by default on openSUSE and dropped from Docker's and containerd's seccomp profiles, and slates uses it
for **two opcodes only** — `PollAdd` and `Nop`, no I/O through the ring (which is why the 2025
syscall-monitoring-bypass class does not reach our R1 tracer: every read and write is still an ordinary
syscall). The trade on the table is therefore a 13% parked-p50 win against the whole io_uring kernel
surface, for a ring we use as an epoll substitute. Also measured here, incidentally: `io_uring_setup`
returning `ENOMEM` under 32-way test parallelism against an 8 MiB `memlock` limit is what makes the
Linux recovery oracle flaky — the epoll arm has no such failure mode.

**Outcome (2026-10-18): Ada chose epoll and io_uring was removed (A-136).** The measurement above is
what it was weighed on: a 13% parked-p50 win, confined to the path the R9 gate does not measure,
against the kernel attack surface and a `RLIMIT_MEMLOCK` boot failure. The driver is deleted rather
than demoted — replace, do not layer. A quiesced re-run of this A/B is still owed as a record of the
parked figure; it cannot change the decision, which rests on surface and on the spinning column.

## 2026-10-18: what the confined-descriptor rule costs a local zero-copy read (A-135)

Since 2026-10-18 the daemon refuses to hand a peer a descriptor that authorizes more than the read it answers
(`Control::send_mapping` → `confined_for_a_peer`; the escape it closes is
`docs/bugs/2026-10-18-a-read-handed-the-peer-the-whole-content-arena.md`). No backing satisfies the rule yet, so
a `MapRead` takes its copy path. This prices that, so the per-tenant-backing decision has a number.

**Hardware and discipline.** x86_64 Linux (cachyos 7.2.9-2), release binaries, a real one-shard daemon per run,
a fresh volume and file per run. Two binaries differing in one line — the rule in force, and the rule lifted —
**interleaved** A,B,A,B over three rounds, so the box's load is shared by both arms rather than attributed to
one. The box is shared and was busy: `/proc/loadavg` 43–68 across the runs, recorded per line by the bench
itself. 200 samples a point, one warm read outside the sample. Command:
`cargo run --release -p slates-client --example map_read_ab -- <file_kib> <samples>`.

**64 KiB (one content chunk), the only size where both arms return the same bytes:**

| round | arm | p50 | p99 | max |
|---|---|---|---|---|
| 2 | copy (as shipped) | 62 µs | 294 µs | 353 µs |
| 2 | zero-copy (rule lifted) | **4 µs** | **12 µs** | 46 µs |
| 3 | copy (as shipped) | 61 µs | 296 µs | 388 µs |
| 3 | zero-copy (rule lifted) | **6 µs** | **13 µs** | 17 µs |

So the rule costs roughly **10–15× at p50 and ~23× at p99** on a single-chunk read. That is what a confined,
sealed per-tenant backing would buy back.

**No figure is given above 64 KiB, and the reason is the better finding.** The zero-copy arm returned exactly
65,536 bytes for 256 KiB, 1 MiB and 4 MiB files — one chunk, silently — so its "throughput" at those sizes was
meaningless (the bench was reporting 676,704 MiB/s for a 4 MiB file it had read 64 KiB of). That is
`docs/bugs/2026-10-18-a-zero-copy-read-of-a-multi-chunk-file-returned-one-chunk.md`, fixed and red-checked in
the same change; the bench now reports latency only and prints what each arm actually returned, so an arm that
answers with less can never look fast.

**What this does not measure:** the copy arm's figures are a full paged read of the file and the zero-copy arm's
are an `mmap`, so the ratio is a property of the file size as much as of the paths; only the single-chunk row is
an apples-to-apples comparison, and it is the only one quoted. A quiesced re-run would sharpen all of it.

## One notify thread per FUSE mount: what a mount costs other writers (A-142; 2026-10-22)

**The question.** Every change made through another path runs an invalidation round on each FUSE mount of the volume,
on the volume's owner shard. Until A-142 each round started a thread to write its notifications off the serving thread
and joined it when the round came back. A thread start costs far more than the rest of the round.

**The command.** `crates/cli/examples/write_round_ab.rs`: an in-process daemon, one volume, and an SDK client timing
2,000 sequential 64-byte whole-file writes of one file per phase. Phases alternate between no mount and a mount made by
the real `slates mount`, which reads the file first, four phases each way. The "before" and "after" binaries were built
from one tree, differing only in `crates/bridge-fuse/src/channel.rs` (before: `c5f5a212`; binary hashes `00b2caa9`,
`a67eb13a`). They ran interleaved, before then after, three times:
`cargo build --release -p slates-cli --bin slates --example write_round_ab && target/release/examples/write_round_ab`.

**The machine.** The bare-metal Linux box (x86_64, 32 cores, kernel 7.2.9, `fusermount3`), 2026-10-22 15:27–15:31 UTC,
with a load average of 10–11 from other work.

| run | arm | phase | no mount p50 | no mount p90 | mounted p50 | mounted p90 | load (1, 5, 15 min) |
|---|---|---|---|---|---|---|---|
| 1 | before | 0 | 10.2 | 15.6 | 9.8 | 30.5 | 10.89 9.44 8.79 |
| 1 | before | 1 | 10.4 | 14.1 | 10.2 | 24.1 | 10.89 9.44 8.79 |
| 1 | before | 2 | 10.0 | 13.0 | 10.3 | 24.8 | 10.89 9.44 8.79 |
| 1 | before | 3 | 9.6 | 13.9 | 10.6 | 23.2 | 10.89 9.44 8.79 |
| 1 | after | 0 | 10.1 | 13.6 | 10.3 | 17.6 | 10.89 9.44 8.79 |
| 1 | after | 1 | 9.6 | 13.3 | 10.2 | 17.6 | 10.89 9.44 8.79 |
| 1 | after | 2 | 9.6 | 13.3 | 10.4 | 14.8 | 10.89 9.44 8.79 |
| 1 | after | 3 | 9.6 | 13.3 | 9.2 | 14.3 | 10.89 9.44 8.79 |
| 2 | before | 0 | 10.6 | 29.7 | 10.1 | 22.9 | 10.89 9.44 8.79 |
| 2 | before | 1 | 9.8 | 13.5 | 10.7 | 22.7 | 10.89 9.44 8.79 |
| 2 | before | 2 | 9.3 | 14.4 | 9.1 | 29.7 | 11.06 9.50 8.81 |
| 2 | before | 3 | 9.9 | 13.5 | 9.3 | 26.5 | 11.06 9.50 8.81 |
| 2 | after | 0 | 8.4 | 11.9 | 9.1 | 16.4 | 11.06 9.50 8.81 |
| 2 | after | 1 | 8.9 | 12.5 | 8.5 | 15.0 | 11.06 9.50 8.81 |
| 2 | after | 2 | 9.1 | 13.4 | 9.3 | 15.3 | 11.06 9.50 8.81 |
| 2 | after | 3 | 8.9 | 12.0 | 9.0 | 14.3 | 11.06 9.50 8.81 |
| 3 | before | 0 | 8.2 | 10.6 | 8.7 | 20.2 | 11.06 9.50 8.81 |
| 3 | before | 1 | 9.1 | 11.9 | 9.3 | 17.5 | 11.06 9.50 8.81 |
| 3 | before | 2 | 8.8 | 11.9 | 9.5 | 21.3 | 11.06 9.50 8.81 |
| 3 | before | 3 | 9.3 | 11.6 | 8.8 | 23.3 | 11.06 9.50 8.81 |
| 3 | after | 0 | 9.2 | 13.3 | 9.6 | 16.4 | 11.06 9.50 8.81 |
| 3 | after | 1 | 8.8 | 11.8 | 9.1 | 16.4 | 11.06 9.50 8.81 |
| 3 | after | 2 | 8.2 | 11.3 | 9.2 | 14.2 | 11.06 9.50 8.81 |
| 3 | after | 3 | 8.6 | 11.2 | 9.2 | 13.3 | 10.90 9.49 8.81 |

A bare thread start and join, timed in the same runs: p50 16.2–19.8 µs, p90 26.4–29.7 µs.

**What it shows.**
- **The mounted p90 fell from a median of 23.3 µs (17.5–30.5) to 15.1 µs (13.3–17.6),** within about 2 µs of the
  unmounted p90 (a median of 13.3 µs across both arms, and 12.9 µs in the after arm's own phases).
- **The p50 does not move in either arm,** because the writer is answered before the round runs. What the old thread
  start cost was the next write, waiting behind it on the shard. That fits the old mounted p90 sitting about one spawn
  above the unmounted one.
- **The p99s and means are this box's background load,** in both arms and both modes (0.3–1.5 ms), and are not read.

**What remains**, about 2 µs at the p90, is the round itself: the gather, the encode and the hand-off over the
notifier's channel.

## A change drops only its bytes from a mount's cache: what a guest's DAX map saves (A-146; 2026-10-22)

**The question.** A guest's DAX map of a file shared through a slates mount lives in the host kernel's page cache for
that file (`virtiofs.md` §8 item 7). Until A-146, every content change through another path dropped the whole file from
that cache, so the guest re-fetched every page of its map through the mount.

**The command.** On the box, a slates anchor runs the release `slates` with one 2 GiB volume mounted twice by FUSE, and
a 128 MiB file of one byte value is written through mount one. crosvm (built from upstream source on 2026-10-16) shares
mount one into a guest running the box's own kernel, with its own virtio-fs device (`dax=true`, its sandbox on). The
guest's static `/init` maps the file shared (`dax=always`) and scans one byte per page: once cold, twice warm, then six
rounds. In each round it signals through a file, waits touching memory only until the middle byte changes, and times
one scan and then another. The host writes the middle byte through mount two, one `pwrite` a round.

The two arms are release builds of one tree differing only in A-146 (before: `1e6d7253`), each in its own target
directory, run interleaved before then after, three times (`dax_cost_run.sh SLATES 128` per run). The probe and the
driver are below, kept here rather than in the tree. A gated test that runs them is owed (GAPS).

**The machine.** The box (x86_64, 32 cores, 124 GiB, kernel 7.2.9-2-cachyos), 2026-10-22 21:30–21:31 UTC. The load
average was 39–43 from other sessions' builds at the start of each run, and 99 just after the last.

All times are µs.

| run | arm | cold scan | scan after each change | the scan after that |
|---|---|---|---|---|
| 1 | before | 244,986 | 301,358 · 277,229 · 367,014 · 823,576 · 1,263,233 · 1,174,719 | 923 · 458 · 400 · 1,819 · 3,082 · 16,786 |
| 1 | after | 213,095 | 1,479 · 929 · 527 · 432 · 597 · 483 | 233 · 595 · 232 · 248 · 224 · 237 |
| 2 | before | 238,206 | 408,092 · 232,255 · 284,675 · 310,996 · 387,225 · 217,906 | 2,415 · 674 · 830 · 571 · 921 · 629 |
| 2 | after | 199,913 | 440 · 429 · 277 · 301 · 341 · 257 | 227 · 163 · 230 · 226 · 238 · 220 |
| 3 | before | 190,089 | 241,785 · 268,905 · 164,072 · 214,258 · 189,710 · 231,134 | 876 · 1,118 · 384 · 430 · 1,152 · 6,862 |
| 3 | after | 208,472 | 463 · 473 · 374 · 348 · 909 · 574 | 210 · 324 · 236 · 206 · 1,037 · 175 |

**What it shows.**
- **The scan after a one-byte change:** a median of 273 ms (164–1,263 ms) before, and 0.45 ms (0.26–1.48 ms) after,
  about 600× at the median. Before, every change re-fetched the whole 128 MiB through the mount.
- **The cold scan does not move** (190–245 ms before, 200–213 ms after): a first fault-in is the same work either way.
- **After a change, the scan sits near the warm scan** (0.2–0.3 ms), plus the one folio re-fetched.
- **Not read:** the guest's wait to see each change (8–137 ms). That is the host driver's poll and Python's start, not
  the delivery.

An earlier run of the same probe on a debug build, before A-146 only, gave 356–382 ms for each scan after a change and
113 ms cold.

**The probe** (`gcc -static -O2`, the initramfs's `/init`):

```c
/* Measures what one byte changed through another path costs a guest's DAX map of a large file on the mount route:
   the guest scans every page of its map cold, warm, then after each change, and reports each scan's time. A static
   /init; markers on the console; the host makes the changes. Experiment artifact, not product code. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/stat.h>

#define PAGE 4096
#define ROUNDS 6
#define SECOND (1000L * 1000)

static void say(const char *fmt, ...) {
  char line[512];
  va_list args;
  va_start(args, fmt);
  int n = vsnprintf(line, sizeof line, fmt, args);
  va_end(args);
  if (n > 0) write(1, line, (size_t)(n < (int)sizeof line ? n : (int)sizeof line - 1));
}

static long now_us(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec * 1000000L + t.tv_nsec / 1000;
}

static long scan(const volatile unsigned char *map, long size, unsigned long *sum) {
  long started = now_us();
  unsigned long total = 0;
  for (long at = 0; at < size; at += PAGE) total += map[at];
  *sum = total;
  return now_us() - started;
}

static int write_file(const char *path, const char *text) {
  int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
  if (fd < 0) return -1;
  int ok = write(fd, text, strlen(text)) == (ssize_t)strlen(text) && fsync(fd) == 0;
  close(fd);
  return ok ? 0 : -1;
}

int main(void) {
  mount("dev", "/dev", "devtmpfs", 0, 0);
  int console = open("/dev/console", O_RDWR);
  if (console >= 0) { dup2(console, 0); dup2(console, 1); dup2(console, 2); }
  mount("proc", "/proc", "proc", 0, 0);
  say("GUEST-BOOTED\n");
  if (mount("slates", "/mnt", "virtiofs", 0, "dax=always") != 0) { say("GUEST-MOUNT-FAILED errno=%d\n", errno); goto out; }
  int fd = open("/mnt/big.bin", O_RDWR);
  struct stat info;
  if (fd < 0 || fstat(fd, &info) != 0) { say("GUEST-OPEN-FAILED errno=%d\n", errno); goto out; }
  long size = info.st_size;
  volatile unsigned char *map = mmap(0, (size_t)size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  if (map == MAP_FAILED) { say("GUEST-MAP-FAILED errno=%d\n", errno); goto out; }
  unsigned long sum = 0;
  say("COST size=%ld pages=%ld\n", size, size / PAGE);
  say("COST cold-scan-us=%ld\n", scan(map, size, &sum));
  say("COST warm-scan-us=%ld\n", scan(map, size, &sum));
  say("COST warm-scan-us=%ld\n", scan(map, size, &sum));
  long middle = size / 2;
  for (int round = 1; round <= ROUNDS; round++) {
    char name[64];
    snprintf(name, sizeof name, "/mnt/cost-%d", round);
    if (write_file(name, "ready") != 0) { say("GUEST-SIGNAL-FAILED errno=%d\n", errno); goto out; }
    unsigned char want = (unsigned char)('A' + round);
    long started = now_us();
    while (map[middle] != want && now_us() - started < 20 * SECOND) {}
    long seen = map[middle] == want ? now_us() - started : -1;
    long after = scan(map, size, &sum);
    long again = scan(map, size, &sum);
    say("COST round=%d saw-change-us=%ld scan-after-change-us=%ld scan-again-us=%ld\n", round, seen, after, again);
  }
  say("COST-END\n");
out:
  sleep(1);
  reboot(RB_POWER_OFF);
  for (;;) pause();
  return 0;
}
```

**The driver** (one run of one arm):

```bash
#!/bin/bash
# What one byte changed through another path costs a guest's DAX map on the mount route. Usage: dax_cost_run.sh SLATES
# [SIZE_MIB]. Experiment, not product.
set -u
S=${1:?the slates binary}
SIZE_MIB=${2:-128}
R=~/r12-daxcost-run
I=gcost-$$
CROSVM=~/crosvm/crosvm
KERNEL=~/crosvm-ab/vmlinuz
rm -rf "$R"; mkdir -p "$R/m1" "$R/m2" "$R/initramfs/proc" "$R/initramfs/sys" "$R/initramfs/mnt" "$R/initramfs/dev"
cleanup() {
  pkill -9 -f "crosvm run.*$R/m1" 2>/dev/null
  sleep 0.5
  for m in "$R/m1" "$R/m2"; do mountpoint -q "$m" && { "$S" --instance "$I" unmount "$m" >/dev/null 2>&1 || fusermount3 -u "$m"; }; done
  [ -n "${APID:-}" ] && kill "$APID" 2>/dev/null && wait "$APID" 2>/dev/null
  pgrep -f "instance $I" >/dev/null && pkill -f "instance $I"
}
trap cleanup EXIT
gcc -static -O2 -o "$R/initramfs/init" ~/r12-guestdax-run-src/dax_cost_init.c || exit 1
( cd "$R/initramfs" && find . -print0 | cpio --null -o --format=newc 2>/dev/null | gzip -9 > "$R/initramfs.cpio.gz" )
setsid "$S" --instance "$I" anchor --quick --shards 2 > "$R/anchor.log" 2>&1 &
APID=$!
for _ in $(seq 1 600); do grep -q "serving on" "$R/anchor.log" && break; sleep 0.1; done
"$S" --instance "$I" bootstrap root >/dev/null || exit 1
ID=$("$S" --instance "$I" volume create gc --bounded 2GiB | sed -n 's/^id: //p')
"$S" --instance "$I" mount "$ID" "$R/m1" >/dev/null || exit 1
"$S" --instance "$I" mount "$ID" "$R/m2" >/dev/null || exit 1
head -c $((SIZE_MIB * 1048576)) /dev/zero | tr '\0' 'h' | dd of="$R/m1/big.bin" bs=1M status=none
timeout 600 "$CROSVM" run -m 2048 --cpus 2 \
  --shared-dir "$R/m1:slates:type=fs:cache=always:timeout=3600:dax=true:writeback=false:posix_acl=false:security_ctx=false" \
  --initrd "$R/initramfs.cpio.gz" -p "console=ttyS0 earlycon=uart8250,io,0x3f8,115200 rdinit=/init" \
  --serial type=stdout,hardware=serial,num=1,console,earlycon \
  "$KERNEL" < /dev/null > "$R/crosvm.log" 2>&1 &
for round in 1 2 3 4 5 6; do
  for _ in $(seq 1 30000); do [ -e "$R/m2/cost-$round" ] && break; pgrep -f "crosvm run.*$R/m1" >/dev/null || break; sleep 0.01; done
  [ -e "$R/m2/cost-$round" ] || { echo "no cost-$round"; break; }
  letter=$(printf "\\$(printf '%03o' $((65 + round)))")
  python3 -I -c "import os, sys; descriptor = os.open(sys.argv[1], os.O_WRONLY); os.pwrite(descriptor, sys.argv[2].encode(), int(sys.argv[3])); os.close(descriptor)" "$R/m2/big.bin" "$letter" $((SIZE_MIB * 1048576 / 2))
done
for _ in $(seq 1 3000); do grep -q "COST-END\|FAILED" "$R/crosvm.log" && break; sleep 0.1; done
grep -aE "COST|FAILED" "$R/crosvm.log"
```
