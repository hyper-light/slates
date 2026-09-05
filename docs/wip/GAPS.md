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

- Security spec (principals, access lists, audit counters) beyond §4.13 prose — owed before Phase 2.
- Observability spec (span roster, health signal catalog, metric names) beyond §4.14 prose — owed before Phase 2.
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

Open in Phase 1 (owed in this phase, in order): AC-1.2 the differential harness against tmpfs
with the reviewed equivalence policy (`docs/wip/EQUIVALENCE.md`, Linux CI lane; a macOS RAM
disk is a system-state change Ada has not authorized, so the local run skips loudly); task 14
the deriver; task 10 `slates-base`; tasks 11–13 `slates-land`, its oracle and baselines.

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
