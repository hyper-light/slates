# slates — project rules for Claude Code

slates is a hermetic, purely in-memory, copy-on-write virtual filesystem service in Rust that
coding agents provision in under 50 µs, see as a normal path, merge through, and land onto disk
only under a human grant. It runs the same code on a laptop and across regions.

The design is the law. `docs/wip/SLATES_DESIGN.md` is the single unified design; its numbered
identifiers (rules R1–R10, decisions D-1…D-27, sections §4.1–§4.16, phases 0–9, acceptance
criteria AC-n.m, tests T-n.m) are load-bearing: code comments, commits and tests cite them.
`docs/wip/GAPS.md` is the gap ledger and is updated in the same change as anything it tracks.
`docs/wip/research/` holds the evidence behind every decision. Read the design section for the
subsystem you are touching before writing a line; do not guess at architecture from code.

The quality bar is `../vorpal`: measured claims, oracle tests, determinism gates, hostile-input
tests, typed refusals, module docs that state the design and its evidence, benchmarks as recorded
commands with hardware and dates, rejected experiments kept on record. Where vorpal enforces a
rule by habit, slates enforces it by a lint, a structural test or a CI gate. Where vorpal has a
gap, slates does not inherit it.

## 1. Locked rules and how each is enforced

| Rule | Enforcement (mechanism, not promise) |
|---|---|
| **R1 RAM only; disk is the source of truth; disk is written only inside a granted landing.** slates reads the directory a volume overlays and writes a host path only through `materialize` under a grant. Never `/tmp`, never on-disk sockets, symlinks or mount-point directories if any alternative exists. | `std::fs`, `std::net` and every file-creating syscall are denied by the lint wall outside `bridge-*`, `ipc`, `base` (read-only) and `land`. A structural test walks the dependency graph and fails the build if a write-capable file syscall links anywhere but `land`. The hermeticity tracer run (Part 6) proves zero writes outside granted targets. |
| **R2 No `Arc`.** `Arc`/`Rc` are runtime overhead; the only allowed sites are the three exceptions in D-8 (bindings objects a GC may drop mid-call, foreign APIs that take `Arc` by signature, test harnesses), each with a comment naming the two owners. | `clippy::disallowed_types` denies `Arc`, `Rc`, `Mutex`, `RwLock` workspace-wide; allowed sites are listed in `clippy.toml` per module with the reason. Shared references are generational handles; sharing is a move over a bounded channel or an epoch-published immutable root. |
| **R3 No magic numbers.** Every tunable is measured from the machine or the data and derived by a stated formula (D-11). | A numeric literal in a tuning position fails review and the literal check in CI; allowed ones carry `derived!("formula", anchors)` or a `/// Derived:` doc line; the boot profile logs every derived value with its inputs. |
| **R4 Maximal correctness, robustness, performance, scalability, efficiency, speed.** Complexity is accepted when the evidence justifies it and never weighed against those. | Every decision cites tiered evidence (A paper, B standard or vendor doc, C deployed code, D blog flagged, M measured here). Performance floors ratchet from the first CI baseline and only tighten. |
| **R5 Tests exercise use and functionality.** No test asserts a file exists, a constant equals, or an internal field. | The taxonomy in Part 6 has no such category; a test drives a mount, an SDK, MCP, the wire or the CLI and asserts on observable behaviour. |
| **R6 Everything async** where sensible: server, database, libraries, both SDKs. | The runtime is thread-per-core with completion drivers; every server and database operation is a future; SDK async is fd-readiness based; sync facades are thin wrappers only. |
| **R7 Plain English.** A human implementer can act on every sentence. | Module docs and comments are written for the next engineer, in prose, citing the design section. |
| **R8 Laptop ≡ fleet, one code path.** | Every subsystem has a laptop-degenerate section; the N=1 differential test in CI asserts identical semantics with one node and with the simulated fleet. No mode switches, ever. |
| **R9 Sub-50 µs provisioning.** | The provisioning histogram (p50/p99/p999/max, spinning and parked) is a permanent, ratcheted CI gate. |
| **R10 Disk writes happen only on a user permission grant; no privilege is ever required.** | Grants exist only through the CLI or a confirmation surface; the MCP and SDK channels have no grant verb (refused by kind). The daemon never requests `CAP_SYS_ADMIN` or root; the only privileged pieces are OS-shipped brokers installed once. |

## 2. Banned (explicit authorization required, per item, in the conversation)

Do not implement, add, suggest, or leave in place any of the following unless Ada explicitly
authorizes that specific item. Implied or assumed permission is not authorization. If you meet
one: STOP, ASK ("This would require X. Do you explicitly authorize it?"), WAIT, and if found in
the tree, flag it for removal.

