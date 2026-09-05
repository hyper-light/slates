# Architect's running notes (inputs to SLATES_DESIGN.md)

Working notes kept while reading the research. Each block records what a source settles for
slates, what it leaves open, and what it gets wrong for a hermetic in-memory VFS. These notes are
inputs, not decisions; decisions land in `SLATES_DESIGN.md` Part 3 with citations.

## From survey-hecate.md (read in full 2026-09-03)

Carry over verbatim in spirit:
- Process law: one decision per exchange worked to settlement; research before argument with
  receipts inline; every design examined at laptop and fleet scale where the laptop is the derived
  N=1 of one formula family ("no modes, ever"); constants derive from anchors with the derivation
  at the definition site; floors ratchet from the first CI baseline; observe-mode-first for any new
  influence; overrules are recorded, not argued away; a stale gap ledger is itself a gap.
- Vocabulary to reuse: volume (declared object: identity, role, version domain, access mode);
  attachment (the per-(consumer, volume) control object created at bind that pins the version,
  holds the lease, runs prefetch, carries accounting; "a view never changes without a re-attach");
  failure-domain tree (laptop = depth-one tree); epoch scope (the fencing authority lives in the
  smallest domain containing every legal holder); provenance class (host-observed vs
  client-reported); trace / span / trace context (three-id law: request_id pairs a response,
  trace_id+span_id thread an operation, caused_by is lineage); Masked / Degraded / Refused as the
  three cells of the fault × obligation matrix; AbsenceIs { Degraded, Unknown } with
  `(value, freshness)` always delivered together; "loud degenerate"; tripwire; ratchet; THIN.
- Memory doctrine: `Arc`/`Rc` denied workspace-wide by lint except named FFI edge modules;
  intra-component references are generational handles (u32 index + u32 generation) into
  owner-managed arenas; stale handle = typed error; shared-immutable fan-out via explicit
  acquire/release counts stored as data in the owning arena (auditable, single-threaded); buffers
  move across shards, loans within a shard; no std HashMap in component state (determinism);
  no-panic law with `panic = "abort"` and typed allocation exhaustion.
- Runtime doctrine: N shards, one pinned OS thread per shard, single-threaded executor with a FIFO
  ready queue and hierarchical timer wheel; tasks never migrate; cross-shard is move-only over
  bounded SPSC/MPSC channels; no untracked tasks; completion-shaped driver seam (io_uring with
  linked SQEs on Linux, kqueue on macOS, IOCP on Windows); cancellation is a request with
  guaranteed completion; deterministic SIM driver (seeded RNG, logical ticks).
- Store doctrine: one content-addressed chunk store per node; BLAKE3 the single hash family;
  content-defined chunking with derived min/avg/max; files at or below the minimum are one chunk;
  memory is O(unique bytes) node-wide; snapshot = manifest reference (O(1) to take); manifest
  versions are the invalidation unit; inode = (volume, path-entry) allocated monotonically at first
  lookup, stable for the volume's lifetime, generation-guarded; digest exposed as an xattr only
  when clean; range reads first-class; RO layers answer writes with a typed EROFS-equivalent.
- Cache-coherence posture: immutable layers get infinite entry/attr TTLs plus explicit
  invalidation; the sole-writer mutable volume runs in writeback mode.
- Wire doctrine: one value, one encoding (canonical); schema hash in the envelope; append-only
  evolution enforced at compile time; cross-version decode by ancestor hash, never tolerant
  reading; request_id distinct from trace context; fencing tuple on every message; credit-based
  flow control with absolute offsets; no class's latency bound may contain a term from another
  class's object size or queue depth; per-path MTU derived, never a 1500 constant.
- Consensus doctrine: pure algorithmic core `step(Message) -> {outbound, to_append, state_delta}`
  with logical ticks and injected randomness; entries-then-hardstate; no message before covering
  durable state; PreVote and CheckQuorum always; ReadIndex reads; explicit conf-change activation;
  exemplars (etcd-raft as extended by cockroachdb/raft, tikv/raft-rs) never dependencies; the
  known bug record ships as an executable conformance suite; laptop = 1-replica self-ack group on
  the same call path; replica count derives from the failure-domain tree.
- Skills doctrine: one typed Rust definition is the single authoring surface; it generates MCP
  tools, a `skill://` instructional resource (the raw SKILL.md plus supporting files,
  content-digested), registry document, and dispatch glue; façade-first (one skill, one closed
  Action enum); omission is the strongest gate; progressive disclosure.
