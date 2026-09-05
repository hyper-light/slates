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
6. Panics in non-test code: `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`, `unreachable!` (except `expect` with a message for a statically impossible failure, and the no-panic law applies: typed refusals instead).
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
- **Errors, not panics.** Every refusal is a typed variant of the subsystem's closed refusal taxonomy (§4.4, §4.8, §4.15, §4.16). An uncategorized refusal is a bug. `Result<T, TypedError>` everywhere; `String` errors only where a type cannot cross a boundary and the comment says why. Lock poisoning does not exist because locks do not exist on data paths; on cold paths recover, never propagate.
- **Ownership by handle, sharing by move.** Objects live in arenas and are named by generational handles; a stale handle is a typed miss, never a dangling pointer. Cross-shard work is a message on a bounded ring. Process-lifetime singletons are `&'static` through `OnceLock` or `Box::leak`, never `Arc`.
- **Hot atomics are cache-line padded** (`#[repr(align(128))]` or `CachePadded`), sharded per thread where contended, `Relaxed` for statistics and `Acquire`/`Release` for flags. vorpal measured four global atomics doubling kernel-scale CPU on ping-pong; that mistake is not repeated.
- **Platform code is paired `#[cfg]` functions with identical signatures**; pure decision logic is cfg-free and unit-tested on every host (vorpal `mem/src/policy.rs` is the model). SIMD sits behind runtime detection cached in `OnceLock` with a scalar arm always present and bit-exact tests.
- **Bounded work everywhere.** Every loop over user-scaled data is chunked into cooperative slices under the shard's per-iteration budget (destroy, large clones, landings). A task exceeding the budget is a counted bug signal.
- **Cancellation safety by construction.** Resources are owned by arenas keyed by handle and released by the owning operation's terminal step; dropping a future leaks nothing and corrupts nothing.
- **Integers are checked.** `u64 → usize` on any mapped or user-supplied size goes through `usize::try_from`; i686 is a tested target, not a build-only one. Every parser of external bytes checks length against the class's cap before allocating and verifies the checksum before decoding.
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
  - A seam with a simulated implementation as the oracle's other leg: `crates/vfs/src/host` (the read-only host trait, `SimHost` with outsider edits and a controllable clock) driven by `crates/vfs/tests/base.rs`; the real implementation lives in its own crate behind the lint wall (`crates/base`). Gotcha: the simulated host must model what the real one keeps alive (a replaced file's old inode behind an open descriptor), or the oracle certifies the wrong rule.
  - A gate that measures its own noise floor before judging: `destroy_rows` in `crates/vfs/examples/vfs_bench.rs` (scheduling jitter probed in-process, allocator time attributed per slice).
  - Crash injection at every instruction with a resume: `crates/land/tests/oracle.rs` (`t_1_15_crash_at_every_write_instruction_then_resume`: a reference run counts the write steps; one fresh scenario per step crashes there; every path must be old or new; the resume must reach the reference and a further plan must be empty). Gotcha: the reference must be built from a separate clean run, never from the crashed state; and an oracle host's own cost must stay flat, or the bench measures the oracle (the sim's descriptor lookup walked the tree; measured 813 µs, now 5.6 µs per entry).
  - A cross-process fixture as the test: `crates/anchor/tests/anchor.rs` re-invokes the test binary as the supervised child (an environment variable selects the role), so the handoff, the seqlock reads and the restart bound run across real processes. Gotcha: the child's exit code and beat count are shape constants the parent asserts on; keep both in one place.
  - Guard-then-apply for a replayed log: `crates/db/src/partition.rs` (`check` refuses before the append; `apply` is unconditional, so replay is deterministic) with `crates/db/tests/model.rs` (generated histories, the database dropped and recovered at random points, the recovered state compared whole). Gotcha: any decision taken inside `apply` from the clock or from state that a snapshot restores in a different order is a recovery bug; the model test's first run found one.
  - A paired-`#[cfg]` module with one seam and a cross-process test of the real thing: `crates/ipc/src/rendezvous.rs` (Linux abstract socket with `SCM_RIGHTS`; macOS and Windows bootstrap object with claim slots) driven by `crates/ipc/tests/rendezvous.rs` (the test binary re-invoked as the client). Gotcha: lint the other platforms' branches from this machine (`cargo clippy --target x86_64-unknown-linux-gnu`; Windows with `--no-default-features --features slates-machine/pure-hash`), or the CI lane is the first compiler to see them.
  - The write seam and its only real implementation: `crates/vfs/src/host/mod.rs` (`LandFs`) with `crates/land/src/os.rs`; every verb takes a handle, never a path, and the engine (`crates/land/src/engine.rs`) is written against the seam so the oracle runs it unchanged over `SimHost`.
- **Keep design docs in sync with code.** Changing a subsystem's behaviour updates its design section, its status blockquote and the ledger in the same commit.
- **Commit everything, in imperative area-prefixed summaries that carry the measured fact**, in vorpal's style: `merge: one graph tool over seven relations; deferred-schema cost measured`. Always `git add -A && git commit -m "<message>"`. Never `git checkout`, `git stash`, `git reset`, or any destructive git command.
- **Debugging protocol.** Add logging to file first; consult the design section rather than assuming from code; confirm scope, intended behaviour, timing and frequency with Ada; after root cause is confirmed by logs and analysis, write `docs/bugs/<date>-<slug>.md` with description, root cause, impact and exact edits; fix in plan mode; touch nothing beyond the fix, but report every sibling inconsistency found; never preserve the behaviour that caused the bug.
- **New functionality is piecewise**: the smallest, maximally correct, robust, performant piece that Ada can confirm directly. That is not a licence for hardcoded values, shortcuts, or deferred rigour; those violate the rules above.

## 7. Working with Ada

- Work serially. No subagents, no background jobs, no tool installs, no long computations, no system-state changes unless Ada asks for that specific thing; if a requested run can blow up in time or disk, bound it, keep its scratch output outside the tree, and stop it at the first sign.
- Answer the question asked; when challenged, give the maximal solution with evidence, not a hedge. Say "wrong" about your own earlier answer when it was.
- Every claim carries a number, a date, and a command or a citation. "From memory" is flagged as such and listed for verification.
- Plain English in every document and comment; no jargon that a careful engineer new to the project cannot act on.