1. `Arc`, `Rc`, `Mutex`, `RwLock`, atomics on a per-item path, or any lock on a data path (D-7, D-8).
2. tokio, async-std, smol, or any external async runtime anywhere; the custom executor is the only runtime (D-9).
3. `std::fs`, `std::net`, `tempfile`, `/tmp`, `mkdir`, symlinks, or any disk write outside the `land` crate under a grant (R1, A-4).
4. Any capability or privilege requirement: `CAP_SYS_ADMIN`, root, setuid helpers of our own, FUSE passthrough (D-O14 closed).
5. A hardcoded tuning number, timeout, size, threshold, percentage, or retry count (R3).
6. Panics in non-test code, **ever, with no exceptions**: `unwrap`, `expect` (a message does not make it acceptable), `panic!`, `todo!`, `unimplemented!`, `unreachable!`, `assert!`/`debug_assert!`, slice or map indexing that can go out of bounds (`v[i]`, `&s[a..b]`, string slicing), and arithmetic that can overflow or divide by zero. Every failure returns gracefully as a typed refusal that the caller handles; use `.get()`, `checked_*`/`saturating_*`, `try_from`, and `Option`/`Result` flow instead. Enforced by the lint wall (`unwrap_used`, `expect_used`, `panic`, `indexing_slicing`, `string_slice`, `panic_in_result_fn`, `unwrap_in_result`, … in `Cargo.toml`) and the xtask structural check.
7. A fallback that preserves legacy behaviour, a compatibility shim, a "just in case" second path, or a mode switch. Replace, do not layer.
8. Unbounded growth: an unbounded queue, cache, log, retry loop or task set; every structure has a derived bound and a typed refusal at it.
9. A lost or swallowed error, a fire-and-forget task, an orphaned future, a spawned thread without an owner that joins or cancels it.
10. Consensus, a lock service, or a coordination call on a per-write path; the configuration group is touched only on membership, takeover, neighbourhood and home changes (D-14).
11. An inferred merge (diff3, three-way text merge, OT, LLM resolution) anywhere; the verdict is accept, identical, or conflict (D-27, D-26).
12. A global catalog or index for lookups; ids route to owners (D-14).
13. Adding non-Rust tooling, a JDK, TLA+ tools, model checking, or any long-running job to the project's CI or to Ada's machine. The TLA+ models under `docs/wip/models/` are architecture artifacts re-run only by whoever changes §4.8, never by CI.
14. Subagents, background jobs, tool installs, or system-state changes during any task unless Ada asks for that specific thing.

## 3. Code shape (the vorpal habits, made mandatory)

