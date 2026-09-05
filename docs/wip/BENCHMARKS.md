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
delta, with the ramp's settled depth) run only where `SLATES_TEST_RAMDIR` names a RAM-backed
directory (the Linux lane, `/dev/shm`); a macOS RAM disk is a system-state change Ada has not
authorized, so their first numbers are the lane's.

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

The Phase 2 task 5 run (2026-09-05, `cargo xtask ratchet` after the client and the CLI landed):
82 rows against the baseline, 0 regressions; no new rows, since the client's cost is the IPC
round trip already gated (`ipc.*`) and the CLI's is a process start plus one rendezvous. The
suites themselves are the facts of the day: `cargo test -p slates-client --test client` 1.2 s
for two daemons and a restart; `cargo test -p slates-cli --test cli` 1.2 s for a real anchor,
a real daemon, fourteen verbs through the binary, and the daemon leaving after the anchor is
killed (Apple M5 Max, macOS 26.4.1). The provisioning histogram (AC-2.1, T-2.6) is task 6's.