- Documentation skeleton (the "COLLECTOR bar"): role; the whole machine in plain terms; terms;
  data model with ownership facts; per-component state machines and the tick; architecture map;
  networking hop by hop; boot order; mechanics; failure and recovery matrix; refusal and loss
  taxonomy (closed); derived constants (Constant | Formula | Anchors); worked example including a
  failure; laptop degenerate ("Same code, zero modes."); integration list; acceptance criteria
  (# | Criterion | The failure it catches); test matrix (Test | Asserts | Catches).

Left open by hecate, so slates must decide (and the survey's numbering):
1. host-OS mount technology per OS; 2. attachment state machine and detach ordering; 3. volume
provisioning, directory home, retention, deletion; 4. dynamic sizing; 5. page pinning, hugepages,
NUMA, memory bandwidth anchors; 6. sub-100 µs provisioning (no precedent in the corpus);
7. shared-mutable volumes (hecate: "RWX does not exist"); 8. hard links; 9. async client SDKs and
an MCP server; 10. the laptop-collapse map for the VFS.

Where hecate is wrong for slates (survey §8.4), with my disposition:
- "Witness" defined by fsync: slates redefines acked-means-committed-in-memory (and replica-acked
  where a replica exists); power loss is a designed Degraded cell, not a bug.
- RAM/NVMe "two tiers of one store": dropped; the arena is the store; exhaustion is typed.
- Hashing on the write path: hash lazily (snapshot/archive/transfer/dedup pass), never per write
  on the 50 µs path; the content identity of a dirty file is "unknown" until sealed.
- Chunk-size anchors: re-derive from page size, cache line, memcpy and hash throughput, and the
  measured file-size distribution, not from a device.
- Shared slab pages mapped to consumers can leak foreign volumes' bytes: any zero-copy/mmap
  exposure must be per-volume page-granular; dedup shares chunks by reference, not by mapping.
- "Lease" needs one meaning in slates: an ownership lease with an epoch fencing token.
- Thread-per-core vs kernel entry points: FUSE/NFS/WinFsp requests arrive on threads we do not
  own; the design must route them to the owning shard by handle without an extra copy, or make
  the bridge worker itself the shard.
- Encryption × dedup hardenings: out of scope for a hermetic in-memory store; keep only "an id is
  never a bearer capability".

Numbers hecate recorded (for cross-checking research, all tier D until confirmed by a paper):
shm/mmap 4.7–5.3M msg/s (~0.2 µs RTT) vs pipes 162k / UDS 130k (~6.2–7.7 µs) vs TCP loopback
70k (~14.2 µs); Dapper ~200 ns span create/destroy; macOS F_FULLFSYNC 17–24 ms.

## From survey-sylk-vfs.md (read in full 2026-09-03; 497 lines, §0–§8)

What sylk actually is (so the design does not repeat it):
- No volume abstraction. One real in-memory CoW filesystem (`purevfs.Workspace`: path-copied
  directory tree + HAMT inode map, one snapshot per mutation, O(1) branch forks) over a 256-way
  hash-sharded content-addressed ChunkStore and an anonymous-mmap size-classed slab arena. Every
  layer above it (PipelineVFS, ReplicaVFS, green, broker scratch) stores whole-file byte slices and
  copies on every hop, 3–5× per write.
- "Memory-only" is a configuration: sixteen distinct disk-touch sites (image import scans and
  SHA-256-hashes the whole repo; PipelineVFS/FileAccess fall through to os.*; semantic and control
  WALs and the flusher write under StorageRoot/AllowDiskExport). The type system permits host I/O
  everywhere.
- Content CoW is one chunk per write with no re-chunking; refcounts are never released in
  production; snapshots and journals are never reclaimed; memory grows monotonically.
- Mounting is per command: bazil or hanwen go-fuse under /dev/shm with bubblewrap on Linux; cgofuse
  over macFUSE or FUSE-T under /Volumes with sandbox-exec on macOS; cgofuse over WinFsp on a drive
  letter with a restricted token and job object on Windows. Path-hash inode numbers (rename changes
  the inode, hard links impossible), forced direct I/O (no shared mmap, no page cache), no symlink,
  xattr, fsync, statfs; argv/env/cwd string rewriting on macOS and Windows; no crash cleanup; no
  long-lived attach.
- Merge/commit: one MergePipe goroutine does byte-level operational transform on whole-file diffs
  into green, semantic WAL bump, FIFO commit queue with fsync per transition, resolver, then a
  whole-overlay disk flush. Rejected heads block the queue. CopyAt replays the entire WAL per call.
- Agent API: FileAccess (no rename, copy, symlink, ranged I/O) exposed as workspace_read and
  workspace_write verbs with soft not-found, 2-minute write leases carrying a staleness reason,
  DAG-ordered batches of at most 64 ops. Nothing for provision, attach, detach, archive, destroy,
  quota.
- Concurrency: coarse mutexes on every hot path, reads that take write locks, mutexes held across
  fsync; a real data race on broker handle reads; fifteen races/limits catalogued.
- Performance: zero recorded VFS benchmark numbers; about forty magic constants (64 KiB chunks,
  256 shards, 1 GiB image cap, 2 GiB budget, 128 MiB read cache, 100 ms attr TTL, 2 min lease,
  64-deep channels, 16-slot slabs). SHA-256 per write alone exceeds a 50 µs budget.

KEEP for slates (with the sylk evidence line): persistent per-mutation snapshots with structural
sharing; hash-prefix-sharded content-addressed chunks ("shard count = cardinality of the index
byte" is the right derivation); off-heap size-classed slab arena with lazily populated anonymous
mappings; extent lists with blob/zero kinds and adjacent-extent merging; generation-fenced write
sessions (one atomic load per write); three-state overlay resolution (hit / shadowed-by-delete /
miss) and union-find compression of abandoned layers; WAL shape as an in-memory operation log
(fixed header, CRC per entry, truncate at first corruption, monotonic seq across compaction,
majority-rule compaction); one serializer per mutable target and a write-ahead state-machine
queue; named retention holders with a water line and fixpoint GC; host-derived limits and hard
failure instead of spill; hierarchical reserve/resize/release memory governance; soft
`missing:true` results, directory-read degradation, structured truncated/unavailable flags,
leased write bases with explicit staleness reasons, dependency-layered batches; mutation-origin
tagging (ephemeral build noise vs authored content); per-agent serialization of mutating tools;
tracked task scopes.

AVOID: whole-file byte slices as the unit of storage in any layer; one chunk per write; refcounts
never decremented; coarse locks; disk fall-through in the types; per-command mounts at fixed
locations; three FUSE libraries; path-hash inodes; forced direct I/O; truncated op set; argv/env
rewriting; byte-level OT with a no-op conflict resolver; blocked FIFO on rejection; whole-overlay
flushes; O(repo) provisioning; flat absolute-path maps as the directory index; legacy layers kept
alive; env-var budgets; timeouts as coordination; dropped events by design; finalizers as leak
nets; heap-backed arena on Windows; dangling slices after arena close.

IMPROVE (direction the design takes): provisioning as pointer work from a pre-sized pool (fork =
copy one root handle + bump an epoch); chunk-granular CoW with re-chunking on write and lazy fast
hashing; persistent ordered directory maps and offset-keyed extent trees; epoch reclamation of
snapshots; shared-nothing one-owner-task-per-volume with lock-free readers on immutable roots;
one long-lived mount per host with volumes attached as subtrees, stable inodes, full POSIX op
set, kernel cache kept coherent by explicit invalidation; commits as root pointer swaps and merges
as explicit three-way merges with surfaced conflicts; archive as a sealed snapshot streamed to the
client; bounded volumes reserve at provision, dynamic volumes grow in measured slab increments;
explicit lifecycle verbs plus a POSIX-complete verb set; every constant derived and every
benchmark recorded.

## From survey-hyperscale.md (read in full 2026-09-03; 1,033 lines, §0–§8)

Patterns to PORT into slates' server/cluster (each with the hyperscale evidence and the reason):
- Lifeguard-correct SWIM core: direct probe → k indirect proxies → SUSPECT → DEAD; suspicion
  timeout `max − (max−min)·log(C+1)/log(K+1)` with the originator's own vote excluded; refutation by
  incarnation bump; gossip priority dead/leave > suspect > alive/join; λ·ln(n+1) rebroadcasts;
  MTU-bounded piggyback; peer confirmation before suspicion (a peer can only be suspected after one
  successful bidirectional exchange); UNCONFIRMED lifecycle with role-specific passive timeouts;
  gossip-informed callbacks fired once on the NOT-DEAD → DEAD edge; single source of truth for node
  state with incarnation conflict rules; timers that never reschedule on confirmation (one timing
  wheel; adaptive polling); LHM ±1 scoring with a bounded multiplier and event-loop lag feeding it;
  prob-OR composition of uncertainty signals (the multiplicative form "blew up > 90×").
- The continuous direct-probe budget `[base, 3·base]` derived from n, LHM, peer load, and
  reliability is the one already-measured parameter; the template for slates' rule.
- Pre-vote; term-as-fence; monotone heartbeat sequence with the leader-granted lease length;
  deterministic tiebreak (higher term wins, equal term lower address); quorum from configured
  membership never the live count; fence tokens on every mutation (reject lower, accept equal;
  do NOT accept arbitrary higher without replicated lease state).
- Log framing `CRC | len | LSN | clock | state | type`, group commit with per-write completion,
  bounded queue with graduated states, recovery that stops at the torn tail; deterministic apply
  layer (timestamps minted by the leader into the entry; no wall clock, randomness, or I/O in
  apply; byte-equal replicas proven by a replay test).
- Priority admission with a never-shed control class and a bounded control reserve; per-destination
  queues so a slow peer cannot head-of-line block a fast one; CONTROL/DISPATCH/DATA/TELEMETRY
  message taxonomy; hybrid overload detection (EMA delta + absolute rails + resources; the only
  self-calibrating mechanism); correlation-aware eviction holding (many peers failing at once means
  network, not death); three-signal health (liveness / readiness / progress).
- Idempotency key `{client}:{sequence}:{nonce}` with PENDING coalescing on one future so retrying
  agents get at-most-once provisioning; TTL must exceed the client retry window.
- Testing discipline: injected clock and randomness seams; lint ratchets (no raw task spawn, no
  direct time/random, no direct disk I/O); continuously polled invariant checker with a
  safety/liveness split and a diagnostic snapshot dumped before the violation is raised;
  composable fault matrix over an injected transport (partition, delay, drop, bandwidth, reorder,
  duplicate, reset, kill/pause/resume); genuine-cancel discrimination; terminal-abort barriers;
  sub-quantum deadline epsilon; nine standing invariants (at most one leader per domain, monotonic
  fence tokens, acknowledged work reaches a terminal state, cancelled work frees resources within
  budget, available + reserved ≤ total, member-count convergence, cluster-id isolation).
- AD-52 target concepts: deterministic bootstrap list hashed and compared; joint consensus for
  every membership change; learners promoted at a lag threshold; a fence header validated before
  any handler runs (wrong cluster / stale member / stale term / stale membership); tombstone before
  remove; ephemeral node ids ("restarts are new joins, never rejoins"); watch streams with a resume
  ring; disconnected mode with bounded-staleness reads and linearizable cancel.

ADAPT: every one of ~140 constants becomes a measured derivation (the survey's §8.4 gives the
formula sketch per row: period from RTT p99 and the target detection latency; k proxies from
per-link loss; suspicion bounds from gossip convergence; gossip λ from measured convergence;
piggyback budget from per-path MTU; lease = k × heartbeat p99 with renewal at lease/3; election
timeout ≥ 10 × broadcast RTT p99; queue capacities by Little's law from measured arrival and
service rates; backpressure thresholds from projected time-to-full vs measured drain latency;
retention from measured rejoin p99; idempotency TTL from measured client retry window). Election
quorum must use configured size. Per-job Raft groups → one replicated log per shard. Disk WAL
tiers → replication quorums. Text-delimited wire and cloudpickle → fixed binary schema. Hash ring
MD5/150 virtual nodes → stable fast hash with node-count-derived virtual nodes and lease records
in the replicated log. Vivaldi kept with measured reference RTT.

AVOID: executor threads and lock-around-await; fire-and-forget tasks and swallowed errors (557
`except: pass` sites, 47 raw create_task); disk-backed anything; duplicated contradictory configs;
monotonic timestamps compared across hosts; wall-clock LWW; unbounded containers; the VSR sketch
(no view-change quorum, swallowed prepare timeouts, seq reset race); "accept any higher fence
token"; fixed 15-minute reaps and 2 s control-plane polls; cloudpickle.

The "HLC" in hyperscale is a Lamport clock with a wall annotation; slates uses a real HLC
(max(physical, remote) merge with a bounded drift) or leader-minted timestamps only.

## From survey-vorpal.md §3–§9 (read 2026-09-03; 1,524 lines total)

Allocation (vorpal's measured practice, to reuse):
- jemalloc is the binary's allocator (never the library's), gated `not(any(msvc, aarch64-musl))`,
  compiled-in conf `narenas:8,dirty_decay_ms:0,muzzy_decay_ms:0` exported as `_rjem_malloc_conf`;
  measured on the Linux tree: default malloc 2.05 GB peak → 1.13 GB, equal wall time. Batch runs
  turn decay off (2.25 M soft faults / 10.6 s sys → 0.47 M / 6.6 s); long-lived daemons keep default
  decay so idle memory returns. jemalloc 5.3.1 vendored to fix a `MALLCTL_ARENAS_ALL` out-of-bounds
  crash, with a sync ledger row. The allocation ledger (opt-in feature) counts 444 M allocator
  events per kernel build; 32-way `repr(align(128))` sharded counters because "the measurement
  must not manufacture the contention it measures".
- For slates: the daemon is a long-lived process with pre-sized arenas, so the global allocator
  matters only for the cold control plane; the hot paths must not allocate at all. The "fault
  economics" pass is the template for measuring first-touch page-fault cost at boot (feeds the
  pre-fault policy). Keep an `alloc-ledger`-style opt-in feature that is never a default.
- `crates/mem` shape to reuse: probe (page size via sysconf, THP state, hugetlb pools, NUMA nodes)
  → pure `decide_page` policy function testable on every platform → store (anonymous mmap with
  MAP_HUGETLB via log2 size, madvise hints that are never correctness inputs) → zero-copy typed
  views with validated bounds/alignment → per-worker bump arenas reset per batch → prefetch hints.
  Huge pages Linux-only ("macOS Apple Silicon has no superpages"); Windows page size is an honest
  constant 4096 in vorpal (slates must query GetSystemInfo instead).
- Every literal is a named const with a doc comment stating the measurement and the env override;
  "Constants become policies … Derivations must be deterministic (seeded, pure functions of input)
  and stamped into provenance."

Lints/unsafe/error policy: clippy `-D warnings` workspace-wide with a three-item commented allow
list; `unsafe` per edition-2024 idiom with `// SAFETY:` on every block; typed errors never
panics; `expect()` only for statically impossible failures; `String` errors across thread
boundaries.

Cross-platform patterns: no per-platform modules or build.rs; paired `#[cfg]` functions with
identical signatures so call sites are unconditional; pure decision logic cfg-free and tested on
every host; SIMD behind runtime detection cached in OnceLock with a scalar arm always present and
bit-exact tests; `libc` only under `cfg(unix)` owned by one crate (`mem`); single `extern "C"`
symbols declared locally rather than adding deps; no rustix/windows-sys direct deps (slates will
need windows-sys for WinFsp, IOCP, named pipes, VirtualAlloc, WaitOnAddress — a deliberate
departure, documented). i686 is build-only in vorpal with 32-bit ceilings enforced at the format
level and NO `target_pointer_width` cfg anywhere; slates maps arbitrary user data so every
`u64 → usize` on a mapped size must be `usize::try_from`. Page size probed on Unix; `CachePadded`
(128 B on aarch64 and x86_64) or `repr(align(128))` for contended lines; on-disk/wire metadata
explicit little-endian with a big-endian decode fallback.

Formats and wire: INDEX_FORMAT's five policies (magic + u32 version per file; readers fail typed on
foreign versions; sidecars self-validating; docs table generated from source constants by an
assert-only test with an `--ignored regenerate` writer); `.vseg` container (page-sized header and
footer, 64-byte directory entries with reserved bytes, cache-line-aligned hot stripes, LE
metadata, blake3 over header and whole file at seal, zero-copy bytemuck views); wire = hand-encoded
16-byte LE header (magic, version, flags, channel, msg_type, len, checksum) + postcard bodies,
length checked before allocation on both ends, checksum before decode, `Incomplete` as control
flow, golden hash vectors, canonical JSON for anything digested.

MCP and skills: vorpal's MCP is a hand-written JSON-RPC 2.0 server over newline-delimited stdio on
serde_json (no rmcp), `handle_line` as a pure function testable in-process; protocol versions
["2025-06-18", "2025-03-26", "2024-11-05"] echoing the client's else oldest; tools only (no
resources/prompts); membership from one `Profile::allows` enum consulted by both tools/list and
tools/call so the advertised and callable surfaces cannot drift; results `{content,
structuredContent, isError}` with stable `structuredContent.code` and opaque cursor pagination;
child-process supervision for risky work; human-only enrollment of servable roots ("a
confirmation delivered through the MCP surface would be answered by the same agent that may have
been influenced"); `mcp install` writes Claude Code / Claude Desktop / Cursor / VS Code / Windsurf
configs idempotently with backups and `--dry-run`. Skills: nine `.claude/skills/<name>/SKILL.md`
with `name` + `description` frontmatter, usage line, tables, recipes, pitfalls, doc pointer; no
installer, no skills-over-MCP (slates must add both). Evals drive the installed server over stdio
against independent ground truth with contract checks, latency columns, and determinism gates.

Bindings: napi-rs 3.x with `AsyncTask` on the libuv pool via one generic boxed-closure Task,
`Async`-suffixed twins, sync methods for sub-millisecond reads, `napi-noop-in-unit-test` feature,
per-target npm packages (no Intel macOS), OIDC publishing, Node ≥ 10 floor (build with Node 24);
pyo3 0.29 with `abi3-py39` behind a `python` feature, maturin, `.pyi` + `py.typed`, awaitables via
a Rust-owned bounded thread pool + `loop.call_soon_threadsafe` (no tokio, no
pyo3-async-runtimes), `search_many` batching N jobs behind one Future with the one accepted
`Arc<Batch>`; wasm with exact wasm-bindgen pin and `Rc`.

Benchmarks and tests: no criterion/divan; release binaries under `/usr/bin/time -l` with the exact
command recorded, hardware and dataset commits stated once, load-average discipline (runs on a
loaded machine discarded, not averaged); measurement seams behind `bench-internals`; dated
"Pass N" write-ups keeping rejected experiments ("measured-and-rejected" is a first-class
outcome); no numeric perf thresholds in CI (only determinism gates and pinned retrieval floors).
Test kinds: oracle (serial spec vs parallel impl), non-vacuity counters, double-run determinism,
golden vectors, doc-truth generators, hostile-input parsers, env-gated differential tests that
skip loudly. No proptest/loom/miri in use (planned). Docs: short user docs in docs/, living design
docs in docs/wip with numbered load-bearing section ids cited from code, "Locked decisions" and
"Load-bearing invariants" up front, status blockquotes, ledgers for vendored drift.

Departures slates makes from vorpal, each for a stated reason: tokio nowhere (own thread-per-core
runtime; the brief forbids Arc-heavy stacks and needs completion I/O on three OSes); windows-sys
as a direct dependency (WinFsp, IOCP, named pipes, VirtualAlloc, WaitOnAddress have no std
surface); `cargo fmt --check` gate (no inherited code); loom/shuttle/miri as CI gates for the
lock-free cores (the brief demands ruthless correctness testing); i686 tested, not build-only,
because a VFS mapping user data on a 32-bit address space is exactly where truncation bugs live;
criterion-style microbenchmarks are still avoided in favour of recorded release-binary runs, but
the provisioning-latency histogram becomes a numeric CI gate (ratcheted from the first baseline)
because sub-50 µs is a requirement, not a datum.

## Amendments accepted 2026-09-04 (Ada)

- A-1: FSKit first on macOS 26+ (Swift shim forwarding FSVolume handler operations over the app-group-shared ring; per-volume `slates://` URL mounts; `Slates.app` bundle with the FSKit entitlement), NFSv3 loopback kept as the fallback for 14.4–15.x and as the differential oracle. Reason: with NFSv3 the coherence limits are Apple's kernel client and cannot be fixed; with FSKit the whole path is ours. Gate: the Phase 4 spike with measured latency, cache/invalidation behaviour, fsx, non-root mount, shim overhead.
- A-2: three state classes with three mechanisms (Mononoke/EdenFS shape): sealed content by WAL-first W-of-N quorum multiplex with a healer; pointers by the consensus group (range-sharded only past measured capacity); live state owner-local with auto-seal at a derived cadence, live op-log shipping opt-in. Dropped: primary-backup shard log as default, Raft per shard. Durability statement now names the per-volume loss window.
- Verified this pass: Linux MLOCK_LIMIT 8 MiB (kernel header), systemd DefaultLimitMEMLOCK 8M, Node 22 maintenance / 24 active / 26 current, libuv polls sockets only on Windows, napi_get_uv_event_loop under NAPI_VERSION >= 2, external buffers refused on Electron.
- A-4 (accepted 2026-09-04): disk is the source of truth. Volumes are scratch or overlays over a host directory; untouched entries served from disk on demand; copy-up records a witnessed base (fingerprint + BLAKE3); whiteouts and redirects; drift detected by fingerprints with git's racy-clean rule and reported, never absorbed; watchers are hints. `materialize` is the only disk-writing verb: manifest proportional to the delta, grant bound to the manifest hash issued by a human through the CLI or a confirmation surface (never MCP/SDK), single-holder landing lease, pure per-entry verdict (apply/skip/accept-identical/conflict; never merge), per-file compare-and-swap (exchange + verify on Linux/macOS, share-mode-guarded POSIX-semantics replace on Windows), zero-copy parallel write-back with an online concurrency ramp, data then directory syncs, witnesses advanced, audit log. New §4.15, D-25, D-26, crates `base` and `land`. Ada's framing, verbatim: "*Disk is the source of truth* period." and "The goal is to *only* write to disk on user permission grant."
- hecate documents read directly this pass (2026-09-04): `docs/adr/0005-canonical-rebase-merge.md`; `docs/specs/MERGE.md` §3 (two pure passes) and §8 (conflict windows); `docs/specs/SESSIONS.md` §1 (lineage, materialization lease), §3 (physical contract: materialized-iff-diverged, no reconcile), §5 (landing engine), §6 (review gates: prompt always, zero unresolved conflicts); `docs/specs/VFS.md` §3 (RWX does not exist). Takeaway: keep the verdict pure and the human gate non-negotiable; adapt the pinned baseline to a per-entry witness because slates' base is a live disk.
- Fetched this pass for A-4: rename(2) `RENAME_EXCHANGE` (3.15), open(2) `O_TMPFILE` (3.11), openat2(2) `RESOLVE_BENEATH` (5.6), ioctl_ficlone(2) (4.5), inotify(7) overflow rules, kernel overlayfs.rst (whiteouts, redirect_dir, offline lower changes undefined), kernel fuse-passthrough (CAP_SYS_ADMIN), git racy-git, macOS rename(2) `renamex_np`/`RENAME_SWAP`, fcntl(2) `F_FULLFSYNC`/`F_BARRIERFSYNC`, clonefile(2), getattrlistbulk(2), Apple FSEvents `MustScanSubDirs`, Microsoft ntifs `FILE_RENAME_INFORMATION` (POSIX semantics, RS1), `ReadDirectoryChangesW` overflow, `FSCTL_DUPLICATE_EXTENTS_TO_FILE` (ReFS).
- Still proposed, not accepted: the remainder of A-3 (fenced head records in the multiplex, hedged placement, pre-granted placement blocks, erasure coding) — `GAPS.md` D-O13.
- A-5 (2026-09-04): hecate's merge architecture brought in as D-27 and §4.16 with a new Phase 6 (old 6-8 renumbered 7-9). Kept: green as a chain of immutable versions written only by the merge task; constant-size increments naming sealed content; composed-net-ops deriver, never diff; one-directional position mapping with the composition law; two-pass verdict (sweep line, then memcmp for same-range candidates): Accept / AcceptIdentical / Conflict; placed before referenced; appliers recompute, mismatch fatal-and-loud; attach to versions, move only by `advance`; submission transaction with identity dedup, park/resume, piggyback refusals, single-flight refresh; byte-exact conflict windows; rebase as the only corrective path; streaming submission at auto-seal; hecate's M1-M17b as tests. Departed: proposer = leased, epoch-fenced standing writer on green's owner shard (slates has one pointer group; partitioned execution) with a tripwire; deriver composes declared ops only (hecate's SERVING/VFS say 'diff against prev_version', MERGE says never diff: a contradiction in hecate, resolved for never-diff); splice by extent surgery over fixed chunks; placement holders recompute; hard links/symlinks merged per path; evidence policy instead of Guardian/Arbiter; excluded subtrees instead of scratch volumes; no eg-walker (both sides declare ops on one chain; checkpoints make every base mappable). Gaps closed: journal now records byte ranges and per-inode versions; POSIX overwrites vs SDK inserts as distinct kinds; SDK `edit`; whole-file rewrites conflict conservatively unless identical; A-4's `rebase` renamed `rewitness`.
- Decisions 2026-09-04 (Ada): A-3 part 3 accepted (erasure coding as a measured cold policy; fragment record kind in the format now); TLS 1.3 ratified (D-O7 closed); no `CAP_SYS_ADMIN` ever, FUSE passthrough removed (D-O14 closed): "we need to be cross platform, and CAP_SYS_ADMIN requires root privileges we are not guaranteed to have access to ... a small, secure, localized server to handle the transactions" — which is the daemon plus the OS-provided brokers; landing-surface suggestions accepted (D-O16); D-O18 confirmed. A-3 parts 1 and 2 answered with the maximal solution (fenced per-volume registers; creator-owner local provisioning) as the A-6 proposal.
- Research pass for A-6 (2026-09-04, Ada: "Push harder. Do actual research"): read Vertical Paxos (Lamport, Malkhi, Zhou 2009) in full; RAMCloud SOSP'11 recovery paper and the 2009 position paper; Chubby OSDI'06 (sequencers); PNUTS VLDB'08 (record-level mastering, 85% write locality); Paxos Quorum Leases SoCC'14; fetched Ceph peering docs, BookKeeper protocol, Kafka replication design and KIP-101, CockroachDB leader-leases docs (default since v25.2), Hermes abstract, Copysets abstract (99.99% to 0.15%), FaRM SOSP'15 abstract (140M TATP tx/s, <50 ms recovery). Verdict: the maximal shape is Vertical Paxos II (configuration master = regional Raft group; owner = leader-acceptor; content write-all f+1; records majority of 2f+1; lease for local reads) with per-host copyset neighbourhoods of derived scatter width; per-volume random placement (A-6 v1) is withdrawn because of copysets. Recorded in `research/metadata-replication.md`.
- Six remainders of A-6 solved (2026-09-04, Ada: "Let's solve these"): hedged and tied content puts with recorded holder sets make straggler stalls impossible (Tail at Scale, read directly: 5% load at p95, 1,800 ms to 74 ms with 2% more requests); no global catalog (route by id to the owner; successor by rendezvous over the neighbourhood; CRUSH/PNUTS/RAMCloud precedents); one quorum rule (2f+1 candidates, commit at f+1, per-class send eagerness); cross-region durability as a per-operation await over epoch-ordered mirroring (Spanner 14.4 ms writes even within one datacenter; PNUTS hundreds of ms WAN; RAMCloud); ownership follows the writer automatically (PNUTS mastership migration; F1 leader placement); TLA+ models written in docs/wip/models (TLC run declined this session; JDK and tla2tools installed in Homebrew and the scratchpad).
- A-6 applied (2026-09-04, Ada: "Run both models, then apply A-6"): D-14 rewritten (Vertical Paxos II with copyset neighbourhoods; one quorum rule, 2f+1 candidates, commit at f+1, hedged content puts, recorded holder sets; configuration by consensus only; registers for heads/chains/leases/catalog under host epochs; takeover by epoch bump and batched phase one; route by id; per-operation durability scope over epoch-ordered mirroring; ownership follows the writer); §4.8 and §4.10 rewritten; Phase 8 rewritten with AC-8.12-8.17; D-O12, D-O13, D-O18 closed. Models: Reconfig passed (20,478 states); FencedRegister passed at two epochs (1,432,929 states); three-epoch run pending. Two modelling bugs fixed on the way (unfixed value per seq; over-strong Fencing invariant replaced by Continuity + StaleNeverCommits).
- v2 consolidation (2026-09-05, Ada: the design and plan needed combining after the merge engine was added): read all 3,614 lines front to back; removed amendment scaffolding from headings, labels and code comments; corrected four statements superseded by A-6 (merge records described as consensus entries or pointers in §2.3, §4.15's networking table, §4.16's role, ownership facts, record structs and failure matrix); rewrote the one-paragraph statement as four paragraphs; fixed Phase 5 task order (landing surfaces before API conventions) and Phase 9 acceptance order; renumbered Part 7's open questions to thirteen with closed items moved to GAPS; added a reading order note in Part 0 (§4.15 and §4.16 before the cross-cutting §4.13 and §4.14, numbering kept stable); prefaced the amendment log as history with the body winning on disagreement. Section numbering unchanged, so CLAUDE.md, GAPS and the research files still resolve.
- hecate documents read directly for A-5: `docs/specs/MERGE.md` (all sections), `docs/adr/0003-streaming-merge-gate.md`, `docs/adr/0005-canonical-rebase-merge.md`, `docs/specs/SERVING.md` §2-§4, `docs/specs/VFS.md` §5, `docs/specs/CONSENSUS.md` §6, `docs/architecture/LEDGER.md:218`.