- **Module docs state the design and its evidence.** Every `lib.rs` and every module opens with a `//!` paragraph naming the design section it realizes (e.g. `§4.5`, `D-6`), the invariants it holds, and the measurement or paper that justifies the shape. vorpal's `kg/src/ledger.rs` and `mem/src/lib.rs` headers are the model: what it is, why, what was tried and lost, and the number that decided it.
- **Every constant explains itself.** `///` on every `const`: what it is, its derivation or measurement, and its anchors. A bare number with no `///` is a review failure. (vorpal's code carries this on 12 of 568 constants and keeps the rest in its docs; slates keeps it at the definition site, every time.)
- **Unsafe is budgeted and only shrinks.** `unsafe-budget.toml` holds a per-crate ceiling on `unsafe` blocks, functions and impls; `cargo xtask unsafe` fails a crate over it and `--tighten` lowers it; raising one is an edit that names the new site and its reason. Reach for the safe wrappers first (`rustix` for syscalls, `memmap2` for maps, `RefCell`/`Cell` and `&'static` for single-threaded state, atomics for shared words); `unsafe` is for FFI without a safe wrapper (Apple sysctl, mach, IOKit, Win32), intrinsics behind runtime detection, the `RawWaker` vtable, and wrappers that are unsafe by signature (`kevent`, io_uring `push`). No `unsafe impl Send`/`Sync`: make the type safe to share instead.
- **Every `unsafe` block has a `// SAFETY:` line directly above it** stating the invariant that makes it sound. `clippy::undocumented_unsafe_blocks` and `unsafe_op_in_unsafe_fn` are denied. (vorpal covers about half of its blocks; slates covers all of them, by lint.) Miri runs the memory, wire and runtime-simulation tests in CI and locally (`cargo +nightly miri test`).
- **Errors, not panics — never a panic in production code.** No `unwrap`/`expect`/`assert!`, no indexing or slicing that can go out of bounds, no overflowing arithmetic: return and handle a typed error. Every refusal is a typed variant of the subsystem's closed refusal taxonomy (§4.4, §4.8, §4.15, §4.16). An uncategorized refusal is a bug. `Result<T, TypedError>` everywhere; `String` errors only where a type cannot cross a boundary and the comment says why. Lock poisoning does not exist because locks do not exist on data paths; on cold paths recover, never propagate.
- **Ownership by handle, sharing by move.** Objects live in arenas and are named by generational handles; a stale handle is a typed miss, never a dangling pointer. Cross-shard work is a message on a bounded ring. Process-lifetime singletons are `&'static` through `OnceLock` or `Box::leak`, never `Arc`.
- **Hot atomics are cache-line padded** (`#[repr(align(128))]` or `CachePadded`), sharded per thread where contended, `Relaxed` for statistics and `Acquire`/`Release` for flags. vorpal measured four global atomics doubling kernel-scale CPU on ping-pong; that mistake is not repeated.
- **Platform code is paired `#[cfg]` functions with identical signatures**; pure decision logic is cfg-free and unit-tested on every host (vorpal `mem/src/policy.rs` is the model). SIMD sits behind runtime detection cached in `OnceLock` with a scalar arm always present and bit-exact tests.
- **Bounded work everywhere.** Every loop over user-scaled data is chunked into cooperative slices under the shard's per-iteration budget (destroy, large clones, landings). A task exceeding the budget is a counted bug signal.
- **Cancellation safety by construction.** Resources are owned by arenas keyed by handle and released by the owning operation's terminal step; dropping a future leaks nothing and corrupts nothing.
- **Integers are checked.** `u64 → usize` on any mapped or user-supplied size goes through `usize::try_from`, so a width assumption cannot silently truncate. (Until 2026-10-20 this rule rested on i686 being a tested target; slates ships Windows, macOS and Linux on arm64 and x86_64 only, so the 32-bit lane and the i686 and Mac-Intel release targets are gone. The checked conversions stay — they are correct arithmetic, not a 32-bit concession.) Every parser of external bytes checks length against the class's cap before allocating and verifies the checksum before decoding.
- **Names are long and honest.** No `i`, `tmp`, `data`, `utils`, `helpers`, `manager`. A function does one thing; if cognitive complexity trips the clippy threshold in `clippy.toml`, split it.
- **Two-space indentation, edition 2024, MSRV 1.98 pinned exactly**, `panic = "abort"` in release, `cargo fmt --check` and `cargo clippy --workspace --all-targets -- -D warnings` in CI (Appendix B).

## 4. Tests (kinds you must use, and how they are written)

Every test is written as "do X, expect Y" and names the acceptance criterion or test id it serves
(`/// AC-1.7`, `/// T-3.4`) in its doc comment. Institutionalized kinds, from vorpal and beyond:

- **Model-based and oracle tests.** A serial specification implementation kept in the test module; the real implementation must equal it on every generated history (proptest with shrinking). vorpal's `pipeline.rs` serial reference table is the pattern.
- **Non-vacuity counters.** A fast path exports a counter the test asserts moved, "so a silently-dead reuse path can never masquerade as a passing oracle" (vorpal `walk_reuse.rs`). Every fast path in slates has one.
- **Determinism gates.** Build twice, compare identities byte for byte: seals, ops documents, archives, manifests.
- **Golden vectors** for everything hashed on the wire or in a format.
- **Hostile-input tests** on every parser of external bytes: `len = u32::MAX`, truncated headers, bit flips, foreign magic, overlapping ranges.
- **Differential tests** against the host filesystem (tmpfs, an APFS RAM disk, an NTFS RAM VHD) with the reviewed equivalence policy; against a pinned upstream where one exists; env-gated ones **skip loudly**, printing the skip and passing, never failing on machines without the pin.
- **Conformance suites** with reviewed expected-failure lists that only shrink: pjdfstest, fsx, fsstress in CI; xfstests and LTP nightly.
- **Concurrency**: loom on rings and handle cores, Miri on `mem`, `rt`, `wire`, shuttle nightly, TSan nightly. vorpal only planned these; slates ships them from Phase 0.
- **Simulation and chaos**: the deterministic driver with the nemesis library over the seed budget; the register invariants the design models proved (TotalOrder, Continuity, StaleNeverCommits, ReadSafety, NoLoss) encoded as history checks; linearizability and Elle nightly.
- **Doc-truth tests**: any table in the docs that could drift from source constants is asserted by a test with an `--ignored regenerate` writer; the test never mutates the tree in a normal run.
- **Workloads**: git, cargo, npm, python, rg, rsync, sqlite, editors, watchers, byte-identical to the host.
- Tests never write outside a RAM-backed temp directory named with the process id and removed at the end. Env mutation is `unsafe` with a SAFETY note. Hardware- and network-dependent tests are gated.
- **Bug fix = failing test first**, then the minimal change, then a sweep for sibling instances reported to Ada.

## 5. Benchmarks and measurement

- A benchmark is a recorded release-binary command with the hardware, the dataset commits, the date, the load discipline (quiescing, load average bound), and best-of-N with all N shown. `docs/wip/BENCHMARKS.md` follows vorpal's format: "Numbers are honest points, not marketing; the commands are the contract."
- Rejected experiments stay on record, dated, with their numbers ("measured-and-rejected"). A dead-even A/B does not land.
- CI gates on instruction counts (iai-callgrind); nightly gates on latency with change-point detection; the <50 µs histogram is a permanent ratchet.
- Nothing is probed by writing disk, at boot or in tests, except inside a granted landing (D-26).

## 6. Documentation and process

- Two tiers: `docs/*.md` short and user-facing; `docs/wip/*.md` the living design and measurement record. Numbered sections are identifiers; status blockquotes record implemented versus planned; the gap ledger is updated in the same change as any acceptance, closure or tripwire; the amendment log records every design change with an "Applied in the same change to" list.
- **Pattern by canonical example.** When a pattern has a best implementation in slates, name the file here rather than restating the pattern; record gotchas next to it.
  - Lint wall: `Cargo.toml` `[workspace.lints]` and `clippy.toml`; `cargo xtask check` (structural, literals, unsafe budget) in `xtask/src/`.
  - `derived!` and `Derived<T>`: `crates/machine/src/derived.rs`; used for a tunable in `crates/vfs/src/content.rs` (`chunk_bytes`, `inline_bytes`) and for a bench gate in `crates/vfs/examples/vfs_bench.rs` (`heap_budget`).
  - Paired `#[cfg]` platform modules with one seam: `crates/rt/src/driver.rs` over `kqueue.rs`, `epoll.rs`, `uring.rs`, `iocp.rs`, `sim.rs`.
  - Oracle tests: `crates/vfs/tests/model.rs` (the volume against a map model with POSIX rules, proptest state machine, refusals compared, both counters compared after every step) and the tree oracle in `crates/vfs/src/dirtree.rs` (splits, merges and copy-on-write epochs against an ordered map). Gotcha: the model must state the design's rule, not the implementation's (the chunk-window charge), or it certifies drift.
  - Measured-and-rejected record: `docs/wip/BENCHMARKS.md`, Phase 1 baseline table; every rejection names the number and the replacement in the same change.
  - A pure module with a reference applier: `crates/vfs/src/algebra.rs` (the interval algebra; the byte-level replay in its tests is the oracle, and a property that must hold by the design's words, such as commuting disjoint operations, is a named test).
  - A seam with a simulated implementation as the oracle's other leg: `crates/vfs/src/host` (the read-only host trait, `SimHost` with outsider edits and a controllable clock) driven by `crates/vfs/tests/base.rs`; the real implementation lives in its own crate behind the lint wall (`crates/base`). Gotcha: the simulated host must model what the real one keeps alive (a replaced file's old inode behind an open descriptor), or the oracle certifies the wrong rule; and what it reuses (ext4 hands a freed inode number to the next file: `SimHost::replace_file_with_number`, A-151). A real filesystem never stamps a birth time of zero, which hosts report for "none recorded", so the simulated clock's first instant is not one.
  - A gate that measures its own noise floor before judging: `destroy_rows` in `crates/vfs/examples/vfs_bench.rs` (scheduling jitter probed in-process, allocator time attributed per slice).
  - Crash injection at every instruction with a resume: `crates/land/tests/oracle.rs` (`t_1_15_crash_at_every_write_instruction_then_resume`: a reference run counts the write steps; one fresh scenario per step crashes there; every path must be old or new; the resume must reach the reference and a further plan must be empty). Gotcha: the reference must be built from a separate clean run, never from the crashed state; and an oracle host's own cost must stay flat, or the bench measures the oracle (the sim's descriptor lookup walked the tree; measured 813 µs, now 5.6 µs per entry).
  - A cross-process fixture as the test: `crates/anchor/tests/anchor.rs` re-invokes the test binary as the supervised child (an environment variable selects the role), so the handoff, the seqlock reads and the restart bound run across real processes. Gotcha: the child's exit code and beat count are shape constants the parent asserts on; keep both in one place. A re-invoked test binary prints libtest's banner on stdout before the role runs, so tag any line the parent reads (`crates/client/tests/reap.rs`), and look for the tag anywhere in a line: libtest prints `test <name> ... ` without ending the line, so the role's first line comes glued to it (`probe_map_in_container` in `crates/cli/tests/cli.rs`). To run the role inside a container image that lacks the binary's libraries, bind its loader and each library `ldd` names read-only and start the binary through that loader (`probe_binaries`, same file), so the lane pulls no other tooling.
  - Guard-then-apply for a replayed log: `crates/db/src/partition.rs` (`check` refuses before the append; `apply` is unconditional, so replay is deterministic) with `crates/db/tests/model.rs` (generated histories, the database dropped and recovered at random points, the recovered state compared whole). Gotcha: any decision taken inside `apply` from the clock or from state that a snapshot restores in a different order is a recovery bug; the model test's first run found one.
  - A paired-`#[cfg]` module with one seam and a cross-process test of the real thing: `crates/ipc/src/rendezvous.rs` (Linux abstract socket with `SCM_RIGHTS`; macOS and Windows bootstrap object with claim slots) driven by `crates/ipc/tests/rendezvous.rs` (the test binary re-invoked as the client). Gotchas: lint the other platforms' branches from this machine (`cargo clippy --target x86_64-unknown-linux-gnu`; Windows with `--no-default-features --features slates-machine/pure-hash`), or the CI lane is the first compiler to see them. And Windows keeps a kernel object's name while any process holds one and cannot unlink it (A-176): a name a successor makes again (a client's region, the bootstrap object) is refused while a client of the dead daemon still maps the old one, so name it afresh at every start (`region_name_key`) or decide its owner by a token only the owner opens (the rendezvous's `Owner`).
  - Shard-thread-local state served by a registered poller: `crates/server/src/state.rs` and `daemon.rs` (`serve_loop` idles with `futures::idle` and is woken by the runtime when a client ring has a slot; a client reaches its shard as a spawned task that moves the ring end in and wakes the loop). Gotcha: an in-task `futures::spawn` is joinable and keeps its arena slot after it ends until it is joined or detached, so detach what nobody joins (a shutdown releases such slots since 2026-10-06; before, it never completed).
  - A restart oracle over one segment: `crates/client/tests/client.rs` (`a_session_outlives_a_daemon_restart_and_its_retry_meets_the_completion_record`: the test is the anchor; two daemons in turn over one handoff; the client's reconnect count is the non-vacuity counter; the retry must return the original id). Gotchas: anything process-local (a runtime's shard ids) must never be persisted or routed by; the first run found volume ids carrying the runtime's shard id. And the test plays the whole anchor, the rendezvous too (`SegmentSource::Handoff`'s `rendezvous`, A-176): a test that held only the segment let each daemon make its own rendezvous, which hid on Linux and macOS what Windows refused.
  - A real-process CLI test with a guard: `crates/cli/tests/cli.rs` (`AnchorProcess` kills and reaps the anchor on drop, so a failed assertion leaves no daemon behind; the daemon's own departure after `SIGKILL` of the anchor is asserted through the instance answering exit 3). Gotcha: every CLI invocation is a new client on a new shard, so a rule kept per shard (a name's uniqueness) is not a rule; the first run created one name twice. And the tests share one process: every child a test spawns holds copies of every test's open descriptors from the fork until its `exec`, so an unmount right after closing files can meet a transient holder (`unmount_through_the_cli` retries the busy answer within `WAIT_FOR`).
  - The landing engine wired through the server with durable records off the ring: `crates/server/src/landing.rs` (the Phase 1 `slates-land` engine runs; grants/leases/landings/audit are §4.8 ops; the grant is created only on the control channel, never the ring — R10). Gotcha: a landing/grant/attachment id must route to the partition that holds its record (a volume-bound `Land` does; a bare id needs the owner in its high bits, as attachment ids carry).
  - A daemon-level benchmark belongs in its own quiesced lane, not the microbench omnibus: `crates/client/examples/provision_bench.rs` (it spawns a daemon; back-to-back with microbenches it measures contention). Gotcha: a size-independence or park-count AC must allow measured jitter (bootstrap-interval width; a spurious wakeup), not the bare timer.
  - An f-parameterized protocol whose laptop is the degenerate, with an N=1 differential: `crates/db/src/register.rs` (`Quorum`, `Fence`, `Configuration`; the commit rule tested at f=0 and a simulated f=1 with identical observable outcomes, AC-2.5). Gotcha: the f=0 case must be the same code with `f=0`, never a branch; and every reply carries `placed`/`mirror_age`/`host_epoch` from the first version so the fleet adds no interface.
  - A histogram bench that gates a floor on the uncontended case and ratchets the rest: `crates/client/examples/provision_bench.rs` (AC-2.1/T-2.6; the single-client spinning p99 is held to 50 µs, the 1/8/64 sweep is recorded, an oversubscribed run — more client threads than cores past the shards — is informational). Gotcha: a client thread spinning for its reply competes with the shards for a core, so gate only the concurrency the machine can run.
  - A verb's effect and its completion record as one durable atom: `crates/db/src/replay.rs` (`Db::begin`…`commit` write one `LogEntry`) with `crates/server/src/verbs.rs` (`run_recorded`). Gotcha: a `kill -9` between two separate log records is a durability hole; the transaction closes it, and a forwarded verb must record at its owner partition, not the origin.
  - Kick a shard only when it is parked: `crates/rt/src/registry.rs` (`kick_if_parked`) with the shard setting its parked flag and re-checking its inbox in `crates/rt/src/shard.rs` (`park`). Gotcha: the flag and the message must be sequentially consistent, or a wake is lost; a spinning shard needs no kick, which is the saving.
  - A readiness wait that names its own descriptor: `crates/rt/src/interest.rs` with `crates/rt/src/readiness.rs`
    (a registration hands the driver an interest word, and the shard's table marks which registration fired, so a task
    racing several waits learns which one happened; A-149). Gotchas: anything that turns a word into a wake goes
    through `ShardContext::deliver`, or an interest word is taken for a task's; and a loop that another source must
    interrupt (a revoke, a drain kick, a volume change) races that source explicitly beside its readiness wait
    (`crates/bridge-virtiofs/src/serve.rs` `doorbell_or_request`, `crates/server/src/fuse.rs` `wait_for_work`). A
    readiness future no longer answers on another source's wake, and the virtio-fs loop had leaned on it.
  - A publication's two encodings and the oracle that holds them equal: the struct builders (`to_image` in
    `crates/vfs/src/recover.rs`, the delta's `roots` in `crates/vfs/src/delta.rs`) and the streamed encoders the daemon
    actually writes (`encode_image_into`, `encode_delta`), compared byte for byte after every step by
    `check_publication` in `crates/vfs/tests/model.rs`. Gotcha: an oracle run at a degenerate parameter certifies
    nothing about it. With the inode reserve at zero the live and recorded counters coincide, and the streamed encoders
    shipped writing the live one while the builders wrote the reserved ceiling (A-150), so the model now runs with a
    nonzero reserve.
  - A liveness question asked on a cold path only: `crates/ipc/src/rendezvous.rs` (`Liveness`: the socket's peer on Linux, the bootstrap header's start stamp elsewhere) consulted by `crates/client/src/client.rs` after a reply has stalled past the derived deadline, never per request.
  - Property-test persistence without a disk: `crates/test-seeds` (`seeded(config, include_str!("<file>.proptest-regressions"))` or `unseeded(config)` in every `proptest!` block and hand-built `TestRunner`; `cargo xtask check` refuses one that bypasses it). A new failing seed is printed on stderr in the seed file's line format; review it and add it to the seed file. Gotcha: a test proving "nothing was written" must set `source_file`, or even proptest's default writes nothing and the test is vacuous.
  - A real VM as the test: `crates/cli/tests/guest_dax.rs` with `crates/guest-probe` (crosvm shares a `slates mount`
    into a guest whose static `/init` speaks a line protocol over the serial console; the initramfs is written into a
    slates volume, so nothing touches disk; gated on the environment, skipping loudly). Gotchas: crosvm's virtio-fs
    device runs unprivileged only with its sandbox on (`--disable-sandbox` makes it pivot without a user namespace, which
    needs root); and the workspace's musl rustflags leave the C runtime dynamic for the cdylibs, so the probe is built
    with `RUSTFLAGS="-C target-feature=+crt-static"`.
  - A copy handed to a same-kernel peer as its only capability, sealed and kept live: `crates/mem/src/dax_object.rs`
    (`PeerObject`: the owner's writable map first, then `F_SEAL_FUTURE_WRITE | SHRINK | GROW | SEAL`; `PeerView`: the
    peer's read-only view, loaded through atomics) with `crates/server/src/direct_map.rs` (a sliced fill under a
    deferred reply; the hand-off judged at `confined_for_a_peer`) and `crates/client/tests/client.rs` (the hostile peer:
    every write path refused with the seal's errno; the descriptor scan judged by inode, since the daemon runs in the
    test and its own descriptor of the copy is new too). Gotchas: the write seal refuses the owner's own hole punch, so
    zeroing is stores over `SEEK_DATA` runs; a read-only duplicate alone is no defence (`/proc/self/fd` reopens it
    read-write); and a slice over memory another process writes is unsound, so it is read through atomics (A-171).
    The copy's hold outlives its file: a destroy or a last unlink only detaches it, since the client's map keeps pages
    the daemon was charged for; only a release or the reap frees it (A-172). And a share per client seat is no share:
    `clients_per_shard` is itself the reserve over one ring region, so the reserve over it is about four regions on any
    machine; share the shard's capacity among the clients served now.
    macOS has no seals (A-173): the peer gets a second descriptor of an unlinked shared-memory object opened read alone,
    offered on the socket route (`crates/ipc/src/route.rs`, `crates/server/src/shards_route.rs`), and the access mode is
    the capability. Gotchas, each measured: hold every refusal to the read-only mode's own errno (`EPERM` map, `EBADF`
    write, `EACCES` turned map), since a read-write descriptor's write is refused too, `ESPIPE`, and XNU refuses any
    second sizing, `EINVAL`, whatever the mode; and an offer holds the object, so letting a copy go must withdraw its
    unclaimed offer (`direct_map::let_go`), or a map–release loop keeps live objects outside the share. To fail a
    hand-off for real, cap a re-invoked client's `RLIMIT_NOFILE` at its lowest free descriptor number
    (`crates/client/tests/reap_map.rs`): the kernel drops a passed descriptor (Linux) and refuses a new socket
    (macOS), so the client's give-back (A-174) runs where it would in the field.
    Windows passes no descriptor (A-175): the copy is a section under `D:P(A;;RC;;;OW)` and a read-only handle is
    duplicated into the client's process (`crates/ipc/src/handle_route.rs`). Gotchas, each measured: under a section's
    default descriptor a read-only handle's holder duplicates one that writes, so the descriptor is the confinement;
    the completion record must not keep the handle's value, since the client's handle outlives the daemon that made it;
    and the client binary's tests run one at a time on Windows CI: at 32 test threads 14 of 20 failed, and none one at a
    time. The cause is oversubscription. Each in-process daemon sizes itself from the whole machine, and Windows charges
    commit when memory is made. So thirteen daemons met the commit limit: the process aborted allocating 873,462,528
    bytes, and the shards spent over a second of their own CPU in one poll (GAPS).
  - The write seam and its only real implementation: `crates/vfs/src/host/mod.rs` (`LandFs`) with `crates/land/src/os.rs`; every verb takes a handle, never a path, and the engine (`crates/land/src/engine.rs`) is written against the seam so the oracle runs it unchanged over `SimHost`.
  - A per-shard singleton shared by the shard's tasks: `crates/rt/src/shard.rs` (`ShardContext::keep`/`keep_with` return a `Kept<T>` handle; `Kept::with` lends inside a closure only while its own context runs) with `crates/transport/src/demux.rs` (`DemuxId` is the handle; its receive loop awaits readiness outside the borrow). Gotcha: a `&'static` into state an owner frees is unsound however "only tasks hold it" — the type must confine the lend; and in edition 2024 an `impl Future` returned from `&self` captures the borrow unless it says `use<>` (`UdpSocket::readable`).
  - A detector's non-vacuity canary: `crates/mem/tests/race_canary.rs` with `xtask/src/tsan.rs` (the lane requires ThreadSanitizer's report from a deliberate race before it trusts a clean run of the suites). Gotcha: ThreadSanitizer v3 reports a race only while the earlier access is still in its thread's trace (three parts of 32,763 events by default) and otherwise drops it without a word, so the canary's first writer parks rather than polls between the writes; a polling first writer went unreported 2 times in 99 CI runs and 120 in 120 on a loaded Mac. The same bound holds in the suites: a race whose earlier access is further back than that is never reported.
  - A loop-responsiveness test that names its own holds: `crates/sdk-python/tests/test_sdk_async.py` (`Ticker`: a separate-process control, an event loop ticking as the ticker does, bounds the machine's own scheduling delay; an in-process sibling thread samples the loop thread's frame while a tick is overdue, so a tick is charged only for what held the loop — from the last moment the loop was seen idle in its selector, otherwise whole — and tells a hold that keeps the GIL from one that releases it; every run prints its figures; the harness's own first uses — spawns, the executor's first thread and its lazy imports — come before the ticker). Gotchas: a probe must wait through the primitive of what it is a control for. On macOS, under a launchd job's timer policy (CI's runner, kiln's agents), a 10 ms `nanosleep`, `select`, `time.sleep` or `park_timeout` comes back 76 ms late at the median while a kevent timeout comes back 1.2 ms late, and a `time.sleep` sampler had made that delay the bound (`docs/bugs/2026-10-26-the-async-sdk-tests-noise-was-another-waits-timer-delay.md`). And late is not held: an idle loop woken late by the kernel holds nothing. Nothing may create or join a thread inside the measured interval (`faulthandler.dump_traceback_later` starts a thread on every arm and joins it, holding the GIL, on cancel: 74.7 ms at worst per tick at load 15); and the first `asyncio.to_thread` imports `concurrent.futures.thread` on the loop's thread, which a slow CI disk turned into a 0.672 s hold.
  - A copy another process maps, kept equal by write-through and compared back at sync points: `crates/vfs/src/shards_backing.rs`
    (the store owns the copies; the volume's one write boundary writes every change into them before it returns) with
    `crates/server/src/shards/dax.rs` (the fill, the compare-back a copy slice a turn, the refill) and the oracle
    `crates/vfs/tests/shards_backing.rs` (A-163). Gotchas, each found by a test:
    - The compare-back cannot tell the other process's store from a stale byte, so every way the served bytes change
      without a write must reach the copy. Beneath an overlay that is the disk: the base plane marks backings stale
      where it adopts a change (`adopt_observed`), and a writable copy's file is copied up first.
    - A write-back must not be written through into its own source (`ShardsBackings::suppress`), or a newer store is
      overwritten with what the compare read.
    - A refused write-back must fail the sync that asked for it; it once answered `FSYNC` 0.
    - A reply that can reveal a change (a new size) waits until the copy shows it (`flush_unless_refill_owed`).
    - A bridge answers at most its read cap a call (the volume bridge's, 256 KiB), so a copy slice is read whole
      (`read_whole` in `crates/bridge-core/src/dax_backing.rs`): taken for the file's end, a capped call lost a guest's
      stores at the ingest and zeroed its map at the refill. The slice is derived from the machine, and every
      development machine derived one within the cap, so the oracle runs a whole range a slice too (`Slices` in
      `crates/server/tests/shards.rs`); only CI's runner had derived the regime that broke.
  - A foreign-thread framework served by a shard: `crates/server/src/winfsp.rs` with `crates/bridge-winfsp/src/host.rs`
    (WinFsp's dispatcher threads send each request over a bounded channel and wake the mount's task; an owned mount
    thread holds the file system; `crates/cli/tests/winfsp.rs` on CI's Windows runner). Gotchas, each found on the
    runner: WinFsp turns Windows' data cache on only when `FileInfoTimeout` is infinite (`src/sys/create.c`), so
    paging writes (`ConstrainedIo`, clip them to the file's size) and lazy write-back (`FlushAndPurgeOnCleanup`) arrive
    with it; without `UmFileContextIsUserContext2` every open is handed the first open's context, so per-open boxes
    are freed twice; and WinFsp's notification cannot purge a mapped file (A-177). jupiter has no WinFsp, so the live
    tests run on CI only: a request log on a diagnostics branch (never main) is how the runner is read.
- **Keep design docs in sync with code.** Changing a subsystem's behaviour updates its design section, its status blockquote and the ledger in the same commit.
- **Commit everything, in imperative area-prefixed summaries that carry the measured fact**, in vorpal's style: `merge: one graph tool over seven relations; deferred-schema cost measured`. Always `git add -A && git commit -m "<message>"`. Never `git checkout`, `git stash`, `git reset`, or any destructive git command.
- **Debugging protocol.** Add logging to file first; consult the design section rather than assuming from code; confirm scope, intended behaviour, timing and frequency with Ada; after root cause is confirmed by logs and analysis, write `docs/bugs/<date>-<slug>.md` with description, root cause, impact and exact edits; fix in plan mode; touch nothing beyond the fix, but report every sibling inconsistency found; never preserve the behaviour that caused the bug.
- **New functionality is piecewise**: the smallest, maximally correct, robust, performant piece that Ada can confirm directly. That is not a licence for hardcoded values, shortcuts, or deferred rigour; those violate the rules above.

## 7. Working with Ada

- Work serially. No subagents, no background jobs, no tool installs, no long computations, no system-state changes unless Ada asks for that specific thing; if a requested run can blow up in time or disk, bound it, keep its scratch output outside the tree, and stop it at the first sign.
- Answer the question asked; when challenged, give the maximal solution with evidence, not a hedge. Say "wrong" about your own earlier answer when it was.
- Every claim carries a number, a date, and a command or a citation. "From memory" is flagged as such and listed for verification.
- Plain English in every document and comment; no jargon that a careful engineer new to the project cannot act on.
