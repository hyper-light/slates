# The gap ledger (authoritative, kept current in the same change as any acceptance or tripwire)

Rubric per item: research on file? spec section exists? test matrix? acceptance criteria? laptop
degenerate stated? open decisions named? Classification: `undesigned | designed-unspecced |
specced-untested | decision-open | drift (owed-and-forgotten)`. A stale ledger is itself a gap.

## 0. The one global fact

- Phase 0 is in progress (2026-09-04): the workspace exists with the lint wall, the structural
  test and the literal check (`cargo xtask check`); `slates-machine` measures the boot profile,
  `slates-mem` holds the arenas, slabs, handles and rings, and `slates-rt` runs the executor on
  kqueue, epoll or io_uring, IOCP and the simulation (BENCHMARKS.md records the baselines).
  `slates-wire` frames, checksums and canonically encodes with a compile-time schema hash.
  Phase 0's crates exist; Phase 1 is next; nothing below is closed by code until its phase says
  so.

## 1. Component inventory

| Subsystem (design §) | Research on file | Spec section | Tests named | ACs | Laptop degenerate | Status |
|---|---|---|---|---|---|---|
| Machine profile (4.1) | memory-and-system-awareness.md | yes | T-0.* | AC-0.3, 0.5 | yes | specced-untested |
| Memory (4.2) | memory-and-system-awareness.md; arc-free-rust-architecture.md | yes | T-0.1, 0.2, 0.4, 0.5 | AC-0.4, 0.5 | yes | specced-untested |
| Runtime (4.3) | low-latency-ipc-and-runtime.md; arc-free-rust-architecture.md | yes | T-0.3, 0.6-0.9 | AC-0.6-0.9 | yes | specced-untested |
| Volume lifecycle (4.4) | cow-data-structures.md; edenfs-scale-distribution.md | yes | T-1.*, T-2.* | AC-1.*, AC-2.4 | yes | implemented in `slates-vfs` (Phase 1 tasks 1–9, §8c); AC-1.1, 1.3–1.8 gated; AC-1.2 waits on the Linux tmpfs lane |
| Namespace and content (4.5) | cow-data-structures.md | yes | T-1.* | AC-1.* | yes | implemented (§8c): inline small directories, block tree, chunk windows, epoch histogram accounting; base plane and landing still specced-untested |
| Bridges (4.6) | os-filesystem-bridge.md (§7-§8 FSKit) | yes | T-3.*, T-4.* | AC-3.*, AC-4.* | yes | specced-untested; macOS FSKit gated by the Phase 4 spike |
| IPC (4.7) | low-latency-ipc-and-runtime.md | yes | T-2.* | AC-2.1-2.3, 2.6 | yes | specced-untested |
| Database, registers and configuration (4.8) | database-design.md (§7); metadata-replication.md (A-6) | yes | T-2.*, T-8.* | AC-2.*, AC-8.* | yes | specced-untested; the register and reconfiguration protocols are model-checked (`models/`, see §10) |
| Wire (4.9) | database-design.md; survey-hecate.md; survey-vorpal.md | yes | T-0.8, T-8.* | AC-0.8 | yes | specced-untested |
| Distribution (4.10) | edenfs-scale-distribution.md; metadata-replication.md | yes | T-8.* | AC-8.* | yes | specced-untested |
| Compression/archive (4.11) | compression-archive-dedup.md | yes | T-7.* | AC-7.* | yes | specced-untested |
| Agent surfaces (4.12) | mcp-skills-sdks.md | yes | T-5.* | AC-5.* | n/a | specced-untested |
| Security (4.13) | mcp-skills-sdks.md; survey-hecate.md | prose | T-2.7 | — | n/a | designed-unspecced |
| Observability (4.14) | survey-hecate.md §5 | prose | — | — | n/a | designed-unspecced |
| Base plane (4.15, D-25) | disk-source-of-truth.md; edenfs-scale-distribution.md | yes | T-1.10-1.13, T-3.10-3.11, T-4.11-4.12 | AC-1.9-1.11, AC-3.8-3.9 | yes | specced-untested |
| Landing (4.15, D-26) | disk-source-of-truth.md; survey-hecate.md | yes | T-1.14-1.17, T-2.10-2.12, T-3.12, T-5.9-5.10, T-8.10 | AC-1.12-1.14, AC-2.8-2.10, AC-4.10, AC-5.7-5.8, AC-8.10, AC-9.6 | yes | specced-untested |
| Merge engine (4.16, D-27) | merge-engine.md; survey-hecate.md | yes | T-1.18-1.19, T-6.1-6.14, T-8.11 | AC-1.15, AC-6.1-6.12, AC-8.11 | yes | specced-untested |

## 2. Decision-open (named owners: the phase that closes each)

- D-O1 compio audit (custom executor is the plan of record) — Phase 0.
- D-O2 `LocalWaker` stabilization on Rust 1.98 — Phase 0.
- D-O3 macOS unprivileged RAM disk for the mount point — Phase 4.
- D-O4 macOS attribute-cache timeout derivation — Phase 4.
- D-O5 FUSE-over-io_uring mixed-size buffers on target kernels — Phase 3.
- D-O6 Erasure coding of chunks for memory-bound fleets — DECIDED 2026-09-04 (A-3 part 3 accepted): a measured cold-content policy; the fragment record kind is in the format now (§4.11); Phase 8 measures the class boundary, (k, m) and reconstruction cost.
- D-O7 TLS 1.3 versus Noise between hosts — RATIFIED 2026-09-04: TLS 1.3 via rustls; Noise is not pursued.
- D-O8 FSKit as a future macOS bridge — superseded by D-O9.
- D-O9 FSKit-first on macOS 26+ with NFSv3 fallback (Amendment A-1) — ACCEPTED 2026-09-04 and applied; the Phase 4 spike records the numbers and the macOS 15.x path.
- D-O10 Replication recast as quorum-multiplexed sealed content + consensus pointers + owner-local live state with auto-seal (Amendment A-2) — ACCEPTED 2026-09-04 and applied.
- D-O11 (opened 2026-09-04) macOS 15.4–15.x path: RAM-disk block resource for the FSKit module versus the NFS fallback — Phase 4 spike.
- D-O12 (opened 2026-09-04) Pointer-group sharding threshold — CLOSED 2026-09-04 by A-6: there is no pointer group; the configuration group commits only on failures and moves. The auto-seal cadence constants are still measured in Phase 8.
- D-O13 — CLOSED 2026-09-04 by A-6: heads and chains are fenced registers in the Vertical Paxos II form; hedged placement adopted; pre-granted placement blocks unnecessary; erasure coding accepted earlier. Original text of the item: volume heads as fenced single-writer records in the content multiplex (consensus only for membership, placement and leases); hedged placement N > W for pointer records; pre-granted placement blocks so fleet `create` stays local; erasure coding as a measured policy for cold sealed content. Its first item (POSIX-native durability points at `fsync`, `snapshot`, `detach`, `archive`, all in RAM) is subsumed by A-4's `fsync` wording. Owner: Phase 8, decided by Ada.
- D-O14 (opened 2026-09-04) FUSE passthrough for untouched base files — CLOSED 2026-09-04: not used; slates never requires `CAP_SYS_ADMIN` or root beyond the OS-provided brokers installed once (`fusermount3`/user namespaces, the FSKit extension, the WinFsp driver); the daemon copy path is the only base read path.
- D-O15 (opened 2026-09-04) The landing fallback where the target filesystem lacks an atomic exchange (verify-then-rename-over with a reported window) versus a staging-directory strategy — Phase 1 measures the window; Phase 4 measures per platform.
- D-O16 (opened 2026-09-04) Landing filters — DECIDED 2026-09-04: the confirmation surface offers suggestions the human toggles (ignore-file-aware); the agent's filter stays explicit; a toggle produces a new manifest hash (§4.15 step 2). Phase 5 builds it.
- D-O17 (opened 2026-09-04) Conflict rate from whole-file tool rewrites versus SDK `edit` operations: if the measured share of conflicts caused by whole-file rewrites on shared files exceeds the operator SLO, reopen the ergonomics (a declared-edit bridge path for editors; stronger skill guidance) — Phase 6 measures.
- D-O18 (opened 2026-09-04) Merge proposer authority: slates departs from hecate's leader-fused proposer (one shared pointer group; partitioned execution) and uses a consensus-issued lease with an epoch check at commit; CLOSED 2026-09-04 by A-6: the owner is the distinguished proposer of its own registers under its host epoch, so no leader-versus-leaseholder split can exist; a resumed stale owner is refused at the first holder (model-checked as StaleNeverCommits and Continuity).

## 3. Undesigned (charter only)

