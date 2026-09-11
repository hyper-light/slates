# slates — unified design and phased implementation plan

Status: DESIGN v3, 2026-09-05. This is the target contract, not a release or a claim that all
acceptance criteria pass. Source review at `a1059ed` finds foundations, a volume core, local
metadata/IPC/client/CLI, part of the Linux bridge, and pure merge/archive/protocol components.
The current implementation does not yet establish strict locked-memory admission, content
recovery across daemon restart, complete mounted POSIX behavior, authenticated isolation
between agents sharing a uid, guest attachments, or fleet safety. MCP, SDKs and native
macOS/Windows bridges remain planned. The ledger's §8i records the blocking gaps, including
source-level correctness findings in the FUSE ABI and takeover protocol.

A-9 corrects the contract using Ada's requirements and the sibling Hecate specifications.
[The contract review](research/hecate-contract-review.md) distinguishes source evidence from
measured results; [the audit](../bugs/2026-09-05-system-contract-audit.md) records the concrete
findings. No implementation test, benchmark, mounted workload or model checker was run for
this documentation amendment. Earlier measurements and model runs retain their original dates
and scope; they do not validate A-9 or close the new regressions.

This version integrates A-1, A-2, A-4, A-5, A-6, A-7, A-8 and A-9 into the body. Existing
numbered identifiers remain stable. The amendment log is history; the body is authoritative.
The gap ledger is [GAPS.md](GAPS.md); measurements are [BENCHMARKS.md](BENCHMARKS.md).

Contents: Part 0 how to read; Part 1 what slates is; Part 2 the shape of the system; Part 3 the
decision ledger (D-1 … D-27); Part 4 subsystem designs (4.1 … 4.16); Part 5 the phased plan
(Phases 0 … 9); Part 6 tests, benchmarks, CI gates; Part 7 open questions and risks;
Appendices A (bibliography), B (conventions), C (cross-platform constraints).

---

## Part 0 — How to read this document

### 0.1 Who this is for and how to use it

This document is written for the engineers who will build slates. It is one document, read
front to back once, then used as a reference. It has three kinds of content:

1. **Decisions** (Part 3). Every decision has a number (D-1, D-2, ...), a one-sentence statement,
   the evidence it rests on, the alternatives that lost and why, and the things that must be
   measured at runtime instead of assumed. If you disagree with a decision, argue with its
   evidence, not with the sentence.
2. **Design** (Part 4). One section per subsystem. Each section states its data model, its state
   machines, the step-by-step algorithms with the derivation of every constant inline, its failure
   and recovery matrix, its closed list of refusals, a worked example including a failure, its
   laptop-degenerate statement, and its integration points.
3. **Plan** (Parts 5 and 6). Phases in build order. Each phase says what is in and out of scope,
   the ordered tasks, worked examples, numbered acceptance criteria, and the test cases that prove
   them, including the error, edge, property, concurrency, fault, chaos, and benchmark cases.

Nothing in this document is a placeholder. Where a fact is unknown, the document says "unknown"
and names the measurement or experiment that resolves it.

Reading order for a first pass: Parts 0 to 3, then §4.1 to §4.12 in order, then §4.15 (the base
plane and landing) and §4.16 (the merge engine), which are data-path subsystems, and only then
§4.13 (security) and §4.14 (observability), which are cross-cutting. The section numbers are
stable identifiers cited from code, tests and the ledger, so the two cross-cutting sections keep
their numbers.

### 0.2 The rules this design obeys, and how each is enforced

| # | Rule (from the commissioning brief) | How the design enforces it, not just promises it |
|---|---|---|
| R1 | RAM only, and disk is the source of truth. Our software reads the host directory a volume overlays, writes a host path only inside a landing a human granted, and otherwise never writes to disk, never uses `/tmp`, never creates on-disk sockets, symlinks, or mount-point directories if any alternative exists. | The volume core crates forbid `std::fs`, `std::net`, and every file-creating syscall by a compile-time lint wall; the only crates that may name a host path are the per-OS bridge and rendezvous crates, the read-only base crate, and the landing crate, and each such site is listed in one table with its justification. A structural test walks the dependency graph and fails the build if a forbidden symbol appears elsewhere, and it fails if any write-capable file syscall is linked outside the landing crate. The hermeticity tracer run (Part 6) proves zero writes outside granted targets. |
| R2 | No `Arc`. Atomic reference counting is runtime overhead; any unavoidable use is documented in place. | `Arc` and `Rc` are denied by lint across the workspace except in named FFI edge modules; every allowed site carries a comment stating the two owners that force it. Ownership is by arena and generational handle; sharing is by move over bounded channels or by immutable epoch-published roots. |
| R3 | No magic numbers. Every parameter is measured from the machine or the data and derived by a stated algorithm. | Every constant lives in one derivation table per subsystem with columns Constant, Formula, Anchors (the measured inputs). A bare numeric literal in a tuning position fails review; CI greps for them. The daemon builds a machine profile at boot and every derived value is logged with its inputs. |
| R4 | Maximal correctness, robustness, performance, scalability, efficiency, speed; complexity is acceptable when the evidence justifies it. | Each decision cites tiered evidence. Performance floors ratchet from the first CI baseline and may only tighten. Correctness is proven by conformance suites, model-based tests, linearizability checks, and deterministic simulation, not by review alone. |
| R5 | Tests exercise use and functionality, never static checks of file locations or values. | The test taxonomy (Part 6) has no category for "assert this file exists" or "assert this constant equals". Every test drives the system through its public surface (a mount, an SDK, MCP, a wire message) and asserts on observable behaviour. |
| R6 | Everything is async: server, database, libraries where sensible, both SDKs. | The runtime is thread-per-core with completion-shaped I/O drivers; every server and database operation is a future; the Python SDK exposes `async` methods on any event loop through file-descriptor readiness; the TypeScript SDK returns Promises and async iterators. Synchronous facades exist only as thin wrappers. |
| R7 | Plain English. A human implementer must be able to act on every sentence. | Terms of art are defined once in the glossary (0.4) and used consistently. Each algorithm is written as numbered steps. Each test case is written as "do X, expect Y". |
| R8 | Both laptop and EdenFS scale, one code path. | Every subsystem carries a "laptop degenerate" section: the laptop is the derived N=1 case of one formula family, never a mode switch. A named differential test asserts that the N=1 configuration has the same observable semantics as the fleet configuration. |
| R9 | Sub-50 µs provisioning. | The provisioning path is decomposed step by step with a cited cost per step and a per-step budget; the end-to-end histogram (p50, p99, p999, max) is a permanent CI gate. |
| R10 | Disk is written only on a user permission grant. | A grant is a database record created only by the CLI or a confirmation surface a human operates; the MCP server and the SDKs have no verb that creates one; the landing engine refuses without a grant bound to the exact manifest it is about to write; every grant, manifest, and outcome is in the audit log (§4.15). |

### 0.3 Evidence tiers and how citations are written

Every claim that could be wrong carries a bracketed citation with a tier letter:

- **[A]** a well-cited peer-reviewed paper or PhD thesis (venues such as FAST, ATC, OSDI, SOSP,
  NSDI, EuroSys, SIGMOD, VLDB, ASPLOS, PPoPP, PODC; ACM TOS, TOCS, TOPC; IEEE TPDS).
- **[B]** a textbook, a standard (RFC, POSIX), or official kernel/OS/vendor documentation.
- **[C]** a widely deployed implementation and its design documents or source, cited to the file.
- **[D]** a blog post or an individual benchmark. Gap-filler only; always flagged; never the sole
  support for a decision.
- **[S]** a local specification or source audit. Hecate at `103c078` is a documentation tree,
  not a deployed implementation; earlier Hecate citations labelled [C] are specification
  evidence [S], wherever they occur in this document. They establish intended contracts only.
- **[M]** a measurement made here, with its command, date, hardware and workload recorded.

Numbers always come with their measurement conditions (hardware, OS and kernel version, year).
When sources disagree, both are cited and the disagreement is stated. "Must be measured at
runtime" is written where it applies; it is a design finding, not a gap.

### 0.4 Glossary (plain English, one line each, in the order a reader meets them)

- **Volume**: a named, independently provisioned copy-on-write filesystem tree with its own
  quota, owner, version history, and access mode, whose base is either empty (a scratch volume)
  or a retained base reference (a live directory or an immutable snapshot). The unit an agent asks for.
- **Snapshot**: a frozen root with explicit coverage. `Complete` covers the whole logical tree;
  `DeltaWithLiveBase` freezes the delta and witnesses but retains a live base dependency. The
  latter is not an immutable view of untouched files. Root publication is O(1); flushing clients,
  capturing a base, hashing and placement are separately costed operations.
- **Base reference**: the retained identity and authority needed to resolve untouched entries:
  empty, an immutable snapshot root, or an enrolled live directory served by its host. A path
  string on another host is not the same base.
- **Base capture**: explicit construction of a complete immutable base. Atomic point-in-time
  capture requires a stable source (quiescence or a supported read-only snapshot facility);
  scanning an arbitrarily changing directory cannot establish that guarantee.
- **Clone**: a new volume whose first snapshot is an existing snapshot. O(1) to create; diverges
  by copy-on-write.
- **Copy-on-write (CoW)**: writing to shared data first copies only the piece being changed, so
  every older snapshot stays intact.
- **Chunk**: the unit of file content in memory. Immutable once sealed; identified by its content
  hash so identical chunks are stored once.
- **Manifest**: the table of contents of a snapshot: every path with its metadata and the list of
  chunks that make up its content.
- **Consumer**: an authenticated workload identity enrolled by a trusted human or harness;
  distinct agents may share one OS uid but must not inherit each other's VFS rights.
- **Attachment**: a binding of consumer, transport, access rights, view, lease generation and
  accounting. A snapshot attachment pins its version; a writable attachment follows its owned
  live head. Attach succeeds only after the requested serving path or device is usable.
- **Lease**: a time-bounded, renewable right of ownership carrying an epoch number (fencing token).
  Any write presenting a stale epoch is refused. Two kinds exist, both with epochs: the volume
  lease (this entry; a green volume's merge task holds one) and the landing lease (below). A
  "grant" is never a lease: it is a human's permission for one landing.
- **Epoch (fencing token)**: a monotonically increasing number stamped on a lease; it lets a
  resource reject a writer that paused and woke up after losing ownership.
- **Bridge**: a transport adapter over the same VFS operations: native host filesystem bridges
  or a virtio-fs device serving a Linux guest through FUSE-over-virtio.
- **Root mount**: the single kernel mount point per host (or per user) under which volumes appear
  as directories. Established once; a volume is then a metadata operation.
- **Chosen path**: the path at which an agent asked for a volume to appear. Honoured per OS by the
  rule set in Part 4.6 without writing to disk.
- **Daemon**: the slates server process on a host. Owns all volumes on that host, the bridge, the
  local metadata database, and the IPC rendezvous.
- **Shard**: one pinned OS thread with its own single-threaded executor and its own share of state.
  Shards never share mutable state; they exchange messages.
- **Handle (generational)**: a small copyable id (index + generation) that names an object in an
  arena. A stale handle is detected and refused; it can never dangle.
- **Arena**: a pre-sized region of memory from which objects of one kind are carved; freeing is
  returning a slot, never calling the system allocator.
- **Machine profile**: the set of quantities the daemon measures at boot (page size, cache line,
  cores, memory, wake latency, memcpy and hash bandwidth, and so on) from which every tunable is
  derived.
- **Quota**: a limit on a volume's logical referenced bytes, distinct from physical capacity.
- **Claim**: an admitted entitlement backed by usable, prefaulted, locked capacity on each
  serving/holding host, including the costs of honoring that entitlement (§4.2).
- **Bounded volume**: a volume whose full quota is backed at creation; other volumes, caches
  and snapshots cannot consume its remaining entitlement. Writes past quota return ENOSPC.
- **Dynamic volume**: a volume whose reservation grows in measured increments against the host's
  measured free memory and pressure signals, with a stated maximum.
- **Archive**: a sealed, self-verifying byte stream of a snapshot (manifest plus chunks), held in
  RAM compressed or handed to the client. Slates itself never writes it to disk.
- **Scratch volume**: a volume with no base; every entry lives in memory. After its first landing
  it becomes an overlay volume over the directory it landed into.
- **Overlay volume**: a volume whose base is an existing directory on the host's disk. Untouched
  entries are read from disk on demand; only diverged entries live in memory.
- **Base**: the host directory an overlay volume sits on. The disk is the source of truth for it;
  slates reads it and never writes it outside a granted landing.
- **Witnessed base**: what an entry looked like on disk when the volume first changed it: a stat
  fingerprint (device, inode, size, mtime, ctime) plus the content hash. The third input to every
  landing verdict.
- **Copy-up**: the first write to a base-backed file, which records the witnessed base and gives
  the volume its own copy of the changed bytes.
- **Whiteout**: an overlay entry meaning "this base entry is deleted here".
- **Drift**: the disk changing under a witnessed entry. Reported as a typed event, never silently
  adopted into the agent's view.
- **Landing**: writing a snapshot's diverged entries into a host directory under a grant, after a
  per-entry verdict against the witnessed base and the disk as it is now.
- **Verdict**: the pure per-entry decision at landing: apply, skip, accept by identity, or
  conflict. Never a merge.
- **Grant**: a human's permission for one landing: this snapshot into this directory, once or for
  the session, bound to the landing manifest's hash.
- **Landing manifest**: the exact list of what a landing will do to disk, entry by entry, with
  hashes; what a grant is bound to; what the audit log records afterwards.
- **Landing lease** (hecate calls it the materialization lease): the single-holder lease on a
  target directory during a landing, carrying a fencing generation.
- **Green volume**: a shared volume whose only writer is its merge task. It is a numbered chain
  of versions; readers attach to a version and never see it move until they ask.
- **Work volume**: a clone of a green version that one agent writes; its declared operations
  since that version are what it submits.
- **Version**: one snapshot in a green volume's chain, numbered, immutable, produced by exactly
  one accepted increment (or the origin).
- **Declared operation**: a mutation as it was declared at the boundary: a write with its offset
  and length, a truncate, an insert or delete from the SDK, a create, unlink, rename, link or
  mode change. Never inferred by comparing file states.
- **Increment**: a constant-size description of a work volume's composed declared operations
  since its base version, naming the sealed post-state and the ops document by identity. Never
  carries bytes.
- **Ops document**: the flat array of fixed-size operation records an increment names.
- **Canonical delta**: the ops document of an accepted increment, kept per version so later
  increments can be mapped through it; old deltas fold into exact checkpoint deltas.
- **Position map**: moving an increment's ranges through the size changes of every canonical
  delta between its base version and the head, one direction only; maps compose.
- **Conflict window**: the exact byte range on each side, with the version that landed the
  other side, returned when two operations overlap. Evidence, never markers.
- **Basis**: the green version a work volume was cloned from plus the paths it has touched.
  Used to skip work when nothing overlaps; never used to decide a conflict.
- **Configuration master**: the regional consensus group that decides membership,
  neighbourhoods, host epochs, takeovers and homes; touched on those events, never per write.
- **Host epoch**: the fencing number a host writes on every record and content put; holders
  refuse anything below the highest epoch they have seen for that host; bumped by the
  configuration master at takeover.
- **Neighbourhood**: the fixed set of hosts, chosen across failure domains, from which a host's
  candidate holders are drawn; its size is the scatter width, which bounds the copyset count.
- **Candidate holders**: the 2f+1 hosts, the owner among them, that a replicated object may land
  on; a write commits at f+1 acknowledgements from any of them.
- **Register**: the per-object record that the owner alone writes under its host epoch (a
  volume head, a landing lease, a catalog entry); a chain is a register written in sequence.
- **Mirror region**: the region that receives every committed record and its content
  asynchronously in epoch order; the lag is measured and exposed.
- **Durability scope**: what `await placed` waits for: `region`, the home commit, or `mirror`,
  the mirror's commit as well; chosen per operation, never per volume.
- **Failure domain tree**: the containment hierarchy (thread, process, host, rack, zone, region)
  against which replication and placement are expressed. A laptop is a depth-one tree.
- **Laptop degenerate**: the configuration a formula family produces when the failure domain tree
  has one node. Same code, zero modes.
- **Masked / Degraded / Refused**: the three possible outcomes of a fault for an obligation: no
  visible effect; a typed, bounded, surfaced loss; a loud stop that never guesses.
- **Skill**: an instructional document (with optional bundled resources) an agent loads to learn
  how to use slates. Authored once; published raw, over MCP as a resource, and as MCP tools.
- **MCP**: Model Context Protocol, the JSON-RPC based protocol agents use to discover and call
  tools, read resources, and fetch prompts.

---

## Part 1 — What slates is

### 1.1 The one-paragraph statement

The following paragraphs state the product target. The status above and GAPS §8i state what
is implemented and what still prevents these guarantees from being offered.

slates is a hermetic, purely in-memory, copy-on-write virtual filesystem service, written in Rust,
that coding agents provision on demand. An agent asks for a volume and gets one in under fifty
microseconds. The volume appears to ordinary programs in the attached namespace (git, cargo, npm,
python, editors, shells) as a normal path at the place the agent chose, with no wrappers and no
special tools. Many agents create, read, write, delete, move, snapshot, clone, attach, detach,
archive, and destroy volumes at the same time. Volumes are either bounded (fixed quota) or dynamic
(grow as needed within measured limits).

A volume is either scratch, with nothing beneath it, or an overlay over an existing directory on
disk. The disk is the source of truth: untouched files are read from it on demand, the agent's
changes live in memory as exactly the entries that diverged, and they reach the disk only when a
human grants a landing. slates never writes a disk outside a granted landing and never writes
anywhere else.

Many agents fold their work into one shared green volume through a merge engine that maps each
agent's declared operations through everything accepted since their base version and answers
accept, identical, or the exact overlapping bytes; it never invents a merge and never lets the
last writer win.

The same code runs on a single laptop and across a fleet the size of Meta's EdenFS deployment
spanning regions, where durability comes from sealed snapshots and records replicated into the
memory of neighbouring machines under the owner's epoch, with consensus used only for
configuration. Provisioning goes through a server and database designed from the ground up for
this job, and agents integrate through async Python and TypeScript SDKs, an MCP server, skills
over MCP, and raw skills.

### 1.2 Requirements, each traced to the brief

| Id | Requirement | Source in the brief |
|---|---|---|
| RQ-1 | A copy-on-write in-memory VFS in Rust, replacing sylk's Go CoW VFS layers. | Goal 1 |
| RQ-2 | Cross-architecture and cross-OS on exactly vorpal's target matrix: aarch64 and x86_64 macOS; aarch64 and x86_64 Linux glibc and musl; x86_64, aarch64, and i686 Windows MSVC. | Goal 2 |
| RQ-3 | Works locally on a laptop and at EdenFS scale with one code path. | Goal 3 |
| RQ-4 | On-demand provisioning through skills with end-to-end latency under 1 ms, targeting under 50 µs. | Goal 4 |
| RQ-5 | Multiple agents concurrently create, read, write, delete, move, detach, archive/compress, and destroy volumes. | Goal 4 |
| RQ-6 | Invisible filesystem: host tools operate on a volume as a normal path at the agent's chosen point. | Goal 5 |
| RQ-7 | Integrations: an MCP server, skills over MCP, and raw skills. | Goal 6 |
| RQ-8 | No disk leakage of any kind; no `/tmp`; entirely hermetic. | Goal 7 |
| RQ-9 | Bounded volumes with a fixed size and dynamic volumes that resize as needed. | Goal 8 |
| RQ-10 | No parameter from guessed numbers, random config, or hardcoded input; everything measured and derived; attention to the underlying system. | Goal 9 |
| RQ-11 | Tests of use and functionality only. | Goal 10 |
| RQ-12 | Provisioning goes through a ground-up server and database we design and write, best in class on a laptop and globally distributed. | Goal 11 |
| RQ-13 | Python and TypeScript SDKs. | Goal 12 |
| RQ-14 | The SDKs, the server, the database, and libraries where appropriate are async. | Addendum |
| RQ-15 | Maximal correctness, robustness, performance, scalability, efficiency, speed; evidence-backed decisions; complexity accepted with discipline. | Base rules |
| RQ-16 | Allocation-, page-fault-, and lock-aware design; no Arc unless documented as unavoidable. | Base rules |
| RQ-17 | Disk is the source of truth: a volume may overlay an existing host directory whose untouched entries are served from disk and whose diverged entries live in memory; a scratch volume starts from nothing. | Ada, 2026-09-04 |
| RQ-18 | Disk is written only on a user permission grant: the only disk-writing verb is a landing under a grant issued by a human outside the agent's channel; RQ-8 is otherwise unchanged. | Ada, 2026-09-04 |
| RQ-19 | Many agents merge into one shared volume through hecate's merge architecture adapted to slates: green volumes written only by a merge task, increments of declared operations, canonical rebase, a pure verdict with no inferred merge, byte-exact conflict windows, streaming submission. | Ada, 2026-09-04 |

| RQ-20 | Host processes, OCI containers and Linux microVM guests can consume the same VFS; guests use virtio-fs. | Ada, 2026-09-05; Hecate review |
| RQ-21 | Claims reserve actual usable host capacity atomically and remain sacred under competing allocation, retention and pressure. | Ada, 2026-09-05 |
| RQ-22 | Local and remote clones retain the complete base reference; accurate live views and immutable complete snapshots have explicit, different guarantees. | Ada, 2026-09-05 |
| RQ-23 | CLI and MCP share one typed operation contract, actionable refusals, bounded output and explicit attachment capabilities. | Ada, 2026-09-05; Hecate review |
| RQ-24 | POSIX transparency is verified through real mounts and guest devices; mount boundaries and unsupported host forms are reported honestly. | Ada, 2026-09-05 |
| RQ-25 | Metadata recovery, fencing, quorum adoption and placement preserve bytes and historical committed values under faults. | Ada, 2026-09-05 |
| RQ-26 | Different agents are authenticated consumers even on one laptop uid; authority to grant disk writes remains outside their channels. | Ada, 2026-09-05; Hecate review |

### 1.3 Non-goals

- slates is not a version control system. It provides the storage primitives (snapshots, clones,
  green volumes with numbered versions) and the merge engine (increments, canonical rebase, a
  deterministic verdict, byte-exact conflict windows, rebase); commit messages, history
  presentation, review, and validation policy belong to the agent harness (sylk or hecate) that
  uses slates. slates never resolves a conflict: at a merge or at a landing, resolution is an
  explicit change by the agent or the human.
- slates is not a sandbox. It does not restrict what a program may do; it only provides the
  filesystem. Isolation of processes is the harness's job.
- slates is not a persistent store. It never promises to survive a total loss of every replica's
  memory. Durability beyond RAM is the caller's explicit choice via archive export or a granted
  landing onto the disk that is the source of truth.
- slates does not provide a shared-mutable volume with multiple simultaneous writers on different
  hosts in its first release. (The consistency model decision in Part 3 records what is offered.)
- Slates does not own the agent's sandbox, container runtime or VM lifecycle. It serves native
  host paths and virtio-fs guest devices; guest consumers are first-class, and a VM is optional.

### 1.4 Scale envelope

The design must hold at both ends of this table with one code path. Rows marked "measured" are
inputs the daemon reads at boot or observes at runtime, never constants in the code.

| Quantity | Laptop | Fleet ("EdenFS scale") | Source | Treated as |
|---|---|---|---|---|
| Hosts | 1 | thousands | brief | measured (membership) |
| Cores per host | 4–18 | 32–192 | author's machine has 18 [M]; server parts | measured |
| RAM per host | 8–128 GiB | 256 GiB–2 TiB | author's machine 128 GiB [M] | measured |
| Files in a base tree | up to 10^7 | up to 10^9 (Google's repository: ~10^9 files, 9x10^6 unique source files, 86 TB) | [A: Potvin & Levenberg CACM 2016] | upper bound |
| Loaded inodes per volume | 10^5–10^6 | 3x10^6+ observed per EdenFS mount at takeover | [C: EdenFS Takeover.md] | measured per volume |
| Private state per volume | tens of files | "fewer than 10 files" on average per CitC workspace | [A: CACM 2016] | expected delta |
| Diverged entries per overlay volume (memory is proportional to this) | tens to a full build tree (190k files under `target/`) | same | [A: CACM 2016]; [M: cow-data-structures.md §2.1] | measured per volume |
| External change bursts under a base | editor saves; `git checkout` touching 10^4-10^5 entries | same | [B: inotify(7), FSEvents, ReadDirectoryChangesW overflow rules] | measured per base (watcher event and overflow rates) |
| Increments per green volume per second | tens (a few agents) | thousands (a fleet session) | [C: hecate MERGE.md §7 latency budget] | measured per green (feeds the increment size budget) |
| Base lag of an increment (versions between its base and the head) | 0-10 | hundreds | [C: hecate MERGE.md §10 tripwire] | measured per green (feeds delta retention and checkpoint spacing) |
| Directory fan-out | p99 37–181, max 61,067 (`target/deps`) | same shape | [M: cow-data-structures.md §2.1] | measured per volume |
| Base-store read rate | 10^3/s | 5x10^5–8x10^5 QPS (Piper) | [A: CACM 2016] | fleet aggregate |
| Provisioning latency (p99) | < 50 µs (client spinning) | same, local; remote attach metadata-only | brief; [A: Karlin et al.] | permanent gate |
| Kernel-cached op vs daemon op | < 1 µs vs 10+ µs | same | [C: EdenFS Caching.md] | must-measure |
| Copies of a volume's sealed content | 1 (archive export on demand) | f+1 across failure domains, chosen from 2f+1 candidates in the owner's neighbourhood | [A: Vertical Paxos; A: Copysets] | f from the failure-domain tree; the scatter width from measured re-replication bandwidth and the accepted loss probability |

---

## Part 2 — The shape of the system

### 2.1 The whole machine, in plain terms

Think of a hotel with one front desk per floor and no shared hallways. Each floor (a *shard*, one
pinned CPU core) owns a set of rooms (*volumes*). Every request about a room goes to that floor's
desk and is handled by one clerk, one at a time, so no two clerks ever argue over a key. Guests
(agent processes) do not walk to the desk; they drop a note into a pneumatic tube (a
shared-memory ring) and watch the reply tube for a moment before sitting down to wait. The
building has exactly one street entrance (the *root mount*); each room is a door off the lobby,
and opening a new door is a matter of writing a name on the directory board, not of building a
new entrance. When a guest wants a room to appear inside their own office (a *chosen path*), the
building gives them a private corridor on Linux (a mount namespace), a second signed door on macOS
(a second mount), or a second lettered door on Windows (a drive letter), and refuses to knock
holes in walls it does not own (no symlinks, no directories created on disk).

Inside a room, the furniture is arranged so that taking a photograph (a *snapshot*) costs one
click: nothing is copied; the room is marked with the moment of the photo (a *birth epoch*), and
any later change replaces only the changed piece of furniture while the photograph keeps the old
one. A copy of the room (a *clone*) is a new door onto the same photograph; it diverges piece by
piece as it is used. Every piece of content is a *chunk* kept in the floor's own store; identical
chunks are kept once, identified by their fingerprint (BLAKE3), but the fingerprint is computed
only when a piece is sealed (closed, snapshotted, archived, or shipped), never while it is being
written, because fingerprinting is expensive and writing must be fast.

The building keeps a ledger per floor (the *metadata database*): which rooms exist, who holds
each key and until when (a *lease* with an *epoch* so a sleepy guest cannot use an old key),
what each room references and what it alone owns (for exact quotas), and an ordered list of
everything that happened (the *operation log*). On a laptop that ledger lives in a small
shared-memory segment owned by a tiny *anchor process* so the building can be rebuilt after a
crash without losing rooms. In a fleet, every building has a fixed circle of neighbouring
buildings in different fire zones. Every photograph (a sealed snapshot) and every pointer (which
photograph is a room's latest, who holds a key) is handed to that circle, and it counts as safe
the moment the fastest f+1 neighbours have signed for it; a slow neighbour is simply not waited
for. Each building stamps everything it hands out with its own epoch number, and a neighbour
refuses anything stamped with an older epoch than it has already seen from that building, so a
building that fell asleep and woke up cannot overwrite what its successor did. A small elected
council per city (a consensus group) decides only who is a member, who neighbours whom, which
epoch each building is on, and who takes over a building that burned; it is never asked about
individual photographs. Whatever a guest has scribbled since the last photograph stays in that
building alone, unless the guest asked for live copying, and the building takes photographs
often enough that the scribbles at risk never exceed a measured, stated amount.

Next to the hotel stands the guest's own warehouse (the disk). Most rooms are windows onto a
shelf of that warehouse (an *overlay volume* over a host directory): the guest sees the shelf as
it is right now, and anything the guest changes is kept in the room, with a note of what the
shelf held when the change was made (the *witnessed base*). If the warehouse staff move a crate
the guest already changed, the clerk reports it (*drift*) and never quietly swaps the guest's
work. Nothing goes back to the warehouse until the owner signs an order naming the room and the
shelf (a *grant*); then one clerk (the *landing engine*) carries the changed crates over one at
a time, checks each shelf tag before swapping, refuses any crate whose tag changed, and writes
down exactly what was moved (the *landing manifest* in the *audit log*).

Several guests can work from one shared room (a *green room*). Each takes a photograph of it
and works in a copy (a *work room*); when ready, the guest hands the clerk a slip listing
exactly which bytes of which pages changed (an *increment*), never the pages themselves. The
clerk lays the slip over every change accepted since that guest's photograph: lines nobody
else touched are accepted, identical changes are accepted as identical, and overlapping
lines come back to the guest as the exact overlap (a *conflict window*). The clerk never
rewrites a guest's page; the guest does, and hands in a new slip. The shared room changes
only by the clerk's hand, one slip at a time, and every guest looking at it sees the
photograph they asked for until they ask for a newer one.

The analogy carries the seven least obvious choices: one entrance and many doors (provisioning is
metadata); one clerk per floor (no locks, no shared counters); photographs by birth epoch (no
reference counts on the write path); fingerprints on sealing (no hashing on the write path);
photographs and pointers signed for by the fastest f+1 neighbours rather than decided by a
council (durability is replication under an epoch stamp, and the council decides only who
neighbours whom); the warehouse
touched only under the owner's signed order (disk is the source of truth; a landing is the only
disk write, and only under a grant); and a clerk who only ever says yes, identical, or here is
the overlap (no inferred merge, no last writer wins).

### 2.2 The provisioning fast path in one picture

```
agent process (SDK)                         daemon shard S (pinned core)
 ─────────────────────                       ─────────────────────────────
 write 64-byte request into ring slot  ──▶  poll ring; read slot (1 cache-line transfer)
 store "parked?" = no; spin on reply         validate; pop volume record from slab
                                             insert name into shard's ART index
                                             carve root dir node + inode slot from arenas
                                             charge quota (two integers) / reserve budget
                                             append op-log record (and replication queue)
                                             release-store new root-namespace pointer
 read reply slot (1 cache-line transfer) ◀── write reply slot; if agent parked, signal fd/event
 return {volume id, path, granted form}
```

Every step on the shard is sub-microsecond on cited hardware (`research/low-latency-ipc-and-runtime.md`
§2.4); the only expensive step is waking a parked client, which the client avoids by spinning for
the measured wake cost before parking (the 2-competitive rule). The mount is never touched.

### 2.3 The data planes

1. **Namespace plane** (per volume, on its owning shard): directory nodes, inode slab, extent lists,
   birth epochs, snapshot records and deadlists, the per-volume operation log.
2. **Content plane** (per shard): chunk arenas (page-multiple slabs and a buddy allocator over
   locked regions), the content-address index (hash-prefix sharded Swiss-style table), dictionaries,
   compressed cold chunks.
3. **Metadata database** (per shard): volume catalog, lineage DAG, leases and attachments,
   accounting counters, completion records for exactly-once, the local op log in the anchor
   segment; in a fleet every head, chain version, lease and catalog entry is a register the
   owner writes to its candidate holders under its host epoch, sealed content lands on the same
   candidates under the same rule, and only configuration goes through consensus; live state
   stays with the owner unless a volume opted into live shipping.
4. **Bridge plane** (per host): the root mount and the per-OS bridge driver (FUSE, NFSv3
   loopback, WinFsp) feeding requests to shards by volume handle and emitting invalidations.
5. **Cluster plane** (fleet only, degenerate on a laptop): SWIM/Lifeguard membership, the
   regional configuration group and the root group, neighbourhoods and rendezvous within them,
   the register and content puts with hedging and recorded holder sets, the healer and probation,
   takeover by epoch bump and batched promotion, mirroring across regions, Merkle anti-entropy,
   auto-seal scheduling, remote attach by id routing, prefetch.
6. **Base plane** (per host, read-only): the host directories overlay volumes sit on; the per-
   directory listing cache keyed by change time; witnessed bases and pinned base content on the
   owner shard; watchers as hints; the drift reports.
7. **Landing plane** (per host, the only writer of host paths): grants, landing leases,
   the landing manifests, the write-back engine with per-file compare-and-swap, the audit log.
8. **Merge plane** (per green volume, on its owner shard): the version chain, canonical deltas
   and checkpoint deltas, the per-path last-changed index, the merge task, and merge records
   written as entries of the green's ledger register.

### 2.4 The agent surfaces

- **Rust client library**: the reference implementation of the ring protocol; used by the CLI,
  the launcher, and both SDKs.
- **Python SDK** (`slates` on PyPI): PyO3 extension; `async` methods on any event loop through
  file-descriptor readiness; a sync facade.
- **TypeScript SDK** (`@slates/sdk` on npm): napi-rs addon with platform packages; Promises and
  async iterators; the TypeScript addon over the typed client.
- **MCP server** (`slates mcp`): the 2026-07-28 stateless protocol with dual-era support; tools,
  resources, and prompts; stdio and loopback Streamable HTTP.
- **Skills**: one source tree of `SKILL.md` documents published raw (installed into
  `.agents/skills/` and `.claude/skills/`), over MCP as `skill://` resources and prompts, and on
  demand through a help tool; packaged as a Claude Code plugin bundling the MCP server.
- **CLI** (`slates`): daemon control, `exec` launcher (Linux), volume verbs, diagnostics, and
  `mcp install` for agent clients.

### 2.5 Architecture map (what runs where)

```
┌──────────────────────────── host ────────────────────────────────────────┐
│ anchor process (tiny): shared-memory segment {machine profile, op logs,   │
│   catalog snapshots}, held fds (FUSE fd / NFS socket / WinFsp state),     │
│   supervises and restarts the daemon                                      │
│                                                                           │
│ daemon: N shards (one pinned core each)                                   │
│   shard k: executor + driver | volumes owned by k | chunk arena k |       │
│            content index partition k | metadata db partition k |          │
│            command/completion rings for clients pinned to k |             │
│            bridge queue k (FUSE channel k / NFS connection set / WinFsp   │
│            requests routed by handle)                                     │
│   control shard: rendezvous listener, admission, machine profile refresh, │
│            membership (SWIM), placement, consensus-group participation,   │
│            MCP server, health/tracing sinks, base watchers (hints)        │
│   base reader (read-only host access for overlay volumes; runs on the     │
│            owner shard's driver; never links a write syscall)             │
│   landing engine (the only writer of host paths; runs only under a grant │
│            bound to a manifest; one holder per target; per-file swap)     │
│   merge engine (one task per green volume on its owner shard; the only  │
│            writer of that green; verdicts, splices, merge records)        │
│                                                                           │
│ kernel: root mount (FUSE | FSKit module, NFS fallback | WinFsp volume) → caches │
│ agents: SDK processes with rings; tools see /root/<volume> or chosen path │
└───────────────────────────────────────────────────────────────────────────┘
```

### 2.6 Boot order and self-observation

1. The anchor process starts (or is already running), maps or creates the shared segment, and
   loads or rebuilds the machine profile.
2. The daemon starts, attaches the segment, and either replays the op logs into fresh in-memory
   indexes (restart) or initializes empty partitions (first boot).
3. Shards are created on performance-class cores; arenas, rings, and task slots are carved,
   pre-faulted, and locked according to the profile; every derived constant is logged with its
   inputs.
4. The bridge attaches to the held fd/socket/state or establishes the root mount once.
5. The rendezvous endpoint is published; SDK clients connect; the MCP server starts on request.
6. In a fleet, the control shard joins membership, learns its neighbourhood and host epoch from
   the regional configuration, and begins replication to its candidate holders.
7. No disk is probed at boot: a disk probe would be a write outside a grant. Disk throughput and
   latency are calibrated inside granted landings by the online ramp of §4.15 and remembered in
   the profile for the session.
The health plane observes all of the above from the outside (host-observed, never self-reported
only) and refuses to serve until every chokepoint has registered (an unregistered emitter fails
startup).

> **Status (2026-09-05).** Steps 1, 2, 3 and 5 are implemented for one host (GAPS §8d):
> `slates anchor` measures the profile, creates the segment, publishes the profile and
> supervises `slates daemon` as a child with the segment in its environment; the daemon
> attaches, replays each partition, rebuilds the recovered volumes' live trees (an overlay's
> base re-opened, a scratch empty; snapshots placed only in the dead process's memory and its
> attachments reconciled out of the catalog as recorded operations), logs every derived
> constant with its inputs, and publishes the rendezvous. The daemon leaves when its anchor
> dies (the parent-death signal on Linux, a parent watch at the heartbeat cadence elsewhere,
> a job object on Windows); the anchor kills a daemon that never beats inside the recovery
> budget or whose heartbeat lapses, and re-derives the restart bound from the longest start it
> measured. Step 4 (bridges) is Phase 3 and 4; step 6 is Phase 8 — its **authority core** now
> exists (A-10): `slates-cluster`'s `FleetNode` composes membership, the configuration group and
> the owner's register acceptor, with the `f = 0` laptop as the same code path (the N=1≡fleet
> differential is a named test) and a head commit driven live over the transport. Step 6 is now
> **reached for a two-node fleet**: the object→owner routing registry, the detector→fleet bridge, the
> live probe/gossip loop, and `FleetNode` wired into the control shard at boot all exist
> (`crates/server/src/fleet.rs`, `Daemon::start_with_fleet`), proven by two in-process daemons that form
> a fleet over real UDP and detect and retire a stopped peer (`crates/server/tests/fleet.rs`). Since built
> on it (A-11, A-12): the cross-node commit path at `f > 0`, the takeover's phase-one recovery over any
> `f` (a record-plane coordinator promoting over every surviving holder), and §4.10 content replication —
> a sealed snapshot archived in bounded slices, put to `f + 1` candidates by missing set, verified before
> held, its head naming the placed content, and a takeover successor serving the content under the
> original id. **Deployed as real processes** (A-13): an operator starts every node from one shared
> manifest (`slates daemon|anchor --fleet PATH --node NAME`); each node derives the same member ids
> from the certificates and its two serve ports from its advertised address
> (`crates/server/src/deploy.rs`), and `slates status` reports the node's place in the fleet — proven by
> three `slates daemon` processes forming an `f = 1` fleet, placing a sealed snapshot across processes,
> retiring a `SIGKILL`ed owner and serving its volume from the successor (`crates/cli/tests/cli.rs`).
> **Every peer on one socket per plane** (A-14): the session plane carries a connection id both ends
> derive from the TLS exporter, a demultiplexer routes each packet to its session, and a peer that
> re-dials after losing its session replaces it (`crates/transport/src/demux.rs`). **Owed** to
> generalize it: the rest of §4.10 (content-defined chunking and the cost model, anti-entropy, the
> healer, remote attach, live shipping, migration, mirroring). The health plane's refusal to serve
> before every chokepoint registers arrives with task 6's signals.
> A-9 correction: rebuilding scratch volumes empty and dropping snapshots is acknowledged
> content loss (BUG-11), not content recovery. Anchor-owned bytes/roots and validated base
> identity handoff are required before the design's restart-survival promise can be offered.

---

## Part 3 — Decision ledger

Each entry: the decision; the evidence (tiered citations point into `research/`); the alternatives
that lost and why; what must be measured at runtime; consequences. "Owed" marks a follow-up the
plan schedules.

### D-1 One root mount per (host, user); volumes are directories; provisioning never touches mount(2)
- Evidence: bb_clientd single mount; CitC single FUSE mount per developer with workspaces as directories [A: CACM 2016]; mount/namespace churn is ms-class and mount-table-size dependent [A: Oakes ATC'18]; sylk's per-command mounts were its worst latency and lifecycle problem [C: survey-sylk-vfs.md §3, §8.2]. (`research/os-filesystem-bridge.md` §2.4)
- Lost: one mount per volume (EdenFS) — kernel latency on the provisioning path, privileged helpers on macOS; per-command mounts (sylk).
- Measure: mount cost per kernel; root-mount establishment time at daemon start.
- Consequence: the bridge namespace's top level is a shard-published array of (name → volume handle); a volume is visible the moment the release store lands.

### D-2 Bridges: native host adapters and a first-class virtio-fs guest device over one VFS
- A-9: Linux guests use an owned FUSE-over-virtio device on the custom runtime, with an in-process
  or inherited-descriptor integration seam for the host VMM. Hecate's libkrun integration is the
  reference; native host bridges remain necessary for host tools. A container on the host gets
  the host attachment through its runtime's mount namespace; a container in a guest uses the
  guest's virtio-fs mount. All forms share the same semantics, authorization and accounting.
  No new daemon privilege, disk socket, mount directory, runtime or fallback is authorized.
  Evidence: [S: research/hecate-contract-review.md §2]; integration contract in §4.6.
- Evidence: FUSE per-request floor is two wakes with the kernel cache answering the hot path; FUSE-over-io_uring gives per-core queues and 2-3x create throughput [A: Vangoor FAST'17; A: Cho FAST'24; C: fuse-io-uring.rst; D: LWN 988186]. FSKit: a sandboxed user-space app extension enabled by one toggle in System Settings, no kernel extension, no Recovery-mode reboot; a complete operation set including xattrs, hard links, a forget call and a readdirplus-shaped enumeration; URL-identified resources from macOS 26; the entitlement ships under Developer ID (macFUSE does exactly this); sandboxed extensions may share POSIX shared memory and sockets with same-team processes through an app group [B: Apple FSKit, FSVolume.Handler, FSGenericURLResource, entitlement and app-group docs; C: macFUSE releases and site; C: FSKitSample]. NFSv3's coherence limits live in Apple's kernel client and cannot be fixed server-side; it lacks xattrs and forget and hangs on daemon death [C: EdenFS macOS.md]. ProjFS and Cloud Files hydrate to NTFS by design [B: Microsoft; C: EdenFS Windows.md]. Every existing Rust bridge crate brings `Arc` and threads or tokio [C: crate sources]. (`research/os-filesystem-bridge.md` §2.1-2.3, §7-§8)
- Lost: `fuser`/`fuse-backend-rs`/`fuse3` (ownership model); macFUSE's kernel extension (security downgrade, dead in CI); NFSv4.0 on macOS (spec size, serialized OPEN, lease-loss remounts); NFSv3 as the primary macOS bridge (kept as fallback and as the differential oracle); ProjFS; Cloud Files; Dokany; SMB and NFS clients on Windows.
- Measure: the Phase 4 FSKit spike (per-operation latency, cache and invalidation behaviour, mmap correctness, non-root mount, shim overhead) decides go/no-go and the macOS 15.x path (RAM-disk block resource); the NFS fallback's attribute-cache timeout from loopback RTT.
- Owed: tracking Apple's yearly protocol churn (Operations in 15.4, Handler in 27) behind the shim; re-checking FSKit's cache semantics each release.
- Base files: untouched entries of overlay volumes are read from the host filesystem by the daemon and served through the same bridge, one copy from the page cache into arena pages. FUSE passthrough is not used: it requires `CAP_SYS_ADMIN` [B: fuse-passthrough.rst], and slates never depends on a privilege the user may lack; the only privileged pieces on any platform are the brokers the OS already ships (`fusermount3` or a user namespace on Linux, the FSKit extension on macOS, the installed WinFsp driver on Windows), none of which belongs to slates.

### D-3 Chosen-path rules per OS with zero disk writes
- Evidence: private mount namespaces and bind mounts modify only the kernel mount table [B: mount_namespaces(7)]; macOS has no mount namespaces and non-root can mount NFS onto a directory it owns [C: xnu vfs_syscalls.c]; drive letters are object-namespace junctions, directory mounts are NTFS reparse points [B: DefineDosDeviceW; C: WinFsp FAQ]; symlinks are disk inodes and defeat "invisible" [B: symlink(7)]. (`research/os-filesystem-bridge.md` §2.4)
- Lost: symlinks; argv/env rewriting (sylk); overlayfs upper layers (captures unrelated writes).
- Consequence: the attach reply always states the granted form; refusals are typed.

### D-4 Namespace: adaptive directories, per-volume inode slab with generational handles, monotonic inode numbers, per-volume name-equivalence policy
- Evidence: directories are tiny with a long tail (median 2; p99 37-181; max 61,067) [A: Agrawal FAST'07; M]; ART/HOT footprints 8-16 B/key and hash-class lookups [A: Leis ICDE'13; A: Binna SIGMOD'18]; Swiss-table probing [C: Abseil]; inode numbers on demand and never reused (EdenFS, ScaleFS) [C: EdenFS Inodes.md; A: Bhat SOSP'17]; APFS normalization-insensitive hashing; git's `core.ignoreCase`/`precomposeUnicode` [B: Apple APFS FAQ; C: git-config]. (`research/cow-data-structures.md` §2.1)
- Lost: hash-only directories (no canonical order); a global dcache-style table (needs RCU machinery); path-hash inodes (sylk: rename changes the inode).
- Measure: the sorted-array cut-over; node fanout; rename rate.

### D-5 Snapshots and clones by birth epoch with deadlists; lazy Merkle identities
- Evidence: WAFL/ZFS snapshots are O(1) and free blocks by one comparison [A: Hitz USENIX'94; C: OpenZFS dsl_dataset.c]; Rodeh's lazy refcounts put writes on the write path [A: Rodeh TOS 2008]; Sapling defers hashing to `persist()` [C: manifest-tree]; path copying per write amplifies by up to 1,000x on the measured worst directory [M]. (`research/cow-data-structures.md` §2.2)
- Lost: lazy refcounted CoW B-tree; pure persistent structures; operation log alone.
- Consequence: a node born in the current epoch is mutated in place; snapshot = record + empty deadlist; clone = fork pinning its origin; destroy = walk the deadlist.

### D-6 Content: page-multiple chunks, extent lists, open mutable extents until seal, re-chunk/hash/dedup at seal, CDC only for the measured large-file class
- Evidence: file bytes are in large files and file counts in small ones [A: Agrawal FAST'07; A: Meyer FAST'11]; whole-file dedup captures ~3/4 of block-level gains [A: Meyer FAST'11]; FastCDC 10x Rabin [A: Xia ATC'16]; FUSE splice thresholds and io_uring buffers favour page-aligned replies [A: Vangoor TOS 2019; B: fuse-io-uring.rst]; sylk's one-chunk-per-write and SHA-256-per-write were disqualifying [C: survey-sylk-vfs.md §1.1, §7.5]. (`research/cow-data-structures.md` §2.3, §2.5)
- Lost: hashing on the write path; fixed 64 KiB chunks by fiat; CDC everywhere; mapping arena pages into client processes (cross-volume leak) [C: survey-hecate.md §8.4].
- Measure: page size; memcpy and hash throughput; per-volume file-size histogram; dedup hit rate.
- Copy-up classes: the first write to a base-backed file pins the whole file for the measured small class and keeps an open descriptor plus the written extents for the large class; the boundary is the measured class boundary of §4.5.

### D-7 Concurrency: one owning shard per volume; bridge queues pinned to the owner; immutable chunk reads from any core; no locks; QSBR only for cross-shard read-mostly tables
- Evidence: delegation beats locks by up to 10x for short critical sections [A: Roghanchi SOSP'17]; partitioned single-writer wins below ~20% cross-partition work [A: Tu SOSP'13; A: Yu VLDB'14]; every slates operation names one volume; QSBR has no per-operation fences [A: Hart 2007]; sylk's coarse locks were its throughput ceiling [C: survey-sylk-vfs.md §6]. (`research/cow-data-structures.md` §2.4; `research/database-design.md` §2.2-2.3)
- Lost: shared OCC/MVCC indexes (10-13% cost and reclamation machinery for no benefit here); RCU-everything (conflicts with in-place mutation of current-epoch nodes).
- Measure: cross-core ring round trip; skew across volumes (the documented escape hatch is a Silo-style shared index if skew is observed).

### D-8 Ownership policy: no `Arc`/`Rc` in the core; handles, moves over bounded channels, epoch-published immutable roots, `&'static` singletons; three documented exceptions
- Evidence: contended refcounts cost 50-400 ns per touch and serialize cores [A: David SOSP'13; A: Schweizer PACT'15]; vorpal's written policy and two exemplar load-bearing `Arc`s [C: survey-vorpal.md §2.0-2.1]; hecate's generational-handle doctrine [C: survey-hecate.md §3.8]; `RawWakerVTable` needs no reference count [B: std docs]. (`research/arc-free-rust-architecture.md` §2.1-2.2)
- Exceptions: bindings objects whose GC may drop them mid-call; foreign APIs that take `Arc` by signature; test harnesses.

### D-9 Runtime: a custom thread-per-core executor with arena task slots and `RawWaker` encodings; io_uring/epoll, kqueue, IOCP drivers; a deterministic simulation driver
- Evidence: monoio lacks Windows, compio has no maturity statement, tokio is work-stealing and `Arc`-based [C: READMEs; C: survey-vorpal.md §2.4]; embassy proves `Arc`-free wakers [C: embassy-executor]; io_uring `DEFER_TASKRUN`+`SINGLE_ISSUER` for jitter and the epoll fallback for containers [B: io_uring_setup(2); C: Docker seccomp]. (`research/low-latency-ipc-and-runtime.md` §2.3; `research/arc-free-rust-architecture.md` §2.3-2.4)
- Lost: tokio; compio (audited runner-up); monoio (until Windows lands).
- Size: 6-7 kLOC plus tests.

### D-10 IPC: shared-memory rings plus a wake word per OS; rendezvous with zero filesystem entries; spin-then-park with the measured threshold; one completion fd per client
- Evidence: rings achieve sub-microsecond handoffs and sockets cost 5-30 µs per round trip [C: LMAX; A: Marty SOSP'19; D: Red Hat, Microsoft loopback]; futex/`os_sync_wait_on_address`(SHARED)/named Event semantics and the process-local limit of `WaitOnAddress` [B: futex(2); B: os_sync header; B: Microsoft Learn]; abstract sockets, `shm_open`, `Local\` sections create no filesystem entries [B: unix(7); B: shm_open(2); B: kernel object namespaces]; Karlin's 2-competitive spin rule [A: Karlin 1990]; asyncio on Windows needs a socket [B: Python docs]. (`research/low-latency-ipc-and-runtime.md` §2.2, §2.6)
- Lost: sockets per request; Mach messages; `/dev/shm` files (a filesystem entry); AF_UNIX on Windows (creates an NTFS reparse point).
- Measure: wake latency, cross-core RTT, syscall cost at boot; publish the spin window.

### D-11 The machine profile: every tunable derived at boot from measurements, logged with inputs, re-measured on power-state change
- Evidence: page sizes differ 4x across targets [B: Apple; B: arm64 Kconfig]; fault costs differ 100x between base and huge pages [A: Panwar ASPLOS'19]; codec throughput differs 10x across CPUs [D: lzbench; D: openzfs]; hyperscale's ~140 hardcoded constants were its weakest point [C: survey-hyperscale.md §8.4]; lmbench methodology and rigorous statistics [A: McVoy USENIX'96; A: Kalibera ISMM'13; A: Mytkowicz ASPLOS'09]. (`research/memory-and-system-awareness.md` §2.6)
- Consequence: a `Derived constants` table per subsystem (Constant | Formula | Anchors); a bare tuning literal fails review.

### D-12 Memory: claims backed by usable prefaulted locked arenas; bounded entitlement protected from every other allocation
- A-9: unsuccessful locking is an admission refusal, not successful RAM-only service with
  swappable content. The host's effective capacity, allocator geometry and every source of
  retained/transient memory constrain admission. Dynamic growth uses only unpromised capacity.
  A quota field, anonymous mapping or OS free-memory sample is not a reservation (§4.2).
- Evidence: Bonwick slabs and magazines, Hoard, snmalloc's message-passing frees [A: USENIX'94/'01; A: ASPLOS'00; A: ISMM'19]; mlock limits and `CAP_IPC_LOCK`, macOS wire limits (108.8 GiB of 128 GiB on the author's machine), Windows `VirtualLock` bounded by the working set [B: mlock(2); M; B: Microsoft Learn]; huge pages help only where measured [A: Gaud ATC'14; A: Panwar ASPLOS'19; A: Hunter OSDI'21]; PSI, memory-pressure sources, memory resource notifications [B: kernel PSI; C: libdispatch; B: Microsoft Learn]. (`research/memory-and-system-awareness.md` §2.2-2.7)
- Lost: THP `always`; a global allocator on the hot path; fixed growth percentages.
- Base content cache: base bytes read into arenas are evictable exactly when they can be re-read from disk with the same fingerprint; witnessed, copied-up and pinned bytes are never evicted; the cache budget follows the dynamic-growth formula of §4.2.

### D-13 Accounting: `referenced_bytes` and `unique_bytes` per volume on the owner; O(1) ENOSPC; shared chunks charged in full per referencer; physical budget charged once
- Evidence: ZFS `referenced`/`used` and deadlists; Btrfs qgroups `rfer`/`excl` [C: OpenZFS dsl_dataset.c; C: Btrfs qgroups]. (`research/cow-data-structures.md` §2.6)
- Lost: fractional accounting (inexact, needs global knowledge).

### D-14 Database and replication: Vertical Paxos II with copyset neighbourhoods; one quorum rule with hedged placement; configuration by consensus, never per write; owner-local live state; route by id; epoch-ordered mirroring with a per-operation durability scope; N=1 is the same code
- The decision: every replicated object, sealed content or record, has 2f+1 candidate holders, the owner among them, drawn by rendezvous from the owner's neighbourhood, a fixed set of hosts across failure domains whose size (the scatter width) bounds the copyset count; a write commits at f+1 acknowledgements from any candidates and the acknowledging set is recorded in the object's head record; records are sent to all candidates at once, content to f+1 with hedged and tied requests to the rest after the measured p95, so a straggler never delays `placed`; every message carries the owner's host epoch and holders refuse lower epochs; volume heads, chain versions, landing leases and catalog entries are registers the owner writes under that epoch, never consensus entries; a regional consensus group (the hecate Raft dialect) holds only configuration: membership, neighbourhoods, host epochs, takeovers, and moved homes, with a root group across regions; takeover bumps the dead host's epoch, assigns each object to the surviving candidate that rendezvous ranks first, and each new owner runs one batched phase-one round per register class; lookups route by id to the current owner with no index; every committed record and its content is mirrored to the mirror region asynchronously in epoch order with an exposed lag, and the durability scope is chosen per operation with `await placed(region | mirror)`; live working state stays owner-local with auto-seal; ownership follows the writer.
- Evidence: Vertical Paxos ("Vertical Paxos algorithms use an auxiliary configuration master that facilitates agreement on reconfiguration. A special case of these algorithms leads to traditional primary-backup protocols"; "a master allows a state-machine implementation to tolerate k failures using only k+1 processors"; the leader-acceptor that "can perform the state transfer all by itself, with no messages"; the primary's lease for local reads; the acceptor's `maxBallot` rule) [A: Lamport, Malkhi, Zhou 2009]; Copysets ("a 5000-node RAMCloud cluster under power outage, Copyset Replication reduces data loss probability from 99.99% to 0.15%") [A: Cidon et al. ATC 2013]; hedged and tied requests ("limits the additional load to approximately 5% while substantially shortening the latency tail"; "from 1,800ms to 74ms while sending just 2% more requests") [A: Dean & Barroso CACM 2013]; FaRM (configuration-managed primary-backup in memory, 140 million transactions per second on 90 machines, recovery under 50 ms) [A: SOSP 2015]; RAMCloud's coordinator that "is not involved in most client requests", decentralised placement, sick-master fencing and parallel recovery (35 GB in 1.6 s) [A: SOSP 2011]; Ceph's peering, acting sets and map epochs [B: Ceph docs]; BookKeeper's single-writer ledgers, ack quorums, last-add-confirmed and fencing [C: BookKeeper]; Kafka's ISR and KIP-101's lesson that the epoch must be on every record [C: Kafka]; Chubby's sequencers [A: OSDI 2006]; PNUTS's record-level mastering with migration by write origin [A: VLDB 2008]; CockroachDB's leader leases, which fuse writer and proposer per range [B: CockroachDB docs]; CRUSH, which places "without relying on a central directory" [A: SC 2006]; Spanner's 14.4 ms writes even within one datacenter and F1's coast-split replicas, and PNUTS's "hundreds of milliseconds or more" for synchronous world-wide writes, which settle cross-region durability as a per-operation scope [A: OSDI 2012; A: VLDB 2008]; hecate's ratified Raft dialect for the configuration group [C: survey-hecate.md §3.1]; the two TLA+ models in `docs/wip/models/` (`research/metadata-replication.md` §1-§9).
- Lost: consensus on the head or merge path (a second hop and a log for objects with one legal writer); range-sharded pointer groups (D-O12 closed: the configuration group commits only on failures and moves); per-volume random placement (unbounded copysets); write-all content with membership-based straggler removal (a straggler delayed `placed`); two quorum rules; a per-volume durability policy; pre-granted placement blocks (unnecessary once ids route and placement is computed); a global catalog index; a leader-fused per-session Raft group (hecate's shape; slates has one configuration group per region and the writer is the proposer of its own registers, which is the property that shape was buying); the primary-backup shard log and Raft per shard.
- Measure: intra-region and cross-region round trips; per-record write cost; acknowledgement latency distributions (set the hedge delay); the scatter width from measured re-replication bandwidth and the accepted loss probability; takeover time against the recovery budget; the configuration group's commit rate (near zero outside failures); mirror lag; the fraction of head reads served by non-owners; the copyset count (must stay under the derived bound).
- Consequence: D-16 gains "the owner is the distinguished proposer of its own registers"; D-18's statement gains the mirror and the scope; §4.8 and §4.10 are rewritten; §4.16's merge record is a ledger entry; Phase 8 builds the register protocol, neighbourhoods, takeover, mirroring and migration; the models of §4.8 are architecture artifacts, checked when the protocol was designed and re-run only when it changes.

### D-15 Wire: fixed-layout little-endian headers, canonical derive-generated bodies with a schema hash, append-only evolution; RIFL completion records for exactly-once; credit flow control with derived windows; TLS 1.3 between hosts, peer credentials on one host
- Evidence: vorpal's and hecate's wire disciplines [C: survey-vorpal.md §5.4; C: survey-hecate.md §4.6]; RIFL [A: Lee SOSP'15]; credit-based flow control [A: Kung SIGCOMM'94; B: RFC 9113]; TLS 1.3 [B: RFC 8446]. (`research/database-design.md` §2.6)
- Lost: cloudpickle-style code-carrying formats (hyperscale); tolerant reading across majors; Noise (TLS 1.3 via rustls is the decision).

### D-16 Consistency model: single-writer volume ownership with a short renewable lease carrying an epoch; snapshot readers everywhere; optional recallable per-subtree delegations for explicitly shared volumes; clone + merge for optimistic parallelism through green volumes and the merge engine of §4.16; never last-writer-wins; never an inferred merge
- Evidence: leases [A: Gray & Cheriton SOSP'89]; AFS callbacks and Coda's finding that write sharing is rare [A: Howard TOCS'88; A: Kistler TOCS'92]; NFSv4 delegations [B: RFC 7530]; Ceph capabilities under conflict [A: Weil OSDI'06]; CitC's one-owner workspaces [A: CACM 2016]; hecate's "RWX does not exist" and fencing on every lease-holder effect [C: survey-hecate.md §2.4-2.5]. (`research/edenfs-scale-distribution.md` §2.4)
- Lost: per-file leases by default (millions of lease records); LWW with vector clocks (filesystem invariants do not merge); synchronous home-node I/O for everything.
- Merge: between work volumes of one green, the only conflict authority is the merge verdict of §4.16 on declared operations; a basis guides (skips work when nothing overlaps) and never guards [C: hecate MERGE.md §3; C: hecate LEDGER.md:230-232].
- Authority: the owner of an object is the distinguished proposer of that object's register under its host epoch (Vertical Paxos II with the leader among the acceptors), so the writer and the proposer are one thing per object, the property CockroachDB's leader leases and hecate's leader fusion buy; ownership follows the writer: creation places it where the creator runs and a write-intent attachment that stays on another host migrates it there through the planned handoff [A: Lamport, Malkhi, Zhou 2009; A: PNUTS VLDB 2008].
- Landing: the only conflict authority between a volume and its base, or between volumes over one base, is the landing verdict of §4.15, a pure function of the witnessed base, the disk now, and the overlay now; conflicts are values, never merges; leases and drift reports guide, never guard [C: hecate ADR-0005; C: hecate SESSIONS.md §5; C: hecate LEDGER.md:230-232].

### D-17 Compression, dedup, hashing, archive: zstd (static contexts) and LZ4; per-class dictionaries trained from the volume's data; a boot-calibrated, online-updated cost model decides per chunk; BLAKE3 fixed in the format; the archive format of `research/compression-archive-dedup.md` §2.6 doubles as the replication and clone-from-archive format
- Evidence: RFC 8878 and static allocation; lzbench curves; dictionary gains 2-5x on small records; Btrfs heuristic and OpenZFS early abort; whole-file dedup yield; BLAKE3 tree hashing [B; C; D; A as cited]. (`research/compression-archive-dedup.md`)
- Lost: Brotli, xz, fixed "save 12.5%" rules (sector-rounding artefacts), SHA-256 (not fixed-cost across the matrix), CDC everywhere.
- Erasure coding: the format carries a fragment record kind from the first release: a chunk may be held as k data and m parity fragments (Reed-Solomon), each fragment with its own BLAKE3, the chunk's identity unchanged, so replication, archive and clone-from-archive all understand fragments from the first release; the policy that codes cold sealed content instead of replicating it is measured in Phase 8 (D-O6): the class boundary comes from measured read rates, the (k, m) from the failure-domain tree, and the reconstruction cost from the profile [A: Rashmi et al., EC-Cache, NSDI 2016; A: Muralidhar et al., f4, OSDI 2014; A: Huang et al., LRC, ATC 2012].

### D-18 Durability: explicit client, process, host and region boundaries
- Client-buffered writes enter a snapshot only after the attachment barrier (§4.6). Local
  acknowledgement promises daemon-restart survival only when bytes, roots, witnesses,
  accounting and completion records are recoverable from anchor-owned RAM (§4.8).
- A complete snapshot is f-fault-tolerant within its declared failure domains only after f+1
  of 2f+1 eligible holders reserve, verify and retain every referenced object and its record.
  `DeltaWithLiveBase` placement protects the delta; it does not replicate the host directory.
- Live owner-local edits since the last placed seal may be lost with that host; live shipping
  must place bytes and records before acknowledgement to offer a stronger scope. Mirror scope
  waits for the corresponding verified prefix and content in that region and exposes time lag.
- RAM replicas do not survive simultaneous loss of all holders. Only explicit export to a
  caller-owned sink or a granted landing provides persistence beyond their lifetime. A local
  `fsync` never implies disk or region durability that the volume did not establish.
- Status: metadata replay and protocol simulations exist; full content recovery, end-to-end
  placement and the A-9 regressions remain open. Evidence: the contract review and audit.

### D-19 Agent surfaces: own MCP server (2026-07-28 stateless, dual-era for legacy clients); skills authored once and published raw, as `skill://` resources and prompts, and via a help tool; PyO3 SDK with fd completion; napi-rs SDK with `uv_poll` completion; CLI with `mcp install` and `skills install`
- Evidence: the 2026-07-28 spec changes and conformance suite; rmcp's tokio/`Arc` coupling; the Agent Skills spec and client install paths; PyO3 free-threading and abi3 facts; Node-API ABI stability; asyncio's Windows limits [B; C as cited]. (`research/mcp-skills-sdks.md`)
- Lost: rmcp; pyo3-async-runtimes (needs a Rust runtime in the client); parallel pure-Python/TS compatibility implementations (not part of the target).

### D-20 Testing doctrine: the five-layer pyramid; behaviour only; conformance suites with reviewed expected-failure lists; deterministic simulation; instruction-count gates in CI and change-point-detected latency gates nightly; the <50 µs histogram as a ratcheted permanent gate
- Evidence: pjdfstest/xfstests/fsx/SibylFS/Metis/CrashMonkey; QuickCheck lineage; loom/shuttle/Miri; FoundationDB simulation; Jepsen/Elle; the failure studies; rigorous benchmarking literature [A; C as cited]. (`research/testing-and-benchmarking.md`)

### D-21 Documentation and process: hecate's spec skeleton (data model with ownership facts, state machines, networking table, failure matrix, refusal taxonomy, derived constants, worked example, laptop degenerate, integration list, acceptance criteria, test matrix); a gap ledger kept current in the same change; overrules recorded
- Evidence: `research/survey-hecate.md` §0, §7.

### D-22 Security: authenticate each consumer, enforce rights before effects, and keep human grant authority outside agent reach
- A-9: OS credentials establish the host account; an enrolled, channel-bound consumer identity
  distinguishes workloads within it. A channel label supplied by a caller cannot establish
  human authority. A trusted confirmation surface binds its approval to the exact manifest;
  transport checks supplement that authority check (§4.13).
- Evidence: `SO_PEERCRED`, `getpeereid`/`LOCAL_PEERTOKEN`, named-pipe client checks [B]; hecate's "an id is never a bearer capability" [C: survey-hecate.md §8.4]; vorpal's MCP enrollment rule [C: survey-vorpal.md §6.1].
- Grants are created only through the CLI or a confirmation surface a human operates, for the same reason vorpal enrols servable roots by hand: a confirmation delivered through the agent's channel would be answered by the agent. A landing opens the target beneath its own directory descriptor (`openat2` with `RESOLVE_BENEATH` on Linux, `O_NOFOLLOW` chains elsewhere) so it can never write outside the granted directory [B: openat2(2)]; the target must be owned by the calling user and must not lie inside a slates mount.

### D-23 Observability: chokepoint spans with the three-id law (request id, trace id + span id, caused-by), content-free signals, host-observed provenance, `(value, freshness)` always together
- Evidence: hecate TRACING/HEALTH [C: survey-hecate.md §5]; Dapper's ~200 ns span cost [C].
- The audit log: every grant, landing manifest and outcome is an append-only record readable through the CLI and exportable; drift, conflict and watcher-overflow counters per base.

### D-24 Engineering conventions: vorpal's target matrix, toolchain pin, lints, release recipes, npm/PyPI packaging, benchmark and test kinds; departures (own runtime, `windows-sys`, fmt gate, loom/Miri gates, i686 tested, latency gate)
- Evidence: `research/survey-vorpal.md` §1-§9; Appendix B.

### D-25 Disk is the source of truth: overlay volumes over host directories with lazily served bases, witnessed bases at copy-up, whiteouts and redirects, drift reported and never absorbed
- The decision: a volume's base is either empty (scratch) or an existing host directory (overlay). Create records the path and nothing else. Untouched entries are served from disk on demand; the first write copies up and records the witnessed base (stat fingerprint plus BLAKE3 of the bytes the edit was based on); deletes are whiteouts; renamed base directories record their origin; drift is detected by fingerprints with the racy-clean rule and reported as a typed event; watchers make reports prompt but are never the truth; a snapshot of an overlay volume is the delta, witnessed bases and retained base reference, with coverage `DeltaWithLiveBase`.
- Evidence: EdenFS's materialization contract ("An inode is not materialized if we have a source control object ID that can be used to fetch the inode contents"; materialization propagates upward; "materialized" on Windows means disk is the source of truth) [C: eden/fs/docs/Inodes.md, Windows.md via `research/edenfs-scale-distribution.md`]; hecate's physical contract ("the overlay contains exactly the materialized-iff-diverged entries (EdenFS's contract)"; "No reconcile operation exists: every write is witnessed at the serving boundary") [C: hecate SESSIONS.md:65-70]; overlayfs's whiteouts, opaque directories, copy-up on first write access, `redirect_dir` for directory renames, and its rule that offline changes to the lower tree make an overlay undefined, which is why slates detects instead of assuming [B: kernel overlayfs.rst]; git's index fingerprint and the racy-clean rule [B: git racy-git]; watcher overflow documented on all three OSes (`IN_Q_OVERFLOW`, `kFSEventStreamEventFlagMustScanSubDirs`, `ReadDirectoryChangesW` zero bytes or `ERROR_NOTIFY_ENUM_DIR`) [B: inotify(7); B: Apple FSEvents; B: Microsoft Learn]; CitC's fewer than ten private files per workspace [A: Potvin & Levenberg CACM 2016]. (`research/disk-source-of-truth.md` §1-§4)
- Lost: copying the base tree into memory at create (O(tree) provisioning; sylk's `memorySnapshotFS` read every file [C: survey-sylk-vfs.md §1.4]); assuming an arbitrary host directory has an available atomic snapshot; FUSE passthrough in any form (it needs `CAP_SYS_ADMIN`, which slates never requires); watchers as the truth (all three overflow); silent adoption of disk changes into the agent's view of witnessed entries.
- Measure: stat cost per entry and listing cost per directory size on each OS; watcher latency and overflow rate per base; copy-up cost per size class; base cache hit rate; base read cost per page class versus reading the host file directly.
- Consequence: memory is proportional to touched entries; provisioning stays one ring round trip; `status` reports drift; `rewitness`, `pin` and `read_base` are explicit verbs; live base service remains on the host that holds the directory; the delta owner may move while retaining that dependency (§4.10).

### D-26 Disk is written only by a landing under a human grant: a pure per-entry verdict, one holder per target, per-file compare-and-swap against outsiders, delta-only zero-copy parallel write-back with data and directory syncs, and an audit trail
- The decision: `materialize(snapshot, target)` is the only verb in the system that writes a host path. It plans a landing manifest (work proportional to diverged entries), obtains a grant bound to the manifest's hash from a human through the CLI or a confirmation surface (never through MCP or the SDKs), takes the single-holder landing lease on the canonical target, validates every entry by the verdict (witnessed base versus disk now versus overlay now: apply, skip, accept by identity, conflict), refuses while any conflict is unresolved, writes the delta with per-file compare-and-swap, syncs data then directories, advances the witnessed bases to what was written, clears those overlay entries, and records grant, manifest and outcome in the audit log.
- Evidence: hecate's landing engine ("three-way against the shared baseline; identical hashes short-circuit; single-side files land by reference; only doubly-touched files proceed"; "emitting conflict values on intersection, never interleaving"; "no automatic resolution of concurrent code edits, anywhere") and its receipts on structural mergers silently missing real conflicts [C: hecate SESSIONS.md:89-125; C: hecate ADR-0005]; its review gate ("materialization to a real target defaults to prompt, always, and requires zero unresolved conflict values") and its single-holder materialization lease (slates' landing lease) with a fencing generation [C: hecate SESSIONS.md:23-25, 128-133, 222, 229]; optimistic concurrency control (read phase, validation, write phase) [A: Kung & Robinson TODS 1981]; the diff3 pathologies that make inferred merges unsafe [A: Khanna, Kunal & Pierce FSTTCS 2007]; sylk's flusher as the shape to avoid (whole-overlay flush, union confirmations, `ResetOverlay`) [C: survey-sylk-vfs.md §1.7, §8.2]; vorpal's rule that a confirmation must never travel through the agent's own channel [C: survey-vorpal.md §6.1]; the swap primitives (`renameat2` with `RENAME_EXCHANGE` since Linux 3.15 and `EINVAL` where unsupported; `O_TMPFILE` since 3.11 with `linkat`; `renamex_np` with `RENAME_SWAP` advertised by `VOL_CAP_INT_RENAME_SWAP`; `FILE_RENAME_POSIX_SEMANTICS` with `REPLACE_IF_EXISTS`, "Existing handles to the replaced file continue to be valid") [B: rename(2); B: open(2); B: macOS rename(2); B: Microsoft ntifs `FILE_RENAME_INFORMATION`]; reflinks (`FICLONE` since 4.5; `clonefile` with `VOL_CAP_INT_CLONE`; `FSCTL_DUPLICATE_EXTENTS_TO_FILE` on ReFS) [B]; `F_BARRIERFSYNC` versus `F_FULLFSYNC` [B: macOS fcntl(2)]; `openat2` with `RESOLVE_BENEATH` since 5.6 [B: openat2(2)]. (`research/disk-source-of-truth.md` §1-§5)
- Lost: automatic three-way text merge at landing (silent interleaves of code nobody reviewed; hecate's one law); writing the whole tree (sylk); a grant by path rather than by manifest (the human would approve a plan that later changed); grants over MCP or the SDKs (the agent would answer its own question); advisory locks against outsiders on POSIX (editors and git ignore them); rename-over without verification (a silent loss window); a disk probe at boot (a write outside a grant).
- Measure: the online concurrency ramp inside each landing; exchange support per target filesystem; time per entry for plan, validate, write and sync; reflink availability and gain; the stage-and-exchange break-even; crash-resume cost.
- Consequence: every byte that reaches a disk is traceable to a human decision; the landing crate is the only crate that links write-capable file syscalls; the hermeticity tracer gains "zero writes outside granted targets".

### D-27 The merge engine: green volumes written only by a merge task; increments as constant-size descriptors of declared operations; canonical rebase by position mapping; a pure two-pass verdict (accept, accept-identical, conflict) with no inference; splice by extent surgery; merge records as fenced pointers; holders recompute; byte-exact conflict windows; rebase as the only corrective path; streaming submission (hecate's merge architecture adapted)
- The decision: a volume created with the `Green` role is written only by the merge task on its owner shard; agents clone a version into `Work` volumes; every mutation is journaled as a declared operation with its byte range and the file's previous version; `submit` seals the work volume, composes its declared operations into a net op set by interval algebra (never by comparing file states), and sends a constant-size increment naming the sealed post-state and the ops document by identity; the merge task deduplicates by increment identity, position-maps the ops through the canonical deltas since the increment's base (maps compose; old deltas fold into exact checkpoint deltas), runs the two-pass verdict (sweep-line overlap on descriptors, then memcmp only for same-range candidates), splices accepted ops into a new version by extent-list surgery without copying bytes, and commits the merge record as the next entry of green's ledger register, sent to all 2f+1 candidate holders under the owner's host epoch, committed at f+1 acknowledgements, and only when every referenced identity is placed; conflicts return byte-exact windows and the agent `rebase`s and resubmits; readers attach to versions and move only by `advance`; holders in a fleet recompute the verdict and the manifest identity before serving a version and compare head identities per version, mismatch fatal-and-loud.
- Evidence: hecate `MERGE.md` §0-§13 (the pipeline, the leader-fused proposer, the two-pass verdict, the composed-net-ops deriver with the never-diff clause, placed-before-referenced, appliers recompute, attachments to immutable versions, the submission transaction, conflict windows, the tripwires, the laptop degenerate, tests M1-M17b), ADR-0003 (the streaming gate: "green = increment-validated work; disk = claim-satisfied work"), ADR-0005 (canonical rebase, "no automatic resolution of concurrent code edits, anywhere"), `SERVING.md` §2-§4, `VFS.md` §5, `CONSENSUS.md` §6 [C: read directly 2026-09-04, quoted in `research/merge-engine.md` §1]; optimistic concurrency control [A: Kung & Robinson 1981]; input-logging state-machine replication [A: Schneider 1990; A: Calvin SIGMOD 2012]; Raft follower apply, PreVote and CheckQuorum [A: Ongaro & Ousterhout ATC 2014]; the OT falsification record and the diff3 pathologies [C: hecate ADR-0005; A: Khanna, Kunal & Pierce 2007]; sweep-line interval overlap [A: Bentley & Ottmann 1979]; leases with fencing tokens [A: Gray & Cheriton 1989]; sylk's merge faults (byte-level OT on whole-file diffs, a no-op conflict resolver, a blocked FIFO) [C: survey-sylk-vfs.md §8.2]. (`research/merge-engine.md`)
- Departures from hecate, each with its reason (`research/merge-engine.md` §2): the proposer is the owner of the green's own register under its host epoch, not a per-session Raft leader, because slates has one configuration group per region and partitioned single-writer execution, and the writer-equals-proposer property that leader fusion buys holds per object by construction; the deriver composes declared operations only, resolving hecate's own contradiction between `SERVING.md`/`VFS.md` ("diff against `prev_version`") and `MERGE.md` ("the banned diff inference"); splice is extent surgery over fixed page-multiple chunks with no re-chunking on the merge path; placement holders recompute instead of session-group followers; hard links and symlinks exist and are merged per path, conservatively; there is no Guardian or Arbiter, so validation is an opaque evidence policy on the green volume and the disk stays behind the landing grant of D-26; scratch scope becomes excluded subtrees in the increment filter; eg-walker is unnecessary because both sides have declared operations on one chain and checkpoints make every base mappable.
- Lost: symmetric operational transformation (no correct TP2 algorithm exists for strings; under a serializer rebasing suffices); an inferred three-way merge (diff3's pathologies; silent interleaves); last-writer-wins; locks or per-file leases as the conflict authority; a shared live mount of green (a pod's view must never move without its consent); big-bang merges at the end of a task (the streaming gate makes divergence short and conflicts cheap).
- Measure: per-op verdict cost; mapping cost per intervening delta and the checkpoint spacing; merge-path latency end to end on a laptop and across hosts; merges per second per green; base-lag distribution; conflict rate by source (SDK edits versus whole-file tool rewrites); `StaleEpoch` refusals on merge records; rebase-retry rate; holder recomputation cost.
- Consequence: D-16 gains the merge verdict as the conflict authority between work volumes; §4.5's journal records ranges and per-inode versions; §4.8 gains version chains, canonical deltas and merge records as ledger register entries; §4.10 gains placed-before-committed for increments and holder recomputation; §4.12 gains `slates.merge` and an SDK `edit` verb that declares true insert and delete ranges; a new Phase 6 builds the engine on one node and Phase 8 extends it to the fleet; a new crate `merge`.

---

## Part 4 — Detailed design by subsystem

Each subsystem follows the same skeleton: role; data model with ownership facts; state machines
and the step-by-step algorithms with every constant's derivation inline; failure and recovery
matrix (Masked / Degraded / Refused); closed refusal taxonomy; derived constants (Constant |
Formula | Anchors); a worked example including a failure; the laptop degenerate; integration
points. Decision numbers refer to Part 3.

### 4.1 Machine profile and boot calibration (D-11)

**Role.** Measure the machine once, derive every tunable from the measurements, and keep the
profile current. Nothing in slates carries a tuning literal; it carries a formula and an anchor.

**Data model.**
```rust
struct MachineProfile {
  version: u32,                      // bumped when any measurement method changes
  identity: HostIdentity,            // cpu model, core ids, os build, memory size: invalidates cache
  page: PageInfo,                    // base page, huge page sizes available, allocation granularity
  cache_line: u32,                   // measured/queried; padding for hot atomics
  cores: Vec<CoreInfo>,              // id, class (perf/efficiency), numa node, l2 size
  core_latency_ns: Matrix<u32>,      // measured core-to-core ring round trip / 2
  memory: MemoryInfo,                // total, free at boot, lock capacity (probed), address-space size
  fault_ns: [u32; PageClass::N],     // measured minor-fault cost per page class
  syscall_ns: u32, wake_ns: WakeStats, // measured: trivial syscall; park→unpark p50/p99
  memcpy_bw: Curve,                  // bytes/s by size class
  hash_bw: u64, lz4_bw: u64, zstd_bw: [Codec; LEVELS], // bytes/s
  bridge_rtt_ns: [u32; Op::N],       // measured after the root mount is up
  launcher_ns: Option<u32>,          // Linux: unshare + bind + exec cost
  measured_at: Monotonic, power_state: PowerState,
}
```
Ownership: the anchor process owns the profile in the shared segment; the daemon reads it; shards
copy the few fields they use into per-shard constants at start (no cross-shard reads on hot paths).

**Algorithm (boot).** 1. Query fixed facts (page size via `sysconf(_SC_PAGESIZE)` / `vm_page_size`
/ `GetSystemInfo`; cache line via sysfs / `hw.cachelinesize` / `GetLogicalProcessorInformationEx`,
falling back to 128 when the OS returns 0, the crossbeam rationale; cores and classes; NUMA;
memory totals; address space on 32-bit targets). 2. Probe lock capacity by locking geometrically
growing regions until refusal; record the largest success. 3. Time N iterations of each
microbenchmark (fault per page class with and without pre-population; trivial syscall; park/unpark
across two threads; ring round trip between every core pair, or a sampled subset above 32 cores;
memcpy at each size class; BLAKE3, LZ4, zstd per candidate level on a synthetic and, when
available, a sampled real corpus) with `CLOCK_MONOTONIC`, reporting median and bootstrapped
interval; N derives from the observed variance (stop when the interval is narrower than a fixed
fraction of the median, per Kalibera & Jones), with an upper bound on wall time so a slow machine
still boots. 4. Write the profile to the segment with its identity. 5. Re-run steps 3's cheap
subset (wake, memcpy, one hash size) on power-state notifications and on a slow cadence derived
from the observed drift between consecutive runs.

**Failure matrix.** Measurement refused by the OS (e.g. no `mlock` permission): Degraded, recorded
as "lock capacity 0" and surfaced. Profile cache mismatch (hardware changed): Masked, rebuilt.
A microbenchmark exceeding its wall-time bound: Degraded, widened interval recorded, marked
"quick profile".

**Refusals.** `ProfileUnavailable` (segment unreadable), `ProfileStale` (identity mismatch while a
volume requires a matching profile), `MeasurementTimeout`.

**Derived constants.** Spin window = wake_ns.p99 (Karlin); shard count = performance-class
physical cores minus one control core (minimum one); ring depth = ceil(arrival_rate × service_time
× safety) from measured rates with a power-of-two round-up (Little's law); pre-fault batch = idle
window / fault_ns; small chunk = smallest page multiple ≥ p90 sealed-file size; large chunk = size
where per-chunk fixed cost / memcpy cost < a fixed fraction chosen from the measured curve's knee.

**Worked example.** On the author's laptop the profile reads page 16 KiB, line 128 B, 18 cores in
two classes, 128 GiB with 108.8 GiB lockable; the 6 "Super" cores become shards, 12
"Performance" cores serve bridge work and the control shard; the spin window is the measured
park/unpark p99. Failure case: a locked-down CI container refuses `mlock`; the profile records
lock capacity 0, the diagnostic surface reports why residency cannot be established, and
volume admission refuses `LockCapacityExceeded`; no unlocked content claim succeeds.

**Laptop degenerate.** The same profile; one node in the failure-domain tree; no cluster fields.
Same code, zero modes.

### 4.2 Memory: arenas, slabs, handles, pages, RAM-only guarantees (D-8, D-12, D-13)

> **Status (A-9, 2026-09-05).** Allocator and quota components exist. The server does not
> establish the reservation below: `require_locked` is recorded without locking its store,
> mapped length can exceed buddy-allocatable length, and dynamic pressure does not account
> for competing claims. BUG-1–BUG-4 and GAP-A9-1 remain open.

**Ownership and residency.** Each shard owns bounded generational slabs and chunk arenas;
foreign frees are messages to the owner. Every allocation has a charge owner and a terminal
release step. Releasing a handle reuses its slot with a new generation; repeated open/close
cannot grow a vector of tombstones. Segment, slab and buddy geometry report usable capacity,
not mapping length. Conversion, rounding, counter arithmetic and generation exhaustion are
checked and refuse before mutation.

Before admitting content, the server prepares and locks the memory that will back it and its
metadata. This includes rings, logs, parse buffers, decompression, copies, archive construction,
base caches, guest request buffers and retained versions. An uncharged heap allocation cannot
sit outside the bound. Failed prefaulting or locking returns `LockCapacityExceeded` or
`BudgetExceeded`; a requested strict guarantee never silently becomes swappable service.
OS lock and working-set limits are inputs, not a reason to ask for root. Kernel/client/guest
caches are separate owners: an end-to-end no-spill claim requires the bridge and harness to
establish their residency and memory limits too. Report the established boundary precisely;
absence of explicit file writes alone does not prove absence of swapping.

**Atomic admission.** For each actual holder host, maintain this invariant:

`physical_used + outstanding_entitlement + operation_headroom + control_reserve <= effective_capacity`

`physical_used` includes allocator rounding, metadata and all resident copies. The outstanding
entitlement is the extra physical cost required to honor admitted logical quotas in the worst
permitted allocation shape, including future CoW. `operation_headroom` covers bounded temporary
coexistence during copy-up, seal, compression, transfer, capture and landing. `control_reserve`
keeps completion, cancellation, teardown and repair progressing. Effective capacity is the
usable prepared arena capacity constrained by OS/job/cgroup/lock limits and measured pressure;
raw free RAM and virtual address space do not qualify. Each term has measured or structural
anchors (§4.1); no percentage or retry count is invented here.

Admission reserves all required credits or none before publishing a volume or attachment.
The control owner distributes disjoint capacity credits to shards; the write path consumes
local credits, with no shared lock or consensus call. Credits cannot be spent twice across
shards, replay, delayed replies or cancellation. A remote holder makes the same admission
against its own machine before acknowledging placement. A promise on host A reserves nothing
on host B. Configuration and holder records expose the actual reservation and generation.

**Resource dimensions.** A byte quota cannot bound arbitrarily many empty files, names,
xattrs, open handles or snapshots. Admission therefore publishes a resource vector: content
bytes plus inode/namespace, xattr, handle, in-flight and retention allowances, each derived
from the requested policy and the prepared arena layout. Every dimension has its own bound
and refusal; `statfs` includes backed inode availability. The default allowances and their
cost are visible before admission, not hidden deductions discovered during a write. A write
within all admitted dimensions retains its entitlement. A create beyond an advertised inode
allowance may return ENOSPC even when content-byte space remains, just as on a finite filesystem.

The content quota keeps D-13/A-7's charge definition: head-reachable materialized content,
including shared referenced chunks, charged by the specified chunk-window rule. Unfetched
live host bytes are an external source dependency, not pre-reserved RAM. They consume cache,
copy-up or pin credits when fetched/retained. A live base's total logical size is not known
without a scan; neither create nor statfs invents that total. Complete snapshot roots carry
known referenced counts for O(1) clone admission. API output distinguishes these content
charges, metadata limits and external source size.

**Sacred bounded claims.** A bounded claim backs the whole admitted quota, not only current
usage. Dynamic growth, prefetch, caches and other clients consume only unpromised capacity.
Dedup charges each volume's logical referenced bytes in full and physical shared bytes once,
while preserving the capacity needed if all entitled writers diverge. A new retained snapshot
may require a separate retention charge: it cannot use up a writer's promised future space.
No admission relies on another user's promised space becoming compressible or evictable.

Grow reserves the additional entitlement atomically. Shrink refuses below retained obligations
or current logical usage. Destroy/revoke releases credits only after handles, mappings, queued
operations and all retained references are drained. Pressure first stops new admission and
reclaims only evictable, unpromised content; it never steals an admitted claim. External OS
revocation beyond the established resource boundary is reported as loss of that guarantee,
never hidden as a successful smaller reservation. Admission prevents Slates from exhausting
its verified host budget; it cannot constrain unrelated privileged allocators on the machine.

**Dynamic volumes and base cache.** Growth increments derive from measured allocation rate
and arena preparation time, bounded by the declared maximum and unpromised capacity. Cached
base bytes are evictable only while they can still be re-read from their identified source.
Witnessed, copied-up and explicitly pinned bytes are retained obligations. Watcher hints speed
invalidation but cannot prove a cache entry clean; revalidation and the racy-clean rules in
§4.15 remain authoritative. Cache overflow and pressure have typed results.

**Refusals and status.** `BudgetExceeded{requested, available}`, `LockCapacityExceeded`,
`ArenaExhausted`, `QuotaExceeded` (ENOSPC at a mount), `RetentionBudgetExceeded`, `StaleHandle`,
`GenerationExhausted`, `ResourceGuaranteeLost`. Report logical quota/used, physical used,
reserved remaining, retention, transient/control reserve, locked bytes and evictable bytes
with freshness. `statfs` derives its result from these counters (§4.6).

**Worked example.** A host has 6 GiB of usable prepared capacity after control and operation
reserves. A claim whose full physical obligation is 4 GiB leaves at most 2 GiB for every other
admission combined, even while that volume is empty. A second 3 GiB obligation refuses without
changing either account. These are illustrative quantities, not tuning constants. Concurrent
writers to the admitted volume must still be able to consume its remaining entitlement.

**Laptop degenerate.** One host, the same accounting and per-shard credits; the fleet applies
the invariant independently at every holder. Acceptance is observable competing-allocation,
pressure, fragmentation, restart and open/close behavior, not assertions on quota fields.

### 4.3 Runtime: thread-per-core executor, drivers, rings, cancellation (D-7, D-9)

**Data model.**
```rust
struct Shard { id: u16, core: CoreId, tasks: TaskArena, run_queue: IntrusiveList, timers: TimingWheel,
               driver: Driver, inbound: [SpscRing<Msg>; N_SHARDS], wake_word: WakeWord }
struct TaskSlot { state: u8, generation: u32, future: PinnedBox, parent: Option<TaskId>, links: ListLinks }
struct RawWakerData(u64) // shard:16 | slot:24 | generation:24, packed; Copy
```
Ownership: a task belongs to one shard for its whole life; a `Waker` is a `Copy` encoding, so
`clone`/`drop` are no-ops and `wake` from another shard enqueues (slot, generation) on the target's
ring and kicks the driver; a stale generation is ignored.

**Loop.** Each iteration: drain the driver's completions (io_uring CQ / kqueue events / IOCP
packets) into the run queue; drain inbound rings (client command rings, cross-shard rings, the
bridge queue) up to a batch bound derived from the measured service time and the latency budget;
run ready tasks to their next await; expire timers; store the loop generation (QSBR); if nothing
is ready, spin for the measured idle-spin window (while any client is active), then park in the
driver. Cancellation: dropping a future releases nothing it did not own (resources live in arenas
keyed by handle and are released by the owning operation's terminal step), so every operation is
cancel-safe by construction; a cancellation request is a message that guarantees a terminal
completion.

**Drivers.** Linux: io_uring with `SINGLE_ISSUER|DEFER_TASKRUN` when the kernel is ≥ 6.1 and
io_uring is permitted (probed at boot: a refused `io_uring_setup` selects epoll); registered
buffers for ring and chunk pages; FUSE-over-io_uring per-shard queues on ≥ 6.14; eventfd for
cross-shard kicks. macOS: kqueue with `EVFILT_USER` kicks and non-blocking socket I/O for the NFS
server. Windows: IOCP with `PostQueuedCompletionStatus` kicks; WinFsp requests arrive on WinFsp's
threads and are routed to the owning shard by handle. Simulation: a driver that replaces time,
randomness, sockets, rings, and the bridge with seeded in-process simulations for the whole
cluster.

> Status (2026-09-09): the driver readiness seam carries both directions. `Driver::register_readable`
> and `register_writable` register one-shot interest in a descriptor's readability or writability with
> the shard's driver (kqueue `EVFILT_READ`/`EVFILT_WRITE`, epoll `EPOLLIN`/`EPOLLOUT`, each re-armed
> after the edge is consumed); the completion-native drivers (io_uring, IOCP) refuse both as owed with
> a typed `RtError`, and the simulation refuses writability (its in-memory fabric sends never block).
> Over that seam the runtime exposes two async sockets that share one readiness future
> (`crates/rt/src/readiness.rs`, so neither duplicates the register-and-yield dance): `udp::UdpSocket`
> (§4.10a, the fleet plane — a real `rustix` socket or the simulated fabric) and `tcp::{TcpListener,
> TcpStream}` (§4.6, the NFS loopback server — real only, since TCP is host-local and has no simulated
> fabric). `TcpStream::write_all` awaits writability when the send buffer fills, so a write to a stalled
> peer (a soft-mounted NFS client that stopped reading) yields the shard rather than blocking it. Proven
> by use on the readiness-native driver: `crates/rt/tests/tcp.rs` runs an accept→read→write→read round
> trip with both ends on the runtime's own sockets, `tests/udp.rs` the datagram path. The
> completion-native socket path (io_uring, IOCP) stays owed.

**Failure matrix.** Driver setup refused (seccomp): Masked (epoll). Task arena full: Refused
(`TooManyTasks`, admission). A task exceeding the bounded-work rule (measured per-iteration budget
exceeded N times): Degraded, counted, the offending operation is chunked by design (destroys,
large clones) so this is a bug signal.

**Derived constants.** Task arena size = admission limit (Little's law on measured request rate ×
p99 service time); batch bound = latency budget / measured per-item cost; idle spin window =
wake_ns.p99.

**Laptop degenerate.** Shards = performance cores; same loop; the cluster rings exist with zero
peers.

### 4.4 Volume model and lifecycle (D-1, D-5, D-13, D-16, D-18, D-25, D-26)

**Data model.**
```rust
struct Volume { id: VolumeId, name: Name, owner_shard: u16, policy: VolumePolicy,          // bounded|dynamic, name-equivalence, require_locked
                base: Base,                                                                 // retained across every clone
                head: SnapshotId, epoch: Epoch, root: Handle<DirNode>, inodes: Slab<Inode>,
                referenced_bytes: u64, unique_bytes: u64, quota: Quota,
                lease: Option<Lease>, attachments: Vec<Attachment>, lineage: LineageEdge,
                log: OpLog, deadlists: Vec<Deadlist>, state: VolumeState }
struct VolumePolicy { size: Bounded | Dynamic, names: NameEquivalence, require_locked: bool, role: Role }
enum Role { Plain, Work { green: VolumeId, base: Version, excluded: FilterId, stream: bool },
            Green { require_evidence: bool, head: Version /* the chain lives in §4.16's records */ } }
enum Base { Scratch, Immutable { snapshot: SnapshotId }, RemoteLive { reference: BaseRef },
            Path { root: HostDir /* an open directory descriptor, never a string on hot paths */,
                   witnesses: Art<PathKey, Witness>, listings: Art<PathKey, ListingCache>,
                   drift: Art<PathKey, Drift>, watch: Option<WatchHandle>, fs: BaseFsFacts } }
struct Witness { fingerprint: Fingerprint, identity: Blake3, witnessed_at: Monotonic, racy: bool }
struct Fingerprint { dev: u64, ino: u64, size: u64, mtime_ns: i128, ctime_ns: i128, mode: u32 }
struct Snapshot { id: SnapshotId, epoch: Epoch, root: Handle<DirNode>, deadlist: Deadlist, refs: u32, identity: Option<Blake3>,
                  witnesses: Option<Handle<WitnessSet>>, base: BaseRef, coverage: SnapshotCoverage }
enum SnapshotCoverage { Complete, DeltaWithLiveBase }
// BaseRef includes source identity, serving authority and lifetime, not just a host pathname.
struct Lease { holder: PrincipalId, epoch: u64 /* fencing token */, expires: Monotonic }
struct Attachment { id: AttachmentId, consumer: ConsumerId, view: AttachedView, form: AttachForm,
                    rights: Rights, generation: Epoch, state: AttachmentState, accounting: AttachStats }
enum AttachedView { LiveHead(VolumeId), Frozen(SnapshotId) }
enum AttachmentState { Binding, Bound, Advancing, Draining, Detached }
enum VolumeState { Creating, Live, Sealing, Landing, Archived, Restoring, Destroying, Destroyed }
```
Ownership: all fields live on the owner shard; readers on other shards see only published
snapshot roots (immutable) and the catalog record replicated through the database. `HostDir` is
opened once at create (`O_DIRECTORY|O_NOFOLLOW`, or the Windows directory handle) and every base
access is relative to it, so a later rename of the base directory by the user changes nothing.
The base plane's records (`witnesses`, `drift`, `listings`) are volume state on the owner and must be recoverable with content and roots from the anchor segment. This is the target;
current server reconstruction drops content (BUG-11). The sketches above describe the required
model, not the currently encoded wire schema. All collections have §4.2 bounds.

**State machine.** `Creating → Live` (one release store); `Live ↔ Sealing` (snapshot in
progress: the head epoch advances, the old head becomes a snapshot record, in-place mutation of
current-epoch nodes resumes immediately, hashing of the sealed tree proceeds in the background);
`Live ↔ Landing` (a granted landing of one of the volume's snapshots is writing the target; the
volume keeps serving reads and writes; entries being written are marked so a concurrent write to
one of them lands in the next manifest, never in this one); `Live → Archived` (a snapshot
compressed per the cost model, the live tree dropped); `Archived → Restoring → Live`;
`Live|Archived → Destroying → Destroyed` (walk deadlists and unique chunks cooperatively, release
quota, tombstone the id for the lease horizon).

**Operations and their steps.**
- create(name, policy, quota, base?): as in Part 2.2; `base` is either a snapshot id, in which
  case the new head root is the base's root (clone) and the lineage edge records origin pinning,
  or a host directory path, in which case the daemon resolves and opens the directory (refusing
  a path inside a slates mount, a path the caller does not own, or a path that resolves through a
  symlink out of its parent), records `Base::Path` with empty witness, listing and drift tables,
  and returns. No walk, no hashing, no copy; the cost is one directory open beyond the scratch
  case. Initial enrollment/open is separately measured; the <50 µs claim covers the admitted
metadata provisioning path, not arbitrary host pathname resolution. A scratch volume becomes an overlay volume over the directory of its first landing. In
  a fleet the create is just as local: the id carries the creator host, the owner is the
  creator, the candidate holders are computed from the neighbourhood, the epoch-one head record
  goes to them after the local commit, and the reply carries `placed: false` until f+1 have
  acknowledged; nothing is written to any consensus group.
- snapshot(volume) → SnapshotId: establish the attachment barrier, then publish a frozen root,
  witness-set handle, retained `BaseRef` and coverage; advance the epoch. The root publication
  is O(1). With a live base, untouched entries remain live and coverage is `DeltaWithLiveBase`;
  its identity covers the delta, witnesses and source reference, not unfetched host bytes.
  With an empty or complete immutable base, the snapshot covers the entire logical tree.
  `await placed(snapshot, scope)` reports which coverage was placed, never silently upgrading it.
- capture_base(volume, consistency) → SnapshotId: explicitly capture the whole logical tree
  and its metadata into a complete immutable root using §4.15's stable-source protocol.
  Requested atomic point-in-time consistency refuses `ConsistentBaseUnavailable` unless the
  source can be quiesced or read through a supported immutable snapshot. A before/after stat
  check is not proof of atomic capture of an arbitrary changing tree. The operation is bounded,
  cancellable and separately charged; cost is proportional to content examined and retained.
- clone(snapshot) → Volume: share the delta root, witnesses and complete base reference by
  handles; add the claim and lineage record. Root cloning is O(1). A remote clone retains the
  base service dependency or uses an already complete immutable base. It never turns into an
  empty scratch volume merely because its source directory is remote.
- attach(volume|snapshot, consumer, transport, chosen_path?) → Attachment: authenticate the
  consumer and requested access, reserve all costs, pin the view and generation, establish
  the path or device, then publish `Bound`. Refusal rolls back owned resources. A metadata
  record with `path: None` is not a successful requested host mount. Mount/device setup time
  is reported separately from volume provisioning.
- detach(attachment): enter `Draining`, stop new effects, establish the flush boundary and
  fence the generation; drain outstanding requests, invalidate caches and revoke mappings,
  then release view references and credits and enter `Detached`. Failure preserves enough
  state for retry or explicit failed-consumer cleanup; it never reports a clean flush of
  data that a crashed client had not submitted.

- resize(volume, quota): bounded → move regions in or out of the reserve (refuse if
  `referenced_bytes > new quota`); dynamic → change the maximum.
- archive(volume|snapshot) → Archive: seal (hash), compress per the cost model, keep the manifest
  uncompressed, drop the live tree if the volume itself is archived; export streams the archive to
  the caller's byte sink (never to disk by slates).
- restore(archive) → Volume: parse and verify the manifest, create a volume whose chunks are the
  archive's records, decompress lazily on first read.
- status(volume) → Status: counters, the lease, `last_placed_snapshot_age`, and for overlay
  volumes the drift list (every witnessed entry whose disk fingerprint no longer matches, with
  the kind of change: modified, deleted, replaced, type changed) and the watcher state (live,
  overflowed, unavailable).
- read_base(volume, path) → bytes: the entry as the disk holds it right now, for an agent that
  wants to reconcile a drifted entry itself; a read, never a write.
- rewitness(volume, paths?) → Rewitnessed: re-witness the named drifted entries (default: all) to the
  disk as it is now, after the agent has rewritten its copies; the agent's content is untouched;
  the drift records for those entries clear. This is the explicit "a resolution is itself a
  change" step; slates never merges.
- pin(volume, paths?) → Pinned: read the named subtrees (default: the whole base) into the store
  and witness them, so the view of those entries is stable regardless of later disk changes;
  cost proportional to what is pinned and reported in the reply; never implicit.
- materialize(snapshot, target, filter?, grant?) → LandingRequest | LandingReport: the landing
  of §4.15. Without a grant the reply is `GrantRequired{request_id, manifest_hash, summary}` and
  the request waits (or returns at once if the caller asked not to wait); a human issues the
  grant through the CLI or a confirmation surface; the landing then runs and the report lists
  every entry's outcome. The only verb that writes a host path.
- destroy(volume): mark Destroying, refuse new attachments, recall leases, walk deadlists and
  unique chunks in cooperative slices, release quota, tombstone; the base directory is untouched.
- move/rename across volumes: refused (`CrossVolumeMove`); the SDK offers clone-subtree + unlink.
- Merge verbs (D-27, §4.16): `submit(work_volume, evidence?) → Accepted{version} | Conflict{windows}`
  (seal, compose, send the increment to the green's owner, park, resume with the verdict);
  `rebase(work_volume, to?) → Rebased{version} | Conflict{windows}` (move the work volume's base to
  a newer green version by mapping its pending operations; unchanged on conflict);
  `advance(attachment, version)` (re-pin a green attachment; targeted invalidations);
  `versions(green, from?, limit?)` and `changed_since(green, version, paths?)` (reads of the chain
  and the per-path last-changed index); `edit(volume, path, at, delete_len, bytes)` (an SDK write
  that declares a true insert or delete). A mount write to a green volume is `EROFS`; an SDK
  write is `ReadOnlyVolume`.

**Attachment lifecycle.** `Binding → Bound → Draining → Detached`; immutable readers may
`Bound → Advancing → Bound` only after a requested version is authorized, placed to its required
scope and ready to serve. `advance` drains old requests, invalidates old caches/mappings and
publishes the new generation atomically. It cannot combine old names with new bytes. Writable
attachments follow the one-owner lease; a read-only guest cannot acquire write authority by
setting FUSE flags. Revocation stops admission before any later device, queue or VFS effect.
Every request belongs to a live attachment generation; stale work is refused before application.
The bridge barrier and device teardown obligations are §4.6, including consumer crash.

**Leases and fencing (D-16).** Every mutation carries the caller's attachment id and lease epoch;
the owner shard compares the epoch with the volume's current lease; lower epoch → `StaleLease`
(refused, never applied); a lease renews implicitly on activity and expires after a term derived
from the measured renewal RTT and the failover budget (Gray & Cheriton: seconds); a new holder
takes the lease only after expiry or explicit release, with epoch + 1; recallable subtree
delegations for explicitly shared volumes follow the same epoch rule per delegation. A landing
holds a second kind of lease, the landing lease on the canonical target directory, with its own
fencing generation (§4.15); the two never substitute for each other.

**Failure matrix.** Owner shard restart: Masked (op log replay from the anchor segment; leases
survive because they are in the log; witnesses and drift records replay with it). Owner host loss
in a fleet: Degraded (the regional configuration group bumps the host's epoch and assigns the
volume to the surviving candidate holder that rendezvous ranks first; that host runs phase one
across the neighbourhood, adopts the newest head record, and serves; live edits since the last
seal are reported lost as the volume's stated loss window, or recovered from the shipped log for
`live-shipped` volumes). A delta owner may be promoted, but unfetched paths in a live base on
the lost host report `BaseUnavailable`; only complete placed snapshots are independent of it
(§4.10). A replayed id without its bytes must refuse, never return a successful empty tree.
Lease holder (a client) crash: Degraded (writes refused until the lease expires; the SDK reports
`LeaseExpired`). Quota exhausted: Refused (`ENOSPC`). Attach with a chosen path that cannot be
honoured: Refused (`ChosenPathUnavailable{reason}`). Archive with insufficient memory for the
compressed copy: Refused (`BudgetExceeded`) before any chunk is touched. Base directory
unreadable or gone: Refused (`BaseUnavailable{path, errno}`) for untouched entries under it;
witnessed and pinned entries keep serving. Drift under a witnessed entry: Degraded (reported in
`status`; the agent's copy serves; the next landing verdict sees it). Landing failures: §4.15.

**Refusal taxonomy (closed).** `NotFound`, `AlreadyExists`, `StaleLease`, `LeaseHeld`,
`ENOSPC`, `BudgetExceeded`, `LockCapacityExceeded`, `ArenaExhausted`, `ChosenPathUnavailable`,
`CrossVolumeMove`, `InvalidName`, `PolicyMismatch`, `Destroying`, `Archived`, `StaleHandle`,
`DeadlineExceeded`, `Cancelled`, `Unsupported{platform, feature}`, and for the base and landing
planes `BaseUnavailable{path, errno}`, `BaseDrift{entries}`, `TargetNotOwned{path}`,
`TargetIsVolume{path}`, `EscapesTarget{path}`, `GrantRequired{request_id, manifest_hash}`,
`GrantMismatch{expected, got}`, `GrantExpired`, `GrantRefused`, `Conflict{entries}`, `TargetInUse{path}`,
`LandingLeaseHeld{holder, generation}`, `LandingPartial{manifest, failures}`, `Forbidden{verb}`,
`GrantChannelRefused{channel}` (A-8), and for the merge
plane `ReadOnlyVolume`, `NotGreen`, `NotWork`, `UnknownBase{green, version}`,
`MergeConflict{windows}`, `IncrementTooLarge{limit}`, `EvidenceRequired`,
`DuplicateIncrement{original}` (informational), and for the fleet `StaleEpoch{current}` (a
holder refusing a record below the epoch it has seen), `ConfigurationStale{version}` (a request
carrying an old configuration version; the reply carries the current one), `NotPlaced{scope}`
(an `await placed` deadline passed).
A-9 adds `RetentionBudgetExceeded`, `GenerationExhausted`, `ResourceGuaranteeLost`,
`ConsistentBaseUnavailable`, `RecoveryIncomplete`, `AttachmentUnsupported{transport, reason}`,
`BarrierIncomplete{attachment, generation}`, `ConsumerNotEnrolled`, `ConsumerRevoked`,
`GrantIssuerUnverified` and `LeaseUnconfirmed`. `QuotaExceeded` maps to ENOSPC at a mount;
subsystem-specific wire/transfer errors retain their closed kind through adapters. An
uncategorized refusal or a false success is a bug. These required variants are not all
implemented by the current schema.

**Derived constants.** Lease term = k × measured renewal RTT p99 with k chosen so that the
failover delay (one term) stays under the operator's failover SLO; tombstone horizon = lease term
× 2; destroy slice = measured per-node free cost × the shard's per-iteration budget; the base and
landing constants are in §4.15.

**Worked example.** An agent clones snapshot S of a 2 million-file volume: the reply arrives in one
ring round trip; the clone's `referenced_bytes` equals S's referenced bytes (charged in full),
`unique_bytes` is 0; the agent writes one file: the path from the root to that file is copied
(≈ depth × fanout entries, a few KiB), the file's chunk is allocated, `unique_bytes` grows by the
chunk size, `referenced_bytes` by the same. Failure: a second agent with a stale lease epoch writes
to the same clone and receives `StaleLease{current_epoch}`; nothing was applied.

Overlay example: `create("work", dynamic, base="/home/u/proj")` opens the directory and returns
in one round trip with no walk; the agent's `cargo build` reads thousands of untouched source
files from disk through the bridge (cached, evictable) and writes 190k files under `target/`,
all of which live in memory; the agent edits `src/lib.rs`, which copies up: the disk file is
read once, its fingerprint and BLAKE3 become the witnessed base, and the edit lands in an open
extent. The user then runs `git pull` on the host, which replaces `src/lib.rs` and
`src/main.rs`; `status` reports drift on `src/lib.rs` (witnessed, modified) and nothing for
`src/main.rs` (untouched entries show the live disk). Failure: the agent reads a large-class
base file whose remaining extents are still served from disk while the user truncates it in
place; the daemon's `fstat` on its held descriptor sees the size and ctime change and the read
returns `BaseDrift{[src/data.bin]}` instead of torn bytes; the agent's own written extents are
intact.

**Laptop degenerate.** Identical; replication queues have no peers; overlay volumes are the common
case on a laptop; a live base retains its source-host dependency in a fleet.

### 4.5 Namespace and content structures (D-4, D-5, D-6, D-25)

**Data model.**
```rust
struct DirNode { born: Epoch, entries: DirEntries, parent: Option<InodeNo> /* resolved to the head's node through the inode table (A-7) */,
                 inode: InodeNo, name: Box<str> /* its own name in the parent */,
                 base: BaseDirState /* None | Merged{listing: ListingRef} | Opaque */,
                 origin: Option<PathKey> /* redirect: the base path this directory was renamed from */ }
enum DirEntries { Small(InlineArray<Entry, 2> /* names inline, the measured cut-over */), Indexed(Tree /* CoW B+-tree of 4 KiB slotted blocks in the store's block slab, keyed by (hash, folded name); A-7 */) }
struct Entry { name_hash: u64, name: NameRef /* into the node or the block */, kind: Kind /* Dir | File | Symlink | Whiteout */, child: Handle<DirNode> | InodeNo }
enum Body { …, Directory(Handle<DirNode>) /* a directory inode names its current node (A-7) */ }
struct Inode { no: InodeNo, gen: u32, kind, mode, uid, gid, nlink, size, atime, mtime, ctime, btime,
               body: Body, identity: Option<Blake3>, born: Epoch, flags }
enum Body { Inline(SmallBytes), Extents(ExtentList), Open(OpenExtent, ExtentList), Symlink(NameRef),
            Base { witness: Option<Handle<Witness>>, pinned: ExtentList, fd: Option<BaseFd>, lost: bool } }
struct Extent { off: u64, len: u64, src: ExtentSrc } enum ExtentSrc { Chunk{chunk: ChunkHandle, off: u32}, Zero, Base{fd: BaseFd} }
struct Chunk { born: Epoch, len: u32, class: PageClass, bytes: ArenaRange, identity: Option<Blake3>, encoding: Encoding, evictable: bool }
struct ListingCache { dir_fingerprint: Fingerprint, entries: SortedArray<BaseEntry>, read_at: Monotonic }
```
Ownership: all on the volume's owner shard; sealed chunks are immutable and may be read from any
shard by handle while a snapshot pins them. A `BaseFd` is a descriptor the owner shard holds on a
base file (opened `O_RDONLY|O_NOFOLLOW` relative to the base directory descriptor); it keeps the
inode's data alive if the file is renamed over or unlinked on disk, and it is the handle the
drift check `fstat`s.

**Algorithms.**
- Lookup: hash the name under the volume's equivalence policy (byte-exact: hash bytes; fold:
  hash the folded, normalized form; compare folded forms on collision); `Small`: binary search
  over `name_hash` then compare; `Indexed`: hash side index then ordered node for the entry. In
  a `Merged` directory the overlay is consulted first: an overlay entry wins, a `Whiteout` ends
  the lookup with `ENOENT`, otherwise the base listing is consulted (loaded on first use with the
  OS's bulk listing call, validated by the directory's fingerprint, refreshed when a watcher hint
  or a stale fingerprint says so) and a hit creates an unloaded inode whose body is `Base`.
- Readdir: `Merged` directories return the merge of two sorted sequences (overlay entries with
  whiteouts removed and base entries they shadow skipped), canonical order, `readdirplus`
  attributes from the listing cache without a per-entry `stat`; `Opaque` directories (created by
  the volume, or a base directory the volume deleted and recreated) return only overlay entries.
- Mutation (create/unlink/rename): if the directory node's `born < current epoch`, copy the node
  (and its ancestors up to the first current-epoch ancestor) into current-epoch nodes, add the
  replaced nodes to the head snapshot's deadlist (ZFS `block_kill` rule), then mutate in place;
  otherwise mutate in place. `Small → Indexed` conversion at the measured cut-over; `Indexed →
  Small` when the count falls below half of it. Unlinking a base-backed entry writes a `Whiteout`
  in its place (the base listing still holds the name); unlinking an overlay-only entry removes
  it. Creating an entry over a whiteout replaces the whiteout.
- Copy-up (first write, truncate, chmod or other metadata change to a base-backed file):
  `fstat` the base descriptor, compare with the listing's fingerprint (re-hash if racy: the
  file's mtime is not older than the listing's read time by more than the filesystem's timestamp
  granularity), record the `Witness` {fingerprint, BLAKE3, time}, then for the small class read
  the whole file into pinned chunks, and for the large class keep the descriptor and pin only
  the written page-multiple ranges (the rest reads through `ExtentSrc::Base`); the inode's body
  becomes `Open` over the pinned extents. The class boundary is the measured large-file class of
  D-6. Metadata-only changes copy up the witness and pin nothing.
- Rename: within a volume, serialized on the owner (free); cycle check walks parent handles
  (O(depth)); POSIX semantics for existing targets; inode numbers preserved; the journal records
  the old and new paths. Renaming a base-backed directory records `origin` (overlayfs's
  `redirect_dir` rule) so the landing can issue one rename on disk, and leaves a whiteout at the
  old name; renaming a base-backed file copies up its witness (bytes stay wherever they were) and
  leaves a whiteout.
- Write: to `Body::Open`'s mutable extent (owned, page-multiple growth) until seal; a write beyond
  the open extent's capacity grows it by a page multiple from the buddy tree; a write into a
  sealed extent copies only the touched page-multiple range into a new open extent (CoW at chunk
  granularity).
- Seal (on close-after-write when the file is idle, on snapshot, on archive, on replicate): split
  the open extent into chunks (page-multiple fixed sizes; CDC for the measured large-file class),
  hash lazily (a background task hashes sealed chunks and folds identities into the content index;
  until then `identity = None` and dedup is deferred), replace duplicates by reference (the
  duplicate's bytes are released; `unique_bytes` falls, `referenced_bytes` is unchanged).
- Read: extent lookup by offset (binary search in the array, tree search beyond the cut-over), a
  reply that references arena pages (splice / registered buffers); holes produce zero pages from a
  shared read-only zero region. A `Base` body without a witness reads the disk file through the
  descriptor into evictable cache chunks (one copy, §4.6); a `Base` body with a witness reads pinned extents from the arena and unpinned
  extents through the descriptor after an `fstat` drift check; a failed check marks the body
  `lost` and the read returns `BaseDrift`, never torn bytes.
- Drift detection: fingerprints are the truth; a witnessed entry is re-checked on every read of
  an unpinned extent, on every `status`, before every landing verdict, and when a watcher hint
  names its directory; a watcher overflow (`IN_Q_OVERFLOW`, `MustScanSubDirs`, zero-byte
  `ReadDirectoryChangesW`) schedules a full re-check of the volume's witnessed entries and
  invalidates listing caches under the reported directory; hints coalesce at the measured
  inter-arrival p50 of event bursts.
- Inode numbers: `(volume prefix, monotonic counter)`; `gen` bumps on slot reuse; the bridge
  reports `(no, gen)`. Base-backed entries receive their inode number at first lookup, exactly as
  EdenFS allocates them, and keep it for the volume's lifetime.
- Journal: every mutation appends a declared operation `{seq, op, path(s), inode, epoch, at, len,
  prev_version}` to the volume's op log (a mount `write` is `Overwrite` or `Extend` with its
  offset and length; `truncate` records the new length; an SDK `edit` is `Insert` or `Delete`
  with true positions; namespace calls are themselves; `prev_version` is the file's per-inode
  version counter before the operation; bytes are never in the journal, they are in the
  extents); the bridge's change-notification path, the SDK's watch stream and the deriver of
  §4.16 read it; witness, whiteout,
  redirect and drift records are journaled the same way; retention is bounded by a memory budget
  derived from the volume's measured mutation rate and the longest subscriber lag.

**Failure matrix.** Hash collision on equivalence (two names fold equal): Refused (`EEXIST`),
which is APFS behaviour. Cut-over conversion mid-operation: Masked (conversion is a copy of a
current-epoch node). Seal hashing backlog under sustained writes: Degraded (dedup deferred; a
counter exposes the backlog; hashing is skipped for volumes whose measured hit rate does not pay).
Base listing unreadable: Refused (`BaseUnavailable`) for that directory's untouched entries;
overlay entries in it still serve. Base file replaced on disk while the volume holds its
descriptor: Masked for reads through the descriptor (the old inode's data is alive), reported as
drift. Base file overwritten in place: Degraded (`BaseDrift` on reads of unpinned extents; `lost`
set; pinned extents and the agent's writes intact). Watcher unavailable or overflowed: Masked
(fingerprints; the full re-check scheduled; `status` says `watcher: overflowed`). Descriptor
budget exhausted by large-class copy-ups: Degraded (further copy-ups pin whole files until
descriptors are released; counted).

**Derived constants.** Small-directory cut-over = the entry count where measured lookup time in
the sorted array exceeds the hash-side-index lookup (from the profile's cache-line and memcpy
curves; starting point one SIMD group of hashes); ordered node fanout = entries per two cache
lines; small chunk = smallest page multiple ≥ p90 sealed size; large chunk = memcpy-curve knee;
CDC threshold = size above which measured dedup gain per hashed byte exceeds hashing cost; journal
budget = mutation rate × max subscriber lag × record size; copy-up class boundary = the CDC
threshold; racy window = the base filesystem's timestamp granularity (from a cited per-filesystem
table keyed by the type `statfs` reports) plus the measured clock resolution; watcher coalescing
window = measured p50 inter-arrival of event bursts; descriptor budget for large-class copy-ups =
`RLIMIT_NOFILE` (or the Windows handle budget) minus the daemon's measured steady-state use,
divided by the measured number of concurrently open large-class files.

**Worked example.** `cargo build` in a clone creates 190k files under `target/`: each create
touches its directory (mostly current-epoch nodes, mutated in place after the first touch), the
open extents absorb writes with no hashing; a later `snapshot` is one record; the background
sealer then hashes the new chunks, and identical `.rlib` outputs across two clones dedup to one
copy. Failure: `rename("a/b", "a/b/c")` returns `EINVAL` from the cycle check without touching any
node. Overlay case: `rm -r vendor/` on a base-backed directory of 40k entries writes one opaque
whiteout for `vendor` (not 40k whiteouts), and the landing issues one recursive removal under
the verdict that every entry beneath still matches its listing fingerprint.

**Laptop degenerate.** Identical.

### 4.6 OS bridges (D-1, D-2, D-3)

> **Status (A-9, 2026-09-05).** Linux codec, dispatch, base-file and mount/launcher source
> exists, with tests recorded in §8e of GAPS. Complete mounted POSIX behavior is unverified:
> the audit finds a wrong writeback flag, advertised-but-undispatched READDIRPLUS, missing
> fsync/link handling, ignored setattr fields/rename flags, incomplete base lookup and invented
> statfs capacity (BUG-5–BUG-10). Invalidation encoding is not proof of delivered kernel
> coherence. virtio-fs, macOS and Windows adapters remain planned; no mounted suite was rerun.

**Role.** Present the root mount and every attached volume to the kernel; translate kernel
requests into shard operations by handle; emit invalidations; read base files for overlay
volumes; never write to disk.

**Bridge trait (one VFS operation layer, native and virtio-fs transports).**
`lookup`, `getattr`, `setattr`, `readdir`/`readdirplus`, `open`, `create`, `read`, `write`,
`flush`, `release`, `forget`, `fsync` (never writes a disk: on a laptop it returns success once
the operation is in the anchor segment; in a fleet, for a volume whose policy asks for it, it
seals the file's dirty range and returns when f+1 holders have it in RAM; the disk is written
only by a granted landing, §4.15), `mkdir`, `unlink`, `rmdir`, `rename`, `link`, `symlink`,
`readlink`, `statfs` (reports the volume's quota and `referenced_bytes`), `xattr*` (per
platform), `notify` (invalidation). Every request carries `(volume handle, inode no, gen)`;
replies reference arena pages. A mount of a green volume answers every mutating request with
`EROFS` (D-27): its only writer is the merge task; `advance` swaps the pinned version and
invalidates exactly the paths the manifest diff names.

**Linux (own /dev/fuse driver).** Mount: at daemon start (or restore from the anchor's held fd),
using the OS-installed broker for privileged mount establishment; the daemon itself never
requires `CAP_SYS_ADMIN`, root or a Slates-owned setuid helper. Namespace attachment capability
is checked by the launcher and refused when unavailable; it is not a daemon prerequisite. Options: `default_permissions`, `allow_other` only
if `user_allow_other` is set and the operator asked; `FUSE_INIT` negotiates writeback cache,
splice, readdirplus, `EXPLICIT_INVAL_DATA`, `EXPIRE_ONLY`, parallel dirops, and `OVER_IO_URING`
when the kernel offers it; one channel per shard (`FUSE_DEV_IOC_CLONE`, or io_uring per-core
queues). Cache posture: only negotiated, tested features may be advertised. Infinite cache
lifetimes require proven invalidation delivery and recovery for every mutation source; a
notifier encoder alone cannot justify them. Unsupported semantics are explicitly refused.
Live source names/attributes/content cannot have an indefinite kernel cache lifetime:
watchers may miss outsider writes. Current-state operations revalidate through the base seam;
only pinned/immutable views can justify retention without a source check. A transport that
cannot implement the requested coherence refuses that guarantee. §4.15 supplies the source
validation rules; finite stale-data TTLs do not silently become exact live-source semantics. The
launcher (`slates exec`) implements the chosen-path form with `CLONE_NEWUSER|CLONE_NEWNS`,
recursive-private root, and a bind mount; on AppArmor-restricted hosts it reports the exact
setting needed. Base files: a read-only `open` of an untouched base file is answered from
the daemon's descriptor on the backing file; `read` copies from the page cache into arena pages
once and replies by reference, with splice where the kernel allows it; `mmap` of an untouched
base file is served through the same pages. FUSE passthrough is not used: it requires
`CAP_SYS_ADMIN`, and slates never depends on a privilege the user may lack. A drift report or a watcher hint on a base path invalidates the kernel's entry and attributes for it
before the report is published, so a tool never reads attributes newer than the daemon's view.

**macOS 26+ (FSKit module, primary).** The macOS artifact is an app
bundle (`Slates.app`) containing the daemon, the `slates` command, and an FSKit app extension;
all are signed with one team identifier and share an app group named in the macOS
`<team identifier>.<group>` form, which needs no registration and permits POSIX shared memory,
UNIX sockets and Mach IPC between the sandboxed extension and the daemon. The extension is a
small Swift `@main` conforming to `UnaryFileSystemExtension`; its `FSVolume` handler methods do
nothing but marshal each operation into the bridge queue of the owning shard over the shared
ring region and reply from the completion (the forwarding form); the Phase 4 spike also measures
the in-process form (the Rust core linked into the extension) and the design records which is
used. Each attached volume is its own FSKit volume identified by an `FSGenericURLResource` URL
(`slates://volume/<id>/attach/<id>`, scheme declared under `FSSupportedSchemes`), mounted at the
root (`<root>/<volume>`) or, for the chosen-path form, at the requested user-owned directory,
which is a kernel mount-table entry and no disk write. Enablement is a one-time toggle in
System Settings > General > Login Items & Extensions > File System Extensions. Cache posture:
attributes are versioned by FSKit's sequence numbers and returned with every handler result;
invalidation of names and data after writes that arrive through the SDK ring (not through the
mount) is the spike's central measurement; until it is proven, such writes also touch the
item's attributes through the mount path so FSKit's own versioning observes the change. The supported FSKit API revision is pinned by the Phase 4 capability/packaging decision;
no parallel compatibility shim is authorized by this design. On macOS 15.4–15.x the module can be backed by an `FSBlockDeviceResource` on a
RAM disk (`hdiutil attach -nomount ram://`) whose contents are ignored, if the spike shows the
form acceptable; otherwise those systems use the fallback.

> Status (2026-09-08): the FSKit handler exists and **compiles against the real framework**. `crates/bridge-fskit/swift/SlatesVolume.swift` is the `FSVolume` + `FSUnaryFileSystem` handler; it conforms to FSKit's real `FSVolume.Operations`, `ReadWriteOperations`, `OpenCloseOperations` and `FSUnaryFileSystemOperations`, translating each operation into a shim request over the `ShimChannel` seam (the `ShimWire.swift` codec, a pure library cross-checked byte-for-byte against the Rust golden vector). `swiftc -parse-as-library -emit-library ... -framework FSKit` builds a dylib exporting `SlatesVolume`/`SlatesItem`/`SlatesFileSystem` with `@objc` conformance thunks over the real FSKit signatures (built here on macOS 26 / SDK 26.4; the CI macOS step skips loud on a pre-15.4 runner). What remains is the Phase 4 spike as written above: the app-group ring behind the seam, the `Slates.app` packaging and entitlement, the `UnaryFileSystemExtension` `@main`, and the live mount that verifies the `FSItem` lifecycle and the semantics marked `SPIKE:` in the source (the root object id, the time unit, open refcounting). The 20th shim op, `OP_SETATTR`, is now wired end to end: `setAttributes` carries the fields FSKit marks valid (chmod/chown/truncate/utimes) to the bridge's `setattr` and returns the new attributes; the by-use test `serve_sets_attributes_through_the_bridge` drives it over a real `VolumeBridge`, and the Swift codec round-trips it in the cross-check. A 21st op, `OP_ROOT`, corrects a real bug: `activate` learns the volume's true root object (`compose(prefix, 1)`, per-volume prefixed and clone-inherited) from the daemon rather than assuming inode 1 — wrong for any prefixed volume (`serve_returns_the_real_root_object`). Two former spike guesses are now verified against the daemon's code and no longer marked SPIKE: the shim object generation is a stable 0 (D-4, inode numbers never reused) and the attribute times are Unix nanoseconds. The in-process transport form now exists as a verification harness: the `test-harness` feature builds `crates/bridge-fskit` as a cdylib exposing a C ABI over `serve`, and `InProcessTest.swift` links it to drive the real handler through `serve` over a real `VolumeBridge` on a real scratch volume — the whole handler↔codec↔bridge stack end to end in one process, no ring and no mount (create→lookup→write→getattr→remove). The daemon-side serve is built and proven by use: `VolumeBridge::attached` lends the bridge an external open-handle map (non-breaking — FUSE/NFS keep the owned `new`), and `bridge-fskit`'s `MountSession` holds that map per mounted volume, serving each request through a transient bridge over the shard's store and volume (the Rust shape of a GC'd mount handler). `a_mount_session_persists_open_handles_across_requests` drives create→open→unlink→read→release→read across separate requests and shows the content reclaimed only at the last release, which requires the map to persist. The serve handles overlay volumes too: `attached` takes an optional borrowed host (`HostRef::Borrowed`), so the session lends the shard's `OsHost` per request and base entries are served through it (`a_mount_session_serves_an_overlay_base_through_the_borrowed_host`). What remains is genuinely external: the app-group ring that delivers requests (needs the signed bundle) and the mount-session/attachment lifecycle (§4.13). The production transport form (which of the two reaches the daemon's volumes) and the live mount remain the spike's. Separately, the macOS NFS *fallback* path (D-O9) is now proven runnable in this sandbox with no Apple entitlement: `bridge-nfs`'s `serve_connection` (`src/server.rs`) serves portmap/MOUNT/NFSv3 over a TCP loopback onto a `VolumeBridge`; a client mounts `/` and reads a seeded file back byte-for-byte over a real socket in CI (`crates/bridge-nfs/tests/loopback.rs`), and the `nfs_loopback` example serves a live `mount_nfs` — a real kernel mount of a RAM-only volume with no signing, no kernel extension and no privilege beyond the mount (R10). The signing-free fallback gives a live mount today; the FSKit primary still awaits the entitlement. And the **production async server** now runs on slates's own runtime, closing "the production server multiplexes it on slates's runtime": `serve_connection_async` (`src/server.rs`) serves one connection over the runtime's async `TcpStream` — reads and writes await the shard's driver (`TcpStream::write_all` awaits write-readiness, so a stalled client yields the shard rather than blocking it), sharing the RPC engine (`dispatch`) and record codec with the blocking form, which stays the example-and-test driver of the same engine. It rests on the rt's new async TCP (§4.3): `Driver::register_writable`, the `EVFILT_WRITE`/`EPOLLOUT` sibling of `register_readable`, and `tcp::{TcpListener, TcpStream}`. Because a volume is `!Send` (it holds a `Box<dyn Clock>`), the serve loop reaches its shard the way the daemon spawns its own perpetual tasks (§4.3): a `Send` boot task through `spawn_on`, then `futures::spawn` runs the non-`Send` loop locally. Proven by use in CI with no privilege: `crates/bridge-nfs/tests/async_loopback.rs` mounts and reads a seeded file back byte-for-byte from the async server on the runtime, driven by the same hand-rolled ONC RPC client as the blocking test, and the `nfs_async` example serves a real `mount_nfs`. The server also serves **many volumes** now, not one (toward the single-root-mount model above): `MultiExport` (`src/multi.rs`) routes each request to the volume its file handle names — the handle already encodes `(volume, inode, gen)`, so the router reads the leading handle's volume id and hands the untouched request to that volume's `Export`; `NfsService` is the seam the serve loop works over (a single `Export` or a `MultiExport`). And the **single root mount** the design calls for now works: `MultiExport` serves a synthetic read-only root directory whose entries are the volumes — `MNT /` returns its handle, `READDIR`/`READDIRPLUS` list them, `LOOKUP` a name returns that volume's own root handle, mutations are `NFS3ERR_ROFS` — so one `mount_nfs localhost:/` lets a client `ls` the volumes and `cd` into any. The serving core is built to the daemon's storage model: a `VolumeSet` seam supplies the volumes, and since a shard holds many volumes sharing one store, a volume is served through a transient `VolumeBridge` built per request (the "marshal each operation into the bridge queue of the owning shard" shape). `tests/multi.rs` drives that shared-store path — two volumes in one store. **And the daemon serves it end to end:** `crates/server/src/nfs.rs` binds a loopback listener at boot (`Daemon::nfs_port`), serves it on the control shard, and `ShardVolumeSet` implements `VolumeSet` over the shard's `ShardState` (a request resolves its volume through `state::with_state` and a transient `VolumeBridge::attached` per request; connections are concurrent detached tasks). Proven with no privilege by `crates/server/tests/nfs_mount.rs`: a single-shard daemon starts, a client provisions a volume, and over the daemon's NFS port a client mounts it, creates a file, writes bytes, and reads them back — the bytes travel client → NFS → `ShardVolumeSet` → the shard's real volume and back. And it serves volumes on **any shard**: the cross-shard bridge queue (§4.3, D-7 "bridge queues pinned to the owner") is built — a request naming a volume this shard does not own runs the same `serve_call` on the owner shard (spawned there as the client path forwards a verb, `Control::Spawn`), which spawns a task back that hands the reply to the awaiting connection task through a per-shard thread-local pending map (no new runtime primitive, no lock). `tests/nfs_mount.rs` proves a two-shard daemon serves a volume on a shard other than the listener's, byte-for-byte over the bridge queue. `cd`-ing into a volume from the host root spans shards too (a root `LOOKUP` routes by the looked-up id-hex name), so `mount /` then `cd <id>` reaches any shard. And the host root's *listing* gathers every shard's volumes (a root READDIR scatters an entry-gather to each other shard), so `mount /` then `ls /` shows every volume on the host — the whole browse (`mount /`, `ls /<name>`, `cd <name>`, read/write) spans shards, and a volume appears under its friendly provisioned name (§4.6 "Chosen path"): the slot carries the name, the root listing shows it, and a root `LOOKUP`/`MNT` of a name routes across shards by `owner_of_name` (the partition the create routed to and the id encodes, so a name reaches its volume with no global index, D-14), the owning shard resolving it against its own slots. The anchor-held listener for restart survival is built: a supervising anchor binds the loopback listener and hands its descriptor to every daemon it spawns (inheritable across the spawn), and the daemon adopts it rather than binding a fresh ephemeral one, so the port is stable across a restart (Unix; a standalone daemon binds its own), proven by use in `crates/rt/tests/tcp.rs` and `crates/anchor/tests/anchor.rs`. **The client `slates mount VOLUME PATH` / `slates unmount PATH` commands are now built and proven live** on this macOS host (`crates/cli/src/mount.rs`, `crates/cli/src/verbs.rs`): `slates mount` reads the daemon's loopback port from `StatusReport.nfs_port` (a process-global `NFS_PORT` set when the anchor-held listener binds) and the volume's provisioned name, then runs `mount_nfs -o vers=3,tcp,port=P,mountport=P,noresvport,soft,intr,locallocks,nosuid,rdirplus,actimeo=1 localhost:/<name> <path>` at an existing user-owned directory — a real kernel mount with no privilege (`noresvport` uses a high source port, R10), no kernel extension and no Apple entitlement. Proven end to end in this sandbox by a repeatable by-use test (`crates/cli/tests/cli.rs::slates_mount_establishes_a_real_kernel_mount_and_unmount_removes_it`, gated `SLATES_TEST_CLI=1`, skipping loudly where `mount_nfs` is absent) that drives the real binary — provision → `slates mount` → a file written through the mount read back byte-for-byte → `slates unmount` (`umount`) removed it (the mount table clean again) — behind a guard that unmounts and removes the temp point even on a failed assertion; and a runnable example (`cargo run -p slates-cli --example slates_mount`) drives an in-process daemon to the same live kernel mount with the command's exact options (an example cannot import the binary crate's mount code, so it mirrors it and names the source). The command adapts sylk's cgofuse mount lifecycle (`core/purevfs`): a pure, predicate-injected capability detection (`classify` over an injected "is this command on the `PATH`" probe — the shape of sylk's `classifyDarwinFUSEBackend`, so it unit-tests on any host with no live mount), a probe that refuses naming what is missing (no `mount_nfs`, or the daemon not serving NFS) rather than a raw `mount_nfs` error, and the mount/unmount lifecycle. The attribute-cache timeout is resolved (was owed "from the measured loopback RTT"): the loopback GETATTR RTT is sub-millisecond, roughly 10,000× finer than `mount_nfs`'s whole-second `actimeo` knob (`man mount_nfs`: `actimeo=⟨seconds⟩`), so an RTT-derived value floors to the knob's minimum — and both competing goals land there, since `actimeo=0` (`noac`) would send every `getattr` to the server and defeat `rdirplus`'s attribute batching while the macOS default (5–60 s, scaled by file age) is far too stale for an overlay that changes under merges and outside edits; the mount requests the finest nonzero cache, `actimeo=1` (`crates/cli/src/mount.rs::ATTR_CACHE_SECONDS`). Owed (minor refinement): the root-listing gather fans out to the shards in parallel. Each request runs as the mounting user (the daemon reads the uid from the `AUTH_SYS` credential, §4.13; `AUTH_NONE` falls back to root), the subject riding to the owner shard on a cross-shard call.

**macOS fallback (own NFSv3 loopback server; macOS 14.4 and any system where FSKit is
unavailable or disabled).** One TCP loopback listener held by the anchor; ONC RPC record
marking; NFSv3 + MOUNT + a minimal portmap responder; the root mount at an existing user-owned mount point
(or an already established RAM-backed target)
with `nfsv3,tcp,port,mountport,soft,intr,locallocks,nosuid,rdirplus` and attribute-cache
timeouts derived from the measured loopback GETATTR RTT; file handles encode
`(volume, inode no, gen)`; invalidation by directory attribute change after snapshot swaps
(EdenFS's no-op chmod) and by short timeouts; inode GC because NFS never says "forget"; the
chosen-path form is a second mount of the same export at a user-owned directory. The fallback
is also the differential oracle for the FSKit path: the same volume is mounted both ways in
tests and the abstract states must agree.

**Windows (own thin WinFsp binding).** One WinFsp disk volume on a drive letter (object-namespace
junction), `FileInfoTimeout = -1` with `FspFileSystemNotify` invalidations; requests handed to
shards by handle and completed asynchronously; the chosen-path form is a second drive letter;
directory mounts refused (NTFS reparse point); ProjFS never.

**Base reads on macOS and Windows.** As on Linux, the daemon reads untouched base files through its own descriptor or
handle into evictable cache chunks (one copy, `pread`/`ReadFile` on the owner shard's driver) and
replies from arena pages as for any chunk. Listings use `getattrlistbulk` and
`NtQueryDirectoryFile` respectively, so a `readdirplus` of a merged directory costs one bulk call
per directory, not one `stat` per entry.

**virtio-fs and OCI attachment contract (A-9).** The guest transport is FUSE-over-virtio
served by an owned device integrated with the custom executor. Hecate's in-process libkrun
integration is the reference, not a requirement to introduce a standalone daemon with a disk
socket. The seam accepts guest-memory/queue capabilities and completion notification from the
host VMM through an in-process interface or an inherited descriptor. The harness owns VM and
container creation; Slates owns the exported attachment and filesystem service. Native host
mounts remain available without a VM. Evidence: [research/hecate-contract-review.md](research/hecate-contract-review.md) §2.

A host OCI runtime passes the established host attachment into the container mount namespace;
a Linux guest mounts the exported virtio-fs tag, and OCI containers inside it consume that guest
path. Capabilities differ by host, kernel, runtime and VMM and must be reported by `attach`
and `status`: supported transport, target-path constraints, read/write policy, sharing/cache
semantics, residency boundary and conformance evidence. Requesting an unsupported form returns
`AttachmentUnsupported{transport, reason}`. A metadata record is insufficient evidence of a
usable container path or guest device. No disk socket, image construction, target mkdir or
privilege escalation is implicit in attaching a VFS volume.

Device admission authenticates the consumer before creating a queue, mapping guest memory or
publishing a tag. Queue descriptors, scatter/gather ranges, arithmetic and chained lengths are
validated within derived caps before access. In-flight requests, mapped bytes, copy buffers and
replies consume the attachment's credits; cancellation and revocation reclaim them under an
owned terminal step. Separate virtqueues alone do not provide §4.9's end-to-end QoS.

DAX, if exposed, may map only verified immutable content while that version is pinned. Every
page visible to a guest must contain only bytes it is authorized to see, including page padding
and neighboring chunks; mappings must be revoked before reuse or `advance` completes. Mutable
content goes through the VFS write boundary so witness, journal, quota and lease checks run.
The baseline contract does not require DAX; a requested DAX capability cannot be advertised
until mapping isolation, pinning and teardown have been established for that VMM.

**Writeback and snapshot barrier.** Kernel/guest cache negotiation changes when writes reach
the owner. `snapshot`, `submit`, `advance`, clean `detach`, migration and archive must identify
the set of contributing writable attachments, stop admission into the closing generation,
request the supported client flush, drain accepted requests, and publish the root only after
all included writes are recorded with their bytes. New writes belong to the next generation;
no write may straddle both. A failed participant gives a typed incomplete barrier, not a clean
snapshot. Application buffers not submitted by the process remain outside this guarantee;
client `fsync` must reach the bridge and its declared durability boundary. Auto-seal of only
server-visible writes reports that narrower boundary and cannot claim to include dirty guest
pages. Mount setup and barrier latency are outside the O(1) root publication measurement.

**POSIX and transparency acceptance.** The shared operation layer must preserve hardlinks,
unlink-while-open, rename replacement/exchange/no-replace flags, symlinks, truncation/sparse
files, permissions and ownership, timestamps, error codes, descriptor lifetime, `fsync`,
shared mappings and platform locking semantics. Extended attributes, watcher events and
platform-specific flags have explicit capability contracts. Never acknowledge an ignored
`setattr` field or discard a `renameat2` flag. FUSE ABI vectors must be checked against the
kernel headers independently of the encoder; for example WRITEBACK_CACHE is bit 16, while bit
8 is FILE_OPS (audit BUG-6). Advertise READDIRPLUS only with its complete handler.

`statfs` reports logical capacity and remaining space that the physical claim can honor,
using checked block rounding; it must not derive free space from twice current usage. Base
lookups and metadata mutations go through the same overlay rules as reads and writes.
Conformance runs through actual host mounts and Linux guests, including namespace-mutating
base operations, concurrent SDK/kernel writes and invalidation loss. Source-level tests alone
cannot establish the mount guarantee. A native Windows or NFS adapter must report its actual
semantics; a Linux guest provides a separate POSIX target, not evidence for those native paths.

A mount is transparent to ordinary path-based tools within the selected namespace, but remains
a filesystem boundary: `st_dev`, mount tables, cross-boundary links and EXDEV may expose it.
Attaching over an existing directory hides it in that namespace; the retained base descriptor
still serves its original contents. Other namespaces need their own attachment. Missing target
directories are refused because creating one would write disk. Full equivalence to an existing
physical volume's device identity or cross-volume rename is not promised.

**Failure matrix.** Daemon crash: Linux `ENOTCONN` until the anchor restarts the daemon and hands
back the fd (Degraded, seconds); macOS FSKit: the extension's forwarding calls fail typed until
the daemon is back, and the extension survives because it is a separate process (Degraded; the
spike measures whether open files recover or must be reopened); macOS NFS fallback: hard-mount
hang or soft-mount `EIO` until the restarted server answers the retried RPCs (Degraded); Windows:
the volume disappears and reappears (Degraded, open handles error). Extension crash on macOS:
FSKit unmounts the volume; the daemon's state is intact; the anchor re-mounts (Degraded).
Unprivileged user namespaces disabled: Refused for the launcher form with the exact remedy.
`allow_other` requested but not permitted: Refused with the `/etc/fuse.conf` line. FSKit module
not enabled: Refused with the exact System Settings path. Any separately selected limited
NFS form reports its weaker semantics and cannot satisfy a full-POSIX request. Base
file unreadable (permissions changed, base unmounted): Refused (`EIO` at the mount, `BaseUnavailable`
in `status`) for that entry only.

**Derived constants.** NFS fallback `actimeo` for writable volumes = k × measured GETATTR RTT
(k from the multi-process coherence test); FUSE `max_background` and congestion threshold = shard
count × per-shard in-flight budget (from measured service time); readahead = large chunk size;
FSKit forwarding batch = measured ring round trip versus per-operation handler cost (the spike's
number); attribute-return policy = always return attributes with results (the Handler API's
purpose) so the kernel needs no separate getattr.

**Worked example.** `git status` in an attached clone on Linux: after the first walk, every
`lookup`/`getattr` is answered by the kernel cache; the daemon sees `readdirplus` once per
directory and `read` for changed files; a write from the agent invalidates the file's attributes
in the kernel before the write is acknowledged, so a concurrent `git status` in another process
sees the new size. On macOS 26, the same clone is an FSKit volume: the agent's `attach` with a
chosen path mounts `slates://volume/7/attach/3` at `~/proj/build`; `cargo build` in a shell sees
a normal directory; `umount ~/proj/build` or `detach` removes it. Failure: the agent asks to
attach at `/home/u/proj/build` on Linux, which does not exist; the launcher form refuses with
`ChosenPathUnavailable{missing: "/home/u/proj/build"}` and the SDK falls back to the
root-relative path as an explicit alternative; it does not report that the requested path was attached.

**Laptop degenerate.** Identical; one root mount.

### 4.7 IPC and the provisioning fast path (D-10)

> **Status (2026-09-05).** Implemented in `crates/ipc` and `crates/server` (GAPS §8d, Phase 2
> tasks 3–6). As built, the failure matrix's client side: a client tells a dead daemon from a
> slow one through `Liveness` (Linux: the control socket's peer end, closed by the kernel with
> the daemon; macOS and Windows: a start stamp in the bootstrap object's header, rewritten by
> a restarted daemon and unreachable after a dead one), asked only after a reply has stalled
> past the deadline; the rendezvous carries the id a reconnecting client wants (a 4-byte hello
> on Linux; the claim slot's id field elsewhere) and the daemon honours it when no live client
> holds it; a client's shard is its id's residue over the partitions, so a reconnect lands on
> the partition holding its completion records. The daemon side: every shard sweeps at the
> liveness cadence, asks about clients silent for the budget (the control socket's end of
> stream on Linux; the process id probed on macOS and Windows), and reclaims a gone client's
> attachments, region, control channel and id, leaving its leases to expire by their terms
> (T-2.3 in `crates/client/tests/reap.rs`). The heartbeat slot stays the SDKs' way to be seen
> alive without a request (Phase 5); a sync client is seen through its process.
>
> Provisioning is measured end to end from the Rust client through the real rendezvous and
> rings (`crates/client/examples/provision_bench.rs`, AC-2.1, T-2.6): one client spinning
> provisions a volume at a p99 of about 25 µs against the 50 µs floor on the reference laptop;
> the 1/8/64 histogram and a parked form are recorded and ratcheted (the floor is held on the
> single-client latency; the concurrency tails are contention on shared owner shards). A verb's
> effects and its completion record are one log record (`Db::begin` … `commit`), so a crash
> leaves both or neither (AC-2.3). Admission is bounded (AC-2.6): a connect past the derived
> per-daemon client bound is refused `TooManyClients`, a forward to a saturated owner shard
> waits in a bounded queue and past the clients' credit is refused `Overloaded`, and a shard
> kicks another only when it is parked (a message to a spinning shard costs no syscall).

**Data model.**
```rust
#[repr(C, align(64))] struct Slot { seq: AtomicU64, kind: u16, len: u16, payload: [u8; 44], pad: [u8; 4] }
struct Ring { slots: [Slot; N], head: CachePadded<AtomicU64>, tail: CachePadded<AtomicU64> }
struct ClientRegion { magic, version, client_id, shard: u16, cmd: Ring, cpl: Ring, wake: WakeWord, parked: AtomicU32, spin_ns: u32 }
```
Ownership: the daemon creates the region (memfd / shm_open object / pagefile section), locks it,
and hands it to one client; the client writes only `cmd` slots and the `parked` flag; the daemon
writes only `cpl` slots and the wake word; large payloads (archive streams, big reads through the
SDK) travel through a per-client bulk region referenced by offset from a slot.

**Rendezvous.** Linux: connect to the abstract-namespace socket `@slates/<uid>/<instance>`, the
daemon checks `SO_PEERCRED`, sends the region fd with `SCM_RIGHTS`, and both sides close the
socket (kept only as the control channel). macOS: `shm_open` on a name prefixed with the app group (`<team id>.slates/` plus a uid hash,
kept within the 31-character limit), `mode 0600`, so the sandboxed FSKit extension can open the
same object; the client claims a slot by CAS in the bootstrap block; an optional control socket
for streams. Windows: open `Local\slates-<sid-hash>` section and the per-slot `Local\...` Event
(DACL to the user's SID); optional named pipe for streams. Discovery: `SLATES_ENDPOINT`, then the
well-known name.

**Protocol.** Requests and replies are fixed-layout 64-byte slots; request ids are `(client_id,
sequence)`; the daemon keeps a completion record per client until acknowledged (RIFL), so a retry
after a lost reply returns the original result; deadlines are remaining budgets; cancellation is
a slot kind.

**Wake strategy.** Client: write slot, publish `tail` (release), read `cpl` head with acquire in a
loop for `spin_ns` (the daemon-published measured wake cost), then set `parked = 1`, re-check the
reply (to close the race), and wait on the wake word (futex / `os_sync_wait_on_address(SHARED)` /
the Event). Daemon: after writing a reply, read `parked`; if set, wake (futex wake / wake by
address / `SetEvent`) and, for SDK event loops, signal the completion fd (eventfd / pipe / socket)
so `add_reader`/`uv_poll` fires. Shards poll rings while any client has activity within the
measured idle window, then park in the driver; a parked shard is woken by the driver kick the
client sends through the control fd.

**Failure matrix.** Client crash: the daemon detects the dead peer (credential socket closes /
slot heartbeat lapse) and reclaims the region and the client's leases after expiry (Masked for
others, Degraded for that client). Daemon crash: regions survive in the anchor; clients observe a
stalled reply and the control channel reset, reconnect, and resend with the same request ids
(exactly-once by completion records). Ring full: the client blocks on the ring (credit), never
drops.

**Derived constants.** Ring depth = Little's law on measured per-client request rate × p99
service time, rounded to a power of two; `spin_ns` = wake_ns.p99; idle window = wake_ns.p99 ×
measured spin-to-park ratio target.

**Worked example.** From Python: `await client.create("scratch", bounded=4 GiB)`: the extension
writes the slot, spins for `spin_ns`, and returns the reply without ever touching the event loop
(fast path); if the daemon were busy, the extension parks, the reply lands, the daemon writes the
eventfd, `add_reader` fires, and the future resolves. Failure: the daemon is not running; the
rendezvous fails with `DaemonUnavailable{endpoint}` and the SDK does not create any file anywhere.

**Laptop degenerate.** Identical.

### 4.8 Metadata database, registers and configuration (D-14, D-18)

> **Status (A-9, 2026-09-05).** Local records, replay/completion transactions and an
> f-parameterized register core exist. `ledger`, `mirror` and `reconfig` are pure simulations
> using direct calls. They are not an implementation-side proof of the historical TLA models
> or a working fleet. BUG-12 exposed a committed-prefix counterexample at `a1059ed`.
> Separate commit `d9cb6e5` fixes acceptance-epoch refresh and removes BUG-13's candidate-zero
> reachability restriction; its commit reports a before/after regression and 40 passing DB
> tests, not rerun here. Direct adoption-value checks and message-level histories remain owed. Mirror simulation does not establish transport, byte placement or time lag.
> Server wiring, regional configuration consensus and the acceptance gates below remain open.
> A-9 changes the required §4.8 contract; model refinement/revalidation remains explicitly owed
> before closure. No checker, tool installation or new CI job is authorized by this amendment.

**Role.** The authoritative record of volumes, snapshots, lineage, leases, attachments,
accounting, completion records, grants, chains and the operation log; served locally in
microseconds; made durable as follows: sealed content and records
replicate to candidate holders in the owner's neighbourhood under one quorum rule with the
owner's host epoch as the fence; configuration goes through a regional consensus group and is
never consulted per write; live working state is owner-local with auto-seal.

**Data model (per shard partition).**
```rust
struct Partition { volumes_by_id: Slab<VolumeRecord>, volumes_by_name: Art<Name, VolumeId>,
                   snapshots: Slab<SnapshotRecord>, lineage: Art<(VolumeId, SnapshotId), Edge>,
                   leases: Art<PrincipalId, LeaseRecord>, lease_expiry: TimingWheel<LeaseId>,
                   attachments: Slab<AttachmentRecord>, completions: Art<(ClientId, Seq), CompletionRecord>,
                   log: OpLog /* contiguous chunked ring in the anchor segment; every local mutation */,
                   seal_schedule: TimingWheel<VolumeId>, put_wal: PutWal /* records and content awaiting f+1 acknowledgements */,
                   grants: Slab<GrantRecord>, landing_leases: Art<CanonicalTarget, LandingLease>,
                   landings: Slab<LandingRecord>, audit: AuditLog /* append-only; in the anchor segment */,
                   chains: Art<(VolumeId, Version), VersionRecord> /* green volumes, D-27 */, deltas: Slab<CanonicalDelta>,
                   last_changed: Art<(VolumeId, PathKey), Version>, increments_seen: Art<IncrementId, (Version, Verdict)>,
                   registers: Art<ObjectId, RegisterState> /* objects this host owns: seq, last record, candidates, acked set */,
                   held: Art<(HostId, ObjectId), HeldRecord> /* records this host holds for other owners: highest epoch seen, record */ }
struct Record { object: ObjectId, host_epoch: u64, seq: u64, body: RecordBody /* Head{snapshot, holders} | Version{manifest, increment, holders} | Lease{..} | Catalog{..} */ }
struct SnapshotRecord { id, epoch, root, deadlist, refs, identity: Option<Blake3>, placed: PlacementState /* Local | Placed{region: SmallVec<HostId>, mirror: Option<SmallVec<HostId>>} */ }
struct Configuration /* consensus-replicated, one per region; a root group holds region membership and moved homes */ {
                   version: u64, members: Vec<Member>, neighbourhoods: Art<HostId, Neighbourhood>,
                   host_epochs: Art<HostId, u64>, takeovers: Art<HostId, Takeover>, homes: Art<VolumeId, Region> /* moved volumes only */ }
struct Neighbourhood { hosts: SmallVec<HostId> /* the scatter width S, across failure domains */, generation: u64 }
```
Ownership facts: a partition has one writer, its shard; a register has one legal writer, the
owner host, under its current host epoch; a holder accepts a record or a content put only when
its epoch is at least the highest it has seen for that host; the configuration is written only
through the regional group, read by every node from its local copy, and versioned, so a request
that carries a stale version is refused with the current one. Redirect retries consume a
bounded deadline/attempt budget; concurrent reconfiguration can require another refresh.

**Required persistence and protocol invariants (A-9).** Local record recovery must recover
all reachable bytes, roots, bases, witnesses, rights, reservations and completion records from
anchor-owned RAM. Persist effect and completion as one recoverable publication; an idempotent
retry must recover the same result and contents. Rebuilding a scratch volume from only a quota
and id loses acknowledged data. Live directory handles require an anchor handoff or validated
reacquisition of the same source identity; reopening a path alone cannot substitute another
base. Missing resources return `RecoveryIncomplete`/`BaseUnavailable`, never empty success.

For each ledger position, distinguish a holder's promised epoch from its accepted
`(epoch, value)`. Phase one consults an authorized quorum of distinct holders and adopts the
highest accepted proposal consistent with the committed prefix. Phase-two acceptance at a new
epoch records that epoch even if the bytes equal the holder's older value. Matching bytes do
not justify keeping the old ballot: a later quorum could otherwise prefer a conflicting
proposal from between those epochs (BUG-12). A replicated prefix must be checked for extension,
not only length, before any mutation. No historical committed value may ever be rewritten.

Epoch allocation derives from configuration authority, not a simulation's access to unreachable
holders' internal state. Checked counters refuse exhaustion. Quorum counts exclude duplicate,
stale, foreign-generation and unauthorized acknowledgements. Configuration changes fence old
writers and complete required state transfer before retiring holders; every head references
verified content and reservations under a compatible generation. The holder set, record and
its placed status publish atomically. Network receipt is not acceptance, placement or commit.

Owner-local linearizable reads require an established lease safety argument covering renewal,
clock bounds, scheduling pauses, expiry and takeover. Failure suspicion alone gives no read
or write authority. An old owner whose local proof is uncertain returns `LeaseUnconfirmed`;
the already-designed quorum read must itself validate the epoch and adopted prefix. Neither
a local timer nor an unqualified majority GET is by itself proof of a latest committed head.
Configuration ReadIndex remains on the control path; this amendment adds no per-write
configuration call or lock service. N=1 uses the same state transitions and failure semantics.

Required evidence is an implementation-facing message driver with independently delayed,
duplicated, dropped and reordered messages; asymmetric partitions; pauses and restarts;
uncommitted tails; changing reachable majorities; membership changes and stale owners.
The serial oracle remembers historical committed values, not merely current prefix lengths.
Every candidate may be unreachable; the test cannot always retain candidate zero. Agreement,
TotalOrder, Continuity, StaleNeverCommits, ReadSafety and NoLoss must hold with real byte
references and the audit counterexample. Hecate's consensus bug cases are adapted individually,
with their applicability recorded; an aggregate simulation count does not close those cases.

**Transactions.** Every operation is one-shot and names its volume; it executes on the owner in
one step with no awaits inside; cross-partition operations (clone into another owner's quota,
cross-shard chunk reference counts) are sequenced by appending a coordinating entry to the
node's control log and executing at each partition in log order (Calvin-style, with no
reconnaissance because the touched partitions are named up front).

**The three mechanisms.**
1. *Sealed content and records, under one quorum rule.* Every replicated object has 2f+1
   candidate holders: the owner and 2f hosts chosen by rendezvous over the owner's neighbourhood
   with the object id. A record is sent to all candidates and commits at f+1 acknowledgements.
   Content is sent to f+1 candidates first, hedged to the remaining candidates after the
   measured p95 put latency with tied requests that cancel the loser, and commits at f+1
   acknowledgements from any of them; a late copy is redundant and reclaimed. The acknowledging
   set is written into the object's head record, so a reader learns where the copies are from
   the record it reads anyway. `placed` is that commit. Every message carries the owner's host
   epoch. Reads: immutable content and immutable chain versions from any recorded holder,
   verified by identity; the head from the owner under its lease, or from f+1 candidates (which
   intersect every commit) when the owner is unreachable. This is Vertical Paxos II with the
   owner as leader-acceptor and read and write quorums of f+1 of 2f+1; the register's phase one
   is the promotion below; its phase two is every write.
2. *Configuration, by consensus.* One group per region (the hecate Raft dialect: pure core,
   PreVote and CheckQuorum, joint consensus, ReadIndex; the bug record as an executable
   conformance suite) holds membership (fed by SWIM), each host's neighbourhood, each host's
   epoch, takeover assignments, and the homes of moved volumes; a root group across regions holds
   region membership and cross-region promotions. It is written on membership change, takeover,
   neighbourhood change and home moves, never per write; its commit rate is near zero outside
   failures and is a tripwire.
3. *Live state, owner-local.* Open extents, unsealed writes, and current-epoch nodes exist only
   on the owner (and in its anchor segment). The `seal_schedule` seals each volume at its derived
   cadence; a volume with policy `live-shipped` additionally ships its op log to f+1 of its
   candidates and is acknowledged at f+1.

**Promotion and takeover.** When SWIM declares a host dead, or an operator moves it, the regional
group bumps the host's epoch and assigns each of its objects to the surviving candidate holder
that rendezvous ranks first. Each new owner runs phase one in one batched round per register
class across the neighbourhood: every holder raises its fence for that host to the new epoch
and reports the highest record it holds for each object; the new owner adopts the newest
reported record per object (which is at least as new as anything that ever committed under the
old epoch, because f+1 acknowledgements and f+1 replies intersect), completes safe adoption
under the new epoch as specified above, and only then serves under confirmed authority.
No eager whole-tree transfer is required for a metadata handoff; missing referenced content
must be fetched from verified holders before serving the affected reads. Background re-replication restores 2f+1 candidates and f+1 copies. A resumed
stale owner is refused by holders that have installed the new fence and cannot obtain a legal
commit quorum. It drops its role on `StaleEpoch`. The historical `FencedRegister` model checks
its modeled TotalOrder, Continuity, StaleNeverCommits and ReadSafety transitions; A-9 requires
refinement and revalidation before those results can be applied to the corrected implementation.

**Neighbourhood changes.** A host's neighbourhood changes only through the group (a member
left, a fresh member joined, a rebalancing). While a change is in flight the owner writes to a
quorum of the old candidates and a quorum of the new ones (joint writes); the group retires the
old set only after the owner has acknowledged the new configuration and the newest committed
record is held by f+1 of the new candidates; a restarted host rejoins as a new member and holds
nothing until its generation and retained state are validated. The historical `Reconfig`
model checks modeled ReadSafety and NoLoss; byte/capacity publication and delayed-message
integration still require the A-9 tests.

**Leases and reads.** Epoch fencing alone does not authorize linearizable owner-local reads.
Use the explicit lease safety obligations above: only an owner with a currently confirmed,
conservatively bounded lease may serve the latest head locally. Expiry/uncertainty stops those
reads before a takeover can make them stale. Membership heartbeat arrival is not a lease grant;
a majority observation must belong to the relevant authority generation. Immutable complete
snapshot reads need no latest-head lease but still require read rights and verified content.

**Authority scope.** Host failure increments the host epoch and fences every object owned by
that host. Moving one volume changes that object's ownership generation, recorded in the
configuration's moved-object exceptions, without fencing unrelated volumes. Requests and
placement records bind both scopes; cached configuration/attachment generations suffice on
ordinary writes. A region-home move also carries root-group authority. The id still routes by
creator plus these exceptions; this adds no global lookup catalog or per-write coordination.

**Lookup.** A volume id carries its creator host. A lookup by id routes to that host, or, when
the configuration records a takeover of that host, to the candidate that rendezvous ranks first
among the survivors of its neighbourhood at the takeover generation; an ownership or home move is a configuration exception naming the affected volume and its
authority generation. The answer comes from the current owner and is
authoritative; no global index exists. Names live in an enrolled namespace whose owner is
routable by id; the CLI resolves `namespace/name` through that owner, defaulting to its current
context. A name is not a host pathname or a bearer capability. Fleet-wide enumeration is a scatter-gather over
owners, each answer linearizable at its owner and the whole labelled with the configuration
version.

**Membership.** SWIM with Lifeguard: direct probe → k indirect proxies → SUSPECT → DEAD;
suspicion timeout `max − (max−min)·log(C+1)/log(K+1)` with the originator excluded; peer
confirmation before suspicion; gossip with λ·ln(n+1) rebroadcasts under the measured per-path
MTU; a bounded local-health multiplier; randomized probe order; and, from Vivaldi network
coordinates each node learns from its own measured round-trip times and exchanges on the
acknowledgement, a per-peer RTT prediction that selects the indirect-probe relays nearest the
target, so a slow far peer is not mistaken for a failed near one. Every parameter derived from
measured RTT, loss, and convergence (`research/survey-hyperscale.md` §8.4 gives the formula per
parameter; the local-health multiplier is a small integer cap, 3×–4×, not the raw `(LHM+1)`,
which over-dilates timers under sustained probe failure).

**Deployment.** A fleet is described once, in one manifest every node starts from with its own
name: the fleet's TLS name, `f`, and each node's advertised address and operator-provisioned
certificate (§4.13 enrollment distributes these; until it does, DER files beside the manifest). A
node's member id is the leading eight bytes of the BLAKE3 hash of its certificate — the one fact
about a node every peer already holds, since it pins it — so ids agree fleet-wide without a
registry (D-14). A node's advertised port and the next are its two serve sockets — probes on the
first, records on the second — each shared by every peer through the session plane's connection-id
demultiplexer (§4.10a §8; a peer's re-dial after a lost session replaces the old session), and every
peer dials it there, so both ends of every session are computed from the same file and can never
disagree. A manifest with fewer than `f + 1` nodes, a repeated name or certificate, a base at the end
of the port range, or an identity the TLS stack cannot use, is refused by name at boot. `status` reports the node's member id, `f`, host epoch,
the members it holds alive and the peers it has probed. A laptop has no manifest: its member id is
its machine identity's hash and it is its own one member at `f = 0` — the same code path (R8).
(A-13, `crates/server/src/deploy.rs`; proven by three real daemon processes in
`crates/cli/tests/cli.rs`.)

**Slow versus stuck.** A long operation that waits on remote progress — a mirror catching up, a
put filling its quorum — is told apart from a stalled one by a progress witness (a monotone,
operation-defined measure and the time it last advanced): near its deadline, an operation still
advancing is granted a bounded extension rather than declared failed, and only a stalled one, or
one that has spent its extension budget, is left to the hard timeout. This is the per-operation
analogue of the local-health multiplier (slow ≠ dead). The statistical change-point witnesses for
noisy progress (`research/survey-hyperscale.md`, hyperscale's `health/progress_witness`) are owed.

**Placement.** The group assigns every host a neighbourhood of S hosts across distinct failure
domains from the declared tree, with S derived from the measured re-replication bandwidth
needed to restore a host's copies within the recovery budget and from the accepted loss
probability under coincident failures (Copysets); candidate holders per object are rendezvous
over the neighbourhood; a neighbourhood change moves only the objects whose candidates changed,
add before remove. The copyset count is computed at every configuration change and compared with
its bound; exceeding it is a placement bug, not a tripwire.

**Mirroring.** Every committed record and its content is shipped to the mirror region's
neighbourhood of the owner asynchronously, in epoch and sequence order, by the same put
machinery; the mirror acknowledges at f+1 of its candidates. `mirror_age` is measured on the
home clock from the oldest home-committed record still lacking mirror acknowledgement, zero
when caught up; status also reports the mirrored prefix and observation freshness. Lost clock
or acknowledgement knowledge is unknown, not zero lag. A record count is a separate metric; `await placed(mirror)` returns when the named
snapshot's record and content are acknowledged there; elapsed time includes any backlog,
content transfer and quorum acknowledgement, not a guaranteed single WAN round trip. Region loss promotes the mirror through the root group at operator cadence; the loss
window is the mirror lag at that moment, zero for every operation that awaited the mirror.

**Recovery.** Node restart: the anchor segment replays the local log and `put_wal` into fresh
indexes; the node rejoins with a new ephemeral id (a restart is a join) and holds nothing for
others until re-replication fills it; its owned objects are taken over by its neighbours after
the membership horizon. RAMCloud's recovery-time budget is a reference, not a Slates result;
metadata takeover uses a configuration decision and phase-one exchange. A read may additionally
need content fetch and a confirmed lease; measure that complete recovery boundary.

**Failure matrix.** A candidate holder slow: Masked (the hedge completes the put elsewhere;
the slow holder goes on probation after the derived count and the group replaces it). Fewer than
f+1 candidates reachable: Degraded (the object stays `Local`, `placed = false` is reported;
writes continue locally). Owner loss: Degraded for the membership horizon, then Masked after
takeover, with the loss window reported. Configuration group quorum lost: Degraded (no
takeovers, no neighbourhood changes, no home moves; every owner keeps writing to its candidates
under its confirmed authority only while that authority remains valid; seals place only with
eligible quorums and admitted content). Partition of a minority: Refused for writes by owners on
the minority side once they cannot confirm membership; snapshot reads continue. Correlated loss
of all f+1 copies of a snapshot: data loss of that snapshot, documented (D-18); the neighbourhood
bound makes it as rare as the operator chose. Stale configuration on a request: Refused
(`ConfigurationStale`) with the current version; bounded refresh/retry while the request
remains live.

**Refusals.** `LeaseUnconfirmed`, `NotOwner{owner}`, `NotPlaced{scope}`, `StaleEpoch{current}`,
`ConfigurationStale{version}`, `MembershipEpochStale`, `PlacementUnavailable`, `QuorumLost`,
`DuplicateRequest{original}` (informational: the original result is returned).

**Derived constants.** f from the failure-domain tree (0 on a laptop); candidates = 2f+1; commit
at f+1; scatter width S from measured re-replication bandwidth × recovery budget and the accepted
loss probability, defaulting to the candidate floor 2f+1 — one copyset, the tightest and lowest-loss
neighbourhood — until a deployment has sized its recovery (implemented: `ConfigGroup` bounds every
neighbourhood to this width, raised above the floor only with the fixed-copyset construction so the
copyset count stays linear); hedge delay = measured p95 put latency per class; probation threshold = late
count over the measured window that exceeds the hedge rate's variance; detection timeout for
membership from RTT p99 × k; auto-seal cadence as before; healer cadence from the measured
put-failure rate; membership lease from heartbeat RTT p99 × k; election timeout for the
configuration group ≥ 10 × broadcast RTT p99 with the randomization span from RTT variance;
SWIM period = max(k × RTT p99, scheduler quantum); gossip λ from measured convergence;
tombstone retention = measured partition-heal p99; mirror shipping batch from the measured WAN
bandwidth-delay product.

**Worked example.** Two zones, f=1, host A's neighbourhood {B, C, D}: an agent on A creates a
volume; the create returns after the local append; the head record (epoch 1) goes to the two
candidates rendezvous chose from {B, C, D} and commits at the first acknowledgement beyond A's
own; the agent writes for a while (owner-local); the seal schedule seals a snapshot, its chunks
go to A's two content candidates, one is slow, the hedge sends to the third candidate, the put
commits at two acknowledgements, and the head record names the two holders that answered. Node A
loses power; B's SWIM confirms A dead; the regional group bumps A's epoch and assigns the volume
to the candidate rendezvous ranks first among {B, C, D}; that host asks the neighbourhood for A's
highest records, adopts the head, and serves; the agent's SDK reconnects by routing the id to
the new owner, its retries return their original completion records, and the volume reports a
loss window equal to the edits after the last placed seal. Failure: A wakes up and sends a head
record with epoch 1; the first holder answers `StaleEpoch{2}`; A drops the role and nothing is
applied.

**Laptop degenerate.** One node: f=0, every candidate set is the owner, every commit is a local
append, the configuration group is one self-acknowledging voter that never receives a takeover,
the mirror does not exist and `await placed(mirror)` is refused `Unsupported`, and the loss
window is the process-restart window (zero, thanks to the anchor). Same code, zero modes.

### 4.9 Wire protocol (D-15)

**Framing.** A 32-byte header: magic (4), major (2), minor (2), flags (4), channel/class (2),
kind (2), length (4), checksum (4), request id (8); then the body. All little-endian, 8-byte
aligned; the reader checks length against the class's frame cap before allocating; the checksum
is verified before decode; bodies are canonical encodings generated by a derive with a schema hash
carried in the first body word; unknown kinds are refused; append-only evolution within a major.

**Classes.** Control (never shed), Metadata (leases, catalog), Bulk (chunks, archive streams),
Telemetry (shed first). Isolation spans admission credits, queue slots, CPU slices, arena
headroom, network frames and guest/device queues. Each unit of bulk work has a derived bounded
quantum so a large transfer cannot monopolize the executor. Reserved control capacity admits
bounded recovery work even under bulk saturation; "never shed" does not mean an unbounded
control queue. Repair and teardown carry an authenticated purpose, not a caller-selected high
priority. Measure metadata/control latency and memory under a saturated bulk producer; separate
wire labels or queues alone do not establish this guarantee.

**Configuration version.** Every fleet request carries the sender's configuration version and
every record carries the owner's host epoch; a receiver with a newer configuration refuses with
`ConfigurationStale{version}` and the current version, and a holder with a higher epoch for the
sender refuses with `StaleEpoch{current}`. Refreshes consume the operation's bounded retry
budget; the latter refusal ends the sender's authority.

**Exactly-once.** `(client id, sequence)` request ids; completion records kept until acknowledged
(the client acknowledges by advancing its sequence window); retries return the original result;
provisioning is therefore safe to retry after any failure.

**Flow control.** Credit-based, absolute offsets per stream; windows derived from the measured
bandwidth-delay product and the class's latency budget; the sender never exceeds credit; a
stalled receiver stalls only its own class.

**Transfer and cancellation.** Immutable named objects use identity, missing-set exchange,
verified ranges and resumable progress. Do not allocate a whole advertised object before its
class cap, claim and identity are checked. An unknown-length ingest uses an explicitly bounded
RAM session with per-consumer credits, checked offsets, cancellation and a terminal expiry.
A receiver distinguishes received, checksum-verified, identity-verified, placed and referenced;
only the last two imply retention under the required durability contract. Duplicate frames,
reconnects, corrupt chunks and canceled producers neither publish partial identities nor leak
session slots. Compression/decompression workspace is charged before input is accepted.
Completion records and retained response windows have bounds and acknowledgements; exhaustion
refuses admission rather than forgetting a live exactly-once obligation.

**Trace context.** The operation envelope carries a request identity for replay, optional
trace/span context for observation and optional caused-by event identity. These have different
lifetimes and cannot substitute for one another. Authentication establishes consumer and
volume tags separately (§4.13–§4.14); trace fields never authorize effects.

**Security.** Between hosts: TLS 1.3 via rustls with certificates provisioned by the operator
(bulk cost is symmetric crypto at memory speed, and no handshake sits on a hot path); on one host: peer credentials at rendezvous, no encryption.

**Refusals.** `BadMagic`, `UnsupportedMajor`, `FrameTooLarge{cap}`, `ChecksumMismatch`,
`UnknownKind`, `SchemaMismatch{expected, got}`, `CreditExceeded`.

**Derived constants.** Frame caps per class from measured MTU (per path) and class budgets;
credit windows from BDP; checksum choice: CRC32C on the control and metadata classes (hardware
instructions everywhere on the matrix), BLAKE3 identity on bulk chunks (already computed).

### 4.10 Distribution: replication under one rule, auto-seal, remote attach, ownership migration, mirroring, anti-entropy (D-14, D-16, D-17, D-18)

**Content replication.** Sealed chunks and manifests go to the owner's candidate holders under
the rule of §4.8: f+1 first, hedged to the rest of the 2f+1 after the measured p95, committed at
f+1 acknowledgements from any, the acknowledging set recorded in the head record; a chunk
already present on a candidate is never transferred again (the receiver reports its missing
set); anti-entropy walks Merkle manifests between recorded holders and repairs only differing
subtrees; the healer replays puts that never reached f+1 from the owner's `put_wal`, and puts a
repeatedly late candidate on probation for the group to replace. Cold sealed content (a
measured class by read rate, Phase 8) may be held as k+m fragments across k+m candidates instead
of f+1 copies, with hedged fragment fetches on read; the class boundary, (k, m) and the
reconstruction budget are derived, never fixed (D-O6).

**Auto-seal.** Each volume seals at its derived cadence (or on explicit `snapshot`, on `detach`,
on `archive`, and when the owner is asked to drain); publishing a server root is O(1), while
attachment flush barriers, hashing and puts have separately measured costs. An auto-seal
without a client barrier covers only server-visible writes (§4.6); the volume reports `last_placed_snapshot_age` and `mirror_age`
so an agent that needs durability can `await placed(scope)`.

**Remote attach.** An agent on node B attaches a snapshot of a volume owned by A: B routes the id
to A (or A's successor), reads the head record from A under its lease, fetches the manifest by
identity from a recorded holder (metadata only), creates a local read-only attachment record,
and serves the namespace immediately; chunk reads fault to hedged fetches by identity from the
recorded holders, verified on arrival, cached in B's arena under B's budget; a clone of a remote
snapshot is a local volume whose base chunks are fetched lazily and whose own seals go to B's
candidates.

**Prefetch.** Per base snapshot, the daemon records the ordered set of paths read within the
first window after attach (window derived from the measured attach-to-first-build interval); the
next attach of the same base prefetches that set in the background, bounded by the attachment's
budget; the policy is observe-first until a minimum number of attaches has been seen.

**Live shipping (opt-in).** A volume created with policy `live-shipped` ships its op log to f+1
of its candidates and acknowledges writes at f+1; the backups apply entries in order and the
takeover successor is one of them, so promotion has no loss window; replica lag beyond a window
derived from the measured apply rate applies credit backpressure to that volume's writers only.

**Ownership follows the writer.** Creation places the owner on the creator's host because the
writer's ring to its local owner shard is the latency floor. When write-intent attachments from
another host persist for the derived number of operations (the PNUTS rule on the origin of the
last N writes, N from the measured cost of a migration against the measured cross-host write
cost), ownership migrates there by the planned handoff: the current owner seals, the delta ships
by identity to the new host (which is usually a candidate holder already), the regional group
advances that object's ownership generation and names the new owner, the new owner runs phase
one, and the old owner's later writes are refused `StaleEpoch`. Load never moves ownership;
holder duty is balanced by neighbourhood changes; operators keep an explicit move.

**Green volumes in a fleet (D-27).** A green volume's owner runs its merge task; an increment's
post-state is a sealed snapshot of the work volume, placed to the work volume's candidates by
its owner before the increment is sent (the put runs at `submit` if the auto-seal has not already
placed it); the merge record is the next entry of green's ledger register: sent to all 2f+1
candidates under green's host epoch, committed at f+1 acknowledgements, and only when every
identity the new version references is placed; holders of green recompute the verdict and the
manifest identity from the record's inputs before serving the version and compare head
identities per version; a mismatch refuses that version on that holder with an alarm. Owner
loss takes green over on the candidate that rendezvous ranks first, which already holds the
ledger; increments in flight retry at the new owner by identity. Submissions from another host
route the green's id to its owner; a refusal from a non-owner names the current owner and epoch
and refresh/retry consumes the operation's bounded budget; placement refresh is single-flight
per green.

**Overlay volumes in a fleet.** A live base names an enrolled directory identity and its
serving host. Every local or remote clone retains that `BaseRef` together with the delta and
witnesses. Unchanged names, metadata and bytes continue to resolve through it; a clone must not
become a scratch volume seeded only with changed entries. An equal pathname on another host
is not evidence of source equivalence.

The base service remains on its source host; writable delta ownership can migrate by the same
fenced handoff used for other volumes while retaining this dependency. Delta replication and
mirror placement do not replicate unfetched host content. If the base host is unreachable,
available delta/pinned bytes remain readable and untouched paths return `BaseUnavailable`;
status explicitly reports partial availability and `DeltaWithLiveBase` coverage. A whole-view
availability guarantee is possible only for a `Complete` snapshot whose entire reference graph
has been captured, verified and placed. Taking over a delta never silently changes its base.

An explicit base capture (§4.15) creates that complete immutable source. Thereafter remote
clones share its manifest/chunks and fault bytes lazily from eligible holders; cloning remains
O(1) in tree size after admission. This is the preferred delta-plus-base design, not eager
whole-tree copying per agent. Capture and first-read costs are separate measurements; this
amendment claims no measured speedup.

**Placement closure.** A holder acknowledges only after reserving actual capacity and verifying
all required bytes. A version's reference graph, holder generations and placement scope must
be complete before its record can commit. A list of hashes without corresponding retained
objects does not satisfy placed-before-referenced. For a green version, holders recompute the
verdict and resulting manifest before serving; missing inputs or mismatches refuse. Repair
preserves these obligations across pressure, cancellation and membership changes.

**Mirroring across regions.** Every committed record and its content is shipped in epoch and
sequence order to the owner's neighbourhood in the mirror region, acknowledged at f+1 there;
`mirror_age` is exposed per volume; `await placed(mirror)` is the per-operation choice to wait
for it; region loss promotes the mirror through the root group; a promoted volume's owner is
chosen in the mirror region by the same rendezvous, and its loss window is the mirror lag at the
moment of loss.

**Failure matrix.** A recorded holder unreachable during fetch: Masked (another recorded holder,
hedged). All recorded holders unreachable: Refused (`ContentUnavailable{identity}`) for that
read, the rest of the namespace continues. Neighbourhood change mid-attach: Masked (chunks are
by identity; the head record names holders). Fewer than f+1 candidates reachable: Degraded
(seals accumulate as `Local`; reported). Owner loss of an overlay volume: Degraded
(`BaseUnavailable` on unresolved base paths; placed delta bytes remain readable and the
delta owner can be taken over without claiming whole-view availability). Green owner
loss: Degraded for the membership horizon, then Masked after takeover on a holder of the
ledger; no version is lost because none commits before placement. Holder recomputation
mismatch: Refused for that version on that holder, alarm; served from other holders. Migration
in flight when the new host dies: Degraded (the old owner keeps its epoch until the group bumps
it; the migration restarts). Mirror region unreachable: Degraded (`mirror_age` grows and is
reported; `await placed(mirror)` refuses at its deadline with `NotPlaced{mirror}`).

**Laptop degenerate.** No peers; f=0; remote attach is a local attach; prefetch still learns hot
sets; auto-seal still runs (it bounds the archive's staleness and drives dedup); migration and
mirroring have no targets and their verbs refuse `Unsupported`.

### 4.11 Compression, deduplication, hashing, archive (D-17)

**Cost model.** Inputs from the profile (codec throughput per level, hash throughput, memcpy
bandwidth, free memory) and from the volume (LZ4-size to zstd-size regression, expected read
count per chunk: high for attached, ~1 per re-attach for archived); per chunk: zero-detect →
Btrfs-style sampled statistics → LZ4 probe with early exit → predicted savings per level →
choose the encoding maximizing `bytes_saved × value_of_byte(pressure) − (t_compress + E[reads] ×
t_decompress) × value_of_cpu(load)`, subject to the format floor (savings must exceed the chunk's
metadata overhead). Hot volumes stay raw unless pressure raises `value_of_byte`; archived volumes
compress once.

**Dictionaries.** Trained per discovered content class from the volume's own sampled chunks
(FastCover), accepted only when a held-out sample beats the incumbent by more than the amortized
training cost; identified by BLAKE3; stored as chunks; embedded in archives; never mutated.

**Archive.** The format of `research/compression-archive-dedup.md` §2.6: header, dictionaries,
chunk records in manifest order, uncompressed manifest, seek table, trailer; streamable in one
pass; verifiable per chunk and whole; resumable by missing set; the same container is the
replication transfer unit and the clone-from-archive source. A chunk record may be a fragment
record: `{chunk identity, k, m, index, fragment identity, bytes}`; a reader
reconstructs the chunk from any k fragments and verifies the chunk identity; a fragment's own
identity verifies it in transit; fragments are content like any other and dedup by identity.

**Failure matrix.** Corrupt chunk on restore: Refused for that chunk (`IdentityMismatch`), the
rest restores. Insufficient memory to hold a compressed copy: Refused before work starts.

**Derived constants.** All from the profile and per-volume observations; the only fixed rule is
the format floor.

### 4.12 Agent surfaces (D-19)

> **Status (A-9, 2026-09-05).** The Rust client and a CLI subset exist. `docs/cli.md`
> documents the actual grammar. `attach` records metadata but does not establish a mounted
> path; `exec` is Linux-specific and currently needs an externally supplied `SLATES_ROOT`.
> `--locked` does not establish residency (BUG-1). `slates grant` is absent despite the
> server-side grant records/control transport; same-uid human authority remains unestablished.
> MCP, Python/TypeScript SDKs, generated surface parity and user-facing merge/guest flows are
> planned. Descriptions below are required interfaces, not commands verified to work today.

**One operation contract.** A Rust descriptor per operation defines input/output types,
authorization, side effects, replay class, cancellation, limits, refusals and documentation.
It generates the wire body, SDK surface, MCP input/output schemas and CLI help/structured
output. Each adapter tests the same behavior against it. Separate hand-maintained operation
lists are not the authority. CLI human grant operations occupy a privileged surface in this
registry and are structurally excluded from SDK/MCP exports.

**Rust client.** The ring protocol, rendezvous per OS, completion fd, request ids, typed errors.

**Merging from the SDKs (D-27).** `submit(work_volume, evidence=None)` parks and resolves to
`Accepted{version}` or raises `MergeConflict` carrying the windows; `rebase(work_volume,
to=None)` moves the base to the head (or a named version) or raises with windows;
`advance(attachment, version)`; `versions()`, `changed_since()`; `edit(path, at, delete_len,
bytes)` declares a true insert or delete on a work volume. Whole-file writes through `write`
or through a mount are one declared operation covering the file and conflict with any
concurrent operation on it unless identical; the skills say so.

**Landing from the SDKs.** `materialize(snapshot, target, filter=None, wait=True)` returns
an awaitable landing request: awaited, it resolves to the landing report once a human has
granted the request and the landing has run, or raises `GrantRequired` at once when `wait` is
false; `status(volume)` carries the drift list; `read_base`, `rewitness` and `pin` are ordinary
async calls. No SDK method creates a grant.

**Python SDK.** PyO3 extension (`abi3-py312` wheels plus `cp314t` for free-threaded builds);
`async` methods return futures resolved by fd readiness (`loop.add_reader` on Unix, a socket on
Windows); zero-copy reads into `memoryview` through the buffer protocol; a sync facade; stubs and
`py.typed`; errors as a typed hierarchy mirroring the refusal taxonomy with the gRPC-style code and
errno.

**TypeScript SDK.** napi-rs addon with platform packages; Promises resolved by `uv_poll` on the
completion socket/fd; external buffers for zero-copy reads with a copy fallback; async iterators
for listings and streams; `AbortSignal` cancellation; ESM/CJS; the typed addon over the
client channel.

**MCP server.** `slates mcp`: stdio and loopback Streamable HTTP; the 2026-07-28 stateless
protocol with per-request `_meta`, `server/discover`, `resultType`, `ttlMs`/`cacheScope`,
`subscriptions/listen`; dual-era `initialize` handling for the deprecation window; tools:
`slates.volume` (create with a snapshot or a host directory as base, clone, snapshot, list,
stat, resize, destroy), `slates.attach` (attach, detach, list), `slates.archive` (archive,
restore, export), `slates.fs` (read, write, list, move, delete), `slates.base` (status with the
drift list, read_base, rewitness, pin), `slates.land` (materialize, which returns `GrantRequired`
with a request id until a human grants it, and landing reports), `slates.merge` (submit, rebase,
advance, versions, changed_since, windows), `slates.status`, `slates.help`; `slates.fs` also
offers `edit` (a declared insert or delete at a position, the write an agent should prefer on a
work volume);
no MCP tool creates a grant, and the server refuses the grant kind on the MCP and SDK channels; resources: `skill://slates/<name>/SKILL.md`
and `volume://<id>/...` listings; prompts: one per skill; annotations on every tool; opaque
volume ids echoed on every call; results carry `structuredContent` with stable codes; output
bounded under the client's token limits with cursors.

**Skills.** One source tree; each skill a directory with `SKILL.md` (spec fields), `references/`
and `scripts/`; published raw by `slates skills install` into `.agents/skills/` and
`.claude/skills/` (project) and the user equivalents; over MCP as resources and prompts; through
`slates.help`; packaged as a Claude Code plugin with the MCP server; kept under the spec's size
guidance.

**CLI.** `slates daemon`, `slates status [--drift <volume>]`, `slates volume ...` (including
`create --base <dir>`), `slates attach ...`, `slates exec` (Linux launcher), `slates archive ...`,
`slates base rewitness|pin|read <volume> [paths]`, `slates land <snapshot> --to <dir> [--filter ...]`
(plans and prints the landing manifest summary, then waits for a grant), `slates grant
<request-id> [--session]` and `slates grant --watch` (the terminal confirmation surface: prints
each pending request's manifest summary and conflicts, reads the human's answer, issues or
refuses), `slates grants list|revoke`, `slates audit [--export]`, `slates merge submit|rebase|advance|versions|windows`,
`slates mcp [install]`,
`slates skills install`, `slates profile` (prints the machine profile with derivations).

**CLI and MCP behavior contract (A-9).** Human commands accept canonical ids and unambiguous
names in the current enrolled namespace. Name lookup routes through that namespace's owner;
there is no global lookup index. Ambiguous names refuse with candidates instead of guessing.
Every verb supports `--help`; structured operations support consistent `--json`, stable error
codes and bounded pagination. Raw-byte streams use an explicit stream result. Errors name the
failed capability or resource, its observed limit, and an action that can actually resolve it.
A success includes the effective view coverage, claim, attachment form/path or guest tag,
consumer rights, placement and freshness where applicable.

The intended local flow is enroll a base, create a live overlay, clone work views, attach or
execute tools, inspect changes/drift, submit declared operations, preview a landing and obtain
a human grant. A reproducible flow explicitly captures a stable immutable base before cloning.
A guest flow binds an authorized attachment to the harness's VMM/device before reporting a
usable guest path. The same volume operations serve all flows; host capability changes the
available attachment form, not volume semantics. Endpoint/root discovery must be automatic
from the enrolled context; a required undocumented `SLATES_ROOT` is a gap. Provisioning,
mount/device setup and base capture timings are displayed separately.

MCP mutations return typed structured results and request identities; long operations expose
bounded progress and cancellation. Destructive effects and grant requirements are annotated
from the descriptor. Agent-provided tool text cannot enroll new roots, broaden a consumer's
rights or issue a human grant. Skills describe only shipped capabilities; future grammar is
clearly labelled planned. Disk installation of skill/config files cannot bypass R1/R10: it
requires an explicit user-authorized write through the granted landing path; in-memory help
and MCP resources need no installation.

### 4.13 Security and multi-user machines (D-22)

Rendezvous authenticates the host account using uid/SID or a host certificate. A daemon can
serve multiple enrolled consumers under that account, on a laptop or a fleet host with the
same authorization path; no separate shared-daemon mode is needed. Every lease and attachment
records the consumer established by that authenticated channel; volumes carry an owner
principal and an access list (read, write, admin); ids are random 128-bit values but never
authorize by themselves (possession of an id plus a valid principal and lease authorizes);
the launcher never escalates privileges; the MCP server's servable roots are enrolled by a human
through the CLI; grants for disk writes are issued by a human through the CLI or a confirmation
surface and never through the MCP or SDK channels; a landing resolves its target beneath its own
directory descriptor (`RESOLVE_BENEATH`, `O_NOFOLLOW` chains, reparse-tag checks) so no symlink or
junction planted in the target can redirect a write outside it, refuses targets the caller does
not own and targets inside a slates mount, never changes ownership of what it writes, and never
follows a symlink when removing; no code is loaded after start; every refusal is typed and counted.

**Security specification (A-8, 2026-09-05; the spec §3 of GAPS owed before Phase 2).**

*Principals.* Host credentials establish `AccountId`; a trusted enrollment establishes
`ConsumerId` and scoped rights for a workload under that account. A consumer channel is bound
at rendezvous using a capability delivered and retained outside other agents' reach, for
example an inherited endpoint from the trusted harness. Per-request identity strings and
peer uid alone cannot establish consumer identity. Between hosts, authenticated transport
also binds the delegated consumer scope. Rights are checked before resource admission,
namespace lookup that reveals protected content, queue creation or any VFS/device effect.
Replay/session resumption preserves that binding; it cannot change the authenticated consumer.

This requires a harness boundary when untrusted processes share an OS account. Slates cannot
prevent an unsandboxed same-uid process from reading other process memory or writing host files
through unrelated syscalls. It supplies scoped VFS access and grant checks; the harness owns
process isolation and capability delivery. The product must not call uid-only IPC agent
isolation. Unsupported secure enrollment refuses, instead of issuing an ambient admin channel.

*Access lists.* `Access { owner: Principal, entries: SmallVec<(Principal, Rights)> }` with `Rights { read, write, admin }`: `read` covers attach-for-read, snapshot reads, `status`, `read_base`, `versions`, `changed_since`, `export`; `write` covers attach-for-write, the mutating verbs, `snapshot`, `clone` (the clone's owner is the caller), `submit`, `rebase`, `pin`, `rewitness`, `materialize` (the grant is a separate, human-only act); `admin` covers `resize`, `destroy`, `archive`, changing the list, and revoking leases. The owner holds every right. Even a per-user daemon can host mutually isolated consumers; its access lists contain their enrolled identities. N=1 runs the same check and never grants every local process owner rights. Ids never authorize: a request names an id, a principal and (for mutations) an attachment with its lease epoch; all three must agree.

*Audit counters.* The audit log (§4.15, §4.14) records grants, manifests and landing outcomes; refusals are counted, never logged with content: `refusals{kind}` per refusal variant of §4.4's taxonomy, `forbidden{verb}` per verb, `grant_kind_refused{channel}` for the grant kind arriving on the ring or MCP channel (AC-2.8), `cross_uid_connect` at rendezvous (T-2.7), `stale_lease{shard}`, `stale_epoch`, `landing_lease_fenced` (a superseded holder refused by generation, AC-2.9). Every counter is a cache-padded per-shard `Relaxed` word summed on read (§3 of CLAUDE.md), exported through `status` with its freshness, and reset only by restart.

*Grants.* Only an authenticated human confirmation surface holds grant-issuer authority.
The daemon verifies that authority and the exact manifest hash, target identity, intended
consumer, scope and validity before accepting a grant. The issuer's protected channel is
established by trusted enrollment; running a CLI executable, claiming a control header class
or sharing the human's uid is insufficient. Ring, SDK and MCP grant kinds remain refused even
when they use a valid workload channel. Replayed, expired, revoked, retargeted or modified-plan
grants refuse before writing. Refusal to issue a grant never changes the proposed manifest.
The control transport and the VFS consumer channel have different authority, verified by the
server, not inferred from command names.

*Refusals added.* `Forbidden{verb}`, `GrantChannelRefused{channel}`,
`ConsumerNotEnrolled`, `ConsumerRevoked`, `GrantIssuerUnverified`; all are closed variants
carried through §4.4's operation refusal taxonomy. A-9 adds these requirements without claiming
that the current uid-based implementation enforces them.

*Content identity and sharing.* A chunk hash proves bytes, not permission to read them or ask
whether they exist. Missing-set exchange, caches, archives and dedup obey the consumer's
sharing scope and reference authorization; cross-scope existence and timing must not reveal
private data. Slates does not copy Hecate's disk-at-rest layout or salt scheme merely because
it uses content addressing. The RAM-only trust boundary and any allowed sharing are explicit.

### 4.14 Observability (D-23)

> **Status (2026-09-09).** The chokepoint-span roster and the three distinct identity types are built
> (`slates-wire::observe`: a closed `Chokepoint` enum of the nine spans, `SpanContext` = request +
> trace + span + optional caused-by, each a separate type so a trace field never carries a
> `RequestId`'s authority; doc-truth tested). The **emission foundation** now exists: a completed
> `Span` (three-id context + a content-free bounded dimension code + monotonic start/end), a bounded
> **shed-first `SpanSink`** (a ring that keeps the most recent spans and counts every shed one — no
> unbounded growth, explicit loss), and the `ChokepointRegistry` gate. The **registration gate** (§2.6)
> is **wired into the daemon boot**: `Daemon::start` builds the roster via `registered_chokepoints()`
> and refuses to serve — typed `ServerError::ChokepointsUnregistered { missing }`, fail-closed, before
> any resource is acquired — until every chokepoint has registered its emitter, so a daemon never
> serves with a silently missing span source. **Six chokepoints now emit for real**: each shard owns
> a bounded per-shard `SpanSink` (thread-local, no lock — R2; sized to one client ring's depth), and
> emits `shard.op` (the whole verb) and `log.append` (the durable `Db::commit` within it) for every
> verb in `run_recorded`, `ring.request` (ring read → reply written) for a synchronously-served reply
> in `serve_client`/`retry_deferred`, `merge.verdict` (one increment judged) in the submit handler,
> `bridge.request` (one NFS bridge call from arrival to reply) around `serve_local` in `crate::nfs`, and
> `land.entry` (one landing entry) through a **cross-crate span seam**: the land engine's `Observer`
> trait gained an `after_entry(start, end)` callback taking primitives (so `slates-land` stays
> wire-free), the server implements it with a bounded shed-first collector (a large landing never grows
> it unbounded), and drains it into the shard's sink after the landing — the reusable pattern for the
> remaining lower-crate chokepoints. Each carries a request id (a client verb's real one, read from a
> per-verb `current_request` context deep in a verb, §4.7; a bridge call's is the default), a per-shard
> span id, a content-free label, and a trace seeded from the request word until propagation is wired.
> The held/dropped counts ride `ShardReport` to `slates status`; a non-vacuity test shows the count move
> as verbs run. The six live chokepoints are **every one active in the single-node daemon path**. The
> other three are gated not on the seam (which is built) but on their subsystems being live: a laptop
> runs no replication (`ship.record`) or consensus (`consensus.step`) at f=0, and `slates-cluster` is
> not a daemon dependency yet, so those land with fleet integration (§4.8); the archive is not wired
> into the daemon and its codec is Phase 7, so `archive.chunk` lands with §4.10 — each then through the
> same cross-crate seam. Instrumenting them before their subsystems run would be untestable code (R5).
> Also owed: `ring.request` for a *forwarded* reply (its origin span crosses the shard boundary, so
> `read_ns` 0 marks it owed rather than timing it wrongly); the cross-shard aggregation of the per-shard
> sinks into the single control-shard sink (the `Control::Spawn` path the bridge queue uses); real
> cross-boundary trace propagation; and `(value, freshness)` on the daemon-level counters.
> **Typed absence on the shard health signals is done** (A-9): `Signal.value` is now `Option<u64>` with
> an `AbsenceIs` (`Unknown`/`Degraded`, `HealthSignal::absence()`), so an absent sample is never a
> silent healthy zero — a live `catalog.volumes: 0` reads as `0`, a genuinely absent signal (a future
> mirror age at f = 0, a non-reporting shard) as `absent/<meaning>`; pinned by `every_signal_types_its_absence`
> and a status-render unit test.

Chokepoint spans (bridge request, ring request, shard operation, log append, replication ship,
consensus step, archive chunk) with the three-id law; spans emitted asynchronously through
per-shard rings into the control shard's sink; health signals carry `(value, freshness)` and are
host-observed where possible (the anchor observes the daemon); metrics are content-free (no paths
or file contents); the machine profile and every derived constant are exported with their inputs;
per-volume counters (referenced, unique, locked, unlocked, hashing backlog, dedup hit rate,
compression ratio, lease epoch, attachments, base cache hit rate, drifted entries, watcher state;
per green: head version, merges per second, verdict p99, merge-path p99, conflict rate by
source, rebase-retry rate, base-lag p99, `StaleEpoch` refusals, holder recomputation mismatches)
are readable through the SDK and MCP status tool; the audit log (grants, landing manifests,
outcomes) is a separate append-only stream readable through the CLI, content-free except for the
paths a landing touched, which the human already approved.

**Observability specification (A-8, 2026-09-05; the spec §3 of GAPS owed before Phase 2).**

*Span roster.* Nine chokepoints, each a span with the three-id law (request identity, trace id plus span id, caused-by event identity) and a monotonic start and end: `bridge.request{op}` (a bridge call from arrival to reply), `ring.request{kind}` (a ring slot from read to reply written), `shard.op{verb}` (one verb on its owner shard, no awaits inside), `log.append{partition}` (one op-log record appended and published), `ship.record{object}` (one record or content put to its candidates, with the acknowledging count), `consensus.step{group}` (one configuration commit), `archive.chunk{codec}` (one chunk compressed or expanded), plus `land.entry{action}` (one landing entry) and `merge.verdict` (one increment judged). A span is emitted after it ends through the shard's telemetry ring (class Telemetry, shed first) into the control shard's sink; an emitter registers its name at start and the health plane refuses to serve until every name in this roster has registered (§2.6).

*Typed health and trace delivery (A-9).* Signal names form a closed registry. Every signal
specifies `AbsenceIs` (unknown or degraded as appropriate), freshness horizon, observer and
expected producer; missing/stale samples never silently mean healthy. Values may be absent
and must remain distinguishable from numeric zero. Registration does not prove live telemetry.
Bounded rings report dropped spans and missing causal links explicitly. Request identity is
for replay; trace/span identity connects work across bridges, rings, shards and holders;
`caused_by` connects causal events. Consumer and volume are authenticated tags, not substitutes
for trace context. Status exposes these definitions consistently through CLI/MCP.

*Health signal catalog.* Every signal is `(value, freshness_ns)` and host-observed where a host can observe it: `daemon.alive` (the anchor's view: the child is running and answered its last heartbeat), `daemon.restarts` (the anchor's count), `segment.generation` (the anchor segment's generation word), `shard.loop_lag_ns{shard}` (the driver's measured lateness), `shard.tasks{shard}` (live tasks against the arena), `ring.depth{client}` (command slots pending), `client.parked{client}`, `memory.locked_bytes` and `memory.unlocked_bytes` (per shard and rolled up), `memory.pressure` (PSI slope, macOS level, Windows notification), `catalog.volumes{shard}`, `log.bytes{partition}` and `log.replay_ns` (the last replay's duration against the recovery budget), `lease.expiring` (leases within one term of expiry), `base.watcher{volume}` (live, overflowed, unavailable), `land.active` (landings in flight), `placed.pending` (records awaiting f+1), `mirror_age{volume}`, `config.version`.

*Metric names.* One namespace, dotted, unit-suffixed, labels in braces; counters end in `_total`, histograms in `_ns` or `_bytes`. Per host: `slates.requests_total{kind,outcome}`, `slates.refusals_total{kind}`, `slates.provision_ns` (the AC-2.1 histogram: p50, p99, p999, max, spinning and parked), `slates.ring_wait_ns{client}`, `slates.wake_ns`, `slates.spin_to_park_ratio`. Per shard: `slates.shard.step_ns`, `slates.shard.batch`, `slates.shard.parks_total`, `slates.shard.kicks_total`, `slates.shard.arena_exhausted_total`. Per volume (readable through `status`): `referenced_bytes`, `unique_bytes`, `locked_bytes`, `unlocked_bytes`, `hashing_backlog_bytes`, `dedup_hit_ratio`, `compression_ratio`, `lease_epoch`, `attachments`, `base_cache_hit_ratio`, `drifted_entries`, `watcher_state`; per green: `head_version`, `merges_per_s`, `verdict_p99_ns`, `merge_path_p99_ns`, `conflict_rate{source}`, `rebase_retry_rate`, `base_lag_p99_ns`, `stale_epoch_refusals_total`, `holder_mismatch_total`. Per landing (in the report and the audit log): `entries{outcome}`, `bytes_written`, `dir_sync_ns`, `window_ns_max`, `ramp_depth`. The machine profile and every derived constant are exported as `slates.derived{name}` with their inputs.

*Content-freedom.* No metric, span or health signal carries a path, a name or file content; the audit log carries the paths a landing touched and nothing else; a test in the observability crate asserts every emitted label against this rule.

### 4.15 Disk as the source of truth: the base plane and landing under grant (D-25, D-26)

> **Status (A-9, 2026-09-05).** The read-only base seam and landing engine exist, as do
> server records, plan/refusal handling and Unix control transport/write integration. The CLI
> has `land`, grant listing and audit reads, but no `slates grant` issuance verb. Human issuer
> authentication, per-entry audit completeness and end-to-end consumer/grant tests remain open.
> Base data/metadata behavior is not fully connected to every mounted operation (BUG-5 and
> sibling audit), and restart does not yet recover complete volume contents (BUG-11).
> Historical tests in GAPS §8c/§8d were not rerun for this amendment.

**Live source and complete capture (A-9).** Creating a live overlay opens and identifies its
source without walking it. Untouched paths resolve the source's current names, metadata and
bytes through validated reads; copied-up/pinned entries retain their witnessed version and
report outsider drift. This is useful for laptop agents, but does not promise an atomic tree
snapshot across unrelated reads. Every mutation of a base-derived inode, including chmod,
truncate, link, rename and xattrs, passes through the same witness/copy-up rules. Whiteouts,
opaque directories, redirects, hardlink identity and open-unlinked handles must survive both
local and remote cloning. Watcher overflow invalidates affected cache knowledge; hints alone
never prove a source unchanged. Concurrent source edits may produce a typed retry/refusal,
not fabricated metadata or silently discarded operations.

A caller requesting a complete immutable point-in-time base uses `capture_base`. Admission
first reserves capture/retention costs. The source must be quiesced by an authority that can
actually exclude all writers, or exposed through a supported immutable read facility requiring
no Slates disk write or new privilege. Enumerate and retain the complete tree, metadata and
referenced bytes from that stable source, verify identities, then atomically publish its root.
Cancel or source failure releases uncommitted capture state. A mutable source without that
facility refuses `ConsistentBaseUnavailable`; repeated stats alone cannot prove that an
arbitrary multi-file scan corresponds to one instant. Ordinary `pin` can stabilize named
entries as observed, but is not relabelled atomic whole-tree capture. Report source identity,
coverage, dependencies, bytes read/retained and elapsed cost.

A verified content digest xattr, if exported, names exactly the current immutable file bytes.
It is invalidated before any mutation and absent while content is unsealed or unverified;
missing or stale cache knowledge cannot produce a clean digest. Discovery uses bounded scans
and cooperative slices. This is a planned optimization, validated by a counter and a byte
oracle before it supports a fast path; no performance gain is assumed.

**Role.** Make an existing host directory the base of a volume without copying it; keep the
agent's view honest when the disk moves; and write the agent's diverged entries back to that
disk, or to any directory the human names, only under a grant, with a pure per-entry verdict, one
holder per target, per-file compare-and-swap against outsiders, delta-only zero-copy parallel
write-back, data and directory syncs, and an audit trail. The base plane is read-only by
construction (its crate links no write-capable syscall); the landing crate is the only crate in
the workspace that does.

**Data model.**
```rust
struct GrantRecord { id: GrantId, principal: Principal, surface: Surface /* Cli | Confirmation{harness} */,
                     snapshot: SnapshotId, target: CanonicalTarget, manifest: Blake3, scope: GrantScope /* Once | Session{session} */,
                     issued: Monotonic, expires: Monotonic, state: GrantState /* Issued | Consumed | Expired | Revoked */ }
struct LandingRequest { id: RequestId, snapshot: SnapshotId, target: CanonicalTarget, filter: Filter, manifest: Handle<LandingManifest>,
                        state: LandingState }
enum LandingState { Planning, AwaitingGrant, Validating, Writing, Syncing, Advancing, Done, Partial, Refused, Aborted }
struct LandingManifest { hash: Blake3, entries: Vec<LandingEntry>, summary: Summary /* counts by kind and by top-level directory, bytes */ }
struct LandingEntry { path: PathKey, kind: EntryKind, action: Action /* Create | Replace | Delete | Rename{from} | Mkdir | Rmdir | Symlink | Chmod */,
                      witnessed: Option<Witness>, overlay: OverlayIdentity /* hash, size, mode, mtime */, verdict: Option<Verdict>,
                      outcome: Option<Outcome> /* Written | Skipped{reason} | Failed{errno} | Conflict{class} | Undone */ }
enum Verdict { Apply, Skip /* disk already holds it */, AcceptIdentical, Conflict(ConflictClass) }
enum ConflictClass { ModifyModify, ModifyDelete, DeleteModify, RenameRename, CreateCreate, TypeChanged, TargetInUse }
struct LandingLease { target: CanonicalTarget, holder: SessionId, generation: u64, expires: Monotonic }
struct AuditRecord { seq: u64, at: Monotonic, kind: AuditKind /* GrantIssued | GrantRevoked | LandingPlanned | LandingValidated | EntryWritten | EntryRefused | LandingFinished */,
                     grant: Option<GrantId>, landing: Option<RequestId>, manifest: Option<Blake3>, outcome: Option<LandingState> }
```
Ownership facts: a landing request belongs to the owner shard of the snapshot's volume and runs
as one of its tasks; the manifest lives in that shard's arena; grants, landing leases and the
audit log are database records (§4.8) written through the control shard; the target directory
descriptor is opened by the landing task and closed at its terminal step; nothing in this plane
is shared across shards.

**State machines.** Grant: `Issued → Consumed` (a landing bound to its manifest finished, for a
single-use grant) | `Issued → Expired` (its term, or the session ended, for a session grant) |
`Issued → Revoked` (the human revoked it); a landing may consume only a grant in `Issued` whose
`manifest` equals the manifest it is about to write. Landing: `Planning → AwaitingGrant →
Validating → Writing → Syncing → Advancing → Done`; `Validating → Refused` (a conflict, a lease
held, a mismatched grant); `Writing → Partial` (some entries failed their compare-and-swap and
were undone; the rest landed); any state → `Aborted` (cancelled or crashed, with each written
entry either old or new, never torn).

**Algorithms.**
1. *Plan.* Walk the snapshot's diverged entries only (the overlay tree, never the base), apply
   the caller's filter (include and exclude patterns; the summary lists what the filter removed),
   and build the manifest: creates, replacements (with the witnessed base), deletes (whiteouts
   with their witnessed base), directory renames (redirects, as one rename), directory creates
   and removals, symlinks and mode changes; hash the canonical encoding of the manifest. Cost
   proportional to diverged entries.
2. *Present.* Reply `GrantRequired{request_id, manifest_hash, summary}` and record
   `LandingPlanned` in the audit log. The confirmation surface shows the summary, the full entry
   list on demand, and the conflicts found by a preliminary verdict pass (step 4 run without
   writing), because a human decides on evidence, not on a yes/no prompt. The surface may offer
   suggestions the human toggles, such as excluding what the target's ignore files name; the
   agent's own filter stays explicit; a toggled suggestion
   produces a new manifest and a new hash, so the grant always binds what will actually be
   written.
3. *Grant.* A human issues the grant through the CLI or the confirmation surface; the record is
   bound to the manifest hash; a session grant covers later landings of the same volume into the
   same target for the session but each landing still presents its manifest, and any conflict
   still refuses. The server refuses grant creation on the ring and MCP channels.
4. *Lease and validate.* Take the landing lease on the canonical target (refuse
   `LandingLeaseHeld` if another session holds it); open the target directory descriptor with
   containment (Linux `openat2` with `RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS` for every component;
   `O_NOFOLLOW` chains on macOS; reparse-tag checks on Windows), refuse `EscapesTarget`,
   `TargetNotOwned`, `TargetIsVolume`; then for every entry compute the verdict from three
   inputs: the witnessed base (absent for a scratch volume or a created entry), the disk now
   (one `fstat` through a descriptor opened for the entry, with the racy rule re-hashing when the
   fingerprint is inconclusive), and the overlay identity.

   | Witnessed base vs disk now | Overlay vs witnessed base | Verdict |
   |---|---|---|
   | unchanged | changed | Apply |
   | unchanged | unchanged (a whiteout or rename only) | Apply |
   | changed | unchanged | Skip, drift reported (nothing to land) |
   | changed to the same bytes as the overlay | changed | AcceptIdentical |
   | changed differently | changed | Conflict(ModifyModify) |
   | deleted on disk | changed | Conflict(ModifyDelete) |
   | changed on disk | whiteout | Conflict(DeleteModify) |
   | renamed on disk (the origin gone) | redirect | Conflict(RenameRename) |
   | no witness; disk has an entry with different bytes | created | Conflict(CreateCreate) |
   | no witness; disk has an entry with the same bytes | created | Skip |
   | file became a directory or the reverse | any | Conflict(TypeChanged) |

   Any `Conflict` refuses the landing with `Conflict{entries}` and records `LandingValidated`
   with the verdicts; nothing is written. Resolution is the agent's or the human's (`read_base`,
   rewrite, `rewitness`), after which the landing is planned again and presents a new manifest.
5. *Order.* Directories top-down (`mkdirat` batched), then creates, then replacements, then
   directory renames, then file deletes, then directory removals bottom-up; each class runs in
   parallel and the next starts when the previous completes, so a tool reading the tree during
   the landing never sees a referenced entry missing.
6. *Write one entry.* Linux: `openat(dirfd, ".", O_TMPFILE|O_WRONLY)`; `writev` from the arena
   pages (io_uring, registered buffers, one linked chain per file: open → writev → fdatasync →
   linkat to a hidden sibling name → `renameat2(RENAME_EXCHANGE)` with the target); reflink
   (`FICLONE`) instead of writev when the target filesystem supports it and an identical file
   already exists in the target tree (found through the landing's own hash index of the
   manifest, never by scanning the disk); `futimens` before the exchange so incremental builds
   see the volume's mtime; then `fstat` the descriptor held on the displaced old file: if its
   fingerprint is not the witnessed one, exchange back, remove the sibling, and record
   `Conflict(TargetInUse)` for the entry. macOS: a hidden sibling name, `pwritev` on a pool
   thread, `fcntl(F_BARRIERFSYNC)`, `renamex_np(RENAME_SWAP)`, the same verify-and-undo;
   `clonefile` for reflinks where `VOL_CAP_INT_CLONE` says so. Windows: open the target with
   `FILE_SHARE_READ|FILE_SHARE_DELETE` and no write sharing (an outsider's handle without delete
   sharing is a sharing violation → `Conflict(TargetInUse)` before any write), validate through
   that handle, write the sibling with overlapped `WriteFile` on the completion port,
   `FlushFileBuffers`, `SetFileInformationByHandle(FileRenameInfoEx, REPLACE_IF_EXISTS |
   POSIX_SEMANTICS)`, and keep the target handle until the replace returns (it still names the
   old file, so the verify is free). Sparse ranges are preserved (data extents only); large files
   are preallocated. Deletes: `unlinkat` (or `RemoveDirectory`) only after the entry's fingerprint
   matches the witnessed base through a descriptor opened `O_NOFOLLOW`; directory renames:
   `renameat2` / `renamex_np` / `FileRenameInfoEx` of the origin to the new name after the origin's
   fingerprint matches.
7. *Concurrency inside the landing.* In-flight entries start at the measured count of the
   landing pool's cores and double while measured throughput rises by more than its variance and
   per-entry latency p99 stays within the previous step's by the measured variance fraction
   (Little's law applied online); the ramp backs off otherwise; the settled depth is remembered
   in the profile for the session and for that target's filesystem. No disk is probed at boot.
8. *Sync.* Data syncs happen inside each entry's chain; after the last entry, one directory sync
   per touched directory in parallel (Linux `fsync` on the directory descriptors, or `syncfs`
   once when the measured per-directory cost times the directory count exceeds the measured
   `syncfs` cost; macOS one `F_FULLFSYNC` after the barriers when the grant asked for media
   durability, otherwise the barriers only; Windows `FlushFileBuffers` on the directory handles).
   The outcome record states which durability the landing achieved.
9. *Advance.* For every `Written` entry, `fstat` the landed file and make that the entry's new
   witness; clear the overlay entry (the volume now reads it from disk, and its cached bytes are
   evictable); clear whiteouts and redirects that landed; a scratch volume gains `Base::Path` on
   the target. Entries that failed stay in the overlay with their outcome. Record
   `LandingFinished{Done | Partial}`, release the lease, consume the grant, and reply with the
   report.
10. *Stage-and-exchange (measured alternative).* When the target is empty, or when the manifest's
    entry count exceeds the measured break-even (staging cost = total entries × measured link or
    reflink cost + delta bytes × write cost + one exchange; in-place cost = delta entries × (swap
    + verify cost) + delta bytes × write cost), the landing builds the whole tree in a hidden
    sibling directory and exchanges it with the target in one `RENAME_EXCHANGE` / `RENAME_SWAP`
    (Windows: two POSIX-semantics renames, the window reported), then removes the displaced tree
    after verifying it against the witnessed bases. Never the default for a populated target,
    because it changes the directory's inode and every path's identity.
11. *Crash and resume.* Every written entry is old or new, never torn (a temporary is linked or
    exchanged only after its data sync). A crashed landing's manifest is in the anchor segment;
    the next landing of the same snapshot into the same target re-plans, skips entries whose
    disk hash already equals the overlay's (idempotent), and removes hidden siblings that carry
    its landing id inside the granted target (never elsewhere). After a reboot the manifest is
    gone; the re-plan is still idempotent by hash.

**Exchange fallback.** Where the target filesystem lacks `RENAME_EXCHANGE` (`EINVAL`) or
`RENAME_SWAP` (`ENOTSUP`, detected by capability query), the entry is written by verify-then-
`rename`-over: fingerprint check, then rename; the window between them is unverified and its
measured width is written into the entry's outcome. This is a documented Degraded cell and a
tripwire (`GAPS.md`).

**Networking table.** None. Landings are host-local. The landing lease is the only fleet-visible
record (a register the host writes to its candidate holders under its epoch, §4.8).

**Failure matrix.** Grant missing: Refused (`GrantRequired`). Grant bound to another manifest
(the plan changed after the human saw it): Refused (`GrantMismatch`), the new plan is presented.
Grant expired or revoked mid-landing: the current entry finishes, no further entry starts,
`Partial` with the reason. Grant declined by the human: Refused (`GrantRefused`), the request
ends, nothing written. Conflict at validation: Refused (`Conflict{entries}`), nothing written.
Compare-and-swap loss during writing (an outsider replaced the file between validation and the
exchange): that entry is exchanged back and recorded `Conflict(TargetInUse)`; the landing
continues; the report is `Partial`. Sharing violation on Windows: same. Lease held by another
session: Refused (`LandingLeaseHeld{holder, generation}`). Target escapes, not owned, or inside a
slates mount: Refused before the lease is taken. Exchange unsupported on the filesystem: Degraded
(fallback with the window reported). Crash mid-landing: Degraded (old-or-new per entry; resume by
hash; hidden siblings swept). Power loss after the report: Masked (data and directory syncs
preceded the report; on macOS only if media durability was requested, otherwise Degraded and so
stated in the report). Disk full mid-landing: the entry fails `ENOSPC`, its sibling is removed,
the landing continues to `Partial` (the human sees exactly what landed). Base directory removed
during a landing: `Aborted` with every finished entry listed.

**Refusals.** The base and landing entries of §4.4's taxonomy; nothing else.

**Derived constants.**

| Constant | Formula | Anchors |
|---|---|---|
| In-flight entries per landing | online ramp: start at pool cores; double while throughput rises by more than its variance and latency p99 holds; back off otherwise | per-entry latency and throughput samples inside the landing |
| Landing pool size (macOS thread pool) | the settled in-flight depth, bounded by measured free cores | the ramp; the profile's core classes |
| Directory sync strategy (Linux) | per-directory `fsync` unless directories × measured per-directory cost > measured `syncfs` cost | the first directory syncs of the landing |
| Stage-and-exchange break-even | staging cost < in-place cost by the formulas in step 10 | measured link, reflink, write, swap and verify costs from the landing's first entries |
| Racy window | filesystem timestamp granularity (cited table by filesystem type) + measured clock resolution | `statfs` type; `clock_getres` |
| Landing lease term | measured landing duration p99 × k, renewed by keepalive while entries are in flight | landing duration histogram |
| Grant term | the session's lifetime for session grants; the measured plan-to-grant interval p99 × k for single-use grants | audit log intervals |
| Audit retention | landing rate × operator audit horizon × record size | measured landing rate |
| Watcher coalescing window | p50 inter-arrival of event bursts | watcher event timestamps |

**Worked example.** An agent overlays `/home/u/proj` (300k files); edits twelve files, deletes
one, adds three; `cargo build` writes 190k files under `target/`. It calls
`materialize(head, "/home/u/proj", exclude=["target/"])`. Plan: 16 entries, 41 KiB; the reply is
`GrantRequired{request 9, manifest b3:…, summary}`. The human runs `slates grant --watch`, sees
"12 replace, 1 delete, 3 create under src/ and tests/, 190k entries excluded by filter, 0
conflicts", and grants once. The landing takes the lease, validates 16 entries (16 `fstat`s), and
writes them in parallel from arena pages with linked io_uring chains; `src/lib.rs` is exchanged
with its temporary, the displaced file's fingerprint matches the witness, and the entry is
`Written`; directory syncs follow; the witnesses advance; the report lists 16 `Written`. Total
disk work is proportional to 41 KiB and 16 entries, not to 300k files. Failure one: before the
grant, the user edited `src/lib.rs` in an editor; the preliminary verdict shows
`Conflict(ModifyModify)` for it in the confirmation surface; the human declines; the agent reads
the disk version with `read_base`, rewrites its copy to include the user's change, calls
`rewitness(["src/lib.rs"])`, and lands again with a new manifest. Failure two: during the write,
`git checkout` replaced `tests/a.rs` between validation and the exchange; the exchange returns
the new inode, its fingerprint is not the witness, the landing exchanges back and records
`Conflict(TargetInUse)`; the report is `Partial` with 15 `Written` and one conflict; the entry
stays in the overlay.

**Laptop degenerate.** Identical; the landing lease is a partition record with no peers.

> **Status (2026-09-05).** Implemented in `crates/land` (GAPS §8c, Phase 1 tasks 11–13):
> steps 1–6, 8, 9 and 11 as written, with grants, leases and the audit log as in-process
> records until Phase 2; step 7's ramp as a recorded policy (entries run one at a time until
> the runtime's pool); step 10 for an empty target (a populated target waits on a hard-link
> verb in the seam); reflinks and `syncfs` waiting on their measured costs; Windows waiting on
> Phase 4. Two additions the oracle forced: a `Clear` action (a base directory the overlay
> removed and recreated opaque: a fresh directory exchanged with the old one, the displaced
> tree removed), and a resumed landing's recognition of its own finished work (a directory
> holding only what the manifest creates beneath it; a rename whose destination holds the
> witnessed directory), so the re-run is idempotent for directories as it is for files.

**Integration points.** §4.4 (verbs, refusals, states), §4.5 (whiteouts, redirects, witnesses,
copy-up, drift), §4.6 (base reads, invalidation on drift), §4.8 (grant, lease and
audit records), §4.10 (retained live base dependencies and complete captures), §4.12 (surfaces; no grant verb for agents),
§4.13 (containment, ownership, human-only grants), §4.14 (audit log), Part 6 (the tracer's
granted-target exception).

### 4.16 The merge engine: green volumes, increments, canonical rebase, the deterministic verdict (D-27)

**A-9 integration requirement.** The implemented pure merge core is not a user-facing green
volume service. Green's immutable version chain starts from scratch or a complete immutable
base, never an implicitly live host directory. Work volumes preserve that base and their
witnesses. Submission first establishes the contributing attachment barrier, retains every
input to the declared-operation verdict, and places those inputs before a distributed merge
record references them. Role enforcement, CLI/MCP verbs, version-pinned attachments and holder
recomputation remain explicit integration gates; existing pure-core tests do not close them.

> **Status (2026-09-05).** The pure core is implemented and gated (`slates-merge`, GAPS §8f):
> the deterministic verdict (the two passes — the sweep-line range verdict and the memcmp — the
> conflict classes, and the whole-increment fast path, oracle-tested over the hecate M-matrix as
> range cases), the canonical ops document (the declared-operation serialization whose BLAKE3 is
> half an increment's identity, determinism-gated), and the content deriver (one path's declared
> content operations composed into the canonical net op set by interval algebra, never a diff,
> with a byte-level oracle that reconstructs the post-state from the net ops), and position
> mapping (an increment's ranges shifted forward through the intervening deltas to head
> coordinates, overlaps handed to the verdict, with a provenance oracle), and the whole-volume
> deriver (a journal of content, create, unlink, rename and directory create/remove composed into one ops document —
> create-unlink cancellation, base-file replacement, the write-and-rename content replacement,
> the post-state laid out in sorted-path order, invalid operations typed refusals — with a
> whole-filesystem reconstruction oracle), and the splice (a path's accepted ops applied to its
> base extent list by reference surgery, no byte copied, with a read-back oracle). All on every
> host. The whole-volume deriver now composes every §4.16 operation kind (content, create, unlink,
> rename, directories, symlinks, hard links, mode and xattrs; GAPS §8f), and the single-node merge
> engine composes the pieces into submit/verdict/splice/commit — content merges (create/create,
> delete/modify, the range verdict, the fast path, rebase) and the namespace dimensions that merge
> per path (directory creation, mode, symlink, file rename and directory removal, with type,
> metadata, differing-target, rename/rename and not-empty conflicts; a rename captures the source
> content so an intervening edit follows the move, removes apply before sets so a chained rename
> rotates correctly, and a directory is removable once the increment's own removals empty it).
> The engine now consumes the whole ops document (`Increment { doc, post_state }`) rather than one
> change per path, so directory rename (as the deriver's child ops), hard link (a namespace edge)
> and xattr merge too, and several dimensions on one path in one increment merge together. The
> engine's content verdict now decides identity **per range**, not per file (the design's pass-two
> memcmp of the same-range span alone): an overlapping overwrite accepts as a no-op when the green
> already holds exactly those bytes at that span, so a disjoint edit elsewhere in the file no longer
> turns an identical overlap into a false conflict. A generative oracle proves it — a serial
> block-wise reference the engine must equal over every generated history of length-preserving
> block edits (coordinate-free, so the reference states the design's per-range rule directly), plus
> the worked case (T-6.x, `crates/merge/tests/engine.rs`). Per-range identity for a length-changing
> overlap (insert/delete/truncate) still falls back to the whole-file check (its coordinate mapping
> under a conflicting neighbour is owed). An unlink and a rename now act on whatever the path
> names — a file, a symlink or a hard link (both had handled only files: unlink no-oped a symlink,
> rename false-conflicted one); `base_at` reconstructs a lagging work's base at
> an intervening version across *every* dimension (a per-dimension `(version, value)` history the
> commit records; directories/modes/symlinks/hard-links/xattrs had come back empty); and a path the
> increment itself creates or makes now establishes it for that increment's metadata, so a file
> created and chmod'd or xattr'd in one increment accepts (it had conflicted delete/modify against the
> not-yet-committed path). The fully general intra-increment coordination (a path both renamed away
> and recreated in one increment), that shifting-op per-range identity, the checkpoint folding of the
> canonical deltas, and the copy-on-write green chain are the rest of Phase 6; the fleet register,
> mirror and reconfiguration protocols are now simulated (§4.8 status, GAPS §8h).

**Role.** Let many agents work on clones of one shared volume and fold their work back into it
with no locks, no last-writer-wins, and no inferred merge. Each agent's work becomes an
increment of declared operations; the green volume's merge task maps the increment through
everything accepted since the agent's base version, decides a pure verdict, splices accepted
operations into a new version by extent surgery, and commits the version as the next entry of
the green's fenced ledger register. Conflicts are byte-exact windows the agent rebases against. This is hecate's merge architecture
in slates' ownership model; the departures and their reasons are in D-27 and
`research/merge-engine.md` §2.

**Data model.**
```rust
struct Chain /* per green volume, on its owner shard */ { head: Version, records: Art<Version, VersionRecord>,
             deltas: Art<Version, Handle<CanonicalDelta>>, checkpoints: Vec<(Version, Handle<CanonicalDelta>)>,
             last_changed: Art<PathKey, Version>, seen: Art<IncrementId, (Version, VerdictSummary)>, task: MergeTask }
struct VersionRecord { version: Version, snapshot: SnapshotId, manifest: Option<Blake3>, increment: Option<IncrementId>, host_epoch: u64, at: Monotonic }
struct Increment { id: IncrementId /* blake3(work_volume, base, post_state, ops_doc, filter) */, work_volume: VolumeId,
                   base: (VolumeId, Version), post_state: SnapshotId, ops_doc: Blake3 /* a sealed chunk */, filter: FilterId,
                   evidence: SmallVec<Blake3> /* opaque to slates */ }
#[repr(C)] struct OpRecord { kind: u8, flags: u8, path_idx: u16, reserved: u32, at: u64, len: u64, src: u64 }   // 32 bytes, little-endian
enum OpKind { Overwrite, Extend, Truncate, Insert, Delete, Create, Unlink, Mkdir, Rmdir, Rename, Link, Symlink, SetMode, SetXattr, RemoveXattr }
struct OpsDoc { header: WireHeader, paths: PathTable /* path_idx → path bytes, sorted */, ops: [OpRecord] }
struct CanonicalDelta { version: Version, ops: Handle<OpsDoc>, effects: Art<PathKey, RangeSet> /* per path: ranges touched and size shifts */ }
struct MergeRecord { green: VolumeId, version: Version, increment: IncrementId, verdict: VerdictSummary, manifest: Blake3, host_epoch: u64 }
struct ConflictWindow { path: PathKey, mine: (u64, u64), theirs: (u64, u64), theirs_version: Version, class: MergeConflictClass }
enum MergeConflictClass { Overlap, AnchoredInDelete, SamePositionDiffering, RenameRename, CreateCreate, DeleteModify, ModifyDelete, TypeChanged, MetaMeta }
```
Ownership facts: the chain lives on green's owner shard; an increment's ops document and
post-state are sealed chunks and a snapshot on the work volume's owner shard, readable from any
shard by handle while pinned (D-7); the merge record is an entry of the green's ledger register
(a partition log append on a laptop; a record sent to the green's candidate holders in a fleet,
§4.8); nothing in this plane is shared mutable state.

**Declared operations (the journal, §4.5).** Every mutation is declared at the boundary where it
happens: through a mount, `write(off, len)` is `Overwrite` (or `Extend` past the end) and
`truncate` is `Truncate`; through the SDK, `edit(path, at, delete_len, bytes)` is `Delete` then
`Insert` with true positions; namespace calls are themselves. Each record carries the file's
previous version so composition is exact. Bytes are never in the journal.

**Composition at seal (the deriver).** Per path, the declared operations since the base version
compose by interval algebra into a net op set relative to the base version's content:
overlapping overwrites merge into one; an insert followed by an overlapping delete cancels or
splits; a truncate cancels operations beyond the new length; a rename maps the path of later
operations; a create followed by an unlink cancels; a whole-file rewrite (truncate to zero and
write, or write-and-rename) composes to one `Delete` of the base length and one `Insert` of the
new bytes. Composition of declared operations is arithmetic on facts; comparing file states to
reconstruct operations is inference and does not exist in slates. The same journal yields the
same ops document on every platform (its identity is the test). Paths under the work volume's
excluded subtrees are left out; the filter id is part of the increment identity.

**Position mapping.** Through the canonical deltas in (base, head], one direction, per path: an
intervening size-changing operation at position p with delta d shifts the increment's ranges
that lie strictly after p by d; ranges that overlap an intervening effect range are handed to
the verdict; a rename in an intervening delta remaps the increment's paths beneath it. Maps
compose: `map(a..c) = map(b..c) ∘ map(a..b)`. Deltas older than the derived retention fold into
checkpoint deltas by the same composition, which is exact, so every base back to green's origin
maps at O(log) checkpoint lookups plus the raw deltas after the last checkpoint.

**The verdict, two pure passes.** Inputs: the mapped increment, the effect ranges of the
intervening deltas, and the referenced immutable content. Pass one: a sweep line over the
increment's ranges and the intervening effect ranges per path, O((n+m) log(n+m)); disjoint
ranges are `Accept`; same-range candidates are listed for identity checks; everything else is a
`Conflict` with its class: overlap or containment; an edit anchored inside an intervening
delete; same-position differing inserts; rename against rename; create against create with
differing content; delete against modify in either direction; a type change; differing mode or
xattr changes on one path. Fetch, outside the pure core: the candidates' bytes by handle. Pass
two: memcmp; equal bytes are `AcceptIdentical`, unequal are `Conflict`. Neither pass performs
I/O, reads a clock, draws randomness, or calls the system allocator (a lint and a test enforce
it). Fast path: if `last_changed[path] <= base` for every path in the increment, the verdict is
`Accept` with no range work at all; this is what a basis buys, and it never decides a conflict.

**Splice.** For each accepted operation the new version's entry for that path gets an extent
list built from the base version's extents with the operation's range replaced by a reference
into the increment's post-state chunks (`Extent{src: Chunk{chunk, off}}`, sub-chunk offsets
allowed), so no byte is copied; directory operations apply to current-epoch nodes under the
birth-epoch rule; the result is one snapshot, version N+1, whose manifest identity the
background hasher computes, or the merge task computes at once (O(changed)) when a fleet needs
it before commit. The increment's post-state snapshot stays pinned by the merge record until the
version is superseded and its deadlist processed; cross-shard chunk references are acquired in
one batched message per increment (§4.8).

**Commit.** The merge record `{green, version, increment, verdict, manifest, host_epoch}` is the
next entry of green's ledger register: on a laptop an append to the partition log in the
anchor segment; in a fleet a record sent to green's 2f+1 candidate holders under the owner's host
epoch, committed at f+1 acknowledgements, refused by any holder that has seen a higher epoch for
that host, and issued only when every identity the version references is placed. The reply is `Accepted{version}` or `Conflict{windows}`. An
increment id already in `seen` returns its original result (the completion record holds the
reply); retries are always safe.

**Apply on holders (fleet).** Every holder of green's version N+1 recomputes the verdict and the
manifest identity from the record's inputs before serving the version to any attachment, and
compares its head identity with the record at every version; a mismatch refuses that version on
that holder, fatal-and-loud, before any read. On a laptop the owner is the only holder and no
second computation exists; that is the derived N=1 case, and the differential test proves the
fleet path computes the same answers.

**Submission.** `submit(work_volume, evidence?)`: seal the work volume (drain open extents,
compose the net op set, chunk; in a fleet, place the snapshot to its candidate holders now, at
f+1 acknowledgements, if the auto-seal has not), build the increment, send it to green's owner: the same shard is a call, another
shard a cross-shard message, another host the wire, routed by the green's id to its current
owner with the piggyback rule (a refusal from a non-owner names the current owner and epoch;
placement refresh is single-flight per green). The caller parks and resumes with the verdict. A work volume with
`stream = true` submits at every auto-seal; an agent that wants a review boundary submits
explicitly. A green with `require_evidence` refuses increments without evidence references.

**Rebase, the only corrective path.** `rebase(work_volume, to = head)`: map the work volume's
pending operations (since its base) through the canonical deltas up to `to`; if every operation
is `Accept` or `AcceptIdentical`, re-base the clone to `to` by extent surgery (its root becomes
version `to`'s root with the mapped pending operations re-applied), O(pending operations), and
the work volume's base becomes `to`; if any is `Conflict`, return the windows and change nothing.
The agent reads green's bytes for each window (a read of the named version), rewrites its own
bytes, and calls `rebase` again. A rebased increment has a new identity; deduplication never
blocks it. Rejects are non-blocking; the merge task never stalls on a conflict.

**Attachments and versions.** Mounts and SDK attachments of green pin a version;
`advance(attachment, version)` re-pins and invalidates exactly the paths the manifest diff
between the two versions names (O(changed) by structural sharing); no attachment's view changes
without `advance`; `status(green)` reports the head, the attachment's version, and
`changed_since(version, paths)` from the last-changed index so a work volume learns early which
of its pending paths green has moved under (awareness, never load-bearing).

**Failure matrix.** Green owner restart: Masked (chain, deltas and `seen` replay from the log;
in-flight submissions retry by identity). Green owner loss in a fleet: Degraded for the
membership horizon, then Masked after takeover on a candidate that holds the ledger. Work volume owner loss before
`submit`: the volume's loss window (D-18) applies to the unsubmitted work. Submitter crash after
commit: the retry returns the original result; the version stands. Merge record with a stale
host epoch: Refused (`StaleEpoch`) at the first holder, never applied; the stale owner drops the
role. Holder
recomputation mismatch: Refused for that version on that holder, alarm, served from other
holders (Degraded); persistent mismatch is a bug signal that fails CI. Unknown base (green
destroyed, or a version of another green): Refused (`UnknownBase`). Ops document or post-state
unavailable: Refused (`ContentUnavailable`), retryable. Increment above the derived size budget:
Refused (`IncrementTooLarge{limit}`); the SDK splits by path. Evidence required and absent:
Refused (`EvidenceRequired`).

**Refusals.** The merge entries of §4.4's taxonomy plus `StaleEpoch` and `ContentUnavailable`.

**Derived constants.**

| Constant | Formula | Anchors |
|---|---|---|
| Delta retention before folding | measured base-lag p99 (versions) × safety, capped by the delta memory budget | base-lag histogram per green; budget from §4.2 |
| Checkpoint spacing | the version count where measured mapping cost through raw deltas exceeds mapping through one folded delta | per-delta mapping cost samples |
| Increment size budget | ops records such that measured per-record verdict cost × records stays under the merge-path budget | per-record cost; the ratcheted merge-path p99 |
| Stream cadence | the auto-seal cadence of §4.8 | mutation rate; loss-window SLO |
| Submission deadline | measured merge-path p99 × k | merge-path histogram |
| Placement refresh | single-flight per green; bounded refresh/retry using the returned configuration version | refusal timestamps and operation deadline |
| Last-changed index budget | paths touched per green × (key + version) | per-green path counts |

**Worked example.** Green G is at version 42 (a 300k-file repository). Agents A, B and C each
clone it as a work volume. A uses the SDK `edit` on `src/lib.rs`: delete 40 bytes at offset 412,
insert 45; B's editor rewrites `src/main.rs` whole, which composes to one delete of the base
length and one insert of the new bytes; C edits `src/lib.rs` at offset 400 for 60 bytes. A
submits: `src/lib.rs` last changed at version 39, so the fast path accepts without range work;
version 43 references A's chunk for the new bytes and G's chunks around it; the merge record
commits; A's reply is `Accepted{43}`. B submits with base 42: `src/main.rs` last changed at 41,
fast path, `Accepted{44}`. C submits with base 42: `src/lib.rs` changed at 43, so the increment is
mapped through delta 43 (a size shift of +5 after offset 452) and the sweep line finds C's
[400, 460) overlapping A's effect range [412, 457): `Conflict{[src/lib.rs: mine (400, 60),
theirs (412, 45) at 43, Overlap]}`; nothing is written. C reads version 44's bytes for the
window, rewrites its edit, calls `rebase(to = 44)`, which maps C's remaining operations and
accepts, and resubmits: `Accepted{45}`. Failure: G's owner is paused for longer than the
membership horizon while a merge record for version 46 is in flight; the regional group has
bumped that host's epoch and assigned G to a candidate that holds version 45; the stale record
is refused `StaleEpoch` at the first holder and never applied; the paused owner drops the role
on resume; the submitter's retry lands at the new
owner and returns `Accepted{46}`.

**Laptop degenerate.** One node: the merge record is a partition log append, placement is local,
the holder set is the owner, every arrow in the submission is an in-process call, and the
merge path is microseconds.

**Integration points.** §4.4 (roles, verbs, refusals), §4.5 (declared operations, per-inode
versions, the deriver's inputs), §4.6 (`EROFS` on green mounts; invalidation on `advance`), §4.8
(chains, deltas, last-changed index, merge records as ledger entries), §4.10 (placed before committed;
holder recomputation; submission across hosts), §4.12 (`slates.merge`, `edit`), §4.15 (the
landing verdict shares the class vocabulary; entry-level for the disk side because the disk
declares nothing), Part 6 (the merge oracle and the purity lint).

---

## Part 5 — Phased implementation plan

Rules for every phase: nothing is stubbed and left; each phase ends with running, tested code
and a recorded benchmark baseline; every constant lands with its derivation table; the gap ledger
(`docs/wip/GAPS.md`, created in Phase 0) is updated in the same change as any acceptance or
tripwire. Acceptance criteria are numbered AC-<phase>.<n>; test cases T-<phase>.<n>. "Expect"
lines state the observable outcome an implementer must see.

### Phase 0 — Foundations: workspace, machine profile, memory, runtime, wire

**Goal.** A workspace that enforces the rules by construction, plus the four crates everything
else stands on, each measured on the reference machines.

**In scope.** Cargo workspace with the lint wall; `slates-machine` (profile), `slates-mem`
(slabs, handles, arenas, buddy allocator, segmented arrays, locking), `slates-rt` (executor,
timing wheel, rings, drivers for io_uring/epoll, kqueue, IOCP, and the simulation driver),
`slates-wire` (framing, canonical bodies, schema hash, credits); the release matrix and CI from
Appendix B; the gap ledger.

**Out of scope.** Volumes, bridges, database, SDKs.

**Ordered tasks.**
1. Create the workspace from Appendix B (toolchain pin, edition 2024, lints, `panic = "abort"`,
   CI with fmt/clippy/test, the nine-target release workflow as a dry run). Add the structural
   test that walks the dependency graph and fails if `std::fs`, `std::net`, `Arc`, or `Rc` appear
   outside the allow-listed crates. Add the grep-based literal check that fails on numeric
   literals in tuning positions (a `#[derived("formula", anchors)]`-style attribute or a
   `derived!` macro marks the allowed ones).
2. `slates-machine`: implement every query and microbenchmark of Part 4.1 with Kalibera-Jones
   stopping; produce the profile struct and its JSON export with derivations; cache it in a
   segment keyed by host identity (Linux memfd, macOS shm_open, Windows section); re-measure on
   power-state events.
3. `slates-mem`: `Handle<T>`, `Slab<T>`, `ChunkArena` with buddy allocation over locked regions,
   segmented arrays, per-shard free lists with message-passing frees, the RAM-only locking
   sequence with reporting, and the pre-fault batch scheduler; huge-page requests per region
   after pre-fault where the profile shows benefit.
4. `slates-rt`: task arena, `RawWaker` encoding, intrusive run queue, hierarchical timing wheel,
   per-shard inbound rings, cross-shard wake kicks, the three OS drivers with a common completion
   seam, and the simulation driver (seeded time, randomness, sockets, rings); structured
   cancellation (parent-tracked children); the bounded-work watchdog counter.
5. `slates-wire`: the 32-byte header, canonical `#[derive(Wire)]` bodies with schema hash,
   append-only evolution checks (trybuild), credit windows, CRC32C, golden vectors, hostile-input
   tests.
6. Audit compio's request path for `Arc` and driver seams (one page in the gap ledger); confirm
   `LocalWaker` status on 1.98.
7. Record the Phase 0 benchmark baseline (ring round trip per core pair, task spawn/wake cost,
   timer accuracy, slab alloc/free, buddy alloc/free, frame encode/decode) and set the ratchets.

**Worked examples.**
- On the author's laptop, `slates profile` prints page 16384, line 128, 18 cores (6 Super, 12
  Performance), lock capacity 108.8 GiB, wake p99 (measured), memcpy curve, hash 
  and codec throughput, and the derived shard count (5 = 6 Super cores minus one control) with
  the formula beside each number.
- A cross-shard wake: shard 2 wakes a task on shard 4 by writing `(slot, generation)` into shard
  4's ring from shard 2 and writing shard 4's eventfd; shard 4's driver returns from `io_uring_enter`,
  drains the ring, and runs the task; the whole path is one cache-line transfer plus one syscall.
- Failure example: a container refuses `io_uring_setup`; the driver probe records it and the
  shard runs on epoll; the profile marks `io_uring = unavailable(EPERM)`.

**Acceptance criteria.**
- AC-0.1 The structural test fails the build when a forbidden symbol is introduced in a core
  crate (catches: disk fall-through and `Arc` creep).
- AC-0.2 Every numeric literal in a tuning position carries a derivation attribute, verified by
  the literal check (catches: magic numbers).
- AC-0.3 `slates profile` completes within the wall-time bound on all nine targets and reports
  intervals for every measurement (catches: unmeasured constants; slow boots).
- AC-0.4 Slab and buddy allocation never call the system allocator after start (allocation
  counter feature proves zero calls on the hot path) (catches: hidden allocation).
- AC-0.5 Locked-bytes reporting matches what the OS reports (`/proc/self/status` VmLck, macOS
  wired count, Windows working set) within one page class (catches: pretend locking).
- AC-0.6 The executor runs the same task program on all three OS drivers and the simulation
  driver with identical observable results (catches: driver divergence).
- AC-0.7 loom passes on the ring and handle cores; Miri passes on `mem`, `rt`, `wire` unit tests
  (catches: memory-model and UB bugs).
- AC-0.8 Frame decode refuses every hostile corpus input without panic or over-allocation
  (catches: parser DoS).
- AC-0.9 The N=1 differential harness exists and passes for the runtime's public API (catches:
  mode creep).

**Test cases.**
- T-0.1 (unit) Allocate every slot of a slab, free them in random order, reallocate; expect no
  duplicate handles, every stale handle refused.
- T-0.2 (property) Buddy allocator: random alloc/free sequences; expect no overlap, coalescing
  restores the full region, fragmentation bounded by the buddy guarantee.
- T-0.3 (concurrency, loom) SPSC and MPSC rings under all interleavings of one producer and one
  consumer (and two producers); expect FIFO, no lost or duplicated slots.
- T-0.4 (error) Lock capacity exhausted mid-boot; expect the profile records the capacity, the
  diagnostic control surface starts, and volume admission refuses `LockCapacityExceeded`;
  no successful claim is backed by unlocked memory and no panic occurs.
- T-0.5 (edge) A 32-bit target with a 2 GB address space; expect region reservation caps derived
  from `ullAvailVirtual`, and a request above it refused with `BudgetExceeded`.
- T-0.6 (benchmark) Ring round trip per core pair; expect numbers within the profile's interval
  and a ratchet recorded.
- T-0.7 (fault) Kill a shard thread's driver mid-wait (simulation driver injects); expect the
  shard's tasks are cancelled with terminal completions, no leak, counters incremented.
- T-0.8 (hostile) Frames with `length = u32::MAX`, truncated headers, bit flips; expect typed
  refusals.
- T-0.9 (timer) 10,000 timers with random deadlines; expect firing order and accuracy within one
  wheel tick derived from the profile.

**Exit criteria.** All AC pass on Linux and macOS in CI and on Windows nightly; baselines
recorded; the gap ledger lists the compio audit result.

**Risks and fallbacks.** io_uring differences across kernels (probe and fall back); Windows
IOCP driver complexity (keep the seam small; WinFsp integration comes in Phase 4).

**A-9 required acceptance and regression cases (open; 2026-09-05).**

| Acceptance | Test | Required behavior | Do X, expect Y |
|---|---|---|---|
| AC-0.10 | T-0.10 | All allocator and temporary-operation costs are bounded and charged to usable locked capacity. | Exercise fragmented/non-power-of-two regions, exhausted locks and saturated bulk/parse work; expect honest admission refusals and bounded control progress, with no uncharged growth. |
| AC-0.11 | T-0.11 | Health absence and trace identity retain their declared meaning. | Drop a producer and overflow telemetry; expect typed unknown/degraded freshness and loss markers, while request, trace/span and caused-by identities stay distinct. |

### Phase 1 — Volume core (single node, in-process API)

**Goal.** The copy-on-write filesystem as a library: namespace, inodes, content, snapshots,
clones, accounting, journal, tested against a model and the host filesystem, with no bridge and
no server.

**In scope.** `slates-vfs`: directories (small/indexed), inode slab, extent lists, open extents,
seal, birth-epoch CoW with deadlists, snapshots, clones, destroy, quotas (bounded and dynamic),
name-equivalence policies, hard links, symlinks, rename semantics, the per-volume op log and
journal; the executable model and the differential harness; the base plane of §4.15 (`slates-base`:
read-only host access, listings, witnesses, whiteouts, redirects, copy-up, drift with watcher
hints behind the driver seam) and the landing engine (`slates-land`: manifest, verdict, per-OS
write primitives, ramp, syncs, advance, resume), driven through the in-process API against
RAM-backed target directories (tmpfs, an APFS RAM disk, an NTFS RAM VHD).

**Out of scope.** Hashing/dedup (Phase 7 wires the identity pass; Phase 1 leaves `identity =
None`), bridges, IPC, database replication.

**Ordered tasks.**
1. Data model and the volume state machine of Part 4.4/4.5 on one shard's arenas.
2. Directory operations with the adaptive representation and the measured cut-over (start with
   the profile-derived value; record it).
3. Inode slab, monotonic inode numbers, generation on reuse, POSIX attributes with ns timestamps.
4. Content: open extents, page-multiple growth, CoW at chunk granularity, holes, seal into fixed
   chunks (CDC stub disabled until Phase 7).
5. Snapshots and clones by birth epoch with deadlists; destroy as cooperative slices.
6. Accounting: `referenced_bytes`, `unique_bytes`, bounded reservations, dynamic growth policy
   fed by a pluggable pressure source (the real one lands with the daemon in Phase 2).
7. The op log and journal with the bounded retention formula.
8. The executable model (a map with POSIX rules and declared nondeterminism) and proptest
   state-machine tests; the differential harness against tmpfs (Linux) with the equivalence policy.
9. Baselines: create/lookup/readdir/write/read/rename/snapshot/clone/destroy costs versus tree
   size; write amplification per mutation.
10. `slates-base`: the base directory descriptor; listings through the bulk calls with the
    fingerprint-keyed cache; merged and opaque directories; `Body::Base` with descriptors;
    whiteouts and redirects; copy-up with the witness and the small/large class split; the racy
    rule; drift checks on read, on `status`, and on hints; watchers (inotify, FSEvents,
    `ReadDirectoryChangesW`) behind the driver seam with the overflow-to-recheck path; `read_base`,
    `rewitness`, `pin`. The crate's lint wall forbids every write-capable syscall.
11. `slates-land`: the manifest and its canonical hash; the pure verdict function with an
    exhaustive table test; the landing state machine; the per-OS entry writer (temporary, swap,
    verify, undo; reflink where probed; sparse and preallocation; timestamps); ordering by class;
    the online concurrency ramp; sync strategies; advance; crash resume and sibling sweep; the
    stage-and-exchange alternative with its measured break-even; the exchange fallback with the
    reported window. Grants and leases are stubbed by an in-process record in this phase and
    become database records in Phase 2.
12. The landing oracle: an executable model of (disk, overlay, witnesses) whose verdicts and
    post-landing disk state the implementation must match on every generated history, including
    outsider edits injected between validation and write.
13. Baselines: listing cost per directory size per OS; copy-up cost per class; drift check
    cost; landing throughput versus `cp -r` and `git checkout` of the same delta; verify and swap
    cost per entry.
14. Declared operations (D-27): the journal records `{at, len, prev_version}`; per-inode version
    counters; the SDK-level `edit` operation in the in-process API; the deriver (composition by
    interval algebra into the fixed-layer ops document) with its determinism test; the interval
    algebra as a pure module with property tests against a reference applier.

**Worked examples.**
- Snapshot of a volume with 2 million files: one record; the next write to `src/lib.rs` copies
  the nodes from the root to `src` (say 4 nodes, ≈ 3 KiB) and allocates one open extent; the
  snapshot still reads the old bytes.
- Clone then destroy the clone after writing 100 files: destroy walks the clone's deadlist
  (100 chunks and ≈ 400 nodes), releases them, and leaves the origin untouched; `unique_bytes`
  of the origin is unchanged.
- Failure: `link("a", "d/")` where `d` is a directory returns `EPERM`; `rename("d", "d/e")`
  returns `EINVAL`; a bounded volume at its quota refuses a 1-byte append with `ENOSPC` and the
  file's size is unchanged.
- Overlay: create over a 300k-file tree on tmpfs returns in one call with no walk; a lookup of
  `src/lib.rs` loads one listing (one bulk call); a write copies it up (one `fstat`, one read,
  one BLAKE3); an outsider then overwrites the file on tmpfs; `status` lists it as drifted; the
  landing verdict for it is `Conflict(ModifyModify)` and nothing is written.
- Landing: a scratch volume with 1,000 files lands into an empty tmpfs directory; the plan has
  1,000 creates; the writer ramps in-flight depth until throughput plateaus; every file is
  exchanged into place after its data sync; the report lists 1,000 `Written`; a second landing of
  the same snapshot plans 1,000 `Skip` (identical by hash) and writes nothing.

**Acceptance criteria.**
- AC-1.1 The model-based suite passes 10^6 generated operations per CI run with shrinking enabled
  (catches: semantic drift).
- AC-1.2 The differential suite against tmpfs agrees on every abstract state under the reviewed
  policy (catches: POSIX divergence).
- AC-1.3 Snapshot and clone are O(1): measured cost is independent of tree size across 10^3,
  10^5, 10^6 files (catches: hidden copies).
- AC-1.4 Write amplification per mutation is bounded by depth × fanout entries and by one page
  multiple of content (catches: whole-file or whole-directory copies).
- AC-1.5 Memory per file and per directory entry stays within the derived budget (a formula from
  struct sizes and the cut-over), verified on the scale trees (catches: bloat).
- AC-1.6 Inode numbers are never reused within a volume and survive snapshot/clone/re-open of the
  same volume (catches: build-cache breakage).
- AC-1.7 `referenced_bytes` and `unique_bytes` equal the model's exact values after every
  operation (catches: accounting drift).
- AC-1.8 Destroy of a 10^6-file volume runs in bounded slices without stalling other volumes on
  the shard beyond the per-iteration budget (catches: bounded-work violations).
- AC-1.9 Create over a base directory costs one directory open regardless of tree size (10^3,
  10^5, 10^6 entries); memory after create is independent of tree size (catches: hidden walks).
- AC-1.10 The overlay holds exactly the diverged entries: after any generated history, the set
  of overlay entries equals the model's diverged set, and every diverged file carries a witness
  equal to the disk state at its copy-up (catches: over- or under-materialization).
- AC-1.11 Drift is never absorbed: after any outsider edit to a witnessed entry, the agent's
  reads return the agent's bytes and `status` reports the drift; reads of unpinned large-class
  extents after an in-place overwrite return `BaseDrift`, never torn bytes (catches: silent
  adoption; torn reads).
- AC-1.12 The verdict function is pure and matches the table in §4.15 for every generated
  (witness, disk, overlay) triple, including all conflict classes; no landing writes while any
  entry's verdict is `Conflict` (catches: merge creep).
- AC-1.13 Landing atomicity: under the oracle with outsider edits injected at every step and
  crashes injected at every instruction, every entry on disk is old or new, every `Written`
  entry's hash equals the overlay's, every compare-and-swap loss is undone and reported, and a
  re-run is idempotent (catches: torn files; silent overwrites; non-idempotent resume).
- AC-1.14 Landing cost is proportional to the delta: plan time and disk bytes written are
  independent of the base tree size across 10^3 to 10^6 entries (catches: tree walks at
  landing).
- AC-1.15 The deriver is deterministic and exact: the same journal yields a byte-identical ops
  document on every platform, and applying the net op set to the base version reproduces the
  sealed post-state byte for byte on every generated history (catches: split-brain derivation;
  lossy composition).

**Test cases.**
- T-1.1 (property) Rename cycles, rename over non-empty directories, rename of a file over a
  directory; expect the POSIX errno the model predicts and no state change on failure.
- T-1.2 (edge) Names that fold equal under the volume's policy (`README` vs `readme` on a
  case-fold volume; precomposed vs decomposed `é`); expect `EEXIST` and a single entry.
- T-1.3 (edge) Sparse file: write at offset 10 GiB in a dynamic volume; expect one chunk charged,
  holes read as zeros, `referenced_bytes` = one chunk.
- T-1.4 (edge) Truncate to a non-page boundary then extend; expect the tail page zero-filled
  (the clang zero-fill reliance).
- T-1.5 (error) Dynamic volume growth denied by the pressure source; expect `ENOSPC` with a
  pressure event and no partial write.
- T-1.6 (concurrency, shuttle) Two simulated agents interleaving clone-and-write on the same
  base; expect each clone's view independent and the base unchanged.
- T-1.7 (benchmark) 190k-file create burst (a `cargo build` trace replay); expect throughput and
  amplification within ratchets.
- T-1.8 (robustness) Directory with 61,067 entries; expect lookup within the derived bound and
  readdir order canonical.
- T-1.9 (fault) Simulated allocator refusal during a rename; expect the rename fails atomically
  (both names intact) and the refusal is typed.
- T-1.10 (property, base) Random overlay histories over a random base tree with random outsider
  edits; expect the oracle's diverged set, witnesses, drift list and verdicts match after every
  step.
- T-1.11 (edge, base) A base file modified in place within the timestamp granularity without a
  size change (the racy case); expect the racy rule re-hashes and the drift is detected.
- T-1.12 (edge, base) `rm -r` of a 40k-entry base directory then recreate two files inside;
  expect one opaque whiteout, two overlay entries, a merged listing that shows exactly two
  entries, and a landing plan with one recursive removal and two creates.
- T-1.13 (fault, base) Watcher overflow injected (`IN_Q_OVERFLOW`, `MustScanSubDirs`, zero-byte
  `ReadDirectoryChangesW`); expect a full re-check, no missed drift, `status` reporting
  `watcher: overflowed`.
- T-1.14 (concurrency, landing) An outsider thread rewriting target files at random while a
  landing runs; expect every loss detected at the swap, exchanged back, reported
  `Conflict(TargetInUse)`, no outsider write lost, no agent write silently applied over one.
- T-1.15 (fault, landing) Crash at every instruction of the writer under the simulation driver
  and with real `kill -9` on tmpfs; expect old-or-new per entry, a hidden sibling sweep on
  resume, and an idempotent re-run.
- T-1.16 (edge, landing) Target filesystem without exchange (a mocked `EINVAL`/`ENOTSUP`);
  expect the fallback path, the window measured and written into the outcome, and the Degraded
  cell reported.
- T-1.17 (benchmark, landing) Land a 10k-entry delta into a 10^6-entry tree; expect time
  proportional to 10k and within the ratchet versus `cp -r` of the same delta; the ramp's
  settled depth recorded.
- T-1.18 (property, deriver) Random sequences of overwrites, extends, truncates, inserts,
  deletes, renames, creates and unlinks on random files; expect net-apply ≡ raw replay, every
  `src` valid, and composition order-insensitive where the algebra says it is.
- T-1.19 (edge, deriver) A whole-file rewrite by truncate-and-write and by write-and-rename;
  expect both compose to one delete of the base length plus one insert, with identical ops
  documents.

**Exit criteria.** AC-1.* pass; baselines recorded; the equivalence policy document reviewed.

**A-9 required acceptance and regression cases (open; 2026-09-05).**

| Acceptance | Test | Required behavior | Do X, expect Y |
|---|---|---|---|
| AC-1.16 | T-1.20 | Clone preserves delta, witnesses and complete base reference; coverage never overstates immutability. | Clone an overlay with edited and untouched files, mutate the live source, and read both; expect isolated edited bytes, accurate untouched state and explicit live coverage. Capture a quiesced source, change it later, and expect stable complete bytes; refuse atomic capture of an uncontrolled mutable source. |
| AC-1.17 | T-1.21 | Base metadata mutations and digest caches obey the same witness rules as content writes. | Run lookup-before-readdir, chmod, truncate, rename, links, xattrs, unlink-open and watcher overflow against a host oracle; expect correct metadata/bytes, stable witnessed entries, and no stale clean digest. |

### Phase 2 — Local server, database, IPC, anchor process, Rust client

**Goal.** Agents on one machine provision volumes through the ring protocol in under 50 µs
(client spinning), with the catalog, leases, accounting, and op logs surviving daemon crashes
through the anchor process.

**In scope.** `slates-db` partitions (indexes, log, completion records, leases, timing wheel, grants, landing
leases, landing records, the audit log), `slates-ipc` (rendezvous per OS, rings, wake words, completion fds), `slates-server`
(admission, lifecycle verbs, the control shard), the anchor process (`slates anchor`), the Rust
client, `slates` CLI verbs, the `slates.status` data.

**Out of scope.** Bridges (volumes are reachable only through the client API), replication, MCP,
SDKs.

**Ordered tasks.**
1. The anchor: create/attach the shared segment; hold the profile and the op logs; supervise the
   daemon (start, restart on exit, hand back held fds later in Phase 3/4).
2. Partition data model and indexes; the op log in the segment; replay on start; snapshots of
   partition state into the segment on a cadence derived from measured replay throughput and the
   recovery budget.
3. Rendezvous per OS; ring regions; wake words; the completion fd; peer authentication.
4. The lifecycle verbs of Part 4.4 through the server, with completion records (exactly-once),
   leases with epochs, attachments (client form only), quotas fed by the real pressure sources.
5. The Rust client with spin-then-park and request ids; the CLI.
6. The provisioning histogram benchmark and the ratchet; crash-recovery tests.
7. The register protocol at f=0: every head, chain version, lease and catalog entry is a
   register written by its owner under a host epoch; the configuration oracle is the local
   partition with one voter; `await placed(region)` returns after the local append; `placed`
   and `mirror_age` exist in every reply from the first version so the fleet parts of Phase 8
   change no interface.
8. Grants and landings through the server: `GrantRecord`, `LandingLease`, `LandingRecord`, the
   audit log in the segment; the control-channel-only grant kind (refused on the ring); `slates
   land`, `slates grant`, `slates grant --watch`, `slates grants`, `slates audit`; the
   request-then-await flow with deadlines; session grants tied to the client session.

**Worked examples.**
- `slates volume create scratch --bounded 4GiB` from the CLI: rendezvous (one-time), request in
  a slot, reply in a slot; the CLI prints the id and the path it will have once a bridge exists.
- Daemon killed with `kill -9` while 200 clients create volumes: the anchor restarts it; the
  op log replays; clients' retries return their original completion records; no volume is
  duplicated or lost.
- Failure: a client from another uid connects to the rendezvous; `SO_PEERCRED` (or the DACL)
  refuses; nothing is created.

**Acceptance criteria.**
- AC-2.1 Provisioning p99 from the Rust client, spinning, under the ratcheted floor (initially
  50 µs) on the reference Linux and macOS machines; the parked form measured and reported
  separately (catches: hot-path regressions).
- AC-2.2 Zero filesystem entries created by the daemon, anchor, or client during the suite under
  the write tracer, except the documented one-time mount point (Phase 4) and entries inside a
  granted landing target during that landing (catches: hermeticity leaks).
- AC-2.3 Crash-recovery: after `kill -9` at any instruction (simulation and real), every
  acknowledged operation is present and no unacknowledged one is partially present; retried
  requests return original results (catches: durability and idempotency bugs).
- AC-2.4 Leases: a write with a stale epoch is refused and never applied; lease expiry and
  takeover follow the derived term (catches: fencing bugs).
- AC-2.5 The N=1 differential test passes for every verb against the simulated cluster stub
  (catches: modes).
- AC-2.6 Admission: 10,000 concurrent clients on the reference machine stay within the task
  arena and ring budgets; refusals are typed (catches: unbounded growth).
- AC-2.7 Recovery time within the derived budget for a catalog of 10^4 volumes and 10^6 log
  records (catches: slow replay).
- AC-2.8 No grant can be created through the ring or MCP channels: the kind is refused with a
  typed error and an audit counter; a grant created through the CLI is bound to the manifest
  hash and a landing whose plan changed is refused with `GrantMismatch` (catches: agents granting
  themselves; stale approvals).
- AC-2.9 One holder per target: two sessions landing into one directory serialize on the lease;
  a superseded holder's next write is refused at the chokepoint by generation (catches: disk
  split-brain).
- AC-2.10 The audit log replays after `kill -9` with every grant, manifest and outcome present
  and nothing partial (catches: lost accountability).

**Test cases.**
- T-2.1 (error) Create with a name that exists; expect `AlreadyExists` and the original's id in
  the reply.
- T-2.2 (edge) Two clients race to take an expired lease; expect exactly one succeeds with
  epoch+1 and the other gets `LeaseHeld`.
- T-2.3 (fault) Kill the client mid-request; expect the daemon reclaims the region and the
  lease after expiry; other clients unaffected.
- T-2.4 (fault) Kill the daemon between log append and reply; expect the client's retry returns
  the completion record built during replay.
- T-2.5 (chaos) Clock jump on the host; expect leases use monotonic time and no lease is
  wrongly expired or extended.
- T-2.6 (benchmark) The provisioning histogram with 1, 8, 64 concurrent clients; expect the p99
  ratchet holds and the p999 is reported.
- T-2.7 (security) Cross-uid connect; expect refusal and an audit counter.
- T-2.8 (edge, Windows) Client parks; expect the Event wakes it and the socket signals the event
  loop; measure the wake cost and record it.
- T-2.9 (hermeticity) The tracer run, now asserting zero writes outside granted targets.
- T-2.10 (security) An SDK client and an MCP client each attempt to create a grant; expect typed
  refusals and the audit counter.
- T-2.11 (edge) A session grant, then the session ends mid-landing; expect the current entry
  finishes, no further entry starts, `Partial` with the reason, and the grant `Expired`.
- T-2.12 (fault) Kill the daemon between `LandingValidated` and the first write; expect on
  restart the landing is `Aborted` with zero entries written, the lease released, and the grant
  still `Issued` for a re-run bound to the same manifest.

**Exit criteria.** AC-2.* pass on Linux and macOS in CI, Windows nightly; the ratchet recorded.

**A-9 required acceptance and regression cases (open; 2026-09-05).**

| Acceptance | Test | Required behavior | Do X, expect Y |
|---|---|---|---|
| AC-2.11 | T-2.13 | An admitted bounded claim remains spendable despite all competing allocations. | Reserve a claim, then race other shards, dynamic volumes, snapshots, caches, copy-up and pressure for capacity; expect competitors to refuse before stealing it, and every within-entitlement write to succeed. Repeat through resize, cancellation, destroy and restart. |
| AC-2.12 | T-2.14 | Daemon restart preserves acknowledged content and its atomic completion. | Write bytes and metadata, snapshot/clone an overlay, kill the daemon at publication boundaries and retry the same request; expect identical bytes, witnesses, roots, claims and result ids, or an explicit incomplete-recovery refusal where no acknowledgement was promised. |
| AC-2.13 | T-2.15 | Distinct consumers sharing a uid cannot use each other's VFS or grant rights. | Enroll two consumers through a trusted harness, forge/replay ids and channel labels, revoke one and reconnect; expect refusal before any protected lookup, allocation or mutation. A workload invoking the CLI cannot mint grant authority. |

### Phase 3 — Linux bridge (FUSE) and the launcher

**Goal.** Ordinary Linux programs see volumes at `<root>/<volume>` and at chosen paths through
`slates exec`, with kernel caching and explicit invalidation, and pass the conformance and
workload suites.

**In scope.** `slates-bridge-fuse` (own driver, `FUSE_INIT` negotiation, per-shard channels,
io_uring queues on 6.14+, invalidations, splice/registered buffers), root-mount establishment and
fd handoff through the anchor, `slates exec`, the conformance and workload harnesses on Linux.

**Ordered tasks.**
1. The driver: request framing, reply writes, notifications; `FUSE_DEV_IOC_CLONE` per shard;
   the io_uring command path with fallback; negotiation of writeback cache, splice, readdirplus,
   `EXPLICIT_INVAL_DATA`, `EXPIRE_ONLY`, `INC_EPOCH`.
2. Mount establishment with the new mount API when permitted, `fusermount3` otherwise; the fd
   held by the anchor; restart handoff.
3. The `Bridge` trait implementation in the core; inode `(no, gen)`; invalidation on every
   mutation visible through another attachment; `fsync` semantics; `statfs`.
4. `slates exec`: user+mount namespace, recursive-private root, bind mount, exec; AppArmor
   detection and the exact remedy message.
5. Conformance: pjdfstest, fsx, fsstress in CI; xfstests generic and LTP nightly; the reviewed
   expected-failure list.
6. Workloads: git, cargo, npm, python/pytest, rg, rsync, sqlite, editor patterns, inotify.
7. Baselines: per-operation latency versus tmpfs; workload wall times versus tmpfs.
8. Base files through the mount: read-only opens of untouched base files served from the
   daemon's descriptors with one copy into arena pages and splice replies where the kernel
   allows; kernel entry and attribute invalidation on drift reports and watcher hints; the
   landing writer's io_uring linked chains with registered arena buffers; `openat2` containment.

**Worked examples.**
- `slates exec --volume scratch --at /home/u/proj/build -- cargo build`: the launcher creates
  the namespace, binds the volume at `build/`, and execs cargo; cargo sees `target/` inside
  `build/` and writes there; the parent shell's view of `/home/u/proj/build` is unchanged.
- Daemon restart during `git status`: reads return `ENOTCONN` for the restart window; after the
  anchor hands the fd back, the same mount serves again with the same inode numbers.
- Failure: chosen path under a disk-backed parent that does not exist; the launcher refuses with
  the exact missing directory and does not create it.

**Acceptance criteria.**
- AC-3.1 pjdfstest, fsx, and fsstress pass over the mount with the reviewed expected-failure
  list; the list only shrinks (catches: POSIX regressions).
- AC-3.2 Every workload passes with outputs identical to tmpfs and `git status` clean; `cargo
  build` incremental reports "Fresh" (catches: mtime/inode instability).
- AC-3.3 Kernel-cached operations are answered without a daemon round trip (measured: the daemon
  sees no LOOKUP/GETATTR for cached entries), and every mutation invalidates before it is
  acknowledged (a second process never reads stale attributes after the mutating call returns)
  (catches: coherence bugs).
- AC-3.4 Daemon restart with the fd handoff leaves open files usable after the window; the
  window is measured and within the recovery budget (catches: lost mounts).
- AC-3.5 The launcher creates no filesystem entries; `slates exec` refuses unsatisfiable chosen
  paths with the exact reason (catches: silent disk writes).
- AC-3.6 io_uring and `/dev/fuse` read/write paths produce identical results under the same suites
  (catches: transport divergence).
- AC-3.7 Per-operation bridge latency and workload wall time recorded with ratchets versus tmpfs
  (catches: regressions).
- AC-3.8 Overlay workloads: `git status`, `cargo build` and `pytest` run inside an overlay
  volume over a real checkout with outputs identical to running on the checkout itself, and a
  concurrent host-side `git checkout` of another branch is reported as drift without corrupting
  any agent read (catches: coherence between disk and overlay).
- AC-3.9 Base reads through the mount are byte-identical to reading the backing file directly
  under fsx and the workloads, cost one copy per read, and stay within the ratchet versus tmpfs;
  the daemon never requests `CAP_SYS_ADMIN` or any other capability (catches: divergence
  between the base read path and the host; privilege creep).

**Test cases.**
- T-3.1 (conformance) The three CI suites; nightly xfstests generic `rw`, `mmap`, `stress`
  groups; LTP `fs` and `syscalls` subsets.
- T-3.2 (workload) `npm install` from the vendored registry (tens of thousands of files,
  symlinks); expect identical tree and `npm ls` output.
- T-3.3 (workload) sqlite WAL-mode database inside the volume with two processes; expect
  correct results and no corruption (locks through FUSE).
- T-3.4 (edge) mmap a file shared between two processes on the host; expect coherent writes
  (writeback cache, no direct_io).
- T-3.5 (fault) Kill the daemon while a process holds an open file and is writing; expect
  `ENOTCONN` errors during the window, then recovery with every acknowledged write present.
- T-3.6 (error) `allow_other` requested without `user_allow_other`; expect a typed refusal
  naming the config line.
- T-3.7 (chaos) Unmount under load (`umount -l` by an operator); expect typed errors to clients
  and clean re-establishment.
- T-3.8 (benchmark) LOOKUP/GETATTR/READ 64 KiB/WRITE 64 KiB/CREATE latency versus tmpfs; git,
  cargo, npm, pytest wall times versus tmpfs.
- T-3.9 (edge) 100k-entry directory listing with readdirplus; expect one round of requests
  bounded by the derived batch size.
- T-3.10 (workload, overlay) `cargo build` in an overlay over a real repository, then `git
  status` on the host; expect the host tree unchanged (zero writes by the tracer) and the volume
  holding every artifact.
- T-3.11 (concurrency, overlay) An editor process saving a witnessed file on the host every
  100 ms while the agent reads it through the mount; expect the agent's bytes stable, drift
  reported within the measured watcher latency, and kernel attributes invalidated before the
  report.
- T-3.12 (fault, landing) Land through the CLI while `git checkout` runs on the host; expect
  every collision reported as `Conflict(TargetInUse)`, the rest `Written`, and the git tree
  consistent afterwards.

**Exit criteria.** AC-3.* pass on the reference Linux kernels (baseline 5.10, io_uring 6.14+).

**A-9 required acceptance and regression cases (open; 2026-09-05).**

| Acceptance | Test | Required behavior | Do X, expect Y |
|---|---|---|---|
| AC-3.10 | T-3.13 | Negotiated FUSE features have correct ABI values and complete mounted semantics. | Drive independent kernel vectors and real mounts through READDIRPLUS, FSYNC, LINK, all setattr fields, rename flags and statfs; expect actual effects/capacity or precise unsupported errors, never ignored success. |
| AC-3.11 | T-3.14 | Snapshot/submit/detach barriers order kernel-buffered writes and view changes. | Race kernel writes and shared mappings with SDK mutations, snapshot and advance; expect every acknowledged included write in the published generation, no mixed-version view, and a typed incomplete barrier on consumer loss. |
| AC-3.12 | T-3.15 | Attachment handles and helper processes have bounded lifetimes on every exit. | Repeat open/close beyond arena capacity in total operations, then fail the mount descriptor handshake and cancel attach; expect reusable generational slots, bounded memory, no orphan child and no phantom attached path. |

### Phase 4 — macOS bridges (FSKit first, NFSv3 fallback) and Windows bridge (WinFsp); platform matrix

**Goal.** The same behaviour on macOS and Windows, with each platform's documented Degraded
cells and its measured parameters; the FSKit spike decides the macOS 15.x path.

**In scope.** The FSKit spike (go/no-go); `slates-bridge-fskit` (the Swift extension shim, the
forwarding and in-process forms, URL resources, the Operations and Handler protocol generations
behind one shim interface, the `Slates.app` bundle with signing, entitlement and app group);
`slates-bridge-nfs` (ONC RPC, XDR, NFSv3, MOUNT, portmap responder, file handles, inode GC,
attribute-timeout derivation, directory revalidation) as the fallback and oracle; root mounts and
chosen-path forms on macOS; `slates-bridge-winfsp` (thin binding, drive-letter volume,
notifications, request routing by handle, asynchronous completion), the second-drive-letter
form; the conformance and workload suites on both platforms.

**Ordered tasks.**
1. FSKit spike (research §8): build the minimal extension against both protocol generations;
   measure per-operation latency in the forwarding and in-process forms on macOS 26 (URL
   resource) and on 15.x (RAM-disk block resource); establish cache and invalidation behaviour
   for SDK-ring writes, `reclaimItem` under pressure, and snapshot swaps; run fsx and the
   NFS-trimmed pjdfstest; confirm non-root `mount -F` and unprivileged `hdiutil ram://`; record
   the shim overhead. Go criteria as written in the research note; record the verdict and the
   15.x path in the gap ledger.
2. FSKit module: the shim over the bridge trait; app-group-prefixed ring names; `Slates.app`
   packaging (daemon, CLI launcher, extension), signing with the FSKit entitlement, the one-time
   enablement flow with the exact System Settings path in every refusal; per-volume URL mounts;
   chosen-path mounts at user-owned directories.
3. NFS fallback: RPC record marking on the loopback listener held by the anchor; XDR codec from a
   table; NFSv3 procedures; MOUNT; portmap responder; handles `(volume, no, gen)`; readdirplus;
   mount options from the profile-derived timeouts; the chmod-revalidation after snapshot swaps;
   inode GC; the differential harness that mounts one volume through FSKit and NFS and compares
   abstract states.
4. WinFsp binding over `winfsp-sys`; volume creation with `FileInfoTimeout = -1`;
   `FspFileSystemNotify`; routing to shards; asynchronous completion; drive-letter allocation and
   the second-letter form.
5. Suites on both OSes (pjdfstest with the NFS-trimmed set, fsx, the workloads on macOS through
   both bridges; the WinFsp fsx port and WinFsp's own tests on Windows); measured coherence
   parameters; failure-behaviour tests and the documented Degraded cells.
6. Base and landing on macOS and Windows: base reads through the FSKit module, the NFS fallback
   and the WinFsp binding; `getattrlistbulk` and `NtQueryDirectoryFile` listings; FSEvents and
   `ReadDirectoryChangesW` watchers with the overflow path; the landing writer's macOS pool
   (`renamex_np`, `F_BARRIERFSYNC`/`F_FULLFSYNC`, `clonefile`) and Windows path (share-mode
   validation, overlapped writes, `FileRenameInfoEx` with POSIX semantics, `FlushFileBuffers`,
   `FSCTL_DUPLICATE_EXTENTS_TO_FILE` where probed); capability probing for swap and clone per
   target volume; the terminal confirmation surface on both OSes.

**Worked examples.**
- On macOS 26, `attach(volume, at="~/proj/build")` mounts `slates://volume/7/attach/3` through
  the FSKit module at `~/proj/build`; `cargo build` in a terminal sees an ordinary directory;
  Finder shows the volume; xattrs and hard links work; `detach` unmounts.
- On an older macOS host, no spike outcome is assumed. A requested FSKit form either has
  recorded support and an already authorized suitable resource or refuses. A separately
  selected limited NFS form reports its semantics; it is not an automatic POSIX substitute.
- Failure: the FSKit extension is not enabled; `attach` refuses with
  `Unsupported{macos, fskit_disabled}` naming the System Settings path; it does not change
  the requested attachment form or write to disk.

**Acceptance criteria.**
- AC-4.1 The spike's go/no-go is recorded with numbers; if go, FSKit per-operation latency is
  within the recorded factor of the NFS path or better and the SDK-ring write coherence test
  passes (catches: an unmeasured bet).
- AC-4.2 macOS FSKit: fsx and the full applicable pjdfstest set pass, with only reviewed
  platform-standard differences; the NFS-trimmed set is not its acceptance scope. Workloads pass with outputs
  identical to an APFS RAM disk; two local processes observe each other's writes without a
  remount; xattr and hard-link workloads pass (catches: coherence and conformance).
- AC-4.3 macOS NFS fallback: the same suites pass with the fallback's reviewed expected-failure
  list; the FSKit-versus-NFS differential harness agrees on every abstract state (catches:
  bridge divergence).
- AC-4.4 Windows: fsx (WinFsp port) and WinFsp's tests pass; workloads pass identical to an NTFS
  RAM VHD; notifications keep Explorer and watchers current (catches: same).
- AC-4.5 No disk writes by slates on either platform outside a granted landing target during
  that landing; missing mount-point directories are refused, never a one-time exception,
  verified by the tracer (catches: hermeticity).
- AC-4.6 The chosen-path forms (FSKit URL mount, NFS second mount, Windows second letter) work
  and refusals are typed (catches: silent fallbacks).
- AC-4.7 The full nine-target matrix builds and passes unit, conformance, and workload suites
  nightly; i686 included (catches: platform rot).
- AC-4.8 Documented Degraded behaviours occur exactly as documented under daemon kill and
  extension kill on each platform (catches: undocumented behaviour).
- AC-4.9 `Slates.app` is signed, notarized, and installs the extension with the entitlement; the
  ring region is shared through the app group without any filesystem entry (catches: packaging
  and sandbox surprises).
- AC-4.10 The landing oracle passes on APFS (RAM disk) and NTFS (RAM VHD) with the platform
  primitives: swap-and-verify on macOS, share-mode-guarded replace on Windows, old-or-new per
  entry under crash injection, and every outsider collision reported (catches: platform
  divergence in the one path that writes disk).

**Test cases.**
- T-4.1 (spike) Per-operation latency table for FSKit forwarding, FSKit in-process, NFS
  fallback, and APFS RAM disk; expect the table recorded with the profile.
- T-4.2 (edge, macOS FSKit) Write through the SDK ring while a shell holds the file open through
  the mount; expect the shell's next read sees the new bytes.
- T-4.3 (fault, macOS FSKit) Kill the daemon during a read; expect typed errors, extension
  survival, and recovery after the anchor restarts the daemon; kill the extension; expect the
  volume to unmount and re-mount with state intact.
- T-4.4 (edge, macOS NFS) Delete a file another process holds open; expect the `.nfs` temporary
  file behaviour documented and cleaned on close.
- T-4.5 (fault, macOS NFS) Kill the daemon during a read on a soft mount; expect `EIO` after the
  derived `retrans`; on a hard mount expect the read to complete after restart.
- T-4.6 (edge, Windows) Case-insensitive lookup on a case-fold volume; expect one entry and
  `EEXIST` on a folded duplicate.
- T-4.7 (fault, Windows) Kill the daemon with open handles; expect the volume to disappear, the
  anchor to re-create it, and handles to error as documented.
- T-4.8 (benchmark) Per-operation latency versus the RAM disk / RAM VHD on each platform.
- T-4.9 (chaos, macOS) Sleep/wake the machine with an attached volume; expect reconnection and
  valid handles on both bridges.
- T-4.10 (packaging) Fresh macOS machine: install the bundle, enable the extension, attach, run
  `git` and `cargo`; expect success and no disk writes by slates.
- T-4.11 (edge, Windows) An editor holds a target file open without `FILE_SHARE_DELETE` during a
  landing; expect `Conflict(TargetInUse)` for that entry before any write, the rest `Written`.
- T-4.12 (edge, macOS) A target on a volume without `VOL_CAP_INT_RENAME_SWAP` (a mocked
  capability); expect the fallback path with the reported window; on APFS expect the swap path.

**Exit criteria.** AC-4.* pass; the platform matrix job green nightly; the spike verdict and the
15.x path recorded in the gap ledger.

**A-9 required acceptance and regression cases (open; 2026-09-05).**

| Acceptance | Test | Required behavior | Do X, expect Y |
|---|---|---|---|
| AC-4.11 | T-4.13 | OCI containers and Linux guests consume real authorized attachments, with virtio-fs for guests. | Attach a host volume into an OCI namespace and export another through the supported VMM seam; run the same filesystem workload inside both and on the host. Expect byte/metadata agreement, isolated edits and typed refusals for unavailable path/device capabilities. |
| AC-4.12 | T-4.14 | Guest queues and any offered DAX mappings preserve bounds, rights and version lifetime. | Supply malformed descriptor chains, overflow lengths and unauthorized adjacent-page ranges; revoke/advance while requests and mappings are active. Expect refusal before access, no writable immutable mapping, no neighboring-byte exposure and eventual reclamation. Do not advertise DAX without this gate. |

### Phase 5 — SDKs, MCP server, skills, CLI polish

**Goal.** Agents integrate through async Python and TypeScript SDKs, an MCP server, and skills
published three ways, all speaking the same ring protocol and the same vocabulary.

**In scope.** `slates-sdk-py` (PyO3, `abi3-py312` and `cp314t` wheels, fd completion, sync
facade, stubs), `slates-sdk-node` (napi-rs, platform packages, `uv_poll` completion, external
buffers), `slates mcp` (own server, stdio and loopback HTTP, dual-era), the
skills source tree and `slates skills install`, `slates mcp install`, the Claude Code plugin
package, examples in three languages.

**Ordered tasks.**
1. Python: the extension over the Rust client; `add_reader` completion (socket on Windows);
   buffer-protocol reads; the sync facade; stubs; maturin builds for the wheel matrix; free-
   threaded build with `gil_used = false` and thread-safe `#[pyclass]` state.
2. TypeScript: the addon; `uv_poll` on the completion fd/socket; external buffers with copy
   fallback; async iterators and `AbortSignal`; platform packages over the same typed client.
3. MCP: the stateless protocol with dual-era support; tool catalog with annotations and
   structured content; resources and prompts for skills; `subscriptions/listen` for volume
   events; conformance runs against both requirement sets; `slates mcp install`.
4. Skills: author `slates-volumes`, `slates-attach`, `slates-archive`, `slates-fs`,
   `slates-landing` (overlay volumes, drift, `read_base`/`rewitness`/`pin`, how to request a landing
   and what a grant is, why the agent cannot grant), `slates-troubleshooting` under the spec's
   limits; validate with the reference validator; installer and plugin packaging.
5. Landing surfaces: `slates.base` and `slates.land` tools; `materialize` in both SDKs as an
   awaitable request; the harness confirmation surface contract (a request stream the harness
   can render, answered only through the control channel by a human-operated process) with the
   terminal surface as the reference implementation; conformance that the grant kind is absent
   from every agent-facing schema.
6. API conventions: the error taxonomy in all three languages; examples; docs.

**Worked examples.**
- Python: `async with slates.connect() as c: v = await c.create("scratch", bounded="4GiB");
  a = await c.attach(v, at="/home/u/proj/build"); async for entry in c.list(v, "/"): ...`.
- TypeScript: `const v = await client.create("scratch", { bounded: "4GiB" }); const stream =
  client.archive(v).export(); for await (const chunk of stream) sink.write(chunk);`.
- MCP: an agent calls `slates.volume` with `{action: "create", name: "scratch", bounded:
  "4GiB"}` and receives `structuredContent: {volume_id, path, granted_form}`; it then reads
  `skill://slates/slates-attach/SKILL.md` to learn the chosen-path rules.
- Landing from Python: `req = await c.materialize(snap, "/home/u/proj", exclude=["target/"])`
  returns at once with `req.manifest_hash` and `req.summary`; `report = await req` resolves when
  the human has run `slates grant` (or answered the harness surface) and the landing finished;
  `report.entries` lists every outcome; if the human declines, `await req` raises
  `GrantRefused`.
- Failure: a legacy MCP client sends `initialize`; the dual-era server answers with the legacy
  lifecycle; a modern client gets `server/discover`; a client with neither is refused with the
  spec's error code.

**Acceptance criteria.**
- AC-5.1 Provisioning p99 from the Python and TypeScript SDKs (spinning) stays within the
  ratchet plus the measured interpreter overhead, reported per language (catches: SDK overhead).
- AC-5.2 The SDKs work with asyncio, uvloop, trio/anyio (Python) and with Node's loop, Bun, and
  Deno where Node-API is supported (TypeScript), verified by the example programs (catches:
  event-loop coupling).
- AC-5.3 The MCP conformance suite passes for `2025-11-25` and `2026-07-28` requirement sets and
  the `server-stateless` scenario (catches: protocol drift).
- AC-5.4 Skills validate with the reference validator; the same body is byte-identical raw, as a
  resource, and as a prompt (catches: divergence between surfaces).
- AC-5.5 Every error in every SDK carries the taxonomy code, the errno where applicable, and the
  request id (catches: opaque errors).
- AC-5.6 The Claude Code plugin installs the MCP server and skills with `--plugin-dir` and passes
  `claude plugin validate` (catches: packaging rot).
- AC-5.7 The grant kind does not exist in any SDK method, MCP tool schema, or skill; a fuzzed
  MCP client cannot reach a grant by any tool call (catches: the agent answering its own
  question).
- AC-5.8 An agent transcript that creates an overlay volume, edits, requests a landing, receives
  a conflict, reads the base, rewitnesses, and lands succeeds end to end through each SDK and MCP,
  with the human's grants issued through the CLI (catches: broken landing ergonomics).

**Test cases.**
- T-5.1 (error) Python: `create` on a stopped daemon; expect `DaemonUnavailable` with the endpoint
  and no side effects.
- T-5.2 (edge) Python free-threaded build: 32 threads creating volumes concurrently; expect no
  borrow panics and correct results.
- T-5.3 (edge, Windows) asyncio ProactorEventLoop; expect the socket completion path works.
- T-5.4 (edge) TypeScript in Electron (no external buffers); expect the copy fallback.
- T-5.5 (fault) Cancel an `AbortSignal` mid-archive-export; expect the stream ends with a
  terminal completion and the daemon frees the export state.
- T-5.6 (conformance) The MCP suite in CI.
- T-5.7 (workload) An agent transcript replay: create, attach, run cargo through `slates exec`,
  snapshot, clone, archive, destroy; expect every step's structured result and the final state.
- T-5.8 (benchmark) SDK call overhead per language versus the Rust client.
- T-5.9 (edge) `materialize(..., wait=False)` without a grant; expect `GrantRequired` with the
  request id and manifest hash and no disk write.
- T-5.10 (workload) The rewitness transcript of AC-5.8 replayed in Python, TypeScript and MCP;
  expect identical reports.

**Exit criteria.** AC-5.* pass; packages published to a staging registry from the release
workflow dry run.

**A-9 required acceptance and regression cases (open; 2026-09-05).**

| Acceptance | Test | Required behavior | Do X, expect Y |
|---|---|---|---|
| AC-5.9 | T-5.11 | CLI/MCP/SDK adapters conform to one operation descriptor and are navigable. | Execute the same create/clone/base/attach/status/resize flows through each adapter, using ids and scoped names; expect equivalent results/refusals, useful help, stable JSON, bounded cursors and cancellation. |
| AC-5.10 | T-5.12 | Human grants are authenticated and bound to the exact proposed landing. | Preview a plan, change its filter/target/content, replay or forge approval from an agent channel; expect refusal. Approve the unchanged manifest through the enrolled human surface and expect only its granted effects and audit records. |
| AC-5.11 | T-5.13 | Users can discover attachment support and complete supported flows without hidden setup. | Start from an enrolled instance on each host, follow CLI help through a live overlay and an immutable-base/guest flow; expect endpoint/root discovery, actual path/tag readiness, clear coverage/reservation/durability output and actionable capability errors. |

### Phase 6 — Merge engine: green volumes, increments, canonical rebase, the verdict (single node)

**Goal.** Many agents on one machine merge their work into shared green volumes through the
engine of §4.16: declared operations composed into constant-size increments, position-mapped
through canonical deltas, decided by the pure two-pass verdict, spliced by extent surgery,
committed as pointers, with byte-exact conflict windows and rebase as the corrective path;
tested against an oracle and ratcheted on latency.

**In scope.** `slates-merge` (the chain, canonical deltas and checkpoints, the last-changed
index, the position mapper, the two-pass verdict with the purity lint, the splice, the merge
task, deduplication, rebase, `advance` with targeted invalidations); the `Green` and `Work`
roles and the merge verbs through the server, the CLI, both SDKs and MCP (`slates.merge`,
`edit`); the merge oracle; the skills `slates-merge`; single-node placement (the owner is the
holder).

**Out of scope.** Fleet commit of merge records through the consensus group, placed-before-
committed across hosts, holder recomputation on other nodes, submission across hosts (Phase 8).

**Ordered tasks.**
1. The chain: `VersionRecord`s, the origin version from a snapshot, the last-changed index,
   `versions` and `changed_since`; `Green` mounts answer `EROFS`; SDK writes to green refuse
   `ReadOnlyVolume`.
2. Canonical deltas: the ops document as a sealed chunk in the fixed-layer form (`OpRecord`, 32
   bytes, little-endian, cast in place with bounds checks); effect ranges per path; retention
   and folding into checkpoint deltas by composition; the M4 composition test.
3. The position mapper: size shifts, rename remapping, composition; property tests against a
   reference mapper on random delta chains.
4. The verdict: pass one (sweep line per path), the candidate list, the fetch seam, pass two
   (memcmp); every conflict class; the purity lint (no I/O, clock, randomness, or system
   allocation in either pass) and its architecture test; the fast path on the last-changed
   index with an instrumented counter proving zero range work on the common path.
5. The splice: extent-list surgery with sub-chunk references; directory operations; the
   version snapshot; cross-shard chunk acquisition in one batched message; manifest identity
   by the background hasher or at once when requested.
6. The merge task and the submission transaction on one node: `submit` (seal, compose, send,
   park, resume), deduplication by increment identity with completion records, `stream`
   submissions at auto-seal, `require_evidence`, the merge record as a partition log append,
   crash recovery of the chain and `seen` from the log.
7. Rebase: mapping the pending operations, re-basing by extent surgery, windows on conflict;
   `advance` with manifest-diff invalidations through every bridge.
8. Surfaces: `slates.merge`, the SDK verbs, `edit`, the CLI verbs, the `slates-merge` skill
   (what an increment is, why whole-file rewrites conflict, how to read a window and rebase).
9. The merge oracle: an executable model of (chain, work volumes, declared operations) whose
   verdicts and post-versions the engine must match on every generated history, including
   whole-file rewrites, renames, links, symlinks, mode changes, and adversarial interleavings.
10. Baselines and ratchets: verdict µs p99 per increment size; merge-path p99 (seal to reply)
    on a laptop; merges per second per green; rebase cost; `advance` invalidation cost.

**Worked examples.**
- The three-agent example of §4.16: A and B accept on the fast path; C conflicts with a
  byte-exact window, rebases, and lands as version 45; the chain shows 42 → 45 with one
  increment per version and the ops documents retained as canonical deltas.
- Streaming: a work volume with `stream = true` under a build-and-edit loop submits at every
  auto-seal; the base lag stays at one or two versions; the fast path accepts most increments;
  `status` on another agent's work volume reports the paths green moved under it before that
  agent submits.
- Failure: an agent submits an increment whose ops document exceeds the derived size budget
  (a generated 2 GiB file rewritten whole); the reply is `IncrementTooLarge{limit}`; the SDK
  resubmits per path; nothing was applied.

**Acceptance criteria.**
- AC-6.1 Verdict purity: for every generated (chain, increment) pair the verdict is a function
  of its inputs only, identical across platforms and across runs, with zero false accepts and
  zero false rejects against the oracle (catches: inference or nondeterminism creeping in).
- AC-6.2 No silent interleave: every overlap class ends in `Conflict` with byte-exact windows;
  no accepted version contains bytes that neither side wrote (catches: merge invention).
- AC-6.3 Identical concurrent edits cost one range compare on the overlap path and zero content
  reads on the common path, proven by instrumented counters (catches: false conflicts; lazy
  check regressions).
- AC-6.4 Composition: `map(a..c) = map(b..c) ∘ map(a..b)` holds on random chains, and mapping
  through folded checkpoints equals mapping through the raw deltas (catches: lossy folding).
- AC-6.5 Increments are constant-size descriptors: no byte-bearing field compiles in any
  increment type; message size is invariant in op count and content size (catches: content on
  the control plane).
- AC-6.6 Splice ≡ reference applier: the spliced version equals the oracle's byte for byte and
  copies no chunk bytes (allocation and memcpy counters prove it) (catches: hidden copies; lost
  bytes).
- AC-6.7 Apply and record are atomic: a crash at any instruction between splice and reply
  leaves the chain with either the version and its record or neither; a retry returns the
  original result (catches: dangling heads; duplicate versions).
- AC-6.8 Green is written by nothing but the merge task: mounts answer `EROFS`, SDK writes
  refuse, and an attachment's view never changes without `advance`, fuzzed across concurrent
  merges (catches: ambient writes; ground shifting under mounts).
- AC-6.9 Rebase is exact: a rebased work volume equals the oracle's result and its next
  increment has a new identity; conflicts leave the work volume unchanged (catches: rebase
  corruption).
- AC-6.10 The deriver never diffs: an architecture test proves no code path compares file
  states to produce operations; whole-file tool rewrites produce one delete plus one insert
  (catches: the banned inference returning).
- AC-6.11 Merge-path latency: verdict p99 and seal-to-reply p99 on the reference laptop are
  recorded and ratcheted; the fast path stays within one ring round trip plus the seal
  (catches: latency decay).
- AC-6.12 The N=1 differential test covers every merge verb (catches: modes).

**Test cases.**
- T-6.1 (property) The merge oracle over random chains and increments; expect AC-6.1 and AC-6.2.
- T-6.2 (property) Random delta chains; expect the composition law and checkpoint equivalence.
- T-6.3 (edge) Rename matrix: rename against edit under the old and the new name, rename
  against rename, rename of a directory against a create beneath it, rename against delete;
  expect the oracle's verdict in every cell.
- T-6.4 (edge) Hard links and symlinks: an edit through one name and a rename of the other;
  a symlink retarget against an edit of the target; expect conservative per-path verdicts.
- T-6.5 (edge) Mode and xattr changes on both sides, same and differing; expect
  `AcceptIdentical` and `MetaMeta`.
- T-6.6 (edge) An edit anchored inside an intervening delete; expect `AnchoredInDelete` with
  the window naming the deleting version.
- T-6.7 (concurrency, shuttle) Sixteen simulated agents submitting to one green with random
  overlaps; expect linearizable version order, every conflict reported, no lost accepted
  operation, and the fast-path counter matching the oracle's disjoint count.
- T-6.8 (fault) Crash the daemon at every instruction of the merge task under the simulation
  driver; expect AC-6.7 and the chain replaying to the same head.
- T-6.9 (error) Submit with a base from another green, an unknown version, a missing evidence
  reference on a `require_evidence` green, and an oversized increment; expect the typed
  refusals and no state change.
- T-6.10 (edge) Two work volumes rewrite the same file whole with identical bytes; expect one
  range compare and `AcceptIdentical`.
- T-6.11 (workload) Three agents running `cargo build` and editing in work volumes with
  `target/` excluded, streaming submissions, and a reader attached to green advancing every
  version; expect the reader's incremental build to stay "Fresh" except for the files that
  changed, and the ops documents to contain no `target/` paths.
- T-6.12 (hostile) Ops documents with out-of-range `path_idx`, overlapping records, `src`
  beyond the post-state, and truncated headers; expect typed refusals, no panic, no
  over-allocation.
- T-6.13 (benchmark) Verdict cost versus increment size and intervening delta count; merges per
  second per green; seal-to-reply p99; rebase cost versus pending operations.
- T-6.14 (architecture) The purity lint and the never-diff test fail the build when violated.

**Exit criteria.** AC-6.* pass on Linux and macOS in CI and on Windows nightly; ratchets
recorded; the merge oracle runs in CI with shrinking.

**Risks and fallbacks.** Whole-file rewrites by tools inflate the conflict rate on shared files:
measured by source, mitigated by `edit` in the skills and by streaming; a high measured rate
reopens the ergonomics (D-O17). The size budget for increments is derived from measured cost;
if real increments routinely exceed it, the SDK's per-path split is the fallback.

**A-9 required acceptance and regression cases (open; 2026-09-05).**

| Acceptance | Test | Required behavior | Do X, expect Y |
|---|---|---|---|
| AC-6.13 | T-6.15 | Work/Green roles, barriers and the declared-operation merge core form one usable service. | Use CLI/MCP work clones from a complete base, mutate via a mounted client, submit, attach and advance a green reader; expect role refusals, accept/identical/conflict behavior, fixed reader versions and all committed input bytes retained. |

### Phase 7 — Archive, compression, deduplication, dictionaries

**Goal.** Volumes can be sealed, deduplicated, compressed by a measured cost model, archived in
RAM, exported as a verifiable stream, and restored lazily.

**In scope.** BLAKE3 identity pass at seal (background), the content index, dedup by identity,
CDC for the measured large-file class, zstd/LZ4 with static contexts, dictionary training and
lifecycle, the cost model calibrated by the profile and updated online, the archive format, the
export stream through the SDKs/MCP, restore.

**Ordered tasks.**
1. Identity pass: hash sealed chunks in the background on the owner's idle time; content index
   per shard partitioned by hash prefix; dedup by reference with accounting updates.
2. CDC (FastCDC) with parameters derived from the measured file-size distribution; enabled only
   for the class where measured gain pays.
3. Codecs with static contexts carved from arenas; the cost model; the Btrfs-style sampler; the
   LZ4 probe; the regression; per-volume observation and pressure inputs.
4. Dictionaries: sampling, FastCover training, held-out acceptance, identity, embedding, GC.
5. Archive writer/reader; seek table; trailer; verification; resumable transfer by missing set;
   restore with lazy decompression.
6. Baselines: hash and codec throughput versus the profile; ratios per corpus; archive and restore
   throughput.

**Worked examples.**
- Two clones of a 5 GiB monorepo checkout each built once: after the identity pass, identical
  `target/` outputs dedup to one copy; `unique_bytes` of each clone shrinks to its differing
  artifacts while `referenced_bytes` is unchanged.
- Archiving an idle clone under memory pressure: the cost model picks zstd level 9 for source
  chunks with the `rust-source` dictionary and leaves already-compressed `.rlib` chunks raw
  (the probe shows no gain); the manifest stays uncompressed; the archive is exported to an
  agent-chosen sink at the measured throughput.
- Failure: a restore from a stream with one corrupted chunk: that chunk is refused with
  `IdentityMismatch` and the rest of the volume restores; reads of the affected file return
  `EIO` for that range with the identity in the error.

**Acceptance criteria.**
- AC-7.1 Hashing never runs on the write path (the write-path instruction count is unchanged
  with the identity pass enabled) (catches: latency regression).
- AC-7.2 Dedup and compression decisions are traceable: every chunk records the inputs and the
  decision; the fixed-threshold comparison shows the cost model never chooses a net loss
  (catches: mis-calibration).
- AC-7.3 Archives verify per chunk and whole; a single-bit flip anywhere is detected and named
  (catches: silent corruption).
- AC-7.4 Restore is lazy: attach after restore completes in the provisioning budget and content
  decompresses on first read (catches: eager materialization).
- AC-7.5 Dictionaries are accepted only when the held-out test proves a gain; archives referencing
  a dictionary are self-contained (catches: undecodable archives).
- AC-7.6 Ratios and throughput recorded per corpus with ratchets (catches: regressions).

**Test cases.**
- T-7.1 (property) Random files, random edits, seal, dedup; expect byte-exact reads and exact
  accounting.
- T-7.2 (edge) All-zero 1 GiB file; expect holes, no chunks, no compression work.
- T-7.3 (edge) Already-compressed corpus (`.png`, `.gz`); expect raw storage chosen by the probe.
- T-7.4 (hostile) Archive fuzzing: headers, seek tables, chunk records, dictionaries; expect typed
  refusals, bounded memory (window log capped).
- T-7.5 (fault) Kill the daemon mid-archive; expect the partial archive state discarded, the
  source volume intact, and the operation resumable by missing set.
- T-7.6 (benchmark) Hash/codec throughput versus the profile; archive of a 10 GiB volume; restore
  and first-read latency.
- T-7.7 (chaos) Memory pressure during the identity pass; expect the pass yields, dedup is
  deferred, and no refusal to foreground operations beyond the documented Degraded cell.

**Exit criteria.** AC-7.* pass; the cost model's inputs are exported in `slates status`.

**A-9 required acceptance and regression cases (open; 2026-09-05).**

| Acceptance | Test | Required behavior | Do X, expect Y |
|---|---|---|---|
| AC-7.7 | T-7.8 | Resumable transfer verifies and bounds every byte before publication. | Interrupt/cancel named-object and unknown-length ingest, send duplicate/corrupt/oversized ranges and resume; expect bounded memory and sessions, missing-set progress, no unverified identity or partial publication, and release of abandoned charges. |

### Phase 8 — Distribution: membership, the configuration group, neighbourhoods, the register protocol, hedged placement, takeover, migration, mirroring

**Goal.** The same daemon on many nodes across regions: sealed content and records placed to
candidate holders under one quorum rule with hedged puts, fenced by host epochs; configuration
through a regional consensus group and a root group; takeover by epoch bump and batched
promotion without eager whole-tree transfer, with fetched content verified before reads;
ownership following the writer; mirroring
across regions with a per-operation durability scope; remote attach by id routing with lazy
fetch and learned prefetch; verified through message-level simulation and real processes.
Historical models are architecture evidence with A-9 refinement/revalidation still owed under
separate tooling authorization; no checker is added to the implementation or CI.

**In scope.** `slates-cluster` (SWIM/Lifeguard, the configuration group's state machine and
its root counterpart, neighbourhoods with derived scatter width, rendezvous within
neighbourhoods, the register and content puts with hedged and tied requests and recorded holder
sets, the healer and probation, promotion and takeover, joint writes during neighbourhood
changes, id routing with the configuration version and the piggyback rule, ownership migration,
mirroring, missing-set transfer, Merkle anti-entropy, auto-seal scheduling, live shipping), the
Raft core with its executable conformance suite for the configuration group, wire security
between hosts, the simulation cluster harness and nemesis library, the linearizability and Elle
checkers.

**Ordered tasks.**
1. The Raft core (pure state machine; PreVote/CheckQuorum; joint consensus; ReadIndex) and its
   bug-record conformance suite; the configuration state machine (members, neighbourhoods, host
   epochs, takeovers, homes) and the root group; the configuration version on every request.
2. Membership: SWIM with Lifeguard and derived parameters; peer confirmation; failure-domain
   tree configuration; the membership lease as the owner's authority.
3. Neighbourhoods: the scatter-width derivation from measured re-replication bandwidth and the
   accepted loss probability; assignment across failure domains; the copyset count check at
   every configuration change; rendezvous within a neighbourhood; add-before-remove.
4. The register protocol: `put_wal` in the anchor segment; records to all 2f+1 candidates with
   commit at f+1; content to f+1 with hedged and tied requests to the rest after the measured
   p95 and commit at f+1; the acknowledging set in the head record; holder-side epoch fences;
   `StaleEpoch`; the healer and probation; missing-set transfer; any-holder reads verified by
   identity; head reads at the owner under its lease or from f+1 candidates.
5. Promotion and takeover: epoch bump by the group; assignment to the first-ranked surviving
   candidate; batched phase one per register class; adoption; background re-replication; a
   resumed stale owner refused; measured takeover time against the recovery budget.
6. Neighbourhood changes with joint writes and the retirement rule; the restart-identity
   invariant as a structural test.
7. Id routing: creator host in the id; successor and moved-object generation from the
   configuration; bounded refresh/retry on `ConfigurationStale`; scatter-gather enumeration
   labelled with the version.
8. Auto-seal scheduling with the derived cadence; `placed` and `await placed(region)`; the
   loss-window report; opt-in live shipping with credit backpressure and takeover without a
   loss window.
9. Ownership migration by measured write origin through the planned handoff; explicit moves.
10. Mirroring: epoch-ordered shipping to the mirror neighbourhood; `mirror_age`; `await
    placed(mirror)`; cross-region promotion through the root group; the loss-window report at
    region loss.
11. Green volumes in the fleet (D-27): merge records as ledger entries under the host epoch with
    the placed precondition; holder recomputation before serving and head-identity comparison
    per version; the lying-proposer and resolver-storm tests.
12. Overlay volumes in the fleet: retain the live base identity/serving host through every
    clone and delta-owner migration; report `BaseUnavailable` on unfetched source paths after
    source loss. Capture a stable complete base explicitly when source-independent reads are
    required; delta placement alone never promises whole-view availability.
13. Remote attach; lazy hedged fetch; prefetch learning (observe-first).
14. Simulation harness with nemeses; histories; checkers; Jepsen-style real-process runs. The
    TLA+ models in `docs/wip/models/` are architecture artifacts: they were checked when the
    protocol was designed (GAPS §10) and are re-run by whoever changes §4.8, never by CI.

**Worked examples.**
- Three hosts in two zones, f=1, A's neighbourhood {B, C, D}: an agent on A creates a volume
  (local); the head record goes to A's two record candidates and commits at one remote
  acknowledgement; the agent writes; the seal schedule seals a snapshot whose chunks go to two
  content candidates, one is slow, the hedge lands the copy on the third and `placed` is
  reported with the holders that answered; an agent on B attaches the head by routing the id to
  A and fetches chunks from the recorded holders.
- Node A crashes: B's SWIM confirms A dead; the regional group bumps A's epoch and assigns A's
  volumes to the first-ranked surviving candidates; each runs one batched phase one across the
  neighbourhood, adopts the newest records, and serves; the agent's SDK reconnects by id
  routing, its retries return original results, and each volume reports its loss window; a
  `live-shipped` volume reports none.
- Failure: a partition isolates C; C's owners cannot confirm membership and refuse writes with
  `LeaseUnconfirmed` after the bound; reads of placed snapshots continue; seals on C accumulate
  as `Local`; on heal, C rejoins with a new node id, its volumes have been taken over by its
  neighbours, and its `put_wal` is replayed by the healer for anything the takeover did not
  already hold.

**Acceptance criteria.**
- AC-8.1 The Raft conformance suite (every named upstream bug as a test) passes for the
  configuration group; the simulation's histories of the register protocol satisfy the same
  invariants the design models proved (TotalOrder, Continuity, StaleNeverCommits, ReadSafety,
  NoLoss), checked by the Rust harness (catches: consensus and register bugs).
- AC-8.2 Under the nemesis library in simulation for the seed budget, every history of head,
  chain, lease and catalog registers is linearizable and Elle finds no anomaly; every placed
  snapshot survives any f failures; no head record names content that is not placed (catches:
  durability and consistency bugs).
- AC-8.3 The loss window reported after an owner loss equals the edits after the last placed
  seal, measured against the model; `live-shipped` volumes report none (catches: false
  durability claims).
- AC-8.4 Remote attach completes in the provisioning budget once the manifest is resident; first
  read latency is the measured hedged fetch cost (catches: eager materialization).
- AC-8.5 Placement respects failure domains and the copyset bound: no object has two holders in
  one domain, and the copyset count stays under the derived bound at every configuration,
  verified structurally (catches: correlated loss).
- AC-8.6 The N=1 differential test still passes with the cluster code present (catches: modes).
- AC-8.7 Membership, hedge, probation and seal parameters are derived: changing measured RTT,
  loss, put latency or mutation rate in simulation changes timeouts, delays and cadences by the
  formulas; false-positive death rate stays under the operator SLO (catches: hardcoded timers).
- AC-8.8 The healer converges: after injected put failures, every `Local` snapshot becomes
  placed within the derived bound once candidates return; probation removes a repeatedly late
  holder within the derived count (catches: silent under-placement).
- AC-8.9 Live shipping bounds memory on the owner through credit backpressure (catches:
  unbounded queues).
- AC-8.10 A live base stays bound to its identified source while the delta owner may move.
  After source loss, available placed delta bytes still read and unfetched base paths refuse
  `BaseUnavailable`; only complete placed snapshots remain wholly readable. Two nodes cannot
  hold the landing lease for one target (catches: substituted bases and disk split-brain).
- AC-8.11 Green in the fleet: no merge record commits under a stale epoch or with an unplaced
  reference; every holder's recomputation matches at every version under the nemesis library;
  an injected lying proposer is refused by every holder before any read; owner loss takes green
  over on a holder of the ledger with no version lost; submitters under takeover churn see
  bounded retries and single-flight refresh (catches: split-brain green; output-poison
  replication; retry storms).
- AC-8.12 Straggler immunity: an injected slow candidate never delays `placed` beyond the hedge
  delay plus one put, measured; the hedge rate stays within its derived cap under the measured
  put latency distribution (catches: the write-all stall returning; hedge storms).
- AC-8.13 Fencing: a resumed owner with a bumped epoch is refused by every holder that has
  installed that fence, never
  commits a record, and the successor's base is at least as new as every record that ever
  committed under the old epoch, checked against the model in simulation (catches: lineage
  forks).
- AC-8.14 Id routing: after one stable configuration change a stale lookup refreshes once;
  under continuing takeover/migration, bounded retries yield the current owner's answer or a
  typed deadline refusal. No global lookup index exists (catches: catalog inconsistency,
  unbounded redirects and index creep).
- AC-8.15 Mirroring: `mirror_age` tracks the injected WAN delay; `await placed(mirror)` never
  returns before the mirror's f+1 hold the record and content; region loss promotes with a loss
  window equal to the lag at the moment of loss and zero for awaited operations (catches:
  false cross-region durability).
- AC-8.16 Ownership follows the writer: after the derived number of write-intent operations
  from another host, ownership migrates there within the derived bound, the old owner's later
  writes are refused, and no acknowledged write is lost (catches: latency regressions from
  remote ownership; handoff loss).
- AC-8.17 The configuration group's commit rate stays near zero outside injected failures and
  moves (catches: consensus creeping back onto a per-write path).

**Test cases.**
- T-8.1 (simulation) Partition every pair, every triple, during create/write/seal/clone/archive;
  expect invariants hold.
- T-8.2 (simulation) Clock jumps on one node; expect no membership or epoch misbehaviour.
- T-8.3 (simulation) Slow candidate (10x latency) during puts; expect `placed` at the hedge
  delay plus one put and the slow holder on probation after the derived count.
- T-8.4 (simulation) Kill the owner between a seal and its f+1 acknowledgements; expect the head
  unchanged, the successor adopting the newest committed head, the client's retry returning the
  original result after takeover, and the loss window reported.
- T-8.5 (simulation) Membership churn storm (join/leave 100 nodes); expect convergence within the
  derived bound, no false deaths, and neighbourhoods changed with joint writes and no loss.
- T-8.6 (simulation) A resumed owner after a takeover issues records with its old epoch; expect
  holders with the new fence to refuse, no stale quorum commit, and exact historical-prefix
  Continuity to hold.
- T-8.7 (real) Three-process cluster on one machine with iptables/pf partitions; expect the same
  outcomes as simulation.
- T-8.8 (benchmark) Seal cost versus tree size (must be O(changed)); put throughput; record
  write latency; remote attach latency; chunk fetch throughput; takeover time; healer
  convergence time; hedge rate.
- T-8.9 (chaos) Correlated loss of all f+1 copies of a snapshot; expect the documented data-loss
  outcome with a typed `ContentUnavailable` and a clear operator signal, and the copyset count
  to have been under its bound (the loss is the accepted probability, not a placement bug).
- T-8.10 (simulation) Kill the owner of an overlay volume mid-landing; expect the landing
  `Aborted` with old-or-new entries on that host's disk, the lease refused by generation, and no
  other node writing into the target.
- T-8.11 (simulation) Kill, pause and resume, and partition the green owner at every step of the
  submission transaction with sixteen submitters; expect exactly-once apply, truthful verdicts
  on retry, stale records refused, a single version lineage, and takeover at the membership
  horizon with no replay on the critical path.
- T-8.12 (simulation) Lookups by id during takeover and migration from every node; expect the
  one refresh after a stable change, or a typed deadline refusal under continuing churn;
  successful answers carry current authority.
- T-8.13 (simulation) Injected WAN delay and a region loss; expect `mirror_age` to track the
  delay, awaited operations intact after promotion, and the loss window equal to the lag.
- T-8.14 (simulation) An agent's attachments move to another host; expect migration after the
  derived count, no lost acknowledged write, and the old owner refused.
- T-8.15 (simulation) The register invariants of the design models, encoded as checks over
  simulation histories, on every nemesis seed; a violation fails the build.

**Exit criteria.** AC-8.* pass; the simulation seed budget runs nightly.

**A-9 required acceptance and regression cases (open; 2026-09-05).**

| Acceptance | Test | Required behavior | Do X, expect Y |
|---|---|---|---|
| AC-8.18 | T-8.16 | Takeover preserves every historically committed value across changing quorums. | Run BUG-12 then generated minority writes, arbitrary majorities, repeated takeovers and non-extension replication; expect refreshed acceptance epochs and exact committed-prefix continuity. Deliver messages independently and never require candidate zero reachable. |
| AC-8.19 | T-8.17 | Placement and mirroring require verified retained bytes and host-local reservations. | Commit versions while injecting missing chunks, corrupt data, full holders, generation changes and region loss; expect no reference before its required placement, holder recomputation before serving, and truthful time lag/availability rather than identity-only success. |
| AC-8.20 | T-8.18 | Configuration and local-read authority hold under pauses, partitions and membership changes. | Adapt Hecate CS1–CS12 individually to the configuration core, recording applicability; inject stale leaders, duplicate replies, delayed renewal, clock-bound loss, reconfiguration and epoch exhaustion. Expect safe authority refusal, distinct-member quorums and no per-write configuration call; revalidate the model refinement before closure. |
| AC-8.21 | T-8.19 | Remote clones preserve local-source semantics and dependency loss honestly. | Clone a live overlay on another host, change the source, migrate the delta owner and lose the base host; expect correct untouched reads while reachable and BaseUnavailable afterward. Repeat with a complete placed base and expect source-independent reads. |

### Phase 9 — Scale, soak, chaos, hardening, release

**Goal.** Prove the scale envelope, run the soak and chaos programs, close the gap ledger, and
ship.

**Ordered tasks.**
1. Scale runs: 10^6-file volumes, 100k-entry directories, thousands of volumes per node,
   thousands of clients; memory per file/entry/volume within derived budgets.
2. Soak: 24 h mixed workload with RSS plateau detection and counter audits (no unbounded
   growth); leak checks under the allocation ledger feature.
3. Chaos day with the full injection list on real machines across the platform matrix.
4. Hardening: fuzz corpora extended; security review of rendezvous, peer checks, grant channels
   and landing containment; the tracer run for hermeticity across the whole matrix; power-fail
   landings on real machines (dm-flakey on Linux, a pulled RAM VHD on Windows, a forced restart
   on macOS) checked against the oracle.
5. Release engineering: the nine-target release, npm and PyPI publishing, plugin package, docs
   (user docs short in `docs/`, this design and the specs in `docs/wip` promoted).
6. The gap ledger reconciled: every open item either closed or carried with an owner.

**Acceptance criteria.**
- AC-9.1 Every scale target in Part 1.4 met or the measured limit documented with its cause.
- AC-9.2 Soak shows a memory plateau; every bounded structure's bound held.
- AC-9.3 Chaos day finds no undocumented behaviour; documented Degraded cells occur as written.
- AC-9.4 Zero hermeticity violations across the matrix: no write outside a granted target, ever.
- AC-9.5 Release artifacts reproducible from the tag with provenance and checksums.
- AC-9.6 Power-fail landings leave every entry old or new with the reported durability level
  honoured (data and directory syncs; media flush on macOS when requested).

**Test cases.** The full nightly and release suites; the chaos list; the tracer; the platform
matrix; a fresh-machine install test per OS (daemon start, first volume, first attach, first
tool run) with wall time recorded.

**Exit criteria.** Release.

**A-9 required acceptance and regression cases (open; 2026-09-05).**

| Acceptance | Test | Required behavior | Do X, expect Y |
|---|---|---|---|
| AC-9.7 | T-9.1 | Advertised release guarantees have end-to-end evidence on every offered transport. | Run the required POSIX/workload, hermeticity, pressure and failure suites through native, OCI and virtio-fs attachments; trace zero writes outside granted targets, check residency within each claimed boundary, and publish capability-specific results. A skipped lane or pure simulation cannot close its transport guarantee. |

### Dependency and ordering summary

Phase 0 → Phase 1 → Phase 2 → Phase 3 → Phase 4 → Phase 5 → Phase 6 → Phase 7 → Phase 8 →
Phase 9. Phases 3 and 4 may overlap once the bridge trait is fixed; Phase 5 may begin on the
Rust client during Phase 3; Phase 6 may begin on the in-process API after Phase 2 and needs
Phase 5 only for its surfaces; Phase 7 may begin its identity pass during Phase 4. Nothing later
may weaken an earlier phase's gates; ratchets only tighten.

---

## Part 6 — Test taxonomy, benchmark suite, and CI gates (D-20)

Every test drives slates through a public surface (a mount, an SDK, MCP, a wire message, the
CLI) and asserts on behaviour. No test asserts on a file location, a constant, or an internal
field.

| Category | Purpose | Tooling | Pass means (plain English) | Cadence |
|---|---|---|---|---|
| Model-based | Prove the VFS behaves like a simple map with POSIX rules | proptest state machines; an executable model | Every generated sequence of operations produces the same observable state and errors as the model, and shrinks to a minimal failure when not | CI |
| Differential | Prove parity with the host filesystem | Same sequences on tmpfs / APFS RAM disk / NTFS RAM VHD; Metis-style abstract-state hashes | The abstract state after every step matches except where the reviewed equivalence policy allows (timestamps by order, inode numbers ignored, errno within the platform's allowed set) | CI (Linux), nightly (macOS, Windows) |
| Landing oracle | Prove the one path that writes disk | an executable model of (disk, overlay, witnesses); outsider edits and crashes injected at every step; real filesystems on RAM-backed targets | Verdicts match the table; every entry old or new; every collision reported; re-runs idempotent; writes proportional to the delta | CI (Linux tmpfs), nightly (APFS, NTFS) |
| Merge oracle | Prove the verdict, the splice and rebase | an executable model of (chain, work volumes, declared operations); proptest with shrinking; instrumented counters | Verdicts match the oracle with zero false accepts or rejects; spliced versions byte-identical; no content read on the common path; windows byte-exact | CI |
| Conformance | Prove POSIX behaviour through each bridge | pjdfstest, fsx, fsstress (CI); xfstests generic, LTP (nightly) | All tests pass except the reviewed expected-failure list for that bridge, and the list only shrinks | CI / nightly |
| Real workloads | Prove tools work | vendored git, cargo, npm, python, rg, rsync, sqlite, editors, watchers | Exit codes 0; outputs byte-identical to the host run; `git status` clean; incremental builds reuse artifacts | nightly, release |
| Unit and property | Prove each core structure | proptest, oracle tests (serial spec vs implementation), non-vacuity counters | Properties hold; counters prove the fast paths ran | CI |
| Hostile input | Prove parsers never crash or over-allocate | cargo-fuzz corpora for wire, FUSE, NFS/XDR, WinFsp, archive, MCP | No panic, no UB, bounded time and memory on every input | CI smoke, nightly long |
| Concurrency | Prove lock-free cores and schedules | loom (cores), shuttle (schedules), TSan, Miri | All explored interleavings keep invariants; no data race; no UB | CI (loom, Miri), nightly (shuttle, TSan) |
| Linearizability | Prove the catalog and leases are linearizable | recorded histories; P-compositional checker; Elle | Every history is linearizable; no anomaly | nightly |
| Simulation and chaos | Prove fault tolerance | the simulation driver with a nemesis library, in Rust | Invariants hold under every seed, including the register invariants the design models proved; liveness resumes after faults | nightly (seed budget), release (extended) |
| Fault injection on real processes | Prove real-OS behaviour under failure | kill/pause daemon with tools holding files; cgroup/job memory limits; clock jumps | Documented Degraded/Refused outcomes occur; nothing silent; recovery within budget | nightly |
| Benchmarks | Prevent regressions | iai-callgrind (instruction counts) in CI; latency histograms with E-Divisive change-point detection nightly; macro benchmarks vs host | No change-point on a gated metric; the provisioning p99 stays under its ratcheted floor | CI / nightly |
| Soak and scale | Prove no growth and large trees | 24 h runs with RSS plateau detection; millions of files; 100k-entry directories; deep trees | Memory plateaus; operations stay within their bounds; no refusals except intended ones | release |
| Platform matrix | Prove every target | the nine targets including i686 | Build, unit, conformance, and workload suites pass on each | nightly (subset), release (all) |
| Laptop ≡ fleet | Prove no modes | the N=1 differential test | Every client-visible API has identical semantics with one node and with a simulated fleet | CI |

**Benchmark suite (all recorded with the exact command, hardware, dataset, load discipline).**
Provisioning latency end-to-end from each SDK (spinning and parked forms; p50/p99/p999/max);
bridge per-operation latency versus the host per OS; git/cargo/npm/pytest/rg/rsync wall time
versus the host; bytes per file and per volume; snapshot root-publication cost and clone
root-sharing cost (both O(1)); separately measure client barriers, capture, hashing and placement versus tree size; write amplification per mutation; dedup and compression ratios per
corpus; archive throughput; replication lag; recovery time; memory per shard idle and loaded;
listing cost per directory size per OS; copy-up cost per class; drift check cost; landing
throughput and time per entry versus `cp -r`, `rsync` and `git checkout` of the same delta; the
settled in-flight depth per target filesystem; verdict µs p99 by increment size and delta count;
merge-path p99 (seal to reply) on a laptop and across hosts; merges per second per green; rebase
cost; `advance` invalidation cost; takeover time of a volume and of a green after owner loss;
register write latency; hedge rate and the copy it wastes; mirror lag under injected WAN delay.

**CI gates (every change).** fmt; clippy `-D warnings`; the lint wall (no `Arc`/`Rc`, no
`std::fs`/`std::net` outside the allowed crates, no tuning literals); unit, property, oracle, and
model tests; loom and Miri on the `mem`, `rt`, `wire`, and ring crates; the merge purity lint and
the never-diff architecture test; fuzz smoke; pjdfstest and
fsx over a slates mount on the runner's OS; the N=1 differential test; instruction-count
benchmarks; the provisioning histogram (ratcheted); the merge oracle with shrinking.

**Nightly.** xfstests generic, LTP, fsstress; the full workload suite on all three OSes;
simulation with the seed budget; linearizability and Elle; shuttle and TSan; latency histograms
with change-point detection; hostile-input long runs.

**Release.** Soak and scale; the full platform matrix including i686; a manual chaos session
with the injection list; the benchmark write-up in the
"Pass N" style with rejected experiments kept.

**Example test cases (written for the implementer).**
1. *Model, error path.* Generate: create a bounded 1 MiB volume; write 900 KiB to `a`; write 200
   KiB to `b`. Expect: the second write fails with `ENOSPC` after writing exactly the bytes that
   fit (or none, per the declared semantics), `a` is intact, `referenced_bytes` never exceeds the
   quota, and the model agrees.
2. *Differential, edge.* Rename `dir` over an empty `dir2`, then rename `dir2/x` into `dir`
   while a reader lists `dir`. Expect: the listing shows either the old or the new state, never a
   mix; the abstract state matches tmpfs.
3. *Conformance, expected failure.* pjdfstest's chflags cases over FUSE. Expect: skipped by the
   reviewed list with a reason; any newly passing case must be removed from the list.
4. *Workload, parity.* `cargo build` twice in a clone. Expect: the second build reports
   "Fresh" for every crate, exactly as on the host.
5. *Concurrency, fault.* Two agents hold attachments to one shared volume with subtree
   delegations; kill the daemon while both write. Expect: after restart, every acknowledged write
   is present, no unacknowledged write is partially present, both agents' next writes are refused
   with `StaleLease` until they re-acquire, and the history is linearizable.
6. *Chaos.* Partition the owner from its neighbourhood and the configuration group for longer
   than the membership horizon while an agent writes. Expect: writes are refused with
   `LeaseUnconfirmed` on the owner after the bound; the group bumps its epoch and a neighbour
   takes over; after the partition heals, the old owner's first write is refused `StaleEpoch`,
   and the agent's retried requests return their original completion records from the new
   owner.
7. *Benchmark.* 10,000 `create` calls from the Python SDK with the client spinning. Expect: p99
   under the ratcheted floor (initially the 50 µs target) on the reference machine; a change-point
   fails nightly.
8. *Hermeticity.* Run the whole suite under a filesystem-write tracer (Linux fanotify / macOS
   fs_usage / Windows ETW). Expect: zero writes by slates processes outside the documented
   exceptions (the one-time mount-point directory on macOS; install-time files; entries inside a
   granted landing target while that landing runs, each matched to a `Written` outcome in the
   audit log).
9. *Landing, concurrency.* Land a 500-entry delta into a tree while an outsider rewrites 50 of
   the targets at random moments. Expect: every outsider write that happened after validation is
   still on disk afterwards, every such entry is reported `Conflict(TargetInUse)` and remains in
   the overlay, the other entries are `Written` with hashes equal to the overlay, and the report
   is `Partial`.
10. *Landing, power fail.* Cut power (simulated, then real with dm-flakey) at random points of
   a landing that reported nothing yet. Expect: after remount, every entry is byte-identical to
   either its pre-landing content or its overlay content, no hidden sibling remains after the
   next landing's sweep, and the re-run lands only what is missing.
11. *Merge, concurrency.* Sixteen agents submit increments to one green at random intervals,
   half through `edit` and half through whole-file rewrites, over a set of files small enough to
   force overlaps. Expect: every version's bytes equal the oracle's, every overlap is a
   `Conflict` with byte-exact windows, every identical concurrent rewrite is `AcceptIdentical`
   after one range compare, no agent's accepted operation is lost, and the fast-path counter
   equals the number of disjoint increments.

---

## Part 7 — Open questions, must-measure list, and risks

**Must measure at boot (feeds derived constants).** Page sizes and huge-page availability;
cache-line size; core classes and core-to-core latency; memory and lock capacity; fault cost per
page class; syscall cost; park/unpark latency; memcpy, BLAKE3, LZ4, zstd throughput; bridge
loopback RTT per operation; launcher mount cost; address-space size (i686).

**Must measure per volume (feeds policy).** File-size histogram; allocation rate and bursts;
dedup hit rate per hashed byte; LZ4-to-zstd regression; first-read sequences after attach;
mutation rate and subscriber lag; rename rate; for overlay volumes the base cache hit rate,
copy-up rate per class, drift rate, and watcher overflow rate.

**Must measure per green volume (feeds the merge engine's constants).** Per-record verdict cost;
mapping cost per intervening delta; base-lag distribution; increment size distribution; merges
per second; conflict rate by source (SDK edits versus whole-file rewrites); rebase-retry rate;
`StaleEpoch` refusals on merge records (must be zero outside takeovers); holder recomputation
cost and mismatches (must be zero).

**Must measure per fleet (feeds the register protocol's constants).** Intra-region and cross-region
round trips; per-record write cost; put acknowledgement latency distributions per class (the
hedge delay); the scatter width from measured re-replication bandwidth and the accepted loss
probability; takeover time; the configuration group's commit rate; mirror lag; the fraction of
head reads served by non-owners; the copyset count against its bound.

**Must measure per landing (feeds the ramp and the strategy choices; never at boot).** Per-entry
latency and throughput at each in-flight depth; swap, verify, link and reflink cost; data-sync
and directory-sync cost and the `syncfs` break-even; exchange and clone support of the target
filesystem; the stage-and-exchange break-even.

**Open questions (decided by the plan's phases, not by guessing).**
1. macOS: the FSKit spike's go/no-go and the 15.x block-resource path (Phase 4); whether an
   unprivileged `hdiutil attach -nomount ram://` RAM disk is permitted (Phase 4 verifies).
2. macOS: FSKit's name and data cache invalidation for SDK-ring writes (Phase 4 measures); the NFS
   fallback's attribute-cache timeout (derived from GETATTR RTT).
3. Linux: FUSE-over-io_uring mixed-size buffer handling on the target kernels (Phase 3 measures;
   fallback is `/dev/fuse` read/write).
4. Whether compio's request path is `Arc`-free enough to replace the custom executor (Phase 0
   audit; the custom executor is the plan of record).
5. `LocalWaker` stabilization on Rust 1.98 (Phase 0; the encoding works with `Waker` regardless).
6. The Linux `RLIMIT_MEMLOCK` and systemd defaults (irrelevant to correctness: capacity is probed).
7. The cold-content class boundary, the (k, m) and the reconstruction budget for erasure coding
   (Phase 8 measures; the fragment record kind is already in the format).
8. FSKit's evolution (re-evaluated per macOS release).
9. Exchange support per target filesystem in the wild (ext4, XFS, Btrfs, tmpfs, APFS, NTFS are
   expected to have it; network and FAT filesystems are not); the fallback's measured window
   (Phase 1 and Phase 4).
10. The confirmation-surface contract for harnesses other than the terminal (Phase 5 defines
    the request stream; each harness integrates it by hand, never through MCP).
11. Timestamp granularity per base filesystem for the racy rule (a cited table; verified per
    filesystem in Phase 1).
12. Whether the conflict rate from whole-file tool rewrites justifies a declared-edit bridge
    path (an ioctl or xattr protocol by which editors declare ranges) beyond the SDK `edit` verb
    (Phase 6 measures).
13. The scatter width, hedge delay, probation threshold and detection timeout at fleet scale,
    all derived from measurements Phase 8 takes on the reference fleet.

Closed questions are recorded in `docs/wip/GAPS.md` §2 with their dates and reasons.

**Risks and mitigations.**
- The 50 µs target depends on client spinning and on the daemon staying hot: the SDKs default to
  the published spin window; the parked form is reported separately and never hidden.
- macOS FSKit is new: no published latency, undocumented cache semantics, two protocol
  generations in three releases, and an app-bundle packaging requirement; the Phase 4 spike
  gates it with numbers, and the NFSv3 fallback (with its documented weaker coherence) remains
  for older systems and as the oracle. Shared-writable mmap is refused across hosts.
- Windows daemon crash unmounts volumes: documented; the anchor restores state; open handles see
  errors.
- Own bridges and own runtime are a large surface: bounded by the size estimates (FUSE 3-5 kLOC,
  NFS 4-6, WinFsp 2-3, runtime 6-7) and by conformance suites with expected-failure lists.
- MCP's protocol churn: the conformance suite gates each release.
- Correlated failures lose data by design: the durability statement and the archive verb make
  this explicit to agents.
- Outsiders write the base and the target at will: fingerprints, not watchers, are the truth;
  compare-and-swap at the exchange, not locks, is the guard; every loss is detected and reported,
  and a filesystem without an exchange primitive is a documented Degraded cell.
- Huge base directories (`node_modules`, `target/deps`) make merged listings the cost to watch;
  the listing cache is keyed by the directory's change time and the tripwire is armed.
- A human must be reachable to land: headless runs carry pre-issued session grants or do not
  land; there is no headless default that writes.
- Authority depends on correct accepted epochs, quorum adoption, scoped generations and read
  leases. BUG-12 demonstrates that a single-writer intention does not exclude committed data
  loss. The named consensus cases, full historical-prefix oracle and implementation/model
  refinement are required; no safety class is declared impossible merely from the architecture.
- Hedged puts spend a bounded, measured fraction of extra copies transiently; the cap is
  derived from the put latency distribution and hedging is disabled under overload, as Dean and
  Barroso prescribe.
- Semantic conflicts on disjoint ranges are invisible to any textual engine: green's evidence
  policy and the landing grant are the answers, never a smarter merge.

---

## Appendix A — Bibliography

The full, tiered bibliographies live at the end of each research file under `docs/wip/research/`:
`os-filesystem-bridge.md`, `cow-data-structures.md`, `low-latency-ipc-and-runtime.md`,
`database-design.md`, `edenfs-scale-distribution.md`, `memory-and-system-awareness.md`,
`compression-archive-dedup.md`, `testing-and-benchmarking.md`, `mcp-skills-sdks.md`,
`arc-free-rust-architecture.md`, and the four codebase surveys (`survey-sylk-vfs.md`,
`survey-hecate.md`, `survey-hyperscale.md`, `survey-vorpal.md`). Citations in this document use
the same tier letters and the same keys.

---

## Appendix B — Workspace layout and engineering conventions (D-24)

Evidence: `research/survey-vorpal.md` §0–§9.

### B.1 Toolchain, edition, lints (copied from vorpal, with the reasons)

- `rust-toolchain.toml` pins an exact version, deliberately not "stable". vorpal's comment: the
  channel name "resolves differently on every machine — CI's runner and a dev laptop can be four
  stables apart, which made the clippy -D warnings gate unreproducible." Bump the pin and the
  matching pre-install pins in every workflow in one commit, after a local clippy pass.
  [C: vorpal rust-toolchain.toml:1-7]
- Edition 2024, MSRV 1.98, resolver 2, one workspace version, every internal dependency declared as
  `{ path = "crates/x", version = "0.x.y" }`. [C: vorpal Cargo.toml:2-44]
- `[workspace.lints]` with every crate opting in via `[lints] workspace = true`. slates adds to
  vorpal's set: `unsafe_op_in_unsafe_fn = "deny"`, a deny on `std::sync::Arc` and `std::rc::Rc`
  outside the named FFI edge modules (enforced by a clippy `disallowed_types` entry plus a
  structural test), and a deny on `std::fs` and `std::net` outside the bridge, rendezvous, base
  and landing crates; the structural test further denies every write-capable file syscall
  (`open` with write flags, `rename*`, `link*`, `unlink*`, `mkdir*`, `rmdir`, `truncate`,
  `fsync`, `utimens*`, `chmod*` and their Windows equivalents) outside the landing crate, and
  denies `std::fs` write functions in the base crate. [derived from R1, R2, R10 in Part 0]
- `panic = "abort"` in the release profile, no `catch_unwind`; allocation exhaustion is typed.
  [C: hecate RUNTIME.md:109-136, adopted]
- Formatting: `rustfmt.toml` with `tab_spaces = 2`; `.editorconfig` 2-space indent, LF, UTF-8,
  trailing-whitespace trim except Markdown. slates has no inherited code, so a `cargo fmt --check`
  gate is appropriate (vorpal skipped it only because "a red-forever check is worse than no
  check" on inherited code). [C: vorpal .github/workflows/ci.yml:24-28]
- Third-party versions pinned once in `[workspace.dependencies]`; exact pins (`=x.y.z`) only where
  an ABI contract is version-specific, each with a comment saying why; heavy optional features
  documented as "NEVER a default". [C: vorpal crates/ann/Cargo.toml:20-28, crates/index/Cargo.toml:58-61]

### B.2 Target matrix and release recipes (identical to vorpal's)

| target | runner | asset name |
|---|---|---|
| `aarch64-apple-darwin` | macos-latest | `slates-macos-arm64` |
| `x86_64-apple-darwin` | macos-latest (cross via `--target`) | `slates-macos-x64` |
| `x86_64-unknown-linux-gnu` | ubuntu-latest | `slates-linux-x64` |
| `aarch64-unknown-linux-gnu` | ubuntu-24.04-arm | `slates-linux-arm64` |
| `x86_64-unknown-linux-musl` | ubuntu-latest, `rust:1.98-alpine` container | `slates-linux-x64-musl` |
| `aarch64-unknown-linux-musl` | ubuntu-24.04-arm, `rust:1.98-alpine` container | `slates-linux-arm64-musl` |
| `x86_64-pc-windows-msvc` | windows-latest | `slates-windows-x64.exe` |
| `aarch64-pc-windows-msvc` | windows-latest (cross via `--target`) | `slates-windows-arm64.exe` |
| `i686-pc-windows-msvc` | windows-latest (cross via `--target`) | `slates-windows-x86.exe` |

[C: vorpal .github/workflows/release.yml:64-74]

Recipes to copy verbatim:
- One raw binary per platform attached to the GitHub release, no archive; `cargo-binstall`
  overrides name each asset explicitly and "unsupported targets fail instead of guessing".
  [C: release.yml:1-16; crates/cli/Cargo.toml:78-103]
- musl lanes build inside official `rust:alpine` containers, native per arch, with `apk add
  build-base`, `RUSTFLAGS="-C target-feature=+crt-static"`, the `LINKER=cc` environment override
  (env outranks `.cargo/config.toml`), and an `ldd` staticness gate that accepts both "not a
  dynamic executable" (classic static, aarch64) and "statically linked" (static-PIE, x86_64 since
  rustc 1.85). [C: release.yml:24-32, 87-114]
- cdylib bindings for musl are built with `-C target-feature=-crt-static` because `+crt-static`
  is rejected for a cdylib; the container is launched with `docker run`, not `container:`, so
  checkout/upload stay on the host. [C: publish-node.yml:60-66]
- A `guard` job asserts the git tag equals the workspace version. [C: release.yml:40-45]
- `cargo xtask release-artifacts` writes `SHA256SUMS`, a provenance JSON (git commit, rustc
  version, per-file sha256, blake3, size), and an ed25519 signature when a CI-secret key is
  present. [C: xtask/src/main.rs:79-160]
- Weekly scheduled audit re-verifies vendored sources against pinned upstream commits and opens
  one tracking issue on drift; it never edits the tree. [C: grammar-audit.yml:1-13]

### B.3 Packaging for the SDKs (shape copied from vorpal; SDK details in §4.12)

- npm: a scoped CLI package with `optionalDependencies` on eight platform packages named in
  napi-rs convention (`darwin-arm64`, `linux-x64-gnu`, `linux-arm64-musl`, `win32-ia32-msvc`, ...),
  each declaring `os`, `cpu`, and Linux `libc`; a postinstall that resolves the package with
  `detect-libc` and hard-links the binary into place; trusted publishing (OIDC) after a one-time
  bootstrap publish. [C: vorpal npm/package.json, npm/postinstall.js:6-27, release.yml:234-281]
- PyPI: maturin-built wheels; vorpal ships abi3 wheels covering CPython ≥ 3.9 for manylinux,
  musllinux, macOS, Windows plus an sdist. slates' Python floor and abi3 decision are settled in
  Part 4.12 from the SDK research. [C: vorpal publish-python.yml:1-35]

### B.4 The written Arc policy (adopted from vorpal's, tightened for slates)

vorpal's rule, verbatim: "no `Arc` refcount is touched on any hot path (per-element, per-node,
per-edge, per-query-data). `Arc<T>` clone/drop are atomic RMWs on a shared counter; when N cores
clone/drop the same `Arc` the counter's cache line ping-pongs ... serializing work that should
scale linearly and creating a false-sharing hotspot." Its honest scope admits `Arc` inside tokio
task allocation and channel handles, "constant, off the per-item/per-node path."
[C: vorpal docs/wip/ARCHITECTURE.md:288-302]

vorpal's two exemplars of a documented, accepted `Arc` (the kind the brief means by "note why
and accept"): the child-process handle shared between a wait future and a kill handle
("genuinely two independent `'static` owners of one OS process, so an `Arc` here is load-bearing,
not incidental") [C: crates/transport/src/spawn.rs:46-49]; and the scatter-gather result buffer
written by many worker threads in the Python async bridge ("borrows can't express it")
[C: crates/pyo3/src/async_bridge.rs:263-265]. vorpal's survey also lists sites it marks avoidable
(`Arc<ExtractorSet>` where `&'static` suffices; `Arc<W>` for scoped walker threads; inherited
LSP `Arc<DashMap>`), which slates must not repeat.

slates' policy (the exact text goes in Part 4.3):
1. `Arc` and `Rc` are denied workspace-wide by lint.
2. The only allowed sites are in named FFI edge modules where a foreign API forces `'static`
   shared ownership (bindings objects whose GC may drop them mid-call; callback registrations that
   demand `'static + Send`). Each site carries a comment naming the two owners.
3. Process-lifetime singletons are `static` or `Box::leak`, initialised once through `OnceLock`
   (one atomic at init, none afterwards); vorpal's `&'static Index` singleton pattern.
   [C: vorpal ARCHITECTURE.md §7.1]
4. Shared references within a shard are generational handles into arenas; a stale handle is a
   typed miss, never a dangling pointer. [C: vorpal ARCHITECTURE.md §7.2; hecate RUNTIME.md:96-107]
5. Read-mostly shared structures across shards use epoch-published immutable roots (atomic
   pointer swap) with the reclamation scheme decided in Part 3; vorpal rejected plain
   crossbeam-epoch ("unbounded limbo under a stalled pinner") and global hazard pointers
   ("per-pointer advertise + fence on every edge chase wrecks traversal throughput").
   [C: vorpal ARCHITECTURE.md §7.3]
6. Every hot atomic is cache-line padded; vorpal's ledger found "four global atomics doubled
   kernel-scale user CPU purely on cache-line ping-pong". [C: crates/kg/src/ledger.rs:12-18]
7. Locks: none on data paths. Where a lock is unavoidable on a cold path, poisoning is recovered,
   never propagated (the no-panic law). [C: vorpal crates/core/src/meta_var.rs:121]

### B.5 Async runtime placement (from vorpal, as a caution)

vorpal keeps tokio at the edge only (SSH/k8s transports, LSP) and builds the runtime on the
producer's own thread, bridging back over a bounded `sync_channel`; CPU parallelism is rayon plus
bounded crossbeam channels; the Python bridge is "No tokio, no asyncio thread pool" with a
Rust-owned worker pool; the MCP daemon is blocking stdio JSON-RPC. [C: survey-vorpal.md §2.4]
slates goes further: its own thread-per-core executor with completion drivers (decided in Part 3
from the runtime research); tokio appears nowhere.

### B.6 Workspace layout

```
slates/
  Cargo.toml                 workspace; lints; profiles
  rust-toolchain.toml        exact pin
  crates/
    machine/                 boot calibration: the machine profile (page sizes, cache line, cores,
                             NUMA, memory limits, wake latency, memcpy/hash/compress bandwidth)
    mem/                     arenas, slabs, generational handles, per-shard allocators
    rt/                      thread-per-core executor, timer wheel, cross-shard rings, drivers
                             (io_uring/epoll, kqueue, IOCP), deterministic SIM driver
    wire/                    canonical encoding, schema hashes, framing, credits
    vfs/                     namespace, inodes, chunks, snapshots, volumes (no std::fs allowed)
    base/                    read-only host access for overlay volumes: listings, witnesses,
                             copy-up reads, drift checks, watchers (no write syscall linked)
    land/                    the landing engine: manifest, verdict, per-OS writer, grants and
                             leases client, audit (the only crate that writes host paths)
    merge/                   the merge engine: chains, canonical deltas, the position mapper,
                             the two-pass verdict (pure; lint-enforced), splice, rebase, merge task
    store/                   content-addressed chunk index, dedup, compression, archive format
    db/                      metadata database: catalog, lineage, leases, log, registers, held records
    bridge-fuse/             Linux /dev/fuse driver
    bridge-virtiofs/         planned owned FUSE-over-virtio device; custom runtime and VMM seam
    bridge-<macos>/          macOS bridge (decided in Part 3)
    bridge-winfsp/           Windows WinFsp binding
    anchor/                  the anchor process's library: the shared segment's layout (profile,
                             recoverable content/roots, op logs, catalog snapshots, audit, landing manifests, held
                             descriptors), attach, replay hand-off, supervision of the daemon
    ipc/                     local rendezvous per OS, shared-memory rings, wake primitives
    server/                  request handling, admission, accounting, lifecycle verbs
    cluster/                 membership, the configuration group, neighbourhoods, the register and
                             content puts with hedging, takeover, migration, mirroring
    mcp/                     MCP server and skills projection
    cli/                     `slates` binary: daemon, exec launcher, admin
    sdk-py/                  PyO3 extension (async via fd readiness)
    sdk-node/                napi-rs addon (async via uv_poll)
    sim/                     deterministic cluster simulation harness and nemeses
  sdks/python/  sdks/typescript/   the published packages (wrappers, types, docs)
  skills/                    the raw SKILL.md documents (published as-is and over MCP)
  xtask/                     release artifacts, benchmarks, conformance runners
  docs/                      this design, specs per subsystem, gap ledger, the TLA+ models
```

### B.7 macOS packaging

On macOS the release artifact is `Slates.app` (a zip or dmg from the release workflow) containing
the daemon, the `slates` command and the FSKit app extension. Installation is an explicit
human operation; the running service creates no PATH symlink or mount directory. Slates-owned
installation writes, if offered, must use the granted landing contract. The artifacts are signed with one team identifier, the
extension carrying `com.apple.developer.fskit.fsmodule`, the daemon and extension sharing an app
group in the `<team identifier>.slates` form (no registration needed); notarized for Developer
ID distribution. The raw-binary asset is still published for the CLI and for the NFS fallback
path, but the FSKit bridge requires the bundle. The Swift shim is the one non-Rust component in
the tree; it is built with `xcodebuild` in the macOS release lane and kept under a few hundred
lines by forwarding every operation to the Rust core.

---

## Appendix C — Cross-platform constraint list

A-9: this is a design constraint list, not a verified support matrix. Native host mount,
OCI namespace handoff and guest virtio-fs support must each report their own tested semantics.
The Linux guest target is POSIX on supported VMM hosts; this does not certify native Windows
or the limited NFS adapter. No automatic substitution may claim to satisfy an unsupported
requested form. The virtio-fs backend, device residency and mapping matrix are Phase 4 work.

| Constraint | Consequence in the design |
|---|---|
| i686: 32-bit `usize`; 2-4 GB address space | `usize::try_from` on every mapped size; volume caps from measured address space; reserve-then-commit in allocation-granularity units; tested, not build-only |
| i686: 64-bit atomics via `cmpxchg8b` | `#[cfg(target_has_atomic = "64")]` guards; counters that must be lock-free on all targets are 32-bit pairs where needed |
| Windows ARM64 | WinFsp `winfsp-a64.dll`; cross-compiled napi and Python artifacts; measured, not assumed, performance |
| musl | `+crt-static` binaries, `-crt-static` cdylibs; `getauxval(AT_PAGESZ)`; sysfs for cache sizes; the hot path never calls the allocator |
| macOS arm64 | 16 KiB pages; no superpages; `os_sync_wait_on_address` needs 14.4 (minimum version); thread affinity is a hint; QoS steers P/E |
| macOS FSKit | 15.4+ for the framework; URL resources 26+ (15.x needs a block resource); one-time user enablement; sandboxed extension (app-group IPC only); Swift entry point; Operations (15.4–26) and Handler (27+) protocol generations; app-bundle packaging and signing |
| macOS NFS fallback | no xattrs by default; no "forget"; weak close-to-open; `.nfs` temp files on delete-while-open; documented Degraded cells |
| Linux kernel floors | 5.10 baseline; io_uring modes 5.19/6.1; FUSE io_uring 6.14; MADV_POPULATE 5.14; futex_waitv 5.16; user namespaces may be restricted by AppArmor (Ubuntu 23.10+) |
| Windows | `WaitOnAddress` process-local (named Events across processes); asyncio needs sockets; AF_UNIX creates NTFS reparse points (not used); directory mounts are reparse points (refused); large pages need `SeLockMemoryPrivilege` |
| Stable Rust 1.98 | no nightly features; `RawWakerVTable` encodings; `std::thread::scope`; `OnceLock` |
| Containers | io_uring may be blocked by seccomp: epoll fallback is first-class; `mlock` may be limited: probed and reported |
| Linux base and landing | `O_TMPFILE` (3.11; ext4, tmpfs, XFS 3.15, Btrfs 3.16, F2FS 3.16); `renameat2` `RENAME_EXCHANGE` (3.15; `EINVAL` where a filesystem lacks it: fallback with reported window); `FICLONE` (4.5; `EOPNOTSUPP` elsewhere); `openat2` `RESOLVE_BENEATH` (5.6, under the 5.10 floor); FUSE passthrough not used (needs `CAP_SYS_ADMIN`); inotify non-recursive with `IN_Q_OVERFLOW` |
| macOS base and landing | no `O_TMPFILE` (hidden sibling names); `renamex_np` `RENAME_SWAP` advertised by `VOL_CAP_INT_RENAME_SWAP`, `ENOTSUP` otherwise; `clonefile` by `VOL_CAP_INT_CLONE` (APFS); `F_BARRIERFSYNC` for ordering, `F_FULLFSYNC` for media; `getattrlistbulk` ordering not guaranteed and undefined when mixed with `readdir`; FSEvents `MustScanSubDirs` on overflow; no asynchronous file I/O (a pool) |
| Windows base and landing | `FileRenameInfoEx` with `FILE_RENAME_POSIX_SEMANTICS` (Windows 10 1607+, NTFS); share modes are real locks (an outsider handle without delete sharing blocks the replace: reported); `FSCTL_DUPLICATE_EXTENTS_TO_FILE` on ReFS only; `ReadDirectoryChangesW` overflow returns zero bytes or `ERROR_NOTIFY_ENUM_DIR`; 64 KB buffer over the network; reparse-tag checks for containment |

---

## Amendment log (history)

The body of this document is the integrated design as of v2. The entries below record how v1
became v2, in order, with the sections each change touched. They are kept for provenance; where
an entry and the body disagree, the body wins.

### A-1 (accepted 2026-09-04) — macOS bridge: FSKit first on macOS 26+, NFSv3 as fallback and oracle
Applied in the same change to: D-2, §4.4 (attach forms), §4.6, §4.7 (macOS rendezvous), Part 2.5, Phase 4, Part 7, Appendix B.7, Appendix C, GAPS.
- Changes D-2 and §4.6 (macOS): the primary macOS bridge becomes an FSKit module (a small Swift extension forwarding the `FSVolume` handler operations to the slates core), identified per volume by a `slates://` URL resource on macOS 26+; on macOS 15.4-15.x the module can be backed by a RAM-disk block resource if the Phase 4 spike shows it acceptable; NFSv3 loopback remains the fallback for macOS 14.4-15.x and the differential oracle for the FSKit path.
- Evidence and the go/no-go spike: `research/os-filesystem-bridge.md` §7-§8.
- Consequences: the macOS artifact becomes an app bundle containing the daemon and the extension, signed with one team identifier and an app group so the sandboxed extension can share the ring region; chosen-path attaches on macOS become separate URL-identified mounts; xattrs and hard links become available on macOS; the Phase 4 task list gains the spike as its first item; GAPS gains D-O9.
- What it does not change: Linux and Windows bridges; the single-root model on Linux and Windows; the refusal of symlinks and disk-backed mount-point directories.

### A-2 (accepted 2026-09-04) — Replication model recast in EdenFS/Mononoke terms
Applied in the same change to: D-14, D-18, Part 1.1, Part 2.1, Part 2.3, §4.4 (owner loss), §4.8, §4.10, Phase 8, Part 7, GAPS.
- Changes D-14, D-18, §4.8, §4.10: three state classes with three mechanisms. Sealed content and manifests replicate by identity through a WAL-first, write-quorum W-of-N multiplex with a healer (Mononoke's mechanism), W = f+1 from the failure-domain tree. Pointers (head snapshot, lease and epoch, placement, membership) go through the one consensus group. Live working state is owner-local (the EdenFS overlay model), made durable by auto-sealing into snapshots at a cadence derived from the measured mutation rate and the operator's loss-window SLO; live op-log shipping to f+1 backups becomes an opt-in per-volume policy rather than the default, and "Raft per shard" is dropped.
- Evidence: `research/database-design.md` §7.
- Consequences: no consensus on any data path; at most W copies of sealed content and no remote copies of open extents by default; owner loss recovers by promoting the latest quorum-placed snapshot and reissuing the lease; acknowledgement semantics per verb as stated in the research section; the durability statement (D-18) gains the measured loss window per volume; §4.8's derived-constants table gains the auto-seal cadence and W.

### A-4 (accepted 2026-09-04) — Disk is the source of truth: overlay volumes, witnessed bases, drift, and landing under a human grant
Applied in the same change to: Part 0 (R1, R10, glossary), Part 1.1-1.4, Part 2.1, 2.3, 2.5, 2.6, D-2, D-6, D-12, D-16, D-18, D-22, D-23, new D-25 and D-26, §4.2, §4.4, §4.5, §4.6, §4.8, §4.10, §4.12, §4.13, §4.14, new §4.15, Phases 1, 2, 3, 4, 5, 7, 8, Part 6, Part 7, Appendix B.1, B.6, Appendix C, README rule 1, GAPS, `research/disk-source-of-truth.md` (new).
- Changes the volume model: a volume's base is either empty (scratch) or an existing host directory (overlay). Create records the directory and nothing else; untouched entries are served from disk on demand; the first write copies up and records the witnessed base (fingerprint plus BLAKE3); deletes are whiteouts; renamed base directories record their origin; drift is detected by fingerprints with git's racy-clean rule and reported, never absorbed; watchers are hints; a snapshot of an overlay volume is the delta plus the witnessed bases; overlay volumes are pinned to the host that holds the disk.
- Changes the egress model: `materialize` is the only verb that writes a host path. It plans a landing manifest proportional to the diverged entries, obtains a grant bound to the manifest's hash from a human through the CLI or a confirmation surface (never MCP or the SDKs), holds the single-holder landing lease on the target, validates every entry with the pure verdict (witnessed base, disk now, overlay now → apply, skip, accept by identity, conflict), refuses while any conflict is unresolved, writes the delta with per-file compare-and-swap (exchange and verify on Linux and macOS; share-mode-guarded POSIX-semantics replace on Windows) from arena pages in parallel with an online concurrency ramp, syncs data then directories, advances the witnesses, and records grant, manifest and outcome in the audit log. Conflicts are values; slates never merges.
- Fixes the `fsync` wording of §4.6: `fsync` through a mount never writes a disk; "durable" in slates means RAM on f+1 machines or the anchor segment until a human grants a landing.
- Evidence: `research/disk-source-of-truth.md` (hecate ADR-0005, MERGE.md §3, SESSIONS.md §1, §3, §5, §6 read directly; EdenFS; overlayfs; git racy-git; Kung & Robinson; the per-OS primitives with fetched documentation).
- Consequences: two new crates (`base`, read-only by lint; `land`, the only writer of host paths); the hermeticity tracer's exception becomes "inside a granted target during that landing, matched to a `Written` outcome"; no disk probe at boot (disk is calibrated inside granted landings); new refusals, verbs, counters and tripwires; A-3's remaining proposals (fenced head records in the multiplex, hedged placement, pre-granted placement blocks, erasure coding) stay open as `GAPS.md` D-O13.
- What it does not change: the RAM-only rule for everything that is not a granted landing; the bridges; the replication model of A-2; the refusal of symlinks and disk-backed mount-point directories for chosen paths.

### A-5 (2026-09-04) — hecate's merge architecture adapted: green volumes, increments, canonical rebase, the deterministic verdict
Applied in the same change to: Part 0 (glossary), Part 1.1-1.4, Part 2.1, 2.3, 2.5, D-14, D-16, new D-27, §4.4, §4.5, §4.6, §4.8, §4.10, §4.12, §4.14, new §4.16, Phase 1, new Phase 6 (Phases 6-8 renumbered to 7-9 with their AC and T ids), Phase 8, the dependency summary, Part 6, Part 7, Appendix B.6, GAPS, README, `research/merge-engine.md` (new). A-4's `rebase` verb (re-witnessing drifted base entries) is renamed `rewitness` so that `rebase` means what hecate means by it.
- What carries over unchanged in substance: green as a numbered chain of immutable versions written only by the merge task; increments as constant-size descriptors naming sealed content by identity; the composed-net-ops deriver with the never-diff clause; one-directional position mapping with the composition law; the two-pass verdict (sweep line, then memcmp for same-range candidates) with the classes accept, accept-identical, conflict; placed strictly before referenced; appliers recompute and cross-check, mismatch fatal-and-loud; attachments to immutable versions that move only by an explicit advance; the submission transaction (identity deduplication, park and resume, piggyback refusals, single-flight refresh); byte-exact conflict windows with rebase-and-resubmit as the corrective path; the streaming gate; the laptop degenerate; hecate's test matrix M1-M17b as AC-6.* and AC-8.11.
- Departures, each argued in `research/merge-engine.md` §2 and D-27: the proposer is a leased, epoch-fenced standing writer on green's owner shard rather than a per-session Raft leader (one shared pointer group; partitioned single-writer execution; tripwired); the deriver composes declared operations only (resolving hecate's internal contradiction); splice by extent surgery over fixed page-multiple chunks; placement holders recompute; hard links and symlinks merged per path; validation as an opaque evidence policy; excluded subtrees instead of scratch volumes; no eg-walker.
- Gaps found and closed in slates: the journal now records byte ranges and per-inode versions; POSIX overwrites and SDK inserts are distinct operation kinds; the SDK gains `edit`; whole-file rewrites conflict conservatively unless identical, and the skills say so; the `Green` and `Work` roles, the merge verbs and refusals exist; merge records are pointers; the fleet parts land in Phase 8.
- What it does not change: the landing verdict of A-4 (entry-level, because the disk declares nothing); the replication model of A-2; the RAM-only rule; the refusal to resolve any conflict on the agent's behalf.

### A-7 (accepted 2026-09-05) — Volume core as measured: parents by inode number, directory inodes naming their node, nodes carrying their name, an inline small form and a block tree, epoch-histogram accounting, clock-cut destroy slices
Applied in the same change to: §4.5 (data model), D-4 (realized form), Phase 1 (status), GAPS §1, §7, §8c, BENCHMARKS (Phase 1 baseline), `crates/vfs`.
- `DirNode.parent` is an inode number resolved through the inode table, and a directory inode's body names its current node; a node's handle held by a shared node goes stale after a copy-on-write copy, and the model-based suite found the root losing entries through such a link (`crates/vfs/tests/model.rs`, 2026-09-05).
- A node carries its own name in its parent: path building for the op log and re-pointing a parent after a copy are constant, not a scan of the parent (create 2,209 → 1,417 ns in a tree with 434-entry group directories).
- `DirEntries::Indexed` is a copy-on-write B+-tree of 4 KiB slotted blocks in a slab of the store (`crates/vfs/src/dirtree.rs`), keyed by `(hash, folded name)`; D-4's hash side index is not built (the descent probes by the hash word; the measured lookup is the fold and the compare); the ordered node is a block, not two cache lines, because the measured directory fits one block and the block is what the slab hands out and a copy moves. `Small` is inline in the node up to the measured cut-over (2 entries, 98 name bytes). Measured and rejected: the standard map with heap names (two 2.5 ms `dealloc` stalls per 10^6-file destroy; one name stored twice), and a heap name buffer per directory (growth slack). Numbers in BENCHMARKS.md.
- Accounting is a histogram of head-reachable content bytes by birth epoch; `referenced_bytes` is its total and `unique_bytes` its suffix past the newest shared epoch (last snapshot or clone origin); a clone starts with its inheritance in one bucket at the origin. Destroying a snapshot needs no recount. The write charge is the materialized delta under the chunk-window rule, which the model encodes.
- `destroy_step` takes a budget in nanoseconds of the volume's clock and weighs releases by what they free; a clone's destroy walks only nodes born after its origin (the ZFS pruned traversal).
- Clone pins on a snapshot are released by the owner of both volumes through `Volume::unpin`; a pinned snapshot's destroy is the typed refusal `Pinned` (`EBUSY`).
- What it does not change: the birth-epoch rule, deadlists, the chunk rule, quotas, the op log, the base plane, landing, the merge verdict.

### A-8 (accepted 2026-09-05) — The security and observability specifications owed before Phase 2
Applied in the same change to: §4.13 (security specification), §4.14 (observability specification), §4.4 (refusal taxonomy: `Forbidden`, `GrantChannelRefused`), Appendix B.6 (the `anchor` crate), GAPS §3.
- Security: principals established at rendezvous and never carried in a request; access lists with read, write and admin rights per verb; ids never authorize; audit counters per refusal kind, per forbidden verb, for the grant kind on a ring or MCP channel, for cross-uid connects and for fenced landing holders; grants only through the CLI's control channel or a registered confirmation surface, bound to the principal.
- Observability: the span roster (nine chokepoints with the three-id law, registered at start), the health signal catalog (every signal `(value, freshness)`, host-observed by the anchor where it can be), the metric namespace with units and labels, and the content-freedom rule as a test.
- The anchor's library gets its own crate so the daemon, the client and the CLI share one segment layout without linking the server.
- What it does not change: the rules R1–R10; the refusal taxonomy elsewhere; the wire format (the channel is already the header's class).

### A-6 (accepted 2026-09-04) — Authority and durability: Vertical Paxos II with copyset neighbourhoods, one quorum rule with hedged placement, route by id, per-operation durability scope over mirroring, ownership follows the writer, model-checked
Applied in the same change to: Part 0 (glossary), Part 1.4, Part 2.1, 2.3, 2.6, D-14 (rewritten), D-16, D-18, D-27, §4.4, §4.8 (rewritten), §4.9, §4.10 (rewritten), §4.16, Phase 2, Phase 8 (rewritten), Part 6, Part 7, Appendix B.6, GAPS, README, `research/metadata-replication.md` (new), `docs/wip/models/` (new).
- Changes the replication and authority model of A-2: every replicated object has 2f+1 candidate holders from the owner's neighbourhood; a write commits at f+1 acknowledgements from any of them and the acknowledging set is recorded; records go to all candidates, content to f+1 with hedged and tied requests to the rest; every message carries the owner's host epoch and holders refuse lower epochs; heads, chain versions, landing leases and catalog entries are registers the owner writes, never consensus entries; the regional consensus group holds only configuration (membership, neighbourhoods, host epochs, takeovers, homes) with a root group across regions; takeover is an epoch bump, assignment to the first-ranked surviving candidate, and one batched phase-one round per neighbour with no data movement on the critical path; lookups route by id with no index; every commit is mirrored asynchronously in epoch order with an exposed lag and the durability scope is per operation (`await placed(region | mirror)`); ownership follows the writer through the planned handoff.
- Closes: D-O12 (no pointer group to shard), D-O13 (A-3's remaining parts resolved: heads as fenced records in the Vertical Paxos form; hedged placement adopted; pre-granted blocks unnecessary), D-O18 (the writer is the proposer of its own registers, per object).
- Evidence: `research/metadata-replication.md` §1-§9 (Vertical Paxos read in full; FaRM, RAMCloud, Ceph, BookKeeper, Kafka and KIP-101, Chubby, PNUTS, CockroachDB, Hermes, Paxos Quorum Leases, Copysets, Spanner, The Tail at Scale, CRUSH) and the two TLA+ models with their TLC results recorded in GAPS.
- What it does not change: owner-local live state and auto-seal; the volume core; the bridges; the landing gate; the merge verdict; the RAM-only rule.

### A-9 (accepted 2026-09-05) — Correct the VFS, attachment, capacity and distributed contracts

- Authorization: Ada requested documentation/design corrections for all preceding feedback;
  this change implements no Rust behavior and closes no implementation gap. Source review is
  at Slates `a1059ed` and Hecate `103c078`; the latter is specification evidence, not deployed
  virtio-fs or consensus code. The fourteen source findings are recorded in the audit.
- Decisions: preserve delta plus retained base reference; distinguish live coverage from complete
  immutable capture; make virtio-fs first-class alongside host and OCI attachments; require
  authenticated consumers and a protected human issuer; reserve actual usable locked capacity
  including retention/transient costs; establish writeback barriers and truthful POSIX/capability
  reporting; require byte-complete recovery, correct accepted epochs, safe read authority and
  placement-before-reference; carry Hecate's bounded transfer, QoS, health and trace contracts.
- Performance: O(1) root operations and the sub-50 µs provisioning target remain. Source capture,
  mount/device creation, barriers, hashing and replication have separate costs; no new speedup
  or latency result is claimed. No implementation tests, mounted workloads or model checker ran.
- Separate implementation progress during this docs pass: `d9cb6e5` fixes BUG-12 and removes
  BUG-13's reachability restriction with recorded regression evidence. A-9 records this without
  claiming to have performed that fix or rerun its tests; broader protocol gates stay open.
- Verification owed: new AC/T rows in every affected phase and GAPS §8i. Historical TLA runs
  cover only their original models and finite configurations. §4.8 refinement/revalidation is
  required before closure under the explicit tooling authorization rules; no install or new
  CI dependency is added. Existing benchmark records are unchanged.
- Applied in the same change to: this document's status, glossary, requirements, architecture,
  D-2/D-12/D-18/D-22/D-25 and affected subsystem contracts, all phase acceptance additions,
  Appendix B/C; GAPS.md; README.md; docs/cli.md; docs/wip/README.md; EQUIVALENCE.md;
  research/hecate-contract-review.md and survey-hecate.md; current-contract notes in the
  affected research documents; docs/bugs/2026-09-05-system-contract-audit.md.

### A-10 (accepted 2026-09-08) — Cluster plane built: SWIM/Lifeguard complete, the hecate Raft dialect, Vivaldi coordinates, progress-based extension
Applied in the same change to: §4.8 (Membership, new "Slow versus stuck"), §2.6 (boot step 6 status), the `slates-cluster` crate, the `slates-db` register (phase-one promotion), GAPS §1 (Registers/configuration status).
- Authorization: Ada directed building the cluster-plane subsystems ("build all those portions… I set NO rules preventing you"), the register-commit milestone, and — after a study of hyperscale's SWIM — the three follow-ons (LHM derivation, Vivaldi coordinates, progress extension) with "do it". This change records mechanisms already implemented and gated in `slates-cluster`; the module docs carry the per-mechanism evidence.
- SWIM/Lifeguard (§4.8 Membership), sans-io and oracle-tested at N=1, then live over the simulated UDP fabric: incarnation-based membership merge with self-refutation; direct probe; indirect probe (ping-request through k relays); infection-style gossip (λ·log(N) bound); the Lifeguard local-health multiplier (a small integer cap, `health+1` to a derived `health_max` of 2–3, deliberately gentler than the raw `(LHM+1)` which over-dilates timers — hyperscale's measured correction); the confirmation-count suspicion timeout `max−(max−min)·log(C+1)/log(K+1)` with the **originator excluded** (a lone suspicion rides the full window — the bug hyperscale's chaos tests exposed and this fixed), the logarithm in deterministic fixed point; and randomized probe order (a seeded xorshift, so the simulation replays).
- The configuration group is the **hecate Raft dialect** (§4.8 mechanism 2), a pure sans-io core, now complete in its mechanism set: leader election with the §5.4.1 election restriction; log replication with the consistency check, conflict truncation and the §5.4.2 commit-safety rule; PreVote (§9.6, anti-disruption); CheckQuorum (§6.2); ReadIndex (§6.4); joint consensus (§6) — both the overlapping-majority rule and the log-integrated `C_old,new`/`C_new` transition that takes effect on append and reverts on truncation; and snapshot/log compaction with install-snapshot (§7), so the log is bounded and a far-behind follower is caught up. A multi-node conformance suite (`crates/cluster/tests/raft.rs`) checks Election Safety, Log Matching, Leader Completeness and State Machine Safety across an election, a partition, a leader change and a membership change. The `ConfigGroup` folds its committed log into the `Configuration` (reconcile from the SWIM view; takeover to the rendezvous-first survivor with an epoch bump). The dialect also rides the transport: a `RaftMessage` wire codec (`crates/cluster/src/raft_wire.rs`, hostile-input tested) and a live driver drive an election and a replication over mutually-authenticated sim-UDP sessions (`crates/cluster/tests/raft_live.rs`), the same fabric SWIM and register-commit run on. Owed: a full multi-node fleet driver with election/heartbeat timers driven continuously (the two-node live proof and the deterministic multi-node conformance oracle together cover the mechanisms and their safety), real network/process deployment, and the loom/shuttle concurrency pass.
- Phase-one promotion in the **transport-driven** register protocol (§4.8 "Promotion and takeover"): the fenced *ledger* register's phase-one committed-prefix adoption is proven in the `crates/db/src/ledger.rs` simulation (§8h; StaleNeverCommits, Continuity, prefix adoption). The Acceptor-based register protocol the cluster plane ships over the transport (`crates/db/src/register.rs` — `Record`, `Acceptor`, `commit_over_holders`) had phase two but not phase one. Built here, sans-io: `Prepare`/`Promise` wire types (hostile-input tested), `Acceptor::install_authority` (adopt the taken-over generation/owner, fencing the old writer by generation), `Acceptor::prepare` (raise the fence to the new epoch, report the highest accepted record for the object), and `promote_over_holders` (adopt the newest record across a quorum of distinct authorized promises). Oracle-tested by use: Continuity (a head committed under the old epoch to a quorum where one candidate lagged is adopted from the survivor that held it and re-commits under the new epoch), StaleNeverCommits (a resumed stale owner reaches no quorum; a write below the raised fence is refused `StaleEpoch`), and a sub-quorum promotion does not promote. The cluster plane drives it live over the transport (`serve_promotion`/`promote_record`/`promote_under_configuration` in `crates/cluster/src/lib.rs`, the symmetric counterpart of the register-commit driver; `crates/cluster/tests/promote.rs` proves an `f = 1` takeover adopts the committed head over authenticated sim-UDP sessions). This is **single-value** register takeover (heads, leases, chain-version pointers, catalog entries — adopt the current value); the multi-position committed-**prefix** adoption over the transport (the green merge-record ledger, §4.16) is the ledger generalization — **built 2026-09-10**, the ledger simulation's proof now riding the transport: `ledger::LedgerAcceptor`/`LedgerPromise` (a per-node ledger holder and the whole-log promise wire type, hostile-input tested) and `ledger::adopt` (public), with `cluster::serve_ledger_promotion`/`promote_ledger_record` driving a new owner's phase one over the fleet transport — it ships a prepare, each holder reports its whole log, and it adopts per position the identity under the highest epoch across the quorum; `crates/cluster/tests/ledger_promote.rs` proves an `f = 1` takeover adopting the committed prefix over authenticated sim-UDP sessions, recovering a record only the surviving holder still holds (Continuity), the `f = 0` degenerate observably identical (R8). Also owed: the configuration group publishing the taken-over `Configuration` and invoking the promotion from the owner runtime (the distribution `install_authority`/`promote_under_configuration` consume); real network/process deployment.
- The owner runtime composition (§2.6 boot step 6, D-14): `crates/cluster/src/fleet.rs` (`FleetNode`) composes the three built pieces — the SWIM membership view, the configuration group, and the owner's own register acceptor — into the single object the control shard holds to take part in a region, and keeps them in step: a membership event (`observe`) reconciles the neighbourhood and, when the configuration version advances, installs the new authority into the owner's acceptor so the owner writes its next records under the current generation (a holder on the new configuration refuses an older one). The laptop is the `f = 0` degenerate of the same constructor (`solo` = `new` at `f = 0`), and the design's mandated N=1≡fleet differential (R8) is a named test (`the_owner_runtime_has_identical_semantics_at_n1_and_in_a_fleet`, with a non-vacuity guard that the fleet path grows to three candidate holders before degenerating). `FleetNode::commit_head` drives a head commit through the register path against the runtime's own configuration and acceptor (disjoint-field borrow, no clone), proven live over sim UDP (`crates/cluster/tests/fleet_live.rs`) at `f = 1` and `f = 0`. This refutes the earlier "gated on scatter width / hardware" reading: the register core is f-parameterized and fleet semantics are testable in-process over the sim, so the composition is buildable and tested now. `ConfigGroup::new(owner, quorum)` is the f-parameterized single-owner constructor it uses (`solo` is `new` at `f = 0`). Owed next, each at a real boundary: the object→owner routing registry (so cross-node takeover runs per object — the single-owner `Configuration` is this node's authority over its own objects, not a per-object table); the live probe/gossip loop feeding `observe` from the detector plus the async register lifecycle driven from it; and wiring `FleetNode` into the server daemon's control shard at boot.
- Vivaldi network coordinates (Dabek 2004): each node learns a coordinate from its own measured round-trip times and exchanges it on the acknowledgement, so a node predicts the RTT to any peer and picks indirect-probe relays nearest the target. Float, but per-node local state with no cross-host bit-identity requirement (the one operational float in the cluster plane; the Vivaldi constants carry their derivations).
- Progress-based extension (§4.8 "Slow versus stuck"): a progress witness and a deadline extender let a slow-but-progressing remote-waiting operation earn a bounded extension rather than be declared failed. Built standalone; its consumer (mirror catch-up, a soft-deadline put) is owed — the local land/merge engines are cooperatively chunked and do not have the pattern.
- Evidence: SWIM (Das/Gupta/Motivala DSN 2002), Lifeguard (Dadgar/Phillips/Currey DSN 2018), Raft (Ongaro/Ousterhout 2014), Vivaldi (Dabek et al. SIGCOMM 2004); `../hyperscale/hyperscale/distributed/swim` read directly (`local_health_multiplier.py`, `detection/suspicion_state.py`, `coordinates/coordinate_engine.py`, `detection/probe_scheduler.py`, `nodes/worker/extension_trigger.py`).
- What it does not change: the rules R1–R10; the register protocol and its refusal taxonomy; the one-quorum-rule authority of A-6; the RAM-only and grant-gated-landing rules. Fleet consensus, real network/process deployment, and the SDK/mount frontier remain gated.

### A-12 (accepted 2026-09-10) — Content replication built: a sealed snapshot archived in bounded slices, placed at `f + 1` by missing set with holder-side verification, named by its head, and served by a takeover successor
Applied in the same change to: §2.6 (boot step 6 status), §4.10 (Content replication — the first slice built), GAPS §1 (Registers/configuration status), the `slates-vfs` export (`crates/vfs/src/export.rs`), the `slates-cluster` content plane (`crates/cluster/src/content.rs`), the `slates-db` op log (`SnapshotIdentified`), the `slates-server` head value, seal jobs and materialization (`crates/server/src/{head,fleet,verbs}.rs`), and `docs/bugs/2026-09-10-first-snapshot-id-is-the-none-sentinel.md`.
- Authorization: Ada's "build all of fleet" (2026-09-10); this records mechanisms implemented under that directive.
- The seal → archive walk (§4.10 "auto-seal", §4.11 "Archive", §4.3 bounded slices): a volume's newest snapshot is exported to the D-17 archive by a resumable walk whose slice is derived from the profile's measured BLAKE3 throughput and the shard's step budget (`archive_slice_bytes`), so a large volume never takes the whole step from the clients. Chunking is fixed-size at the copy-on-write chunk size (content-defined chunking and the compress-or-not cost model are the codec pass's, owed); a symlink is a file under the link type bits; a base-backed entry is refused rather than archived as zeros (§4.4 coverage is never silently upgraded). Deterministic: restore-byte-identical, the same identity whatever the slicing.
- The content plane (§4.8 mechanism 1, §4.10): three exchanges on the holder's record session, each on its own stream id — `Offer`→`Missing` (the receiver reports its missing set), `Put`→`Ack` (the manifest with exactly the missing chunks; the holder decodes with every check the archive reader makes, refuses unless every referenced chunk is held — "placement closure" — then acknowledges bound to the object, sequence and manifest), `Fetch`→`Have` (a reader fetches by identity). The first round goes to `f + 1` candidates, later rounds hedge to the rest; the round's deadline is the hedge trigger until the p95 put latency is measured (owed). The owner holds its own content, so `f = 0` is the local placement with no dispatch (R8).
- Head-before-content ordering (AC-8.2): the head register's value (`HeadValue`: the manifest identity, the acknowledging content holders, and the catalog essentials — name, size class, name policy, owner) ships only once the content is placed, and the snapshot is recorded placed durably (`SnapshotIdentified`, `SnapshotPlaced`) only once both content and head have. The catalog essentials ride the head value for now; the design's distinct catalog register class (one phase-one round per class) is the owed split.
- The takeover's content serve (§4.8 "adopts the newest records, and serves"): after adopting a head, the successor materializes the volume under its original id and mount name from the archive it holds as a content candidate, or fetches it by identity from a recorded holder, and seals the restored tree as the head snapshot at the adopted sequence with the head's identity and placement. Proven by use: a file written over the owner's NFS port reads back byte for byte over the successor's after the owner dies.
- Found and fixed on the way: a volume's first snapshot has the id the catalog's "no snapshot yet" sentinel uses (slab slot 0, generation 0), so `status` and `await placed` mistook every first snapshot for none; the discriminator is now the volume's epoch. And an observed transport hazard, recorded on the content streams: two exchanges back to back on one stream id lose the second (analysed: its frames land in the first's finished assembler before the holder forgets the stream); per-exchange stream ids avoid it, and a QUIC-style stream lifecycle is the transport's owed fix.
- Every shard is an owner (D-7, same change): the control shard alone holds the peer sessions and probes, so it hands each peer state it folds to every other shard's `FleetNode` (`apply_peer_state`) and reaches every owner shard each period through a cross-shard call (`server::xshard`, typed and deadline-bounded, the spawn-and-spawn-back the verbs and the NFS bridge already use): the seal walk and head values run on the owner shard, archives and heads move to the coordinator by value, acknowledgements and durable placements are recorded back there. A taken-over volume is materialized on the shard its id routes to, carrying the takeover's placed head — sequence, **promotion epoch** and holders — so the successor's next seals of the object are written at the epoch the holders fenced it at (§4.8 "every holder raises its fence for that host to the new epoch"), never at its lower host epoch. Runtime shard ids are process-global and never reused, so a partition index is not a shard id: the fleet's materialization and observers map a partition through the daemon's shard list, as the verbs' dispatch and the NFS bridge already do.
- Evidence: D-14/D-17 and `research/compression-archive-dedup.md` §2.6 (the archive as the replication unit, resumable by missing set); §4.10 "Placement closure" (verify before acknowledging); RFC 9000 §2.1 (stream ids are never reused within a connection — the lifecycle the transport owes).
- What it does not change: the rules R1–R10; the register protocol and its refusal taxonomy; the one-quorum rule; the RAM-only and grant-gated-landing rules; the N=1≡fleet degenerate. Anti-entropy, the healer, erasure coding, remote attach, prefetch, live shipping, migration and mirroring remain owed in §4.10.

### A-11 (accepted 2026-09-10) — Cross-node register commit built over the fleet transport; the reliable exchange's tail-loss recovery driven from the estimated PTO
Applied in the same change to: §2.6 (boot step 6 status — cross-node commit), §4.8 (Distribution — record replication now driven daemon-side), §4.9/§4.10a (the session plane's reliable exchange gains real loss recovery), GAPS §1 (Registers/configuration status), the `slates-server` fleet loop (`crates/server/src/fleet.rs`), and the `slates-transport` endpoint (`crates/transport/src/endpoint.rs`).
- Authorization: Ada's "build all of fleet" (2026-09-10); this records mechanisms implemented under that directive, closing owed items the module docs marked.
- Cross-node commit (§2.6 boot step 6, §4.8 "Content replication"): the daemon's control shard now replicates its volume heads to its peer holder. `crates/server/src/fleet.rs` `ship_records` dials the peer's advertised record address at boot (alongside the probe session, so the peer's serve side handshakes now rather than idling until first use), and each protocol period commits every unplaced volume head over that session through the register path (`commit_record` — the owner's local hold plus the remote holder, committed at `f + 1`), recording the acknowledging `Placement` in `ShardState.placed_heads`, from which the verbs' `region_placed`/`await_placed(Region)` report the head placed. The register/serve sides are the ones already built (`slates-cluster` `commit_record`/`serve_record`, `slates-db` `Acceptor`); this drives them from the daemon over real peer connections. Proven live by two in-process daemons over real loopback UDP with mutual TLS (`crates/server/tests/fleet.rs` `a_provisioned_head_replicates_across_the_fleet`): a volume provisioned on one node replicates to the peer holder and reaches the `f = 1` quorum — non-vacuous, since at `f = 1` a solo head is not region-placed and only the replicated commit places it. The record and probe wire formats are not distinguished by content on a shared stream, so each rides its own socket (the connection-ID demux that would multiplex them is owed, `endpoint.rs`).
- Session-plane loss recovery (§4.9 "Flow control", §4.10a): building the single-shot commit exposed that the session plane's reliable exchange had no real-network loss recovery — over the lossless simulation fabric a packet always arrives, but over a real datagram socket a dropped packet or acknowledgement leaves no later acknowledgement to expose the gap, so the exchange stalled forever (the periodic probe path tolerated this by retrying each period; a single commit cannot). The tail-loss probe mechanism already existed (`Connection::probe` retransmits the oldest in-flight packet); it is now driven from a real timeout. Every reliable exchange (`Endpoint::{request, serve_once, send_stream, recv_stream}`) waits for the next packet only up to the estimated probe timeout (`Endpoint::receive_or_probe` races the socket receive against the PTO, RFC 9002 §6.2.1) and, on a timeout, probes so the next flush retransmits — the timer also re-drives the receive itself, so a datagram already delivered is read on the next attempt. The handshake now seeds the RTT estimator from its own round trip (`establish`, RFC 9002 §5.1), so the timeout reflects the real path from the first packet rather than the coarse two-thirds-of-a-second initial RTT. The loss-recovery and probe paths themselves stay proven by the `connection` oracle; the fleet frame cap is now derived from the RFC 9000 §14.1 minimum datagram (a whole fleet message in one frame) rather than a value copied from a transport test.
- N-peer membership (same change): the control-shard loop now iterates the transport's peers rather than a single peer, binding one serve socket per peer per plane (`Endpoint::accept` pins one peer per socket) and spawning a probe/serve/ship set per peer; each peer's own detector folds into the shared `FleetNode` through the new peer-scoped `cluster::fleet::sync_peer`, which touches only the peer it tracks (the whole-view `sync_membership` would let one peer's detector re-join a peer another has retired, flapping it — `sync_peer` removes that coupling, unit-tested), so N independent detectors compose into one membership. Proven live at N=2 (the two-node fleet is the single-peer degenerate). N > 2 is **not yet reliable**: a fleet of N has N·(N−1) handshakes to establish over `accept` and some intermittently fail to form (a dialer's flights not reaching the peer's serve, or a partial-handshake stall), so a survivor can fail to retire a node it never managed to probe; the N=3 test is written and `#[ignore]`d pending the robust connection management the transport marks owed (a fixed-port mesh, or an accept that re-learns across a dialer's flights). Toward it, **handshake retransmission** now exists (`Endpoint::establish` re-sends its last flight each probe timeout, RFC 9002 §6.2, so a dial socket survives a peer that boots after it is dialed instead of re-dialing from a fresh port — necessary, but not on its own sufficient for the mesh). The connection-ID demux (several peers on one socket) is a further owed optimisation — an O(N) socket count for the mesh's O(N²).
- Owed still: the takeover's phase-one recovery and serving the taken-over head under the new epoch (the head-record recovery needs the held records made durable plus a promotion over the survivors; the content serve needs §4.10 content replication); the connection-ID demux (many peers on one socket); reconnection after a mid-run session loss (the ship loop redials, but the peer's accept side rebuilding is owed); full real (multi-process) network deployment; the loom/shuttle pass.
- Evidence: RFC 9002 §5.1/§5.3/§6.2.1 (RTT sampling, smoothing, PTO), RFC 9000 §14.1 (minimum datagram); the register protocol and its f-parameterization are A-6/A-10.
- What it does not change: the rules R1–R10; the register protocol, its refusal taxonomy, and the one-quorum rule; the RAM-only and grant-gated-landing rules; the N=1≡fleet degenerate (the same commit path runs at `f = 0` with the owner its own sole candidate). Multi-process deployment and the N-node transport remain gated.

### A-13 (accepted 2026-09-10) — Multi-process fleet deployment from one shared manifest: member ids from certificates, the socket map from one port block per node, the node's place in the fleet in `status`
Applied in the same change to: §2.6 (boot step 6 status — deployed as real processes), §4.8 (Membership — new "Deployment"), GAPS §1 (Registers/configuration status), the `slates-server` deployment plan (`crates/server/src/deploy.rs`) and membership config (`FleetMembership.host`), the `slates-ipc` status protocol (`FleetReport`, `ShardReport.peers_probed`), the MCP `slates.status` schema, the `slates` command (`--fleet PATH --node NAME` on `daemon` and `anchor`; `crates/cli/src/fleet.rs`; `status` fleet lines), `docs/cli.md`, `README.md`, and the real-process test (`crates/cli/tests/cli.rs`).
- Authorization: Ada's "build all of fleet" (2026-09-10); this records the deployment mechanism implemented under that directive. Until now `FleetTransport` was built only by tests: no real fleet could be started from the binary.
- One manifest, every node (§2.6 boot step 6): a JSON file naming the fleet's TLS name, `f`, and each node's name, advertised address and operator-provisioned DER certificate and key paths (relative to the manifest). Every node starts from the same file with its own `--node`; the command reads every certificate (the pins) and only its own key. Reads only — the command already reads host paths; the daemon still names none (R1).
- Member ids from certificates: a node's `HostId` is the leading eight bytes of the BLAKE3 hash of its DER certificate (`deploy::host_id_of_certificate`), the same cut the laptop takes of its machine identity's hash. The certificate is the one fact about a node every peer holds, so the ids agree fleet-wide with no registry (D-14). `FleetMembership` now carries the node's own id (`host`); `init_shard` builds the `FleetNode` over it when a fleet is configured and over the machine identity's hash otherwise — one definition of "who am I", two sources, no mode switch in behaviour.
- The socket map from one port block per node: the membership loop binds one serve socket per peer per plane (until the connection-ID demux), so rather than `N·(N−1)` operator-written address pairs each node advertises one base port and owns `2N` ports from it — it serves the node at manifest position `j` on `base + 2j` (probes) and `base + 2j + 1` (records), and that node dials it there (`deploy::serve_port`). Both ends of every session are computed from the same file; the unit test proves, for every ordered pair of plans, that a dial address equals the other side's serve bind on both planes. Refused by name: no nodes, fewer than `f + 1` nodes (nothing could ever commit), a repeated name, a repeated certificate (one member twice), a block past the port range (never wrapped), and an identity the TLS stack cannot use — the plan builds this node's server side with every peer pinned once, at boot (the construction `Endpoint::accept` makes per peer), so a key the provider rejects, a key that does not match the certificate, or a pin that cannot be a trust anchor stops the boot naming the node, rather than a mesh that never forms with only `fleet.accept` refusal counts in `status` to show for it (found by hand with `openssl`-minted material, which `ring` refuses).
- The node's place in the fleet is observable from outside the process: `DaemonReport.fleet` (`host`, `f`, `host_epoch`, `members` the membership holds alive, `peers_probed` — formed probe sessions, summed over the shards' parts since only the control shard forms any), rendered by `slates status` as `fleet_*` lines and under `"fleet"` in `--json` and the MCP `slates.status`. A laptop reports the same fields, degenerate (R8).
- Proof by real processes (`crates/cli/tests/cli.rs`, `SLATES_TEST_CLI=1`): three `slates daemon --fleet` processes, self-signed identities minted by the test as the operator would provision them, one manifest, three loopback port blocks found free by binding them; every process reports the same three certificate-derived members and both peers probed; a volume created and (where `mount_nfs` exists) written through a kernel mount on one node, sealed, places across processes at `f = 1` while its peers refuse `volume stat` for it (holders, not servers); the owner is `SIGKILL`ed; both survivors' `status` retires it (two members, one peer probed); the successor's `volume stat` answers with the name and `placed: true`; and the payload reads back through a kernel mount of the successor. The deployment proof runs wherever the CLI flow runs; the content read-back skips loudly without `mount_nfs`.
- Evidence: §4.8 "certificates provisioned by the operator" (the pin is the identity); D-14 (ids route to owners, no global catalog); the per-peer socket mesh measured in A-11/A-12 (formation 60/60, suite 25/25 under load) that the layout serves unchanged.
- Found and fixed on the way (a runtime bug the real-process test exposed 4/4): the shard's idle spin asked every registered poller whether it was ready and reported "work arrived" without waking the poller; the control loop's doorbell poller consumes its flag when asked, so a client's rendezvous claim that landed during a spin was lost until another client rang or the claimant's one-second claim wait ran out — a false "no daemon" (exit 3) against a live daemon. The spin now wakes the pollers it finds ready (`crates/rt/src/shard.rs`), gated by `crates/rt/tests/pollers.rs`; `docs/bugs/2026-09-10-idle-spin-consumes-poller-readiness.md`. The `slates` exit-3 message and `IpcError::DaemonUnavailable` now carry the client's reason (rendezvous absent, claim unanswered, daemon gone).
- What it does not change: the rules R1–R10; the register protocol, its refusal taxonomy and the one-quorum rule; the membership loop and record plane (they receive the same `FleetTransport` the tests build); the N=1≡fleet degenerate. Enrollment (§4.13 distributing certificates), the connection-ID demux (which would collapse the port block to one port per node), and the rest of §4.10 remain owed.

### A-14 (accepted 2026-09-10) — Connection ids from the TLS exporter and a per-socket demultiplexer: every peer on one socket per plane, and a re-dial replaces a lost session
Applied in the same change to: §2.6 (boot step 6 status), §4.8 (Deployment — two serve ports per node), `docs/wip/fleet-transport.md` §8 (connection ids built), GAPS §1 (Registers/configuration status), the `slates-transport` endpoint and the new demultiplexer (`crates/transport/src/{endpoint,demux}.rs`), the `slates-rt` shard run loop (`crates/rt/src/shard.rs`, the busy-shard I/O harvest), the `slates-cluster` deadline-raced request (`crates/cluster/src/lib.rs`), the `slates-server` membership loop and deployment plan (`crates/server/src/{fleet,deploy}.rs`), the fleet test harness (`crates/server/tests/fleet.rs`), the CLI deployment test (`crates/cli/tests/cli.rs`), `docs/cli.md`, and the bug records `docs/bugs/2026-09-10-{busy-shard-never-harvests-io,abandoned-content-stream-poisons-reused-session}.md`.
- Authorization: Ada's "build all of fleet" (2026-09-10); this records the mechanism implemented under that directive, closing the owed connection-ID demux and the owed reconnection after a mid-run session loss.
- The id (§4.10a §8, RFC 9000 §17.2/§17.3): every 1-RTT short header carries an eight-byte destination connection id. Neither end chooses or negotiates it: both derive it from the TLS exporter (RFC 8446 §7.5, a label private to this dialect) once the handshake completes, so it is unique per session (a function of the session's secrets) and equal at both ends with no wire exchange (`Endpoint::connection_id`; unit-tested: both ends derive one id, two sessions derive different ids, none before completion). Header protection leaves it in the clear (RFC 9001 §5.4.1 masks only the first byte's low bits and the packet number), so a receiver routes by it before any crypto; a packet naming another session is refused at the header.
- The demultiplexer (`crates/transport/src/demux.rs`): owns one socket and its receive loop; routes a raw handshake datagram by its source (a TLS handshake message's first byte never has the short header's fixed bit set) and a 1-RTT packet by its id; opens a server session for a source it has not heard from and hands it to an `accept()` consumer; closes the session previously established under the same peer certificate when a new one binds (the peer re-dialed after losing its session — its old serve loop ends `Closed`, counted `replaced`); counts unknown-id and inbox-overflow drops and refused sessions. Bounded: `max_sessions` slots named by generational handles (the fleet passes two per peer — the live session and its replacement), an inbox of `INITIAL_WINDOW_DATAGRAMS + REORDER_THRESHOLD + 1` datagrams per session (a full initial congestion window, reordered, still lands; past it the peer's probe retransmits), one leaked demultiplexer per socket per boot. An endpoint on a shared socket names its demultiplexer by an id in the shard's thread-local table, never by reference, so `Endpoint` stays `Send` while the demultiplexer's state stays a `RefCell` on one thread (no lock, no `Arc`). `Endpoint::accept` (pin the first source) is replaced, not kept.
- The fleet on it (`crates/server/src/fleet.rs`): a node binds two serve sockets — probes and records — for the daemon's life and spawns each plane's receive and accept loops; every accepted session gets its own serve task (the record side resolves the peer from the certificate the handshake authenticated, against the roster mutual TLS admits); a retired peer's sessions are closed. The deployment manifest's advertised port is now the node's probe port and the next its record port (`deploy::serve_port`), for any fleet size; the test harness allots one pair per node.
- Proof: `crates/transport/tests/session.rs` — two clients dial one accepting socket and each gets its own reply (two sessions opened, none refused, no packet to an unknown id, both slots released when their serve tasks end); a stray packet naming no session is dropped and counted once while the live session serves on; a peer that re-dials with the same identity from a fresh socket replaces its old session (the old serve loop reports ending `closed` after one request, the new one serves; `replaced` = 1); the single-session degenerate. The twelve-test fleet suite and the three-process deployment test pass unchanged in behaviour.
- Evidence: RFC 9000 §5.1/§17.2 (connection ids as the routing key, independent of address), RFC 8446 §7.5 (the exporter as a per-session secret both ends share), RFC 9001 §5.4.1 (what header protection masks); the per-peer socket mesh this replaces measured 25/25 under load (A-11), which the demultiplexed one must match (the fleet suite is the gate).
- Found and fixed on the way (the demultiplexer's one extra task hop per datagram made three latent session-plane defects show under load — the three-process deployment test under eight CPU spinners fell from 12/12 to 5/12 before these, and is 12/12 after; `docs/bugs/2026-09-10-abandoned-request-retransmit-lockstep.md`): (1) a forgotten stream's frames were still retransmitted from packets in flight and from the lost-frame queue, so a probe abandoned at its deadline kept re-asking the peer with the stale request, and (2) its late reply was left for the next exchange on the id to read as its own — together a permanent one-reply-behind lockstep the SWIM nonce check turned into five misses and a false retirement of a live peer; now `forget_stream` drops the stream's frames from flight (RFC 9000 §2.4) and `request` forgets the id before opening (unit-tested: the probe path shown live, then closed). (3) Every idle session armed the estimated probe timeout — a few hundred microseconds on loopback — waking thousands of times a second with nothing in flight; the timer is armed only with ack-eliciting packets in flight (RFC 9002 §6.2.1), the conservative initial PTO otherwise, and it backs off exponentially over consecutive expirations up to that initial PTO, so a dead peer is retransmitted to a few times a second, not thousands. Also: an established session discards a raw handshake retransmit (answering it — a server resends its confirmation, a client its final flight — so the peer completes) and any packet that does not open under the keys or names another session (RFC 9000 §12.2), instead of failing the session; the frame-overhead bound gained the eight id bytes; the serve-socket counters (`unknown_id`, `inbox_full`, `sessions_refused`, `replaced`) are exported in `status` and `slates.status`, and the deployment test asserts them clean at formation.
- Found and fixed on the way, part two (the in-process content-placement fleet tests — two daemons and the test's `await placed` poll sharing the machine — stalled deterministically where the three-process shape passed; two more defects, one of them a runtime-level fairness bug, `docs/bugs/2026-09-10-busy-shard-never-harvests-io.md` and `docs/bugs/2026-09-10-abandoned-content-stream-poisons-reused-session.md`): (4) **a continuously busy shard never harvested driver I/O.** The run loop polls the driver for socket readiness only when it parks; a shard whose client never idles re-queues its serve loop every step (`yield_now`), so the loop `continue`s forever and never parks, and the demultiplexer's single receive task — waiting on socket readiness — is starved while the shard spins serving one client, so the record plane's put to a holder times out. The run loop now harvests the driver **without blocking** once a busy run has gone a step budget (`step_budget_ns`, the design's own "a step longer than a peer's wake starves the shard") since its last wait, so an I/O-bound task keeps pace with a CPU-bound one under any load and an idle shard pays nothing (`crates/rt/src/shard.rs`). (5) **an abandoned content exchange poisoned the reused session** — `cluster::request_within` races `request` against the round deadline and keeps the warm session, but left the abandoned stream open, so its stale bytes rode the next exchange's flush and `serve_once` folded them into an unrelated request; now the caller forgets the abandoned stream (`Endpoint::forget_stream`) and `serve_once` buffers per stream and serves only the completed stream's own bytes. This corrects the sibling-sweep in the lockstep bug doc, which had wrongly held that no caller abandons a content exchange.
- What it does not change: the rules R1–R10; the register protocol and refusal taxonomy; the membership loop's tasks and the record plane (they receive the same `Endpoint`s); the N=1≡fleet degenerate. Multi-stream multiplexing and the MTU budget remain owed in §4.10a §8; enrollment (§4.13) and the rest of §4.10 remain owed.

### A-15 (accepted 2026-09-11) — A retired peer rejoins by SWIM refutation: the serve side re-admits it, and its probe loop idles rather than ending
Applied in the same change to: §4.8 (Membership — a retired peer that returns is re-admitted), the `slates-server` membership loop (`crates/server/src/fleet.rs`), the daemon's fault-injection observer (`crates/server/src/daemon.rs`), and the fleet test harness (`crates/server/tests/fleet.rs`).
- Authorization: Ada's "build all of fleet" (2026-09-10) and the sequencing "finish the focused rejoin first" (2026-09-11). Closes the owed rejoin path.
- Scope correction (2026-09-11): this re-admits a peer retired by a **false suspicion** — a still-live node whose RAM (its holds, fences and records) is intact, refuting a transient network or scheduling glitch. A **restart is not this path**: a RAM-only node that restarts has lost all its state, so it must rejoin as a *new* member with a *fresh ephemeral id* (the old id stays dead and its objects are taken over — §4.8 line ~1801, RAMCloud's recovery model), never be re-admitted under its old id with a reset fence that would accept a stale low-epoch record (a StaleNeverCommits hazard). The membership id is therefore ephemeral (per boot) while the certificate is the stable authenticated identity; A-13's certificate-*derived* stable host id is the piece to change (peers known by certificate and address, ids learned on contact) — sequenced as the next correctness fix. Refutation below covers only the false-positive case.
- The mechanism (§4.8 "Membership fed by SWIM", D-14): a peer the fleet retired by a false suspicion is re-admitted when it comes back, by SWIM's own incarnation refutation, with **no separate death-incarnation tracker or rejoin bump**. slates already keeps a dead member as `{Dead, incarnation}` in the membership view and self-refutes on hearing its own death ([`Membership::refute`] raises the incarnation past what it heard), so the whole rejoin is one wiring fix: the probe **serve** side (`serve_peer_probes`), for the peer its handshake authenticated (its certificate in the roster — a ping's `from` is unauthenticated), reads this node's belief about that peer and, when it is not alive, **echoes it** in the acknowledgement; the returning peer applies that to itself, refutes to a higher incarnation, and gossips its new life, which the serve side then **folds** into the shared `FleetNode` (scoped to the authenticated prober — the `sync_peer` discipline, so it can never flap a third peer). A higher incarnation always overrides the death, so the stale death cannot re-retire the re-admitted member; the fold needs no bump because the refutation already carries one.
- The probe loop **idles rather than ending** on retirement (`probe_peer`): a believed-dead peer is never dialed (that establish would block on a peer that will not answer — the reason a prior "re-dial after K misses" was reverted), and no probe session is held; when the peer rejoins, the neighbourhood regains it and the loop resumes, realigning its detector to the re-admitted belief so it tracks the peer as alive and can detect a *future* death. The record link (`establish_record_link`) idles the same way. No supervisor re-spawns anything — the persistent tasks self-heal.
- Pushed past the reference (hyperscale's Python SWIM, which needs a per-peer death-incarnation tracker, a `minimum_rejoin_incarnation_bump`, and an out-of-band `reset_peer_for_rejoin` RPC): slates carries the death incarnation in the membership view it already keeps, and `refute` supplies the bump, so re-admission is the ordinary incarnation-override merge with none of that apparatus. The **authority** stays the configuration group (D-14): SWIM only detects; a re-admission is an `Admit` the group reconciles, and a returning host adopts the configuration's current host epoch (bumped by the takeover that retired it), which fences its pre-death records — stronger than incarnation refutation alone.
- Proof: `crates/server/tests/fleet.rs::a_falsely_retired_peer_rejoins_by_refutation` — two live daemons form the direct probe mesh; one is made to falsely retire the other (`Daemon::observe_peer_dead`, the same fold the detector performs when it ages a peer to death); the retired peer, alive and still probing, learns of its death from the echo, refutes, and is re-admitted, and stays admitted (no flap). 5/5 serialized; the full fleet suite 13/13. A real process kill cannot be exercised in-process — the demultiplexer's serve socket is `Box::leak`ed to the process's lifetime and cannot be rebound where a live deployment's OS would free it — so the test injects the (false) death and drives the recovery over live sessions instead.
- What it does not change: the rules R1–R10; the register protocol, its refusal taxonomy and the one-quorum rule; the configuration group as the membership authority (SWIM detects, the group decides); the N=1≡fleet degenerate (a laptop has no peers to retire or re-admit). Bounded scatter-width neighbourhoods and the configuration group live over the transport (the Meta-scale membership work) remain owed and are the next fleet pieces; enrollment (§4.13) and the rest of §4.10 remain owed.

[`Membership::refute`]: the SWIM self-refutation in `crates/cluster/src/membership.rs`
