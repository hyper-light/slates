# docs/wip — work-in-progress architecture for slates

This directory holds the design work for slates: a hermetic, purely in-memory, copy-on-write
virtual filesystem service written in Rust that coding agents provision on demand.
`SLATES_DESIGN.md` is the authoritative design even while it lives under `wip/`. Research
notes are evidence and history, not competing specifications. Implementation status is explicit.

## Read this first

- `SLATES_DESIGN.md` — the unified design and phased implementation plan (the deliverable; v3 of 2026-09-05 integrates A-1 through A-9 as listed in the design, including the Hecate contract correction, with the amendment log kept as history).
- `GAPS.md` — the gap ledger: what is specced, what is open, what is owed, armed tripwires.
- `ARCHITECT_NOTES.md` — running notes taken while reading the research (inputs, not decisions).
- `models/` — TLA+ models of the fenced register and of holder-set reconfiguration with TLC configurations; architecture artifacts, checked on 2026-09-04 with results in `GAPS.md` §10; A-9 changes the required §4.8 contract, so refinement/revalidation is owed before closure; no checker was run or installation authorized. Never CI jobs.

## Research (evidence for every decision)

"Complete" below means that dated reading note was written. It does not mean the implementation
is complete or that external API facts and measurements have been revalidated. A-9 notes at
the top of affected files point to the corrected current contract. Hecate at `103c078` contains
design documents, not a deployed implementation.

| File | What it covers | Status |
|---|---|---|
| `research/survey-sylk-vfs.md` | sylk's Go CoW VFS layers, chunk arena, brokers, WAL, skills; KEEP/AVOID/IMPROVE | complete |
| `research/survey-sylk-docs-corpus.md` | verbatim helper report over sylk's VFS design docs | complete |
| `research/hecate-contract-review.md` | A-9: virtio-fs, retained bases, claims, consumer authority, protocol evidence and all carried contracts | source review 2026-09-05; implementation open |
| `research/survey-hecate.md` | hecate's ratified decisions, vocabulary, spec style; what it left open | complete |
| `research/survey-hyperscale.md` | hyperscale's SWIM/Raft/WAL/backpressure patterns; PORT/ADAPT/AVOID; constants table | complete |
| `research/survey-hyperscale-architecture-ranges.md` | verbatim helper reports over hyperscale `architecture.md` line ranges | complete |
| `research/survey-vorpal.md` | vorpal's Rust conventions, target matrix, Arc policy, packaging, tests | complete |
| `research/os-filesystem-bridge.md` | FUSE / NFS loopback / WinFsp evidence; mount model; chosen-path rules | complete |
| `research/cow-data-structures.md` | namespace, snapshots, content, concurrency, dedup, accounting, metadata | complete |
| `research/low-latency-ipc-and-runtime.md` | latency budget, rings, wake primitives, rendezvous, runtime decision, fast path | complete |
| `research/database-design.md` | indexes, transactions, reclamation, RAM durability, consensus, wire, testing | complete |
| `research/edenfs-scale-distribution.md` | EdenFS/CitC, metadata services, leases, lazy attach, replication, scale targets | complete |
| `research/memory-and-system-awareness.md` | pages, faults, RAM-only guarantees, pressure signals, topology, calibration, allocators | complete |
| `research/compression-archive-dedup.md` | zstd/LZ4, dictionaries, cost model, dedup, BLAKE3, archive format | complete |
| `research/testing-and-benchmarking.md` | conformance, workloads, property/model tests, concurrency, chaos, benchmarking, taxonomy | complete |
| `research/mcp-skills-sdks.md` | MCP 2026-07-28, skills spec, Python and TypeScript SDK architecture, rendezvous, API conventions | complete |
| `research/arc-free-rust-architecture.md` | ownership policy, executor, drivers, bridge FFI, unsafe policy, cross-platform constraints | complete |
| `research/disk-source-of-truth.md` | overlay volumes over host directories, witnessed bases, drift, landing under a human grant; hecate's merge and landing rules read directly; per-OS primitives (swap, temp files, reflink, sync, containment, watchers, passthrough) | complete |
| `research/merge-engine.md` | hecate's merge architecture (green, increments, canonical rebase, the two-pass verdict, placed-before-referenced, appliers recompute, submission transaction, streaming gate) quoted from its specs; the fifteen integration gaps and how slates closes each; theory and precedents | complete |
| `research/metadata-replication.md` | authority and durability for heads, chains, leases and the catalog from laptop to multi-datacenter: Vertical Paxos II with copyset neighbourhoods; primary sources read for FaRM, RAMCloud, Ceph, BookKeeper, Kafka, Chubby, PNUTS, CockroachDB, Hermes, Paxos Quorum Leases, Copysets; the A-6 proposal and its downsides | complete |

Some sections of the topic research files were written serially by the architect after the
research agents were stopped; those sections say so and cite what was fetched. Items marked "from
memory" or "verify" are listed in each file's risks section and in `GAPS.md`.

## Evidence tiers used everywhere

- **A** — well-cited peer-reviewed paper or PhD thesis.
- **B** — textbook, standard (RFC, POSIX), or official kernel/OS/vendor documentation.
- **C** — widely deployed implementation and its design documents or source.
- **D** — blog post or individual benchmark; gap-filler only, always flagged.
- **M** — measured on the author's machine during this design session (Darwin 25.4.0 arm64,
  18 cores, 128 GiB); an illustration of what the daemon must measure at boot, never a constant.

## Hard rules the design obeys

1. RAM only, and disk is the source of truth. The software reads the host directory a volume
   overlays and writes a host path only inside a landing a human granted (amendment A-4); it
   never uses `/tmp`, never creates on-disk sockets, symlinks, or mount-point directories if any
   alternative exists, and never writes anywhere else.
2. No `Arc`. Atomic reference counting is runtime overhead; any unavoidable use is documented in place.
3. No magic numbers. Every parameter is measured from the machine or the data and derived by a
   stated algorithm.
4. Maximal correctness, robustness, performance, scalability, efficiency, and speed; complexity is
   acceptable when the evidence justifies it.
5. Tests exercise use and functionality, never static checks of file locations or values.
6. Everything is async: server, database, libraries where sensible, and both SDKs.
7. Plain English. A human implementer must be able to act on every sentence.
