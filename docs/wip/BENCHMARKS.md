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