- Security spec (principals, access lists, audit counters) — CLOSED 2026-09-05 by A-8 (§4.13's specification).
- Observability spec (span roster, health signal catalog, metric names) — CLOSED 2026-09-05 by A-8 (§4.14's specification).
- Skills content (the seven SKILL.md documents, including `slates-landing` and `slates-merge`) — owed in Phase 5 and Phase 6.
- The confirmation-surface contract for harnesses other than the terminal (the request stream a harness renders, answered only by a human-operated process through the control channel) — owed in Phase 5 with the terminal surface as the reference.
- Operator documentation (fleet configuration: failure-domain tree, regions and mirror regions, neighbourhood sizing inputs, certificates) — owed in Phase 8.

## 4. Drift (owed-and-forgotten)

- None. A-1, A-2, A-4, A-5 and A-6 were applied to every affected section in the same change (see the amendment log in SLATES_DESIGN.md for the lists), and the v2 consolidation of 2026-09-05 integrated them into the body text: the amendment tags left the headings and labels, four statements that still described merge records as consensus entries or pointers were corrected to ledger-register entries (§2.3, §4.15, §4.16), the Phase 5 task order and the Phase 9 acceptance order were fixed, and Part 7's open questions were renumbered with the closed ones recorded here.

## 5. Residual literals against the derivation doctrine

- Ratified in Phase 0 (2026-09-04), each at its definition site with a `Shape:` doc line that
  the literal check reads (`crates/machine`): the Kalibera-Jones stopping width (one tenth of the
  median, `stats::CONVERGED_WIDTH_PERMILLE`), the bootstrap resample count (1,000,
  `stats::BOOTSTRAP_RESAMPLES`), the smallest accepted sample (16, `bench::MIN_SAMPLES`), the
  per-probe wall bound (250 ms, `bench::PROBE_WALL_BUDGET`; the whole profile took 472 ms on the
  M5 Max, BENCHMARKS.md), the timer-overhead factor (100, lmbench's one-percent rule,
  `bench::TIMER_OVERHEAD_FACTOR`), the fault probe's region (256 base pages,
  `probes::FAULT_REGION_PAGES`), the full-matrix core limit (32, from §4.1's text), the wake
  probe's convergence batch (64), the zstd candidate levels (1, 3, 9, 19), the cache-line fallback
  (128 B, only when the OS refuses), the hash corpus (1,024 base pages, the large chunk class) and
  the codec corpus (64 base pages, the small class), and clippy's cognitive-complexity threshold
  (10, `clippy.toml`). Every other number in the crate is `Format:` (a layout fact) or derived.
- Ratified in Phase 0 for the runtime and the wire, each at its definition site: the registry's
  shard bound (1,024, `rt::registry::MAX_SHARDS`), the timing wheel's shape (6 levels of 64 slots,
  `rt::timer`, a `Format:` because it fixes the deadline arithmetic), the events drained per driver
  wait (64, the kqueue, epoll and IOCP drivers; it bounds latency, not correctness), and the
  wire's header layout, class words and schema-hash constants (`Format:`). The runtime's tick,
  step budget and ring size come from the profile; the batch bound is one ring until the per-item
  cost is measured (§4.3), which `RuntimeConfig::from_profile` says in its formula string.
- "10 × broadcast RTT p99" for election timeouts is Raft's published rule; ratified as a shape
  constant with the citation.
- The format floor for compression (savings must exceed the chunk's metadata overhead) is
  derived from the format, not a literal.
- The racy-window timestamp granularity per base filesystem (§4.15) is a cited table keyed by
  the filesystem type the OS reports, ratified as a shape table; each row must carry its
  citation and Phase 1 verifies it per filesystem.

## 6. Fit-before-influence milestones

- Phase 0 baseline (first CI run on the reference machines) precedes every ratchet.
- Prefetch and dedup policies run observe-first until the measured sample counts are reached.

## 7. Armed tripwires (metrics must exist from day one)

- Provisioning p99 (spinning) exceeds the ratchet → reopen D-9/D-10.
- Rename rate high enough that per-volume serialization is visible → reopen D-7 (shared index escape hatch).
- Volume skew across shards → reopen D-7 (Silo-style shared index).
- Loss windows exceeding the operator SLO, or put quorums frequently unreachable → reopen D-14 (make live shipping the default for the affected class).
- Pointer commit rate approaching group capacity → engage the measured sharding (D-O12), not a constant.
- FSKit spike fails its go criteria → NFSv3 remains primary on macOS and D-2 is reopened next macOS release.
- NFS fallback coherence test fails at the derived `actimeo` → reopen the fallback's cache posture.
- Hashing backlog persistent → reopen D-6 (hash-on-seal policy).
- Drift checks per second exceeding the measured `stat` capacity of a base (stat storms) → reopen the check cadence in §4.5 (hint-driven checks only, or a coarser listing fingerprint).
- Watcher overflow rate above the operator SLO on a base → reopen the watcher strategy (fanotify mount marks on Linux; the USN journal on Windows).
- Large-class copy-up cost or descriptor use beyond its derived budget → reopen the copy-up class boundary (D-6, §4.5).
- Landings falling back to rename-over (no exchange) above a measured fraction → reopen D-O15 (staging-directory strategy).
- Merged listing cost on the largest base directories above the readdir latency budget → reopen the listing cache (§4.5).
- Any write by a slates process outside a granted target in the tracer → stop the release; it is a rule violation, not a tripwire.
- `StaleEpoch` refusals outside an observed takeover or migration → a fencing or membership bug; fatal in CI, alarm in production (never a tripwire to tune).
- The configuration group's commit rate above its near-zero baseline outside failures and moves → something has put consensus back on a per-write path; investigate before anything else.
- The hedge rate above its derived cap for a class → the p95 estimate or the neighbourhood is wrong; probation and neighbourhood change first, then re-derive the cap.
- The copyset count above its bound at any configuration → a placement bug, fatal in CI.
- `mirror_age` above the operator's mirror SLO → alarm; `await placed(mirror)` callers see it as `NotPlaced{mirror}` at their deadline.
- p99 intervening deltas per merge above the checkpoint spacing's design point → re-derive the checkpoint spacing; a rising conflict rate with base lag → tighten the stream cadence for that green (§4.16).
- Rebase-retry rate on one path region above the derived threshold → the harness is told (contention control lives above the engine, as in hecate MERGE.md §10); slates never serializes work by itself.
- Holder recomputation mismatch anywhere → not a tripwire: a bug; fatal in CI, alarm in production.
- Merge-path p99 or verdict p99 change-point → nightly gate failure (Part 6).
- Destroy slices past the step budget, the clock-check allowance and the measured jitter (the shard's watchdog count, §8c) → a release unit whose cost the weights do not see; re-derive `release_weight` before touching the budget.
- Heap per file above the counted-object budget of the Phase 1 bench at any tree size → an object the formula does not name; add it to the formula, never to the slack.

## 8. External dependencies and port hazards

- WinFsp (GPLv3 with FLOSS exception or commercial license) installed by the user on Windows.
- Linux kernel features by version (5.10 baseline; 6.1; 6.14).
- macOS 14.4 minimum (`os_sync_wait_on_address`); macOS 26 for FSKit URL resources; Apple Developer ID signing, the FSKit entitlement, and an app group for the macOS bundle; Apple's yearly FSKit protocol changes.
- C toolchains for `zstd-sys` on all nine targets.
- The merge engine (A-5) depends on nothing external: fixed-layer ops documents use `slates-wire`; the fleet parts use the consensus group already chosen. Read directly from hecate on 2026-09-04: `MERGE.md`, ADR-0003, ADR-0005, `SERVING.md` §2-§4, `VFS.md` §5, `CONSENSUS.md` §6; hecate's own contradiction on the deriver (`SERVING.md`/`VFS.md` diff versus `MERGE.md` never-diff) is recorded in `research/merge-engine.md` §2 and resolved for never-diff.
- Base and landing primitives (A-4): Linux filesystems with `RENAME_EXCHANGE` and `O_TMPFILE` (ext4, XFS, Btrfs, tmpfs; others fall back); `openat2` (5.6, under the floor); FUSE passthrough only with `CAP_SYS_ADMIN` (6.9+); macOS `RENAME_SWAP` and `clonefile` by volume capability (APFS); Windows 10 1607+ NTFS for POSIX-semantics rename; ReFS for block clone. Items marked "verify" in `research/disk-source-of-truth.md` §7 (batched `statx`, `NtQueryDirectoryFile` classes, `FlushFileBuffers` on directories, fanotify marks, reparse-tag checks, the timestamp-granularity table, `FSCTL_SET_SPARSE`, `F_PREALLOCATE`) are owed verification in Phase 1 and Phase 4.

## 8a. Phase 0 audits (task 6)

- compio (audited 2026-09-04 from its `master` sources): `compio-runtime` holds `Rc<Executor>`,
  `Rc<RefCell<Proactor>>` and `Rc<RefCell<TimerRuntime>>`, and is a thread-local runtime that a
  user assembles into thread-per-core; `compio-driver` stores every operation in a
  `ThinCell<RawOp<dyn Carry>>` (a reference-counted cell, one heap allocation per operation) and
  hands out `std::task::Waker`s from the proactor. That is a reference count and an allocation on
  the request path, which D-8 forbids, and there is no seam for FUSE-over-io_uring or our rings.
  Result: not adopted; the custom executor of `crates/rt` is the plan of record. Re-check per
  release only if compio publishes an allocation-free operation path.
- `LocalWaker` on Rust 1.98.0: still nightly-only (`local_waker`, #118959). The executor uses
  `Waker` with a vtable that is thread-safe by construction over a `Copy` word; `clone` and `drop`
  are no-ops, so nothing is lost. Revisit when it stabilizes (a `ContextBuilder` change only).
- io-uring crate 0.7.14 (tokio-rs): thin syscall wrapper, no reference counting in its core types;
  adopted for the Linux driver with the probe-and-fall-back sequence of D-9.
- Miri: ships only with nightly, which this machine does not have. Installing nightly is a tool
  install and needs Ada's explicit authorization (asked 2026-09-05); until then CI's
  `miri-and-loom` lane runs it on nightly for the `slates-mem` and `slates-wire` unit tests
  (`slates-rt`'s unit tests open a kqueue or an eventfd, which Miri does not model). Local runs
  are loom-only.
- Instruction-count gates (D-20, "iai-callgrind in CI"): iai-callgrind needs valgrind, which is
  non-Rust tooling and therefore banned from CI and this machine without Ada's explicit
  authorization (asked 2026-09-05). The ratchet that exists is `cargo xtask ratchet`
  (`ratchets.toml`): wall-time ceilings keyed by machine identity, hierarchical over runs the
  way Kalibera and Jones prescribe (a ceiling is the highest upper edge across three runs; a
  check fails only when the lowest lower edge across three fresh runs lies above it), tightening
  only; 22 rows recorded on the M5 Max on 2026-09-05 and proven to fail on a planted ceiling.
  Its resolution is the machine's between-run drift, which it prints: on this laptop up to 40%
  on rows under 100 ns (frequency and thermal state between processes) and a full step on the
  1–2 ns header rows (nanosecond quantization), under 3% on rows above a microsecond. That is
  the case for the instruction-count gate: it sees a 1% change the wall clock cannot.
- Cross-target checks: the four shipped crates lint clean for `x86_64-unknown-linux-gnu` and
  `x86_64-pc-windows-msvc` from this machine (the targets were installed; the C dependencies are
  off for those checks behind `slates-machine`'s `codecs` and `pure-hash` features), and CI's
  `cross-lint` lane repeats it. This caught two real defects on 2026-09-05: three Windows imports
  behind an unrequested `Win32_Security` feature and a working-set call in the wrong module.
  Nothing on Windows or Linux has *run* yet: that needs those machines (Phase 1's reference
  boxes).

## 8b. Unsafe, Miri and instruction counts (2026-09-05)

- Unsafe surface, measured by `cargo xtask unsafe` (blocks, functions and impls in shipped
  sources, comments and tests excluded): 161 mentions and 8 `unsafe impl` before the reduction,
  76 sites and 0 `unsafe impl` after. What did it: the rings hold atomic words instead of
  `UnsafeCell` slots (zero unsafe, loom still explores every interleaving); the shard's mutable
  state sits in a `RefCell` with refused, counted nesting instead of a raw pointer behind a flag;
  shard contexts, pair rings, registry entries and the simulation's shared state are leaked
  process-lifetime objects reached through plain `&'static` references; control messages (spawn,
  cancel, shutdown, active) ride a bounded standard channel instead of pointers packed into ring
  words; drivers are built on their own shard's thread from a `Send` seed instead of being sent
  across; `rustix` (the design's syscall surface) replaces raw `libc` for everything it wraps
  and `memmap2` replaces the raw maps, advice and locks. What remains, per crate, is listed with
  its reason in `unsafe-budget.toml`: FFI without a safe wrapper (Apple sysctl, mach, IOKit,
  Win32), the CRC32C intrinsics, the `RawWaker` vtable, and wrappers that are unsafe by signature
  (`kevent`, io_uring's `push`, the file-backed map of the profile segment). The budget only
  tightens. The reduction cost nothing measurable after one recovery: the wall-clock ratchet
  caught the idle step rising from 30 to 37–45 ns (a borrow per phase, a channel poll per step)
  and the step is back at 22–30 ns with one borrow before the polls and one after, the registry
  entry cached, and the control channel polled only behind a pending flag (BENCHMARKS.md).
- Miri (nightly `miri 0.1.0 (0ed41eb414 2026-09-04)`, authorized and installed 2026-09-05):
  `slates-mem` 28 tests and `slates-wire` 19 tests pass with the leak check on; `slates-rt`'s 12
  unit tests and 2 simulation tests pass with `-Zmiri-ignore-leaks`, because the runtime leaks
  its contexts, rings and registry entries on purpose (that is what makes their references
  `&'static` without unsafe code). Tests that need `sysctl`, `mlock` or a kqueue are marked
  ignored under Miri (3 in `mem`, 3 in `rt`). No undefined behaviour was found. CI's
  `miri-and-loom` lane runs the same commands.
- Instruction counts (D-20): `benches/callgrind.rs` in `mem`, `rt` and `wire` under iai-callgrind
  0.16.1, run by CI's `callgrind` lane on Ubuntu with valgrind (authorized 2026-09-05); valgrind
  has no port for macOS on Apple silicon, so the lane is the only place they run. The benches
  compile here (`cargo bench --workspace --bench callgrind --no-run`). The lane prints the counts;
  the comparison against a recorded baseline is the next step once the first run exists.
- New dependencies, accepted for the unsafe reduction: `rustix` 1.1 (the syscall surface named in
  the IPC research §2.4), `memmap2` 0.9 (maps, advice, locks), `toml` (xtask only),
  `iai-callgrind` (dev only; pulls `proc-macro-error2` 2.0.1, which rustc warns will be rejected
  by a future version — a dev-only build dependency, tracked here until iai-callgrind drops it).

## 8c. Phase 1 volume core record (2026-09-05)

What landed: `slates-vfs` (tasks 1–9 of Phase 1): the copy-on-write namespace (radix-16 inode
trie, directory nodes with an inline two-entry form and a copy-on-write B+-tree of 4 KiB
slotted blocks in a store slab beyond it), content as chunk windows (open page-multiple extents
sealed into chunks, copy-on-write per window, holes uncharged), snapshots and clones by birth
epoch with deadlists and a pruned destroy walk, exact accounting as a histogram of content
bytes by birth epoch (`referenced_bytes` is its total, `unique_bytes` its suffix past the newest
shared epoch), bounded and dynamic quotas with pressure events, the op log with a byte budget,
name folding without allocation, the executable model with proptest state-machine tests, the
edge and fault tests, and the baseline bench with its three acceptance gates.

Gates in place: AC-1.1 (the model over 10^6 generated operations, counted, `cargo test -p
slates-vfs --release --test model -- --ignored ac_1_1` in CI), AC-1.3 (snapshot and clone cost
flat from 10^3 to 10^6 files within the timer's resolution), AC-1.4 (five nodes copied for a
create five levels down; one extent copied for a write; one page for a fresh window),
AC-1.5 (heap per file against the counted-object formula at 10^3, 10^5, 10^6), AC-1.6 (inode
numbers never reused, kept by snapshot and clone), AC-1.7 (both counters equal the model's
after every generated step), AC-1.8 (destroy of 10^6 files in clock-cut slices; none past the
budget plus the clock-check allowance plus the measured jitter); T-1.1, 1.2, 1.3, 1.4, 1.5,
1.7, 1.8, 1.9 as named tests; T-1.6 in its one-shard form (a generated interleaving of two
clones; the shuttle form arrives with Phase 2's threads).

Task 14 (the deriver) landed 2026-09-05: the interval algebra as a pure module
(`crates/vfs/src/algebra.rs`: a content map of base and new runs, every declared operation a
splice, hunks as the unique minimal edit against the surviving base runs, 2,000 generated
histories against a byte-level reference applier, disjoint operations proven to commute); the
SDK `edit` on the volume (delete then insert with true positions, journaled as such; the tail
is rewritten, the zero-copy splice is Phase 6's); the deriver (`crates/vfs/src/derive.rs`: the
journal since the base snapshot folded into per-inode maps, the touched paths resolved in both
trees, a state delta over paths with base references, sorted, encoded little-endian, identified
by BLAKE3); a file inode's home (parent and name hash) so a written inode's path costs no walk;
`readdir_in`, `lookup_in`, `resolve_in`, `stat_in`, `readlink_in` reading a snapshot as it was
(`lookup_in` had resolved to the head's node: a latent bug, fixed). Gated: T-1.18 (300 random
histories over random bases: net-apply reproduces the head's files, symlinks and directories
byte for byte; every hunk inside its sources; deriving twice gives the same bytes), T-1.19
(truncate-and-write and write-and-rename give one hunk of the base length and the new length
and byte-identical documents), AC-1.15 (a fixed history's identity
`11476fb81b5bc32d474f28cd2afd2f65b1609c7dc070e1ade41f3c5df4d955c4` pinned for every lane). The
document keys content by post-state path with an explicit base reference rather than by inode,
so a rename over a base path and a rewrite in place read the same; hard-linked inodes are
listed at every path (conservative, as D-27 says).

Task 10 (the base plane) landed 2026-09-05: the read-only host seam (`crates/vfs/src/host`)
with opaque handles, bulk listings carrying fingerprints, `O_NOFOLLOW` opens and watcher hints;
`SimHost`, an in-memory host with outsider edits, a controllable clock and watcher overflow, the
disk leg of the (disk, overlay, witnesses) oracle; the volume's base plane (`crates/vfs/src/base.rs`):
merged lookups and listings validated by the directory's fingerprint on every use, base
entries given inodes on first touch and dropped when the disk loses them, copy-up by size class
with the racy rule, whiteouts and redirects journaled, drift checked first on what the held
descriptor serves (an in-place change marks the body lost and reads refuse with `BaseDrift`)
and then on the path (deleted, replaced, retyped: reported, still served from the held inode),
`read_base`, `rewitness`, `pin`, `status`, hints and overflow re-checks, the diverged set over
loaded nodes; and `slates-base` (`crates/base`), the operating-system host: descriptor-relative
rustix calls on Unix, inotify on Linux and `EVFILT_VNODE` on macOS behind the seam, a
path-relative standard-library form on Windows. Gated: AC-1.9 (one open and one node at 10^3,
10^5 and 10^6 files, and over the workspace's own tree), AC-1.10 and T-1.10 (150 generated
histories of agent and outsider moves over random bases, the diverged set, the drift list and
every readable file compared after each step), AC-1.11, T-1.11, T-1.12 (40,000 entries), T-1.13;
the host's own tests over `crates/` and, in the Linux lane, over tmpfs (descriptor semantics,
`O_NOFOLLOW`, hints). Baselines in BENCHMARKS.md (Phase 1 baseline: the base plane).

Deviations and owed items from task 10:
- The Windows host is path-relative through the standard library and reports no watcher
  (fingerprints alone, the failure matrix's Masked cell); the directory-handle form with
  `FILE_FLAG_OPEN_REPARSE_POINT` opens and `ReadDirectoryChangesW` arrive with the Windows bridge
  (Phase 4). Its timestamp granularity is the table's coarsest until the volume is queried
  through that handle. Compile-checked in the cross-target lint lane; not run here.
- Listings on macOS use `getdents` plus one `statat` per entry (3.4 µs per entry measured);
  `getattrlistbulk` is the design's bulk call for the platform and its gain is owed as a
  measurement before Phase 3's bridge, where listings sit on the `readdirplus` path.
- A large-class copy-up hashes the whole file for its witness identity (6.8 ms measured on a
  file of a few megabytes); D-6's tripwire on large-class copy-up cost stands, and a lazy
  identity (hashed at seal or landing) is the change it would trigger.
- `slates-base` carries two `unsafe` sites (rustix's `kevent`), budgeted.

Tasks 11–13 (the landing) landed 2026-09-05: `slates-land` (`crates/land`), the only crate
that links a write-capable syscall (the structural test's allow-list): the manifest with its
canonical encoding and BLAKE3 hash (`manifest.rs`: creates, replacements and deletes with their
witnessed base, directory renames as one rename, directory creates, recursive removals, a
`Clear` for a base directory the overlay removed and recreated opaque, symlinks; the filter;
the summary), the pure verdict of §4.15's table (`verdict.rs`, every row and every conflict
class in one table test), in-process grants and the single-holder lease (`grant.rs`; a session
grant covers later landings of the same volume into the same target), the online ramp policy
(`ramp.rs`), the state machine (`engine.rs`: present with a preliminary verdict pass, grant,
lease, capability probe inside the granted target, validate, sweep, write by class, sync,
advance, report, with an audit ring), and the write seam over the operating system (`os.rs`:
`O_TMPFILE` linked through `/proc/self/fd` on Linux and hidden-name temporaries elsewhere,
`renameat2(RENAME_EXCHANGE)` and `renameatx_np(RENAME_SWAP)`, `fdatasync` and
`F_BARRIERFSYNC`, `F_FULLFSYNC` as the media barrier, `futimens`, `fchmod`, containment by
`O_NOFOLLOW` per component with the ownership check). The seam gained the write verbs
(`LandFs`) and `SimHost` implements them with crash injection at every write instruction,
a switch for the exchange and one for unnamed temporaries, and a count of every seam call.

Gated (`crates/land/tests/oracle.rs`, over `SimHost`): the worked example of §4.15 with both
of its failures (a conflict refused with nothing written, then `read_base`, rewrite,
`rewitness`, a new manifest; a compare-and-swap lost to an outsider, exchanged back, `Undone`,
the report `Partial`, the outsider's bytes kept, the entry still in the overlay); AC-1.14 (the
sixteen entries take the same seam calls over 10^3 and 10^5 base entries); T-1.14 (eight
seeded runs of random outsider rewrites in both forms, every loss detected at the swap, none
silently applied); T-1.15 and AC-1.13 (a crash at every one of the writer's 96 write
instructions over a delta with every action class: every path old or new after each, the
resume with the same landing id sweeps the siblings, reaches the reference disk, and a
further plan is empty); T-1.16 (no exchange: verify-then-rename, the window in the outcome,
`NoExchange` reported, an outsider edit still refused at the verify); T-1.12 (the 40k-entry
directory: one `Clear` and two creates, exactly two entries after); stage-and-exchange for an
empty target (1,010 entries in a hidden sibling, one exchange, the scratch volume an overlay
after, reads then following the disk); a populated target in place with `CreateCreate`;
grant mismatch, held lease, consumed and session grants, the audit log. Over a real
directory (`crates/land/tests/os.rs`, Linux lane on `/dev/shm`, loud skip elsewhere): the
worked example's shape on the disk, containment refusals, staging, and T-1.15's real `kill -9`
(a child lands round after round until killed; every file is a whole round; the parent resumes
with the child's landing id and sweeps). Baselines in BENCHMARKS.md (Phase 1 baseline: the
landing).

Found by the landing oracle and fixed as rules (each with its test): a merged directory's link
count ignored its base subdirectories, so removing a base subtree bottom-up drove the parent's
count to zero one step early and its own `rmdir` refused `NotFound` (the count is now two plus
the subdirectories, overlay and base, at listing load); a cleared directory renamed aside then
recreated left the name absent between the two steps (now a fresh directory exchanged with the
old one, verified, the displaced tree removed); a resumed landing met its own fresh directory
and its finished rename and called them conflicts (the verdict now knows a directory holding
only what the manifest creates beneath it, and a rename whose destination holds the witnessed
directory); a crash inside the directory syncs reported `Done` (a failed sync now aborts, an
aborted landing advances nothing, and every entry's directory is synced on the resume); the
simulated host's `st_mode` lacked the type bits a real `stat` carries, so a removed directory
planned as a file delete.

Deviations and owed items from tasks 11–13:
- Entries run one at a time; the ramp records the depth it would have chosen (`ramp_depth` in
  the report). Concurrent entries arrive with the runtime's pool in Phase 2; the linked
  io_uring chains and arena-page writes with Phase 4's Linux bridge (bytes are read from the
  volume into a buffer and written through the seam until then).
- Stage-and-exchange runs for an empty target only. A populated target needs every existing
  entry linked into the stage, a hard-link verb the seam gains with its measured cost (the
  break-even policy `LandingCosts::prefers_staging` is written and tested against the
  formula; the remembered costs come from each landing's report).
- Reflinks are not used (`LandCapabilities.reflink` is probed as `false`); `FICLONE` and
  `clonefile` arrive with the manifest's own hash index of identical files.
- The directory sync strategy is one `fsync` per touched directory; the `syncfs` alternative
  waits on the measured per-directory cost the report now carries (`Durability.dir_sync_ns`).
- Windows has no OS writer yet (`FileRenameInfoEx` with `POSIX_SEMANTICS`, the sharing-mode
  verify): the Windows bridge of Phase 4; the crate compiles there without the `os` module.
- `TargetIsVolume` is not checked: there is no mount table before Phase 3.
- The real-target tests and the OS rows of the bench run in the Linux lane; on this machine
  they skip loudly (no RAM disk authorized), so their first numbers are the lane's.
- `slates-land` carries three `unsafe` sites (the macOS libc calls rustix does not wrap:
  `F_BARRIERFSYNC`, `F_FULLFSYNC`, `renameatx_np`), budgeted.

Open in Phase 1: none. AC-1.2's harness (`crates/vfs/tests/differential.rs`)
and policy (`docs/wip/EQUIVALENCE.md`) are written; it runs in the Linux lane against
`/dev/shm` (2,000 histories) and skips loudly elsewhere, since a macOS RAM disk is a
system-state change Ada has not authorized; its first run is the lane's, not a local one.

Deviations from the §4.5 text, each measured (BENCHMARKS.md, Phase 1 baseline) and applied to
the design in A-7:
- `DirNode.parent` is the parent's inode number, not a node handle, and every directory inode
  carries `Body::Directory(current node)`: a handle held by a node shared with a snapshot goes
  stale after a copy, and the model found the root losing entries when a stale parent was
  copied (the second path copy rebuilt the root from the old node).
- Every node carries its own name, so the path for the op log and the parent re-pointing after
  a copy cost no scan of the parent.
- The indexed representation is one tree keyed by `(hash, folded name)`; the hash side index
  of D-4 is not built because the descent already probes by the leading hash word and the
  measured lookup is the fold and the compare.
- The ordered node is a 4 KiB block, not "entries per two cache lines": a block holds the
  measured directory (36 entries of 49-byte names) whole, and it is the unit the slab hands
  out and a copy moves; the small form is inline in the node up to the measured cut-over of 2.
- Clone pins are released by the owner of both volumes (`Volume::unpin`), not by the clone's
  destroy, because volumes hold no reference to each other (D-8: ownership by handle).
- `destroy_step` takes a time budget on the volume's clock, not an object count, and weighs a
  release by what it frees; the count form put 2.5% of slices over budget.
- The write path charges the materialized delta of the chunk-window rule and the model encodes
  that rule; a byte-precise charge left the counter drifting from the extents.

Residual literals: none in `src/`; the bench's shape constants (files per directory, name
bytes, groups, the 4 KiB block) carry their measurements. The example targets of the four
benched crates share the name `bench`; cargo warns of the output collision and may make it an
error, so a rename to `<crate>-bench` is owed before the Phase 2 crates add theirs.

## 8d. Phase 2 record (2026-09-05)

Task 1 (the anchor) landed 2026-09-05: `slates_mem::SharedObject` (`crates/mem/src/shared.rs`),
the shared memory object of §4.7 created without a filesystem entry (`memfd_create`, `shm_open`
under the 31-character limit, a `Local\` section), handed to another process by an inherited
descriptor or a name, mapped whole, with atomic views of the words two processes touch; and
`slates-anchor` (`crates/anchor`): the segment layout (a header with the magic, version,
machine identity, generation and the persisted geometry; a supervision block of atomic words;
the profile, the per-partition log rings, two snapshot slots per partition, the audit ring and
the landing slots, every region page-aligned and every size a derivation the daemon passes in),
create and attach with the seqlock rule (a torn header or payload is refused, a foreign
identity is refused with both hashes, a wrong length is refused), publish and read of payloads,
and the supervisor (start with the handoff in the child's environment, non-blocking `step`,
restart on exit, a restart bound derived from the recovery budget and the measured daemon start
p99, the crash loop recorded in the segment as the health plane's `daemon.alive` input).
Gated: `crates/anchor/tests/anchor.rs` (create, attach through the handoff, the payloads, the
refusals; a real child process, the test binary re-invoked, attaches from its environment,
beats, exits, is restarted three times and refused the fourth, with every step visible in the
segment). Owed from task 1: the profile is published by the daemon's boot (task 2 wires
`MachineProfile` in); `slates anchor` as a CLI command arrives with task 5; held descriptors
(the FUSE fd, the NFS socket) with Phases 3 and 4.

Task 2 (the database) landed 2026-09-05: `slates-db` (`crates/db`): the records of §4.8 with
one canonical `Wire` encoding each (`catalog.rs`: volumes with policy, base path, head, epoch,
accounting, state, lease, owner and access list; snapshots with placement; lineage edges;
attachments; completion records; grants; landing leases; landing records; audit records);
the operations (`op.rs`, 24 kinds, every local mutation); the adaptive radix tree of the
indexes (`art.rs`: four node shapes growing and shrinking, path compression, values at inner
nodes so keys need not be prefix-free; model-tested against an ordered map over 400
histories); the partition (`partition.rs`) with the guard-then-apply split: `check` refuses
what the log must never record (a duplicate name, a missing record, a held or stale lease, a
stale completion) and `apply` is the unconditional transition both the live path and replay
run, so a recorded operation applies the same way forever; leases indexed by holder with the
runtime's timing wheel for expiry; the log record over the segment's ring (`record.rs`: a
32-byte header with magic, length, sequence, CRC32C over sequence, schema and body, the
schema hash; records wrap; the tail is released after the bytes; replay verifies each and
stops at the first that fails); recovery (`replay.rs`: the newest valid snapshot slot then the
records after its sequence, the torn tail cut and overwritten, the replay timed) and the
snapshot cadence derived from the recovery budget and the measured replay throughput
(`SnapshotPolicy`, bytes per microsecond), with a full ring snapshotting and retrying rather
than refusing.

Gated (`crates/db/tests/model.rs`): 60 generated histories of every operation kind with the
database dropped and recovered at random points and random snapshot cadences, the recovered
partition equal to the live one after every crash and refused operations never recorded
(AC-2.3's durability half); the torn tail (a byte flipped in the last record: cut off, the
sequence reused, the next mutation lands over it); four hostile record shapes (a length of
`u32::MAX`, a foreign magic, a bad checksum, a truncated header) refused with everything
earlier intact; lease fencing and expiry through the wheel with epoch + 1 for the next holder
and the wheel rebuilt by recovery (AC-2.4's core); completions exactly-once across recovery;
snapshots trimming the log and recovery restoring a snapshot plus its tail. Baselines in
BENCHMARKS.md (Phase 2 baseline: the database): recovery of 10^4 volumes from 10^6 records in
96 ms against the 1 s budget (AC-2.7), 206 ns per mutation, 95 ns per replayed record, the
tree at 53 ns per insert and 18 ns per lookup at 10^5 keys.

Found by the model test on its first run: a completion recorded at a sequence the client had
already acknowledged (a stale retry) was retained live but released when the state was
restored from a snapshot, so a recovery differed from the live partition. The guard now
refuses it (`StaleCompletion`) and the window itself drops such a record.

Owed from task 2: the register and held-record tables of §4.8 arrive with task 7 (f=0) and
Phase 8; the `put_wal` with Phase 8; the chains, deltas and last-changed index with Phase 6;
the recovery budget is a ratified default (GAPS §5) until the CLI takes the operator's value.

Task 3 (the IPC) landed 2026-09-05: `slates-ipc` (`crates/ipc`): the 64-byte slot (sequence
word, kind, length, request word, 40 payload bytes) and the single-producer single-consumer
ring of slots with per-slot sequences (`slot.rs`; a hostile kind or length is a typed refusal
and the slot is released, so a bad message never wedges the ring; an oversized payload is
refused before the ring is touched); the client region over a shared object (`region.rs`: a
header with the geometry the daemon derived, the wake word, the client's and the daemon's
parked flags, the doorbell, the command and completion rings, the bulk area); the wake word
per OS (`wake.rs`: a shared futex on Linux; `os_sync_wait_on_address(SHARED)` on macOS, two
`unsafe` sites budgeted; Windows waits on the named Event of Phase 4); the two ends
(`endpoint.rs`: the client spins for the published window, sets its parked flag, re-checks the
slot to close the race, and waits on the word; the daemon bumps the word per reply and wakes
only a parked client; the client rings the doorbell per request while the daemon's shard is
parked); and the rendezvous per OS (`rendezvous.rs`: Linux, an abstract-namespace socket named
from the uid and the instance, `SO_PEERCRED` refusing another uid and counting it, the region
descriptor and the completion eventfd sent with `SCM_RIGHTS`, the socket kept as the control
channel; macOS and Windows, a bootstrap object with claim slots taken by compare-and-swap, the
object's per-user name and mode as the authentication, the region's name and length written
into the slot and released to the waiting client). Discovery: `SLATES_ENDPOINT`, then
`default`.

Gated (`crates/ipc/tests/rings.rs`, two mappings of one region on two threads): the round trip
while spinning (no park, no wake), the late reply (one park, one wake), the ring's credit
(`RingFull`, nothing dropped, the order kept), the hostile slot (refused, released, the ring
flows), the deadline, the doorbell; (`crates/ipc/tests/rendezvous.rs`) the test binary
re-invoked as the client connects through the real rendezvous, receives its region, completes
a round trip and exits 0, and a client with no daemon is refused `DaemonUnavailable` quickly.
Both branches lint clean for Linux and Windows from this machine (the Linux tests run in the
CI lane). Baselines in BENCHMARKS.md (Phase 2 baseline: IPC): 278 ns per spinning round trip,
1.05 µs per parked-and-woken round trip.

Owed from task 3: the completion fd on macOS and Windows (Phase 5's optional control socket;
the Rust client parks on the word and needs none); the Windows named Event per client (Phase
4, with the section-and-Event rendezvous compile-checked now); the doorbell thread that turns a
client's wake of a parked macOS shard into the driver's kick, and the heartbeat slot that
tells the daemon a client died where no socket closes, both with the server's integration in
task 4; the bulk region's use by streams (Phase 5); ring depth and spin window are the
daemon's derivation at rendezvous (task 4 wires the profile in).

Task 4 (the server) landed 2026-09-05: `slates-server` (`crates/server`): the daemon's
configuration as derivations from the profile (`config.rs`: clients per shard from Little's
law, the shard reserve, the table and store caps, ring slots and the spin window, the segment
geometry; every derivation logged with its inputs); the shard state in a thread-local cell on
its shard's thread (`state.rs`: volumes with their base host and reservation, the partition,
the store, the clients, the reserve, the deferred replies, the listings in flight); the verbs
of §4.4 (`verbs.rs`: create scratch and overlay with the bounded reservation or the host's
live memory as the dynamic quota's pressure source, snapshot, clone, attach with the lease
taken or renewed under D-16's epoch rule and a read intent needing none, detach releasing the
holder's last lease, resize moving the reservation, destroy in cooperative slices of half the
step budget, status with the drift list, list as a scatter-gather over every shard, base
verbs, acknowledgement, and the grant kind refused by channel and counted); the completion
record of every reply appended before it is sent (RIFL); the rights of §4.13 checked per verb;
a volume-bound verb from a client on another shard forwarded to the owner shard named by the
id's first bytes and the reply routed back (route by id, no index); the daemon (`daemon.rs`:
the segment created or attached from the anchor's environment, one mapping per shard, the
profile published, each shard's partition recovered and its state installed by a task on that
shard, the server loop as a poller of its clients' rings that idles otherwise and marks its
clients' regions parked, the control shard's rendezvous loop woken by the doorbell thread and
handing a client to its shard as a spawned task, the heartbeat at a tenth of the liveness
budget, stop joining everything); the doorbell thread (`doorbell.rs`: Linux waits on the
listening socket's readiness; macOS and Windows wait on the bootstrap object's word and kick
every shard; the value last acted on is what the wait compares against, so a ring during a
kick is never lost). The runtime gained pollers (`ShardContext::register_poller`: a task woken
by the loop whenever its ring says so) and `futures::idle` (yield without re-queue); the IPC a
shared protocol (`protocol.rs`: the bodies with one schema hash each, inline or through the
bulk chunk the slot's ring index owns) and the doorbell handed at rendezvous (the shard's kick
descriptor on Linux; the bootstrap word elsewhere); the volume core a `resize`.

Gated (`crates/server/tests/daemon.rs`, a two-shard daemon in the test process over a fresh
segment, one client at a time through the real rendezvous): the lifecycle (create, the
duplicate refused with the original's id, snapshot, clone, attach with epoch 1, status, list,
detach releasing the lease, resize, destroy completing in slices with the clone surviving);
exactly-once (a retry under the same id returns the retained reply without executing, an
acknowledgement releases it and a later retry is a stale duplicate); leases (a read attachment
takes none; the same principal renews with its epoch); AC-2.8 (the grant kind refused on the
ring and counted); a 200-byte name and an overlay over this crate's own source tree through
the bulk area, `read_base` reading the disk, `pin`, and a missing base refused
`BaseUnavailable`. Linux and Windows branches of the new crates lint clean from this machine.

Found by the daemon's tests and fixed: a task spawned from a task is joinable and stays in
the arena after it ends, so a daemon's perpetual tasks held its shutdown (they are detached at
spawn now); the doorbell thread re-read its word before each wait and lost a ring that landed
while it was kicking (it compares against the value last acted on); a client handed to a shard
whose loop already idled found the parked flag clear on its fresh region and never rang (the
hand-off wakes the loop, which marks the new client before idling again); a volume-bound verb
from a client on another shard was refused `NotFound` (forwarded to the owner now).

Found by the first cross-target lint of the Phase 1 base crate (a lane the Phase 0 crates had
and Phase 1's did not): on Linux rustix's `stat` nanosecond fields are unsigned and the
fingerprint's widening refused to compile; on Windows the fingerprint used unstable
standard-library metadata (`windows_by_handle`, `windows_change_time`). Both fixed in the
same change, and the cross-target lane now lints every shipped crate.

Owed from task 4: the file verbs over the ring (Phase 5's SDKs; until then content is
reachable in-process only); the Windows named-Event wake (Phase 4); one principal per uid
until the fleet's certificates (Phase 8); the clients-per-shard and ring-depth derivations
re-derived from measured rates at the first `status` (task 6 measures); `TargetIsVolume`
and the bridge path in `Attached` (Phase 3); the health signals of §4.14 exported through
`status` (task 6 with the histogram).

Task 5 (the Rust client and the CLI) landed 2026-09-05: `slates-client` (`crates/client`):
`Client::connect` through the rendezvous, one request in flight (the body framed inline or
through the slot's bulk chunk, the client spinning for the daemon's published window and then
parked on the wake word), request ids `(client id, sequence)`, and the two uses of exactly-once
(§4.9): a reply stalled past the reply deadline with the daemon found gone (`Liveness`, §4.7's
"control channel reset": the Linux control socket's peer end, or the bootstrap object's start
stamp on macOS and Windows) makes the client reconnect under its own id and resend, so the
retry meets its completion record; and `Session` lets a later process resume the id and the
sequence. Deadlines are derived (`Deadlines::derive`: the reply deadline is the anchor's
liveness budget, the reconnect budget the recovery budget plus one reply). Every verb of §4.4
is a typed method; refusals are the wire taxonomy as `ClientError::Refused`; the channel's
own refusals are `Stalled`, `DaemonGone` and `SessionTaken`. The rendezvous gained the wanted
id (`connect_as`; the daemon's `accept_pending` takes an in-use predicate and honours a free
id), the typed `TooManyClients` at the daemon's derived client bound (AC-2.6), and the
liveness check. `slates-cli` (`crates/cli`, the `slates` binary): `anchor` (the profile
measured, the segment created, the profile published, `slates daemon` supervised with the
segment and the anchor's pid in its environment; a daemon that never beats inside the recovery
budget or whose heartbeat lapses is killed and the policy decides; the restart bound
re-derived from the longest measured start), `daemon` (attaches and reads the published
profile, or measures and creates a segment when run alone; leaves when its anchor dies:
`PR_SET_PDEATHSIG` on Linux, a parent watch at the heartbeat cadence, a job object on
Windows), `profile`, and the client verbs with a stable plain output (one `key: value` per
line, one record per line for `list`) and exit codes for the taxonomy (0 done, 1 refused, 2
usage, 3 no daemon, 4 failed); a hand-written grammar with every flag listed once
(`args.rs`). The server gained `SegmentSource::Handoff` (an anchor in the same process), the
control channel held for a client's life, the client → shard mapping by the id's residue over
the partitions (a reconnect lands on the partition holding its records), routing by the
persistent partition index rather than the runtime's shard id, the name's owner partition by a
stable hash (`owner_of_name`, FNV-1a; every create of one name lands on one partition, so
uniqueness is that partition's to keep, with no global index), and the rebuild of recovered
volumes at start (`rebuild_recovered`: a live tree again, the reservation retaken, local-only
snapshots and attachments reconciled out of the catalog as recorded operations). `slates-mem`
keeps the object's name on macOS for opened objects, so an attached process can hand the
segment on.

Gated (`crates/client/tests/client.rs`, 2 tests, 1.2 s): the typed verbs over a two-shard
daemon (create, the duplicate refused with the original's id, snapshot, clone, attach with
epoch 1, status, list, detach, resize, destroy in slices, acknowledge; parks never exceed
replies); and a session outliving a daemon restart over one segment with the test as the
anchor: the first daemon stopped, a second started over the same handoff, the client's next
call stalling, finding the daemon gone, reconnecting under its id (one reconnect counted) and
served by the restarted daemon with the volume rebuilt, the retry of its earlier create
answered from the replayed completion record with the same id and no second volume, the
local-only snapshot reconciled away, new work continuing under the session's sequence, and a
second client refused the live session. (`crates/cli/tests/cli.rs`, 2 tests, 1.2 s): a real
`slates anchor` supervising a real `slates daemon`, the binary driven through create (the id
and the path line), the duplicate refused with exit 1, list, snapshot, clone, stat, attach,
status with and without `--drift`, detach, resize, destroy, the usage refusals with exit 2, a
missing volume with exit 1, then the anchor killed with SIGKILL and the daemon leaving so the
instance answers exit 3; `profile --quick` and the usage. The grammar and the value formats
have unit tests. All gates green on 2026-09-05 (`cargo xtask ratchet`: 82 rows, 0
regressions).

Found by the tests on their first runs: (1) routing used the runtime's shard id, which is
process-local (a second runtime in one process numbers its shards after the first's), so a
restarted daemon could reach none of its recovered volumes; volume ids and client ids now
route by the partition index, which recovery keeps (`ShardState::partition`); (2) a volume's
name was unique per shard only: two clients on different shards created one name twice
(T-2.1 across clients), which is what the CLI does on every invocation; (3) on macOS a shared
object opened by name refused to hand itself on, so a daemon attached from the anchor could
not map the segment on its shards and exited, which the anchor restarted and then refused as a
crash loop, exercising that path for real; (4) `detach` found a holder's other attachments by
encoding the whole partition (`to_snapshot`), replaced by `attachments_of`.

The dead-client reclaim landed 2026-09-05 (the daemon's side of §4.7's failure matrix, T-2.3):
the rendezvous carries the peer's process id (`SO_PEERCRED` on Linux; the claim slot's pid
elsewhere); every shard runs a sweep task at the liveness cadence (`reap_loop`, the same
budget the anchor allows the daemon's heartbeat) that expires leases by the wheel with nobody
asking and asks about every client silent for the budget: `peer.rs` (paired `#[cfg]`) peeks
the control socket on Linux (end of stream is the kernel closing the dead client's end),
probes the pid with signal 0 on macOS (`ESRCH` dead, `EPERM` reused by another user), and
waits on the process handle with a zero timeout on Windows. A gone client's attachments leave
the catalog as recorded operations, its deferred replies are dropped, its region and control
channel close with its slot, and its id returns to the control shard's live set (a thread-local
on that shard, reached by a spawned task: sharing by move); its leases keep their terms and
expire by the wheel, since a paused client is not a dead one and the term is the fence (D-16).
The operator's failover SLO moved into `DaemonConfig` (`failover_slo_ns`, ten seconds until
`slates anchor` takes a value; `with_failover_slo` for tests). The clock is read once per serve
round to mark the clients served in it. Gated (`crates/client/tests/reap.rs`, 3.8 s): the test
binary re-invoked as the victim connects, attaches for writing (epoch 1), prints its id and
parks; the parent kills it with `SIGKILL`; the attachment is reclaimed inside the lease term
(observed within two liveness budgets), the lease still shows epoch 1 after the reclaim, the
daemon's reaped counter moved by one, a session under the victim's id resumes (the id is free
again), the lease then expires by its three-second test term with nobody asking, and the
observing client never reconnected. Gotcha kept in the test: a re-invoked test binary prints
libtest's banner on stdout before the role runs, so the victim's line is tagged.

Owed from task 5: the CLI's grant surface
(`slates grant`, `grants`, `land`, `audit`) with task 8; a daemon-wide `slates status` with
task 6's health signals (`CLIENTS_REFUSED`, `RECOVERY_SKIPPED`, `HANDOFF_LOST`,
`INIT_FAILURES` are counted now and printed nowhere); the daemon start p99 for the restart
bound is the longest start measured in this anchor's life until a histogram of starts exists;
the Windows console handler, job object and section-and-Event paths are lint-checked from this
machine and run first in the Windows lane.

Task 6 (the provisioning histogram, exactly-once as a durable atom, admission and the wake
strategy) landed 2026-09-05. `crates/client/examples/provision_bench.rs` (R9, AC-2.1, T-2.6):
a volume created from the Rust client through the real rendezvous and rings against an
in-process daemon, sampled p50/p99/p999/max in a spinning form (the client spins for the 50 us
floor, so the reply lands without a wake when the daemon meets it) and a parked form (paced
past the shard's park, so each pays the doorbell and two wakes), at 1, 8 and 64 concurrent
clients. The 50 us floor is gated on the single-client spinning p99 (the latency claim); every
runnable concurrency's rows are recorded so the ratchet catches regressions; a run with more
client threads than the machine has cores past its shards is informational (it measures the
scheduler, not the path). Baselines (Apple M5 Max, macOS 26.4.1, best-of-3, all shown): one
client p50 9 us / p99 25 us / p999 31 us against the floor; eight clients p99 34-45 us;
sixty-four (oversubscribing thirteen runnable cores) p99 about 2 ms, not gated; the parked form
p99 about 250 us; a status round trip p99 about 9 us. The ratchet holds 102 rows.

Exactly-once became a durable atom: a verb's effects and its completion record now go into one
log record (`Db::begin` opens a transaction over the partition, `mutate` inside it applies and
queues, `commit` writes one `LogEntry` of every queued operation, a full log snapshots
instead), so `kill -9` between the effect and the record can no longer leave one without the
other (AC-2.3). A forwarded verb records its completion at its owner partition, and the reply
travels back already recorded; an acknowledgement is a scatter over every partition that may
hold the client's records; the client acknowledges on its own every half ring of replies, so
the daemon's retained records stay bounded without the caller (§4.9).

Admission and backpressure (AC-2.6): `clients_per_shard` is the client share of the shard's
reserve over a region's bytes (not the request rate, which sizes only what one client holds in
flight); the task arena and control channel are sized from that times the cross-shard traffic
per client plus the shard's own loops; a connect past the daemon-wide bound is refused
`TooManyClients`; a full owner-shard control channel makes a forward wait in the bounded
`pending_forwards` (retried each round) and, past the clients' credit, refuses
`Overloaded{shard}` without starting the verb. The daemon raises its descriptor soft limit to
the hard one at start (no privilege). A daemon-wide `slates status` (a scatter-gather like
`list`) reports each shard's counters and the health signals of 4.14 and the anchor's view.

The wake strategy was completed (4.7): a runtime shard sets a parked flag before it waits and
re-checks its inbox, and a sender kicks only a parked shard, so a message to a spinning shard
costs no syscall (the flag and the message are sequentially consistent, so a lost wake needs
both to miss, which the total order forbids); the server loop keeps polling for a derived idle
window after its last work, so an active client's next request never pays a wake; the client
can spin for a latency floor of its own (`Client::spin_for`).

Found under the histogram: (1) the create verb's completion record and its effect were two log
records, a `kill -9` between them a durability hole, closed by the transaction; (2) a shared
object whose name exceeded the macOS 31-character limit was truncated, so two clients' regions
could collapse onto one object under the bench's long names, now hashed when they would not fit
(`crates/mem/src/shared.rs`); (3) `status` and `detach` walked the whole partition
(`to_snapshot`) to count a volume's attachments, replaced by `attachments_of`; (4) a burst of
concurrent connects exhausted the bootstrap object's claim slots, so the client retries the
rendezvous on `RingFull` as on `DaemonUnavailable`.

Owed from task 6: the histogram runs on the reference machines in CI (this baseline is the
laptop's); the write-tracer hermeticity assertion (AC-2.2, T-2.9) and the simulation crash at
every instruction (AC-2.3's simulation half) arrive with the chaos harness; the cross-uid
security test (T-2.7) needs a second uid, gated on CI (the rendezvous refuses and counts it
now); a completion fd for parked SDK event loops is Phase 5.

Task 7 (the register protocol at f=0) landed 2026-09-05: `crates/db/src/register.rs`, the pure
core of §4.8 parameterized by the fault tolerance `f` so the laptop is the degenerate of one
formula (R8), never a mode: `Quorum` (`2f+1` candidates, commit at `f+1`, `f=0` giving one
candidate and a commit of one, the local append); rendezvous (highest-random-weight) candidate
selection, owner-first and deterministic, so every host computes the same holder set from an
object id with no directory; `Fence`, a holder's monotonic authority for a host (a record under
a host epoch below the highest seen is refused `StaleEpoch`, so a resumed stale owner never
commits); and `Configuration`, the one-voter oracle (`solo`: version 0, one member, `f=0`, no
mirror; `check_version` refuses a stale version with the current one; `await_placed(scope)`
returns for the region — the local append at f=0 — and refuses the absent mirror `Unsupported`).
Every reply carries the placement from the first version so Phase 8 changes no interface: the
wire gained `PlacedState` (`region`, `mirror_age_ns`, `host_epoch`) on `StatusReport`, the
`Scope` enum, and the `AwaitPlaced` request with the `Placed` reply; the server holds a
`Configuration::solo` per shard built from the machine identity's host id, records a snapshot's
placement through it (placed at f=0), reports the head's placement and the host epoch in
`status`, and serves `await_placed`; the client has `await_placed` and the CLI `slates volume
placed ID [--snapshot N] [--mirror]` with the placement fields in `status`.

Gated (`crates/db/src/register.rs` tests): the commit rule is the same code at f=0 and a
simulated f=1 (one candidate vs three, both `placed`), the observable differing only by the
quorum's own count (AC-2.5's register slice); a stale host epoch is refused at every f
(`StaleNeverCommits`); a stale configuration version is refused with the current one; `await
placed(region)` returns and the absent mirror is refused; rendezvous placement is
deterministic, owner-first and spread. The client and CLI tests assert the f=0 placement over
the real rings: `status` shows `placed=true`, `host_epoch=1`, no mirror; `await_placed(region)`
returns `(true, None)` and the mirror is refused `Unsupported`.

Found by the CLI test on its first run: `detach` carried only an attachment id and ran on the
detaching client's shard, but the attachment record lives on the volume's owner shard; it had
passed only because earlier client ids happened to land on the owner shard, and task 7's extra
clients shifted them. Attachment ids now carry their owner partition in the high 16 bits
(`attachment_id`/`owner_of_attachment`) and `detach` routes to it, like a volume id (§4.8
'ids route to owners, no global index'); a latent cross-shard `detach` bug closed.

Owed from task 7 (updated 2026-09-05): the holders, commit at `f+1`, takeover and phase-one
adoption are now implemented as the pure fenced ledger register simulation (§8h); still Phase 8
are the server put path (hedged placement over the real holders, recorded holder sets in the head
record), the healer and probation, mirroring and `await placed(mirror)`, migration on a
write-intent attachment, and the SWIM membership (the register core is f-parameterized so they
raise `f` without a new shape); the host epoch is persisted only as the constant 1 until the
takeover path is wired into the server (Phase 8); a chain is a register written in sequence,
which arrives with the merge engine (Phase 6).

Task 8 (grants and landings through the server) — the durable records, the ring verbs and the
CLI landed 2026-09-05; the control-channel grant transport and the write execution are in the
Linux lane. `crates/server/src/landing.rs` wires the Phase 1 landing engine (`slates-land`)
through the server: a `RequestBody::Land` plans the manifest from the snapshot's diverged
entries and, without a grant, replies `GrantRequired` with the manifest hash, its summary and
the preliminary conflicts, recording a `LandingRecord` (AwaitingGrant) and a `LandingPlanned`
audit record; a grant that binds the manifest lets the landing take the target's lease (one
holder per target, AC-2.9), validate and write through `OsLand`, after which the landing
record, the consumed grant and the audit trail are persisted (all §4.8 ops, so the
accountability replays after a crash, AC-2.10). The grant is never created on the ring or MCP
(R10, AC-2.8): the ring's grant kind stays refused, and `issue_grant` (the control-channel
entry) binds the manifest a human saw, refusing `GrantMismatch` when a re-planned landing's
hash differs. The wire gained `Land`/`Grants`/`Audit` requests, `GrantRequired`/`Landed`
replies, the `LandingSummary`/`LandingOutcome`/`GrantSummary`/`AuditEntry` shapes, the `Filter`
and `GrantScope`, and the refusals `TargetUnavailable`, `LandingConflict`, `LandingLeaseHeld`,
`GrantMismatch`, `GrantInvalid`. The client has `land`/`grants`/`audit`; the CLI has `slates
land ID TARGET`, `slates grants`, `slates audit`. The landing execution and the `os` writer are
Unix-only, so the write path's test runs in the Linux CI lane; the daemon suite here checks the
off-ring refusal, a landing into a target that cannot be opened (refused `TargetUnavailable`
with no write), and the empty grants and audit reads.

Found while wiring: `detach`'s cross-shard routing bug (task 7) had a sibling — a landing id, a
grant id and an attachment id all need to route to the partition that holds their record;
attachment ids now carry the owner partition (task 7), and the landing/grant records live on
the volume's owner shard, reached by the volume-bound `Land` request. A bench-harness flake
surfaced under the omnibus ratchet on a loaded machine and was fixed: the size-independence
check (`vfs_bench` ac-1.3) compared the median growth against the bare timer resolution, so a
lucky-fast small-size sample read as per-file scaling; it now allows the two measurements' own
bootstrap-interval widths (a real per-file term over three decades still fails). The IPC
parked-round-trip bench asserted the client parked exactly once per trip; a spurious futex
wakeup can add a park, so it now asserts at least once per trip.

Owed from task 8: the control-channel grant transport (the Linux control socket reader in the
daemon and `slates grant`/`slates grant --watch`; the socket is the rendezvous control channel,
Unix, with macOS and Windows on Phase 5's control socket) and the Linux landing execution test
(the full plan-grant-write flow into `/dev/shm`, AC-2.9's two-session serialization, and
AC-2.10's audit replay after `kill -9`); the per-entry audit records (`EntryWritten`,
`EntryRefused`) beyond the plan and the terminal record; the grant scatter for a daemon-wide
`slates grants`/`slates audit` (served on the shard now); the write-tracer hermeticity
assertion (AC-2.2, T-2.9) with the chaos harness. The provisioning histogram
(`crates/client/examples/provision_bench.rs`) was pulled out of the omnibus `cargo xtask
ratchet` (it spawns a daemon and needs a quiescent machine; back-to-back with the microbenches
its p99 measured contention, not the path) and is its own recorded command / CI lane for
AC-2.1; its rows were removed from `ratchets.toml`.

## 8e. Phase 3 record (2026-09-05)

Phase 3 (the Linux FUSE bridge) task 1's first piece landed 2026-09-05: `slates-bridge-fuse`
(`crates/bridge-fuse`), the FUSE ABI codec — the pure, transport-free layer. It parses the
kernel's `fuse_in_header` and the opcode-specific bodies slates serves (`request.rs`,
`abi.rs`: the opcode set as `#[repr(u32)]` discriminants that are the wire values, so an
unserved opcode is a typed miss the daemon answers `ENOSYS`), encodes the daemon's replies
(`reply.rs`: `fuse_out_header`, `fuse_attr`, `fuse_entry_out`, `fuse_attr_out`, `fuse_open_out`,
`fuse_write_out`, and a bounded `readdir` buffer), and computes the `FUSE_INIT` negotiation
(`init.rs`: the intersection of the flags slates wants — writeback cache, parallel dirops,
readdirplus, explicit data invalidation, big writes — and the kernel's, the minor version
bounded to slates' 7.31 floor, and the sizes it will use). Every field is read and written in
order through a bounds-checked sequential reader/writer (`wire.rs`), so no byte offset is a
literal and a truncated or oversized message is a typed refusal, never a panic or an
out-of-bounds read; the crate holds no `unsafe`.

Gated (`crates/bridge-fuse/tests/codec.rs`, 12 tests, on every host — the codec is pure): a
`LOOKUP` parses to its header and name; an unserved opcode is `None` not a panic; hostile
headers (truncated, a length below the header, a length past the buffer) and hostile bodies (an
unterminated name, a short read, a write whose declared data runs past the body) are refused
without a panic (§4.9); read and write bodies parse; error and success replies encode to the
exact wire bytes with an undersized buffer refused; the `readdir` buffer packs 8-byte-padded
entries and stops before it exceeds the request's size; `FUSE_INIT` keeps the flag intersection
and handles a version mismatch and a short body. Golden byte checks stand in for kernel vectors
until the transport test runs a real mount.

Owed from Phase 3 task 1 (the rest of the driver, all Linux-only, CI lane): the `/dev/fuse`
transport (request read, reply write, notifications), `FUSE_DEV_IOC_CLONE` per shard and the
io_uring command path with the read/write fallback, mount establishment (the new mount API when
permitted, `fusermount3` otherwise) with the fd held by the anchor and the restart handoff, the
`Bridge` trait implementation over the volume core with inode `(no, gen)` and invalidation on
every mutation, `slates exec` (the launcher), the conformance suites (pjdfstest, fsx, fsstress)
and the workload harnesses, and the base-files read path through the mount. These are Phase 3
tasks 1b–8; the codec is the foundation they build on.

The bridge dispatch landed 2026-09-05 (still Phase 3 task 1, pure): `bridge.rs` defines the
`Bridge` trait (§4.6's methods, one real implementation to come in the daemon over the volume
core) and `dispatch(message, bridge, out)` — the seam between the wire and the semantics. It
parses a request, calls the matching method, and encodes the reply or the errno: `INIT`
negotiates, `LOOKUP`/`GETATTR`/`OPEN`/`OPENDIR`/`READ`/`WRITE`/`READDIR`/`CREATE`/`RELEASE`/
`FLUSH`/`FORGET` reach the bridge, a `Result::Err(errno)` becomes the kernel's negated errno, a
parse failure is `EIO`, and an unserved opcode is `ENOSYS` without reaching the bridge. Gated
(`crates/bridge-fuse/tests/dispatch.rs`, 3 tests, every host): a mock one-file bridge driven
through the dispatch — LOOKUP and its ENOENT, READ returning a slice and WRITE mutating the
file, READDIR packing an entry, INIT negotiating, FORGET reaching the bridge with no reply, and
an unserved opcode answered ENOSYS. The transport (the `/dev/fuse` read/write loop) is the thin
Linux-only layer over this dispatch.

The Bridge implementation over the volume core landed 2026-09-05 (still Phase 3 task 1, pure):
`volume_bridge.rs` (`VolumeBridge`) borrows a `Volume` and its `Store` and turns the kernel's
requests, by FUSE node id (the inode number, node id 1 the root), into volume operations —
lookup, getattr, open/opendir, read, write, readdir, create, release, forget, flush — with a
small file-handle table naming the inode a handle was opened on, and the volume core's refusals
mapped to the Linux errno the kernel expects. The volume core gained by-inode-number wrappers
(`root_inode`, `lookup_no`, `readdir_no`, `create_file_no`) so the bridge speaks inode numbers,
not handles. Gated (`crates/bridge-fuse/tests/volume_bridge.rs`, 2 tests, every host — a scratch
volume is pure RAM): a whole FUSE round trip through the real volume core (CREATE a file, WRITE
to it, LOOKUP it, GETATTR its size, OPEN and READ the bytes back, READDIR the root lists it) and
the typed errnos (a missing name is ENOENT, a stale handle is EINVAL). The metadata ops followed the same day: the Bridge trait, the dispatch and the VolumeBridge
gained mkdir, unlink, rmdir, rename (and rename2), symlink, readlink, setattr (size → truncate,
mode → chmod, by the `valid` mask) and statfs, with the volume core's by-inode-number wrappers
(`mkdir_no`, `symlink_no`, `unlink_no`, `rmdir_no`, `rename_no`) and the setattr/rename/statfs
codec. Gated (two more tests in `volume_bridge.rs`): mkdir then a file inside it, rmdir refused
non-empty (ENOTEMPTY), unlink then rmdir; and rename moving a file, setattr truncating it, and
statfs answering. The FUSE bridge's semantic surface is now complete and pure-tested; the
`/dev/fuse` transport (Linux) is the only remainder of task 1. A pre-existing anchor
test-isolation bug was fixed in the same change: `supervised_child` set process-global env vars
that raced into its own in-process test thread under a concurrent `cargo test --workspace`; it
is now `#[ignore]`d and the parent spawns it with `--ignored`, so cargo never runs it in
process. The `/dev/fuse` transport landed 2026-09-05 (Phase 3 task 1b, Linux): `channel.rs` (Linux-only)
owns the device descriptor and turns it into the request/reply stream — `FuseChannel::open`
opens `/dev/fuse` (a character device, structurally allowed as a non-disk-file open), `from_device`
adopts the fd the anchor hands back on a restart, `read_request`/`write_reply` are the device
I/O (a disconnect is `ENODEV`, typed), and `serve_blocking` is the fallback loop the design
names: read a request, `dispatch` it to the bridge, write the reply, until the kernel unmounts.
No `unsafe` (rustix's I/O-safe wrappers over the owned descriptor). It compiles and cross-lints
for Linux from this machine; the serve loop runs against a real mount in the CI Linux lane. Owed
with the rest of the driver: the io_uring command path, `FUSE_DEV_IOC_CLONE` per shard and the io_uring command path (which drops the request copy
the blocking loop makes), generation-tracked node-id reuse after `forget` (§4.6 `(no, gen)`),
`link` and xattrs, the kernel invalidation notifications, `slates exec`, and the conformance and
workload suites.

Mount establishment landed 2026-09-05 (Phase 3 task 2, Linux): `mount.rs` mounts a slates
connection through `fusermount3`, the OS-shipped setuid helper, so no privilege is required
(R10, D-2): the daemon makes a socket pair, spawns `fusermount3 -o default_permissions,fsname=…`
with one end in `_FUSE_COMMFD`, and receives the `/dev/fuse` descriptor the helper sends back
with `SCM_RIGHTS` (the same fd-passing rustix path the rendezvous uses), returning a `Mount`
whose `FuseChannel` serves it; `unmount` tears it down with `fusermount3 -u`. No `unsafe`. It
compiles and cross-lints for Linux here; the handshake runs against a real `fusermount3` in the
CI Linux lane. Owed: the new mount API (`fsopen`/`fsconfig`/`fsmount`/`move_mount`) where the
daemon has `CAP_SYS_ADMIN` in its user namespace, and the anchor holding the fd across a
restart (§2.6 step 4).

The invalidation notifications landed 2026-09-05 (Phase 3, §4.6 "Cache posture"): `notify.rs`
encodes the unsolicited messages the daemon writes to `/dev/fuse` to drop kernel cache on a
mutation — `inval_inode` (an inode's attributes and a data range), `inval_entry` (a cached
name → node mapping) and `delete` (an entry removed). They are pure encoders (the header with a
zero unique and the notification code in `error`, then the body), tested on every host with
golden byte checks (4 tests). The driver writes them before it acknowledges a mutating request,
so a second process never reads stale attributes after the mutating call returns (AC-3.3); wiring
them into the mutating dispatch paths is the driver's, with the transport.

Base files through the bridge landed 2026-09-05 (Phase 3 task 8's read path, §4.6 "Base files",
AC-3.9): `VolumeBridge::with_base` holds the overlay's read-only `OsHost`, and lookup, getattr,
readdir and read route through `Volume::with_host` (the overlay path that serves untouched base
entries from the disk), the volume core gaining `Overlay::lookup_no`/`readdir_no`. Gated
(`crates/bridge-fuse/tests/base_overlay.rs`, on any Unix host — the base is a real read-only
directory, this crate's own `src`): an overlay over `src`, served through the bridge, lists its
base files, looks `lib.rs` up, and reads it back byte-identical to reading the file straight
from disk, writing nothing. Owed: a standalone `LOOKUP` of an unlisted base entry loads the
directory's base listing on demand (today the listing loads on `readdir`, which the kernel does
first; the realistic sequence is verified); `mmap` of a base file and the splice reply path (both with the
transport). Base writes' copy-up through the bridge is done and verified: a write to a base file
routes through `Overlay::write`, copies the base up into the overlay's RAM, and leaves the base
directory on disk untouched (`base_overlay.rs` second test — the write is visible on a later
read, the rest is the base bytes, and the disk file is byte-for-byte unchanged, R1).

The launcher `slates exec` landed 2026-09-05 (Phase 3 task 4, Linux): `crates/cli/src/exec.rs`
makes a volume visible at a caller-named path for one command, in a new user and mount namespace,
without privilege (D-2, R10) and without writing disk (AC-3.5): it enters `CLONE_NEWUSER |
CLONE_NEWNS`, maps its own uid and gid, makes the mount tree recursively private, bind-mounts the
volume's directory (under the daemon's root, `SLATES_ROOT`) onto the chosen path, and execs the
command — so the parent shell's view is unchanged and the bind lives only in the command's
namespace. An unsatisfiable path is refused with the exact missing directory, never created. One
budgeted `unsafe` (`unshare_unsafe`, sound in the single-threaded pre-exec launcher). The `--`
splits the flags from the command; the parsing is unit-tested on every host, and the launcher
runs against a real daemon mount in the CI Linux lane. Owed: the daemon publishing its root
mount so `SLATES_ROOT` need not be set by hand, and the AppArmor-profile detection with the exact
remedy message (a generic hint is given now).

The xtask unsafe counter was made word-boundary aware in the same change: it counted the
substring `unsafe` inside identifiers like `unshare_unsafe`; it now counts the `unsafe` keyword
as a whole token. The end-to-end CLI flow test (`crates/cli/tests/cli.rs`) is gated behind
`SLATES_TEST_CLI` and runs as its own CI step: it spawns a real anchor and daemon whose shards
spin, and a busy parallel `cargo test --workspace` starves them; on its own (the CI step passes
`--test-threads=1`) it is reliable.

## 8f. Phase 6 groundwork (2026-09-05)

The merge engine's deterministic verdict — the pure core of §4.16 (D-27) — landed 2026-09-05 as
`slates-merge` (`crates/merge`), ahead of the rest of Phase 6, because it is a self-contained
pure function that needs none of the fleet or the green-volume integration to be correct and
directly confirmable. `range.rs` is the byte range and the per-path `RangeSet` (sorted,
non-overlapping, with the half-open overlap rule that an insert at the very edge of a range does
not conflict). `verdict.rs` is the two passes: `path_verdict` sweeps the increment's ranges
against the intervening deltas' effect ranges on one path — disjoint ranges `Accept`, an
identical span becomes a candidate for pass two, an insert anchored inside a change or two
inserts at one point are `SamePositionDiffering`, any other overlap or containment is `Overlap`;
a structural class the sweep cannot see (rename/create/type/meta/delete-vs-modify) is passed in
and returned directly. `compare_bytes` is pass two (a memcmp: equal bytes `AcceptIdentical`,
else a conflict). `fast_path` is the whole-increment shortcut: every touched path last changed
at or before the base means `Accept` with no range work, and it never decides a conflict. The
sweep is a linear merge of two sorted lists; it does no I/O, reads no clock, draws no randomness,
and its hot comparison allocates nothing (a lint and a no-alloc test to pin that are owed).

Gated (`crates/merge/tests/verdict.rs`, 9 tests, every host — the verdict is pure; the hecate
M-matrix as range cases): disjoint accepts, no-intervening-change accepts, overlap and
containment conflict, an identical span becomes a candidate that pass two resolves to
identical-or-conflict, an insert inside a change conflicts while one at the edge accepts, two
inserts at one point are resolved by pass two, every structural class is returned directly, and
the fast path accepts an untouched basis.

The ops document — the canonical serialization of an increment's declared operations, whose BLAKE3 is half the increment's identity (§4.16 "Data model", "its identity is the test") — landed the same day (`ops_doc.rs`). It is the fixed op kinds (§4.16 `OpKind`, wire values 0–14; `from_wire` reads them off the kinds' own list, so no arm is a bare number), a per-document path table that interns paths in insertion order while building and sorts them canonically at `canonicalize` (remapping every op's path index), and one fixed little-endian record per operation (kind, flags, path index, offset, length, source offset). Bytes are never in the document; content an operation adds is named by a source offset into the post-state chunks the splice resolves later. `encode` is a sequential writer, never a struct transmute, so no host-dependent padding enters; `identity` is the BLAKE3 of that encoding. Gated (`crates/merge/tests/ops_doc.rs`, 5 tests, every host): the same operations declared in different orders produce byte-identical encodings and one identity (the determinism gate), encoding is stable and the identity is its hash, different work has a different identity, every op kind round-trips through its wire value, and interning is stable while canonicalize sorts the table and remaps the ops. A caller canonicalizes before taking the identity; the deriver's terminal step will, and that property is what the determinism test pins.

The content deriver — the pure core of "Composition at seal" — landed the same day (`derive.rs`). One path's declared content operations (`ContentOp`: overwrite, extend, truncate, insert, delete), in journal order, compose by interval algebra into the canonical net op set relative to the base content, never by comparing bytes (D-27's never-diff clause: the deriver reads no content). Composition tracks the file as a piece list in current-file coordinates — runs of surviving base bytes and runs of new bytes — that each operation splits and rewrites; a single readout pass then emits the net ops in base coordinates, naming added content by its offset in the sealed post-state (the final file, `slates`' ground truth for bytes), which the splice resolves later. The canonical form is minimal: overlapping overwrites merge into one, an equal-length in-place replacement is one overwrite, an unequal one is a delete then an insert (the whole-file-rewrite shape), an addition past the base end is an extend and within it an insert, a removal to the base end is a truncate and within it a delete. Gated (`crates/merge/tests/derive.rs`, 9 tests, every host): the four worked cases (overlapping overwrites merge, a truncate cancels operations beyond it, an insert then an overlapping delete cancels, a whole-file rewrite is a delete then an insert), an append is an extend, an empty journal is the identity, and three proptests over random in-bounds journals — the deriver oracle (applying the net ops to the base, drawing added bytes from the post-state by source offset, reproduces the post-state exactly, so composition is correct without any byte comparison), determinism (the same journal composes to the same ops), and the net set is ordered by base offset. The oracle draws base bytes and added bytes from two different position-varying patterns, so a misplaced op, a wrong length or a wrong source offset diverges. A `Vec` piece list makes a front split O(pieces); the journal a submit composes is bounded, and a balanced structure is the measured replacement if a length benchmark shows the piece count dominating (owed, not guessed).

Position mapping — "canonical rebase" (§4.16 "Position mapping") — landed the same day (`map.rs`). An increment declared against a base version is mapped forward through the canonical deltas of every version in `(base, head]`, one direction, per path, before the verdict and the splice. For one delta: an intervening operation that turns `old_len` base bytes at `at` into `new_len` shifts every later position by `new_len - old_len`; a range entirely before it is only shifted; a range that overlaps a touched span is returned as `Overlaps` for the verdict to classify (the mapper does not decide conflicts); an insert at the very edge of a range does not overlap it (the range's edge rule), so a range whose neighbourhood only grew or shrank maps cleanly. Maps compose: mapping through `(base, head]` feeds each delta's shifted range into the next. Content effects come from the ops' kinds (overwrite touches its span with zero size change, insert and extend add, delete and truncate remove); namespace ops have no byte-coordinate effect. Gated (`crates/merge/tests/map.rs`, 9 tests, every host): an insert before a range shifts it and after it leaves it, a delete before shifts it left, an overwrite hitting it overlaps, an insert at the edge is clean, maps compose across two deltas, and a later delta meeting the shifted range overlaps — plus a provenance oracle (apply the deltas to a vector recording each head byte's base origin; a range maps cleanly exactly when its bytes all survived contiguously, and then to their position) over random single-op delta sequences, and determinism. Owed here: folding old deltas into checkpoint deltas so a distant base maps in O(log) lookups (the same composition applied ahead of time; the measured optimization when a base-lag benchmark shows the raw walk dominating).

The whole-volume deriver — composing a work volume's journal of content, create and unlink into one ops document — landed the same day (`increment.rs`, replacing the content-only assembler). Per path the journal is a state machine (`VolumeOp`): a create makes a fresh empty file, an unlink removes it, content operations accumulate against whatever file is present. Composition follows §4.16: a create then an unlink of a new file cancels (nothing declared); a base file unlinked is one `Unlink`; a base file edited in place is its content net ops; a base path whose content was replaced (unlinked then recreated, or created over) is a delete of the base and the new content, with no create because the path existed at base; a new file that survives is one `Create` and its bytes as inserts. It then groups the paths, composes each with the content deriver, and lays the post-state out in sorted-path order (each path a contiguous region, an op's source its per-path offset plus the region base), so the identity is independent of the journal's declaration order. An operation the journal could not have produced on a valid volume (content on a missing file, a create over an existing one, an unlink of a missing one) is a typed `DeriveError`, never a panic. Gated (`crates/merge/tests/increment.rs`, 6 tests, every host): create-then-unlink cancels, unlinking a base file is one unlink, creating-and-writing is a create and an insert, recreating a base file replaces its content (a delete and an insert, no create), content on a missing file refuses, and a whole-filesystem oracle over three paths with random valid journals — reconstructing the filesystem from the increment (creating, removing, and applying content drawing from the post-state) reproduces the model the journal produced, base bytes (0-127) and added bytes (128-255) disjoint so any wrong path, offset, source or missing create/unlink diverges.

Rename composition landed the same day, extending the deriver to a moving-entity model (`increment.rs`): each file is an entity carrying its origin, base length and content ops; a create makes one, an unlink kills one, a rename moves one and content accumulates on whichever entity is at a path. §4.16's rename cases: a base file renamed to a fresh path is one `Rename` whose source is the base path, plus its content; a base file renamed over another base file is one `Rename` that replaces the target; a new file renamed over a base file is the write-and-rename pattern — it composes to the destination's content being replaced (a delete and the new bytes), not a rename. A gone base path is unlinked only when no surviving file occupies it, so a rename or replacement onto a path covers its removal and a base file recreated to the same content leaves no trace. The rare rename onto, or create at, a base path already consumed this increment is a typed `Unsupported` refusal (owed). The oracle now generates renames too and reconstructs them (a renamed path's base comes from the rename's source), skipping the owed refusal; 7 tests including worked rename cases (base to a fresh path, write-and-rename replacing a base file's content).

The splice landed the same day (`splice.rs`): once the verdict accepts a path's net ops, the new version's extent list is built from the base version's extents with each op's range replaced by a reference into the post-state chunks — no byte is copied, an unchanged run keeps pointing at its base chunk and an added run points at the post-state chunk (`src`). It is a single walk over the base extents and the (base-ordered) net ops: copy the base extents up to the next op, then place the op's replacement; `Source` is `Base` or `PostState` (a chunk store and offset). Gated (`crates/merge/tests/splice.rs`, 5 tests, every host): an overwrite splits the base and keeps the rest base-sourced (a non-vacuity check that unchanged bytes are not copied), no ops leaves the base extents untouched, a truncate to zero leaves none, and two proptests — the splice oracle (chunk the base into extents, splice a random journal's net ops, read the extents back from the base and post-state buffers, reproduce the final content exactly) and well-formedness (the extents cover exactly the final length, none empty).

Directory composition landed the same day, extending the deriver's input to a `Base` snapshot that knows the base's files and directories (`increment.rs`). Directory operations compose independently of files (a path is a file or a directory, never both at once): a base directory removed is one `Rmdir`; a new directory that survives is one `Mkdir`; a mkdir then an rmdir cancels; a base directory removed then recreated is unchanged (directories have no content). A path used as both a file and a directory this increment, a mkdir over a present directory, and an rmdir of an absent one are typed refusals (`PathIsFileAndDirectory`, `MkdirOverExisting`, `RmdirMissing`). Gated (`crates/merge/tests/increment.rs`, now 15 tests, every host): seven directory worked cases (make, remove, mkdir-then-rmdir cancels, remove-then-recreate is nothing, the file/directory collision, the two refusals) and a directory oracle over random valid mkdir/rmdir journals (applying the document's Mkdir/Rmdir to the base directory set reproduces the set the journal produced). The file deriver's seven tests are unchanged (no regression).

Mode composition landed the same day (`SetMode`, extending `Base` with the base mode per path). A file or directory's mode composes independently, last-write-wins, emitting a `SetMode` (the new mode carried in the op's `len`) only where the mode differs from the base and the path is present at seal — a surviving file (including an untouched base file) or a present directory. A mode set to the base mode declares nothing (minimality); a mode on a path present nowhere is `SetModeMissing`; chmod then rename of one path is the owed `Unsupported` refusal. Gated (`crates/merge/tests/increment.rs`, now 22 tests, every host): setting a base file's mode, setting it to the base mode (nothing), last-write-wins, a base directory's mode, a new file's mode, the missing-path refusal, and the chmod-then-rename refusal.

Symlink composition landed the same day (`Symlink`, composed independently like directories, with `Unlink` on a symlink path routed to it; the target is interned in the path table and named by the op's `src`). A new or retargeted symlink is one `Symlink`; a base symlink removed is one `Unlink`; a symlink created then unlinked, or a base symlink removed then recreated to the same target, is nothing; retargeting a base symlink (unlink then symlink) is one `Symlink` because the survivor rule suppresses the removal. A symlink over a present symlink, and a path used as conflicting kinds (file/directory/symlink), are typed refusals (`SymlinkOverExisting`, `PathKindConflict`). The base grew a `symlinks` list (path to target). Gated (`crates/merge/tests/increment.rs`, now 30 tests, every host): create, symlink-then-unlink cancels, base-symlink removal, retarget, recreate-to-same-target (nothing), symlink-over-existing, symlink at a base file path, and a write on a base symlink. This work reused the file oracle, which surfaced a latent write-and-rename bug: a new file renamed over a base file that had itself been renamed to that path was wrongly treated as a content replacement of a non-existent base path; the fix restricts the content-replacement to a base file at its own path, so the other case is a new file at the destination with the clobbered base file's original name unlinked (the analogous path in `apply_create` already checked the own-path case). The oracle now passes across repeated proptest seeds.

Xattr composition landed the same day (`SetXattr`/`RemoveXattr`, composed like `SetMode` but against the base value). Per `(path, name)` the final state is the last set value or removed; a `SetXattr` is emitted only where the value differs from the base and a `RemoveXattr` only where a base xattr is removed, and only where the path is present at seal. The op names the file in `path` and the attribute name (interned in the path table) in `at`; a set xattr's value is laid out in the post-state after the file content, in sorted `(path, name)` order, and named by the op's `src`/`len`. The base grew an `xattrs` list `(path, name, value)`. An xattr on a renamed-away path is the owed `Unsupported` refusal; one on a path present nowhere is `XattrMissing`. Gated (`crates/merge/tests/increment.rs`, now 36 tests, every host): setting to a new value, setting to the base value (nothing), removing a base xattr, a new xattr's value laid in the post-state, set-then-remove of a base xattr, and the missing-path refusal.

Hard link composition landed the same day (`Link`, composed like symlinks: an independent per-path state machine with `Unlink` routed to it, the target file named in the path table by the op's `src`). A new or retargeted link is one `Link`; a base link removed is one `Unlink`; link-then-unlink cancels; a base link removed then recreated to the same target is nothing. Whether the shared content survives an unlink is the merge's concern at apply time, not the composition's; a write through a hard link is refused as a kind conflict (owed). A link over an existing link, and a link sharing a path with a file/directory/symlink, are typed refusals (`LinkOverExisting`, `PathKindConflict`). The base grew a `hardlinks` list. **With this, the deriver composes every §4.16 declared operation kind** — content (overwrite/extend/truncate/insert/delete), create, unlink, rename, mkdir, rmdir, setmode, symlink, setxattr, removexattr, and link. Gated (`crates/merge/tests/increment.rs`, 41 tests, every host): create, link-then-unlink cancels, base-link removal, link-over-existing, and a link/file conflict.

The merge engine on one node landed the same day (`engine.rs`), composing the pure pieces into the submit pipeline (§4.16 "The verdict", "Splice", "Commit"). A `Green` holds the head content per file, the committed deltas per version (for position mapping), the last version each path changed at (the fast-path index), a `seen` set for idempotent retries, and the head. `submit(increment)` is idempotent by identity; for each changed file it takes the fast path when the path is unchanged since the increment's base (counted, with a non-vacuity test), else maps each edited range forward through the intervening deltas — a range disjoint from every intervening change is accepted and its edit re-applied at the shifted position, a range that meets one is a conflict unless the agent produced exactly the green's current bytes for that file (both made the same edit, which accepts). All files accepting commits a new version; any conflict returns byte-exact windows and changes nothing, and the agent rebases onto the head. Gated (`crates/merge/tests/engine.rs`, 9 tests, every host): a submit accepts and updates the green, disjoint files both accept, disjoint ranges of one file both accept (the range merge), an intervening insert shifts a later edit, overlapping edits conflict, an identical edit accepts, the fast path fires and is counted, a resubmit is idempotent, and a rebase after a conflict accepts. This is the laptop-degenerate engine. It now merges namespace changes too: a `PathChange` is a modify, a create, or a remove; create/create with different bytes conflicts (identical bytes accept), a modify of an intervening-deleted file or a remove of an intervening-modified file is a delete/modify conflict, and a remove of an already-gone file is a no-op — each conflict carries its `MergeConflictClass`. Six more tests: create/create conflict, identical create, delete-versus-modify, a remove accepts, removing an already-removed file, and remove of a modified file. The engine now merges the first namespace dimensions too, as independent per-path dimensions the way the deriver composes them: a directory creation (`Mkdir`) and a mode change (`SetMode`). A file and a directory at one path is a `TypeChanged` conflict (both directions: mkdir over a file, create over a directory, modify of a path now a directory); two agents making the same directory accept; two differing mode changes on one path are a `MetaMeta` conflict while an identical one accepts; a mode change on a deleted path is a delete/modify conflict; and a content edit and a mode change on one path are independent (they do not conflict), each tracked by its own last-changed index. Ten more tests (`crates/merge/tests/engine.rs`): mkdir creates a directory, two mkdirs accept, mkdir over a file conflicts, create over a directory conflicts, setmode sets a file's and a directory's mode, two differing setmodes conflict, identical setmodes accept, setmode on a deleted file conflicts, content and mode are independent, and a modify of a path now a directory conflicts. Symlink merges followed the same pattern: a symbolic link (`Symlink`) merges per path, an identical target accepting, two differing targets a `CreateCreate` conflict, and a file-versus-symlink or directory-versus-symlink at one path a `TypeChanged` conflict in both directions; four more tests (now 29 engine tests) — a symlink is created, identical symlinks accept, differing targets conflict, and a symlink and a file at one path conflict either way. Directory removal (needing emptiness), rename and link (cross-path effects) and xattr (keyed by path and name, several per path) merges through the pipeline, and per-range identity within a mixed file, are owed; carrying the last of these needs the engine to consume the whole ops document rather than one change per path.

Owed (the rest of Phase 6): the deriver's interaction edges (a file/directory/symlink/link transition at one path, a metadata or link then a rename of that path, symlink rename, a write through a hard link, and the rare reused-base-path rename combination above), each a typed refusal today; the remaining namespace merges through the engine (directory removal needing emptiness, rename, link and xattr — directory creation, mode and symlink are in now; xattr and the cross-path ops need the engine to consume the whole ops document); per-range identity in the engine's verdict; the checkpoint folding above; the fenced ledger-register commit and holder recomputation in a fleet (the single-node commit is an in-memory chain append; the fleet register, mirror and reconfiguration protocols are now simulated, §8h); these build on the
green-volume data model (§4.5's journal is in place; the fleet version chain is not yet).

## 8g. Phase 7 groundwork (2026-09-05)

The archive format — the pure core of D-17 and §2.6 of `research/compression-archive-dedup.md` — landed 2026-09-05 as `slates-archive` (`crates/archive`), ahead of the rest of Phase 7, because the container is a self-contained, in-RAM byte format that needs none of the codec, dedup or CDC work to be correct and directly confirmable, and slates never writes it to disk (R1). An archive is one streamable, content-addressed byte sequence: a fixed header (magic, major/minor, flags, page and chunk-size parameters, the manifest's BLAKE3, chunk count, raw and stored byte totals, volume and snapshot ids, the name-policy id and Unicode version), the chunk records in manifest order (each a BLAKE3 identity, raw and stored lengths, an encoding and level, a dictionary identity, and the payload), the manifest bytes, a seek table (a zstd skippable frame mapping each chunk's identity to its offset), and a trailer (the section index, the whole-archive BLAKE3, and the tail magic). `wire.rs` is a bounds-checked sequential reader/writer, so no byte offset is a literal and a short read is a typed refusal; `encode` is deterministic (the same snapshot yields the same bytes), and `decode` verifies the magic, the major, the whole-archive hash (catching truncation or any alteration), each chunk's identity, and the manifest's identity; `chunk_by_identity` locates one chunk through the seek table without scanning. Every malformed stream is a typed `ArchiveError` (bad magic, unsupported major, unknown required flag, truncated, bad trailer, archive-hash mismatch, chunk-identity mismatch, manifest-hash mismatch, bad seek table), never a panic.

Gated (`crates/archive/tests/archive.rs`, 11 tests, every host): an archive round-trips (T-7.1), encoding is deterministic, the seek table finds a chunk by identity (and reports an unknown one absent), a chunk that fails its identity is refused and named (AC-7.3), a bad magic and an unknown major are refused, a flipped body byte is caught by the whole-archive hash, a short stream is refused; and three hostile proptests (T-7.4): every truncation is refused, any single byte flip is caught (never the original snapshot decoded), and arbitrary bytes never panic. The LZ4 codec landed the same day (D-17's probe and hot-path codec, `lz4_flex`, pure Rust and safe-only so the unsafe budget stays 0): `compressed_chunk` compresses a chunk and keeps the LZ4 form only when it is smaller than the raw bytes (the format-derived floor), else stores raw; `content` and the reader decode an LZ4 chunk to its declared raw length and verify the decoded bytes against the chunk's identity, refusing an undecodable payload with a typed `BadPayload`. Two more tests (T-7.3): a repetitive blob is stored LZ4 and shrinks and round-trips, and a tiny unique blob stays raw. Owed: zstd with static contexts and the calibrated cost model (the Btrfs sampler, the LZ4-to-zstd regression, per-volume observation), dictionaries (FastCover training, identity, embedding, GC), the background identity pass and the per-shard hash-prefix partitioning and server wiring of the content index, FastCDC for the measured large-file class, the manifest tree's structure, the export stream through the SDKs/MCP, and restore. zstd is the one piece that pulls a cross-compiled C toolchain (`zstd-sys`); LZ4 and BLAKE3 need none.

The content-addressed store with deduplication landed the same day (`store.rs`), the pure core of Phase 7 task 1's content index. Chunks are keyed by their BLAKE3 identity, so identical chunks fold to one stored copy with a reference count; `unique_bytes` (the distinct chunks' stored bytes) shrinks as duplicates fold in while `referenced_bytes` (the undeduplicated total) is unchanged — the accounting the design's worked example names; `release` evicts a chunk on its last reference. Gated (`crates/archive/tests/store.rs`, 4 tests, every host): identical chunks deduplicate with exact accounting, the two-clones-share-an-output worked example, eviction on the last release, and a property test over random chunks with duplicates (byte-exact reads, exact accounting, a non-vacuity check that deduplication shrank the count). The per-shard hash-prefix partitioning, the background identity pass on idle time, and the server wiring are the runtime integration (owed).

The manifest tree landed the same day (`manifest.rs`), the archive's canonical, sorted, Merkle-hashed directory tree (§2.6 item 4). A directory node holds its entries (a name and a child) sorted by name; a file node holds its extent list (offset, length, chunk identity, chunk offset), a hole a zero-chunk extent. Each node's BLAKE3 identity is a Merkle hash over an encoding that names its children by identity, so the root identity fingerprints the whole tree and any change to any node changes it. Two canonical, deterministic encodings: the Merkle encoding (children by identity) defines the identity; the tree encoding (children inlined) is stored and parsed back through the bounds-checked reader, a truncated or malformed tree a typed `ManifestError`. Gated (`crates/archive/tests/manifest.rs`, 8 tests, every host): a tree round-trips, the identity and encoding are independent of entry order, changing a leaf changes the root, a file and a directory differ, a truncated tree and an unknown kind are refused, arbitrary bytes never panic, and generated trees round-trip with stable identities. The archive container now embeds the tree: its manifest section stores the tree's canonical encoding and the header's manifest hash is the tree's Merkle root, which `decode` recomputes and verifies (the archive's 13 tests use a `Node` manifest). Per-node metadata landed the same way: each directory `Entry` carries a `NodeMeta` (inode number, mode, modification and change times in nanoseconds, size, hard-link count, and an xattr-present flag — the field list of §2.6 item 4), written into both encodings and hashed into the Merkle identity, so a change to any entry's mode or times changes the root identity as a content change does; the archive carries, hashes and round-trips it, while applying it to a host path is the landing engine's job under a grant (§4.15). This bumped the format minor to 1 (a v1.0 and a v1.1 tree of the same shape have different roots). Two more tests (`crates/archive/tests/manifest.rs`, now 10): the metadata round-trips through the canonical encoding, and a mode change changes the root identity while identical metadata gives one identity. The root directory has no naming entry, so its own metadata is not carried; every named node's is (owed only for the root, a minor gap).

Restore landed the same day (`restore.rs`): reconstruct a volume's files from an archive by walking the manifest tree and resolving each file's extents against the chunks — a normal extent reads its bytes from the named chunk (decoded and identity-verified), a zero-chunk extent is a hole of zeros, a multi-extent file concatenates a sub-range of each chunk. An extent naming a chunk the archive does not hold is a typed `MissingChunk` refusal. Gated (`crates/archive/tests/restore.rs`, 7 tests, every host): a file restores to its bytes, a hole restores as zeros, a multi-extent file concatenates its chunks, a tree restores files by path and records directories, a missing chunk is refused, restore works after an encode/decode round trip, and an LZ4-compressed chunk restores correctly. The eager whole-tree restore is here; lazy restore (attach after a metadata-only pass, decompress on first read, AC-7.4) is the runtime's job on top of it (owed).

Resumable transfer by the missing set landed the same day (`transfer.rs`; §2.6): the receiver reports the chunk identities it holds, `missing_set` returns the archive's chunks not in that set (sorted, so a resumed transfer computes the same set), and `chunks_for` packages exactly those — the basis of replication and clone-from-archive. Gated (`crates/archive/tests/transfer.rs`, 3 tests, every host): the missing set is everything with nothing held and empty with all held; a receiver holding a previous version's chunk receives only the changed one and combining held with shipped restores the whole archive; duplicate chunks ship once.

## 8h. Phase 8 groundwork (2026-09-05)

The fenced ledger register landed 2026-09-05 as `crates/db/src/ledger.rs`, the register over time built on Phase 2's f=0 register primitives (§8d task 7, `register.rs`), ahead of the rest of Phase 8, because the protocol — §4.8 "Promotion and takeover" and §4.16's "merge record as a fenced ledger entry; holders recompute" — is a pure, deterministic state machine that needs none of the networking, membership or server work to be correct and directly confirmable, and §10 already names the implementation's proof as "the Rust simulation harness with the same invariants encoded as checks (AC-8.1, T-8.15)." A register is not one value but a growing log. A `Cohort` owns the object's `2f+1` candidate holders (from the rendezvous placement of `register.rs`) keyed by host id, each a `Fence` (the highest host epoch it has accepted under) and a dense log of `Record`s (an epoch and a payload identity). An `Owner` is a small proposer view — its host id, the epoch it proposes under, and its log — that acts against a cohort by `&mut` (ownership by handle, sharing by move, no `Arc`, R2). `propose` appends a record at the next position, offers the whole log to the reachable holders under the owner's epoch (each reconciles: the agreeing prefix skipped, divergent or missing positions overwritten or appended, a stale tail dropped), and commits the moment `f+1` acknowledge; a superseded owner, its epoch below the holders' fences, is refused by every fenced holder and so reaches at most `f` — it never commits. `take_over` is the Vertical Paxos II recovery round: it fences and reads a reachable quorum (refused `NoQuorum` below `f+1`, rather than splitting the register), bumps the epoch to the successor of the highest any holder has seen (a structural increment, as `2f+1` and `f+1` are — not a tuning constant), and adopts, per position, the record carried under the highest epoch it read. Safety is quorum intersection: a committed record reached `f+1` holders and the read quorum is `f+1`, so the read intersects every commit quorum and sees every committed record; no later owner ever proposes a different record at a committed position (adoption keeps the committed value there), so the highest-epoch record at a committed position is the committed one. A record acknowledged by fewer than `f+1` holders before a takeover has not committed and may be adopted or dropped — either is safe, because the owner that proposed it is fenced and no reader saw it commit (the Continuity subtlety the `FencedRegister` model exposed, §10). It is a pure simulation (R8): holders are in memory, a message is a direct call, so one body of code is the laptop (`f=0`: one holder, a commit of one, the local append) and the fleet (`f>0`), with no network, no clock, no randomness, no mode switch.

Gated (`crates/db/tests/ledger.rs`, 10 tests, every host): three proposals to a full cohort commit and read back as the committed prefix (T-8.15); `f=0` is the local append (one candidate, a commit of one — the same code as the fleet, AC-8.1); a minority partition still commits (two of three) and a majority does not (one of three); a takeover is refused below a quorum (`NoQuorum`); a takeover fences the old owner, bumps the epoch, and adopts the committed prefix; a superseded owner never commits while the new owner does, and no position diverges (StaleNeverCommits); a partial write acknowledged only by the owner is dropped by a takeover whose read quorum lacks it, while every committed record survives (Continuity); and the commit outcome is the same at f=0 and f=1 (the N=1 differential). The proptest oracle drives arbitrary histories of proposals, partitions and takeovers at f in {0,1,2} — generated as plain tuples so the strategy needs no `Arc`-backed `prop_oneof` — with a guaranteed leading full-cohort commit for non-vacuity, and asserts Agreement (no position holds two quorum-agreed identities), NoLoss and TotalOrder (the committed prefix only extends and never rewrites a committed value), and Continuity (a takeover adopts at least the committed prefix) after every step. These are the properties the `FencedRegister` TLA+ model proved (§10); the simulation is their implementation-side proof.

Asynchronous mirroring landed the same day (`crates/db/src/mirror.rs`), the shipper of D-18's durability statement ("committed records reach the mirror region asynchronously in epoch order with a measured, exposed lag; an operation that needs them there awaits the mirror scope"). A `Mirror` is a second `Cohort` in another failure domain with its own writer; `ship` replays the home region's committed prefix onto it through `Owner::replicate` — offering the authoritative prefix whole, so it catches up a returned holder and appends the new tail without rewriting a committed position (idempotent and resumable). Replaying the committed prefix is epoch order (positions commit in order, epochs never decrease), so the mirror commits a dense prefix and never a record the home did not, and it can lag but never diverge. `lag` exposes the count of records committed at home but not yet mirrored, and `placed_through` is `await placed(mirror)` for a record. This closes the `DurabilityScope::Mirror` that `register.rs` declared but refused at f=0. Gated (`crates/db/tests/mirror.rs`, 6 tests, every host): a reachable mirror catches up and the lag falls to zero; `await placed(mirror)` follows shipping; a minority partition of the mirror's own holders still ships (a quorum reachable) and a majority holds the lag then resumes on the next ship after healing (replaying the same prefix); mirroring tracks new home commits incrementally; and f=0 is a single mirror holder (the same code as a fleet mirror, R8). `Owner::replicate` is the primitive the healer will reuse to bring a lagging holder current.

Reconfiguration landed the same day (`crates/db/src/reconfig.rs`), the faithful port of the `Reconfig` model (§10): changing a register's holder set from an old configuration to a new one while the owner keeps writing, Raft's joint consensus / Vertical Paxos I made concrete. Three phases — old (commit at a majority of the old set), joint (after `announce`, commit at a majority of the old set *and* the new set, both active, a departed holder never accepting again and a fresh one starting empty), and new (after `retire`, a majority of the new set) — with state transfer (`transfer` copies the newest committed record into a lagging new-set holder) and a retirement gate (only after the owner acknowledged the change and the newest committed record is held by a majority of the new set). The register here is a single newest-wins value (a head, a chain version, a lease), the shape the model checks, distinct from the growing log of the fenced ledger register. Gated (`crates/db/tests/reconfig.rs`, 7 tests, every host): the old phase commits at an old majority; a departed holder cannot accept; the joint phase needs both majorities; state transfer carries the newest to a fresh holder; retirement is gated (refused outside joint and before owner-ack); a full reconfiguration preserves the joint-committed record (NoLoss, non-vacuous); and a proptest oracle over arbitrary histories of issue/accept/announce/owner-ack/transfer/retire (tuple-generated, no `Arc`-backed `prop_oneof`) asserts ReadSafety (every read majority — of whichever configuration a reader knows, which may lag one phase — holds a record at least as new as every committed one, computed by enumerating the majorities) in every reachable state and NoLoss once retired. These are the two properties the `Reconfig` TLA model proved; the simulation is their implementation-side proof.

Owed (the rest of Phase 8): wiring the register into the server's put path (hedging content to `f+1` with tied requests to the rest after the measured p95, the acknowledging set recorded in the head record), the healer and probation for a lagging or returned holder (over `replicate`), driving the mirror shipper from the runtime on a cadence with a real cross-region transport and a time-based lag, migration of ownership on a write-intent attachment, the SWIM membership feeding the neighbourhood and host epochs, and the regional configuration consensus group (the hecate Raft dialect) that carries the membership decisions the fenced register, mirror and reconfiguration simulations already implement at the protocol level (assigning epochs at takeover, announcing and retiring configurations); the auto-seal cadence constants remain measured in Phase 8 (D-O12). The simulations stand in for all of these at the protocol level; the runtime carries the same code with a real transport.

## 9. Blocking order toward first light

Phase 0 (foundations) → Phase 1 (volume core) → Phase 2 (server, database, IPC) → Phase 3
(Linux bridge). First light = a tool running against a volume on Linux through `slates exec`.

## 10. Model-checking record (A-6)

| Model | Configuration | Result | States | Depth | Date |
|---|---|---|---|---|---|
| `models/Reconfig.tla` | Old {a1,a2,a3} → New {a2,a3,a4}, three records | no error (ReadSafety, NoLoss, TypeOK) | 20,478 distinct | 19 | 2026-09-04 |
| `models/FencedRegister.tla` | 3 holders, 2 hosts, 2 epochs, 2 records per epoch | no error (TotalOrder, Continuity, StaleNeverCommits, ReadSafety, TypeOK) | 1,432,929 distinct | 27 | 2026-09-04 |
| `models/FencedRegister.tla` | 3 holders, 2 hosts, 3 epochs, 2 records per epoch | not completed: stopped after 3 h 7 min with 83 GB of queued states on disk; needs symmetry reduction (TLC symmetry sets over Acceptors and Hosts) and a bounded record alphabet before it is feasible; the two-epoch result stands (one takeover plus a resumed stale owner) | — | — | 2026-09-04 |

The models are architecture artifacts, not CI jobs: this is a Rust project, and the
implementation's proof is the Rust simulation harness with the same invariants encoded as checks
(AC-8.1, T-8.15). Re-run TLC only when the protocol in §4.8 changes, and always with `-metadir` pointing outside
the project tree: TLC's disk-backed state queue otherwise lands in `docs/wip/models/states`, which
is what happened on 2026-09-04 (83 GB, removed).

Two modelling bugs were found and fixed before the runs passed: the first draft let an owner issue two
different records for one sequence number (a model error, not a design error), and its Fencing
invariant was stronger than Paxos promises (a record partially acknowledged before a promotion may
still complete; the correct property is Continuity: the successor's base is at least as new as any
such record). Both are recorded so the implementer knows exactly what the models guarantee.
