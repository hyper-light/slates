# The gap ledger (authoritative, kept current in the same change as any acceptance or tripwire)

Rubric per item: research on file? spec section exists? test matrix? acceptance criteria? laptop
degenerate stated? open decisions named? Classification: `undesigned | designed-unspecced |
specced-untested | decision-open | drift (owed-and-forgotten)`. A stale ledger is itself a gap.

## 0. The one global fact

- Phase 0 is in progress (2026-09-04): the workspace exists with the lint wall, the structural
  test and the literal check (`cargo xtask check`), and `slates-machine` measures the boot profile
  (BENCHMARKS.md records the first baseline). No other crate exists; nothing below is closed by
  code until its phase says so.

## 1. Component inventory

| Subsystem (design §) | Research on file | Spec section | Tests named | ACs | Laptop degenerate | Status |
|---|---|---|---|---|---|---|
| Machine profile (4.1) | memory-and-system-awareness.md | yes | T-0.* | AC-0.3, 0.5 | yes | specced-untested |
| Memory (4.2) | memory-and-system-awareness.md; arc-free-rust-architecture.md | yes | T-0.1, 0.2, 0.4, 0.5 | AC-0.4, 0.5 | yes | specced-untested |
| Runtime (4.3) | low-latency-ipc-and-runtime.md; arc-free-rust-architecture.md | yes | T-0.3, 0.6-0.9 | AC-0.6-0.9 | yes | specced-untested |
| Volume lifecycle (4.4) | cow-data-structures.md; edenfs-scale-distribution.md | yes | T-1.*, T-2.* | AC-1.*, AC-2.4 | yes | specced-untested |
| Namespace and content (4.5) | cow-data-structures.md | yes | T-1.* | AC-1.* | yes | specced-untested |
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

## 8. External dependencies and port hazards

- WinFsp (GPLv3 with FLOSS exception or commercial license) installed by the user on Windows.
- Linux kernel features by version (5.10 baseline; 6.1; 6.14).
- macOS 14.4 minimum (`os_sync_wait_on_address`); macOS 26 for FSKit URL resources; Apple Developer ID signing, the FSKit entitlement, and an app group for the macOS bundle; Apple's yearly FSKit protocol changes.
- C toolchains for `zstd-sys` on all nine targets.
- The merge engine (A-5) depends on nothing external: fixed-layer ops documents use `slates-wire`; the fleet parts use the consensus group already chosen. Read directly from hecate on 2026-09-04: `MERGE.md`, ADR-0003, ADR-0005, `SERVING.md` §2-§4, `VFS.md` §5, `CONSENSUS.md` §6; hecate's own contradiction on the deriver (`SERVING.md`/`VFS.md` diff versus `MERGE.md` never-diff) is recorded in `research/merge-engine.md` §2 and resolved for never-diff.
- Base and landing primitives (A-4): Linux filesystems with `RENAME_EXCHANGE` and `O_TMPFILE` (ext4, XFS, Btrfs, tmpfs; others fall back); `openat2` (5.6, under the floor); FUSE passthrough only with `CAP_SYS_ADMIN` (6.9+); macOS `RENAME_SWAP` and `clonefile` by volume capability (APFS); Windows 10 1607+ NTFS for POSIX-semantics rename; ReFS for block clone. Items marked "verify" in `research/disk-source-of-truth.md` §7 (batched `statx`, `NtQueryDirectoryFile` classes, `FlushFileBuffers` on directories, fanotify marks, reparse-tag checks, the timestamp-granularity table, `FSCTL_SET_SPARSE`, `F_PREALLOCATE`) are owed verification in Phase 1 and Phase 4.

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
