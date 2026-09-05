# Hecate contract review, 2026-09-05

This review supplies the evidence for amendment A-9 of `../SLATES_DESIGN.md`.
It follows Ada's requirements in the September 5 conversation: intuitive CLI and MCP;
robust metadata servers; RAM-only volume storage; POSIX paths for host processes,
OCI containers and MicroVMs; accurate local and remote bases; protected capacity
claims; and the correction that delta storage is preferred when it retains its base.

## 1. Evidence and limits

The reference is `../../../../hecate` at commit `103c078`. Its `docs/GAPS.md` §0
explicitly says it has no code. Hecate supplies an accepted design, not deployed-code
evidence or a performance measurement. Slates was inspected at `a1059ed`; existing
uncommitted archive edits were outside this review. Commands were read-only:
`git rev-parse --short HEAD`, `git status --short`, `rg`, and `sed` over the cited files.
No benchmark, executable regression, mount, model checker or tool installation ran.
Source-derived counterexamples are recorded in `../../bugs/2026-09-05-system-contract-audit.md`.

## 2. Contracts to carry across

| Contract | Hecate source, relative to its root | Slates disposition in A-9 |
|---|---|---|
| One guest protocol across host OSes; owned FUSE-over-virtio device integrated with the custom runtime | `docs/specs/SERVING.md` §7; `docs/adr/0001-forked-libkrun-microvm-isolation.md` | First-class virtio-fs attachment over the same volume core; native host bridges remain necessary for host tools. No dependency on a VM for host use. |
| Claimed mounts only; attachment pins identity, version, access and accounting | `docs/specs/VFS.md` §3b; `docs/specs/MERGE.md` §6 | An authenticated consumer binding checked at every effect; explicit attach, advance, drain and revoke lifecycle. Hecate's detailed Branch 36 lifecycle is unfinished and is not treated as an implementation. |
| Distinct workload principals and host-side pre-effect enforcement | `docs/specs/IAM.md` §1, §6, §8; `docs/specs/PODS.md` §6 | Separate OS identity from a harness-enrolled consumer identity. A same-UID process is not automatically the human confirmation authority. Slates enforces its serving boundary; the harness contains other process effects. |
| Delta over a pinned baseline; unchanged chunks shared | `docs/specs/VFS.md` §2; `docs/specs/SESSIONS.md` §3 | Keep a base reference in every clone. Distinguish live host views from complete immutable snapshots. Never silently turn a remote overlay into a scratch tree missing its base. |
| Drain client writeback before seal; dirty content cannot bypass witnessing through DAX | `docs/specs/SERVING.md` §2, §3, FS4, FS7 | A consumer barrier precedes snapshot publication, submit and clean detach. Immutable DAX mappings require isolation and revocation proofs; mutable mappings cannot bypass server checks. |
| Atomic admission and complete charges | `docs/specs/SCHEDULER.md` §5a, §6; `docs/specs/CACHE.md` §2; `docs/specs/PODS.md` §3 | Reserve usable host capacity, including metadata, fragmentation and operation headroom; protect accepted claims from dynamic growth and cache fill. Slates still owns locking and live-resize design, absent in Hecate. |
| Control latency independent of another traffic class's object size | `docs/specs/PROTOCOL.md` §3 | Carry isolation through credits, queues, CPU slices, arena admission and device rings; saturation tests include replication and cleanup. |
| Content placed before references; holders recompute before serving | `docs/specs/MERGE.md` §5, §7 | Retain the existing design law, but mark integration as incomplete. Identity-only protocol simulations do not establish content recovery. |
| Pure message-driven consensus, named production regressions and whole-cluster faults | `docs/specs/CONSENSUS.md` §2–§4, §9–§10; `docs/specs/FAULTS.md` §4 | Preserve the configuration-only consensus decision. Add concrete conformance mapping and arbitrary asymmetric message histories. Never call direct-call simulation a proof of the deployed protocol. |
| Named content resumes by missing set; verify before release; publication is distinct from transport credit | `docs/specs/TRANSFER.md` §1–§5 | One bounded RAM transfer path for remote clone, repair, archive and replication; separate received, verified, placed and referenced outcomes. |
| One typed definition generates wire and agent-facing contracts | `docs/specs/SKILLS_API.md` §1, §3–§4 | Generate schemas, typed refusals, SDK contracts and help from one operation definition; CLI and MCP lifecycle semantics agree. Human grant authority is absent from agent schemas. |
| Missing signals have declared meaning; trace/request/causation are distinct | `docs/specs/HEALTH.md` §1; `docs/specs/TRACING.md` §1–§2 | Typed absence semantics, propagated trace context, loss markers, and no free-text fields in operational signals. Volume and principal IDs are tags, not replacements for trace context. |
| Clean-file digest xattr and bounded cache fill | `docs/specs/SERVING.md` §5, FS12, FS15 | Expose verified content identity only while valid; a mutation removes it until reseal. Cache misses and crawls stay inside their admitted memory and work budget. |

## 3. Delta storage and base identity

The performance choice is **delta plus retained base reference**. A complete logical
manifest can share its unchanged directory nodes and chunks; completeness does not
require a full physical copy. Remote reads fetch only missing content and verify it.
No latency or bandwidth advantage is claimed as a new measurement here.

A live directory reference is a dependency on the host serving it. It preserves current
host-file semantics but cannot promise a fixed historical value after an outsider edit.
A portable immutable snapshot must retain the complete referenced content within its
declared placement scope. Capturing an arbitrary changing directory atomically cannot
be achieved by a lazy reference or a sequence of stats alone: source quiescence or an
appropriate read-only snapshot facility is required for a single-point-in-time claim.
Without such a source, capture must detect observed drift and state its weaker guarantee;
it must not label the result a point-in-time host snapshot. A-9 specifies the typed refusal
for a requested guarantee that the source cannot supply.

## 4. Deliberate departures

Slates keeps RAM-only content and granted landing. Hecate's content-bearing disk WAL,
pack-volume NVMe tier and power-failure acknowledgement contract do not transfer.
Neither do its prohibition of hard links, inferred seal-time diffs, agent roster,
claims governance, full IAM product or VMM ownership as a requirement for host clients.
Slates uses configuration consensus and per-object fenced registers, rather than
Hecate's per-session Raft writer. That departure retains independent proof obligations,
particularly owner-local read leases under pauses and clock uncertainty.

Transport encryption, dedup authorization and mapping isolation are distinct concerns.
An identity or successful missing-set query never grants permission to read content.
Private sharing scopes must be enforced at the serving and cache boundaries without
importing Hecate's at-rest encryption design merely because it exists.

## 5. External checks used in the source audit

- Linux's [FUSE ABI](https://github.com/torvalds/linux/blob/master/include/uapi/linux/fuse.h):
  `FUSE_WRITEBACK_CACHE` is bit 16, not bit 8. Checked September 5, 2026.
- The [OCI runtime mount specification](https://github.com/opencontainers/runtime-spec/blob/main/config.md):
  host containers can receive bind-mounted sources; the runtime must be able to resolve
  the source in its own namespace. Checked September 5, 2026.
- [virtio-fs](https://virtio-fs.gitlab.io/) describes guest/host filesystem sharing.
  The chosen VMM still needs an integration for its device and memory-mapping APIs.
- [mlock semantics](https://man7.org/linux/man-pages/man2/mlock.2.html): an anonymous
  mapping alone is not a residency guarantee. Locking and full accounting are separate
  from avoiding explicit filesystem writes.
