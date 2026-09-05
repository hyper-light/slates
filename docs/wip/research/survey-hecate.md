# SURVEY: hecate — decisions, vocabulary, and documentation style slates can reuse

Status: research survey, 2026-09-03 (consolidated from two survey passes the same day;
every GRILLING.md and spec citation below was re-verified by direct read on 2026-09-03 —
see Appendix B). Source repo: `/Users/adalundhe/Projects/hecate`
(docs-only; `GAPS.md` §0 line 14: "**No code exists.** The repo is docs-only (zero
Cargo.toml, zero .rs)"). Every claim below cites a file and line range in that repo;
quoted text is verbatim. Where this survey offers an opinion for slates it says so
explicitly ("slates note", §8). This survey does not propose a slates design; it
records what hecate decided, why, and what it left open.

Files read in full: `CONTEXT.md`; `docs/architecture/{PLATFORM,SUMMONING,SKILLS,LEDGER,
AGENTS}.md`; `docs/adr/0001–0006`; `docs/GAPS.md`; `docs/specs/{VFS,STORE,WAL,CONSENSUS,
PROTOCOL,WIRE_FORMAT,WIRE_SECURITY,ARCHIVE,OBJECT_TIER,CACHE,MATERIALIZER,SERVING,QUEUE,
LEDGER_SUBSTRATE,LEDGER_CORE,SESSIONS,FAULTS,TRANSFER,RUNTIME,HEALTH,MONITORING,TRACING,
SKILLS_API,IAM,SCHEDULER,PODS,MERGE,AGENTS_RUNTIME}.md`; `COLLECTOR.md` headings only.
`GRILLING.md` (8,861 lines, 631 KB) was grepped for every term in the brief and read in
the ranges cited below (its structure: L1–30 standing rules; L31–103 SETTLED table;
L104–130 ON THE TABLE; L131–1261 OPEN BRANCHES; L1262–1296 cost ledger; L1297–8861
research reports and decision log, newest last). Paths given as `PLATFORM.md`,
`SUMMONING.md`, `SKILLS.md`, `LEDGER.md`, `AGENTS.md` are under `docs/architecture/`;
all other `*.md` specs are under `docs/specs/`.

Grep census over `GRILLING.md` (lines matching, case-insensitive unless noted): VFS 45 ·
volume 68 · attachment 15 · lease 95 · materializ 82 · prefetch 5 · consensus 151 · Raft 77 ·
Paxos 6 · etcd 24 · WAL 205 · store 154 · memfd 12 · FUSE 48 · NFS 20 · ProjFS 1 · tmpfs 8 ·
disk 39 · `Arc`/`Rc` (Rust, case-sensitive) 9 · allocation 7 · arena 50 · io_uring 7 ·
thread-per-core 2 · lock-free 9 · laptop 88 · degenerate 42 · **hermetic 0 · WinFsp 0 ·
macFUSE 0 · FSKit 0 · mlock 0 · hugepage 0**. The zeros matter for §8: hecate never
discussed a host-OS mount technology or page pinning, because its consumers are Linux
microVM guests, never host tools.

---

## 0. The standing rules (the author's own process law)

These are the rules the author imposed on the design dialogue. They are the single best
description of how the author wants a spec written and argued, and slates should adopt
them wholesale. Quoted verbatim from `GRILLING.md:7-29`:

> ## Standing rules (user-established; enforce every exchange)
>
> 1. **Ledger scope test**: the ledger = proof of work — claims, testaments,
>    validations, artifacts, nothing else. Config→config; ops→logs; working
>    state→its owning service. (User corrected twice; never again.)
> 2. **One decision per exchange, worked to settlement.** No bundle ratifications.
>    Assent without a shown spec = direction only, spec owed. Specs are shown
>    IN-MESSAGE before any file is written (violated once with the Sibyl draft —
>    deleted and re-presented; do not repeat).
> 3. **Research before argument** for every substantial decision; receipts inline.
> 4. **Maximal test**: every design examined for maximally correct/robust/
>    performant/efficient at BOTH laptop and Meta scale; laptop is always the
>    derived degenerate of one formula family — no modes, ever.
> 5. **No ambient anything crosses a session fence.** Cross-session = same-user
>    Archivalist recall or Guardian-staged registry publication only.
> 6. **Agents never allocate**: they issue claims; machinery executes; issuers
>    monitor/evaluate. Judgment above, deterministic consequence at chokepoints.
> 7. **Coined names are provisional** until mechanisms settle and the glossary
>    captures them. "Workspace" is RETIRED (grep-gated) → lineage / session /
>    work volume.
> 8. **User review is a first-class gate** (default prompt for materialization).
> 9. Constants derive from anchors with derivations at definition sites; floors
>    ratchet from first CI baseline; observe-mode-first for any new influence.

Rules 2, 3, 4, 7 and 9 transfer to slates unchanged. Rule 1 (ledger scope) is
hecate-specific — slates has no claims ledger — but its *shape* ("config→config;
ops→logs; working state→its owning service") is a good partition rule for any service.
Rule 6 ("agents never allocate") becomes, for slates, "agents never allocate
*directly*; the service allocates on a typed request and the caller monitors the
result" (§8). Rule 5 becomes the volume-isolation law: nothing ambient crosses a
volume/tenant fence.

Three further process laws sit outside the numbered list and matter for style:

- **Status-line authority** (`GAPS.md:454-464`, the C-7 rule; quoted in §7.1): a
  spec's `Status:` header is the single source of truth for acceptance state, and the
  GRILLING "SETTLED" table means only *direction-settled*, "strictly weaker than
  spec-accepted — never read it as acceptance."
- **The stale-ledger rule** (`GAPS.md:8-10`): "**A stale gap ledger is itself a
  gap**: every branch acceptance, spec verdict, or tripwire firing updates this file
  in the same change." The same-change doctrine generalizes it: companion amendments
  land in the same commit as the acceptance that caused them (§7.6).
- **Overrules are recorded, not argued away.** When the user overrules a
  recommendation the record keeps both the verdict and the overruled advice, e.g.
  `GAPS.md:221-225`: "**SETTLED 2026-08-18 (user, verbatim): "no fallback. Period.
  QUIC + UDP over TCP utilizing the standard(s) we just designed."** … (my
  telemetry+contingency recommendation overruled — recorded per discipline)"; and
  `GRILLING.md:3103-3110`: "ARENA-REUSE OVERRULED (user, 2026-08-18): "That doesn't
  even make any sense. …" The "reuse the ledger's in-memory edge arena" framing is
  REJECTED and recorded as the architect's error". The no-panic law arrived the same
  way (`RUNTIME.md:3-6`: user correction "we do NOT panic. Period. Ever.").

---

## 1. Vocabulary and concepts slates should reuse

### 1.1 The glossary (`CONTEXT.md`) — verbatim definitions

`CONTEXT.md` is a glossary with a `**Term**:` / definition / `_Avoid_:` pattern; the
corpus rule is "Where this document and the glossary disagree, the glossary wins"
(`docs/architecture/AGENTS.md:17-18`). The entries slates should lift, with line numbers:

- **Lineage** (`CONTEXT.md:39-41`): "The fork tree over a body of work — baseline
  manifests, fork relations, and its materialization target (a local tree or a
  source-control ref). Sessions attach to lineage nodes; the lineage outlives its
  sessions; the materialization lease is lineage-scoped." `_Avoid_: workspace, repo binding`
- **Session** (`CONTEXT.md:43-45`): "The isolation and namespace unit: an independent
  workstream attached to a lineage node, born template-stamped (never bare) with its own
  colocation unit, key root, and pods — nothing inside a session is reachable from another
  absent a brokered grant. **A session spans machines**: its pods place on any nodes; only
  its home services colocate; its volumes attach from anywhere. Forkable, mergeable via
  landing, disposable; proof outlives it in the archive." `_Avoid_: workspace, environment`
- **Colocation unit** (`CONTEXT.md:47-48`): "The session's home services — ledger core,
  merge service proposer, frontier, field service — placed together on one node for
  locality, with their logs replicated across the session group. The unit is the
  services, never the session: pods are not in it and place anywhere."
- **Attachment** (`CONTEXT.md:50-51`): "The per-(pod, volume) control object created at
  bind: pins the version, holds the lease, wires warden scopes, runs prefetch, carries
  accounting. A pod's view of a volume changes only through a re-attach. No claim, no
  attachment, no mount."
- **Summon** (`CONTEXT.md:53-55`): "A claim requesting allocation of a workload — pods,
  VFS volumes, permissions, network endpoints, agent assignment, health validation.
  Issued by an orchestrating agent (the Guide, the autoscaler as system participant),
  **executed by the scheduler**, gated by Guardian admission validations on the same
  claim, and monitored and evaluated by its issuer like any work. No agent allocates
  directly." `_Avoid_: spawn, activate, mint`
- **Lease** (`CONTEXT.md:114-115`): "An optimistic, expiring write-basis snapshot used to
  guide work and skip merge effort where changes are provably disjoint. A work-reduction
  aid — never a substitute for real conflict detection at the merge." (Note for slates:
  this is the *merge* lease. hecate also uses "lease" for Chubby-shaped standing-writer
  leases in `CONSENSUS.md:219-233` and for the lineage-scoped *materialization* lease in
  `SESSIONS.md:23-25` — three meanings under one word; see §8.)
- **Failure-domain tree** (`CONTEXT.md:117-118`): "The physical containment hierarchy —
  node, availability zone, region — against which placement, quorum spread, and authority
  scoping are expressed. A laptop is a depth-one tree; every topology collapse is derived
  from the tree, never switched by a mode."
- **Epoch scope** (`CONTEXT.md:120-121`): "The failure domain in which a fencing/epoch
  authority lives: the smallest domain containing every legal holder of the fenced
  resource, so the resource and its authority always die together."
- **Provenance class** (`CONTEXT.md:76-77`): "Every signal's trust label — host-observed
  or guest-reported. Authority decisions read host-observed only; divergence between the
  two is itself a signal."
- **Trace** (`CONTEXT.md:85-87`): "The execution story of one operation across subsystems
  — the tree of spans sharing one trace id, from the root that minted it through every
  chokepoint it crossed. Operational signal, sampled-for-keep; distinct from the claims
  graph (work proof) and from request/response pairing (transport)." `_Avoid_: conflating
  with caused_by or the claims graph`
- **Span** (`CONTEXT.md:89-90`): "One chokepoint's timed slice of a trace — its ids, a
  chokepoint name from a closed registry, timing, a typed status, bounded tags.
  Content-free; emitted async, never blocking the operation it measures."
- **Trace context** (`CONTEXT.md:92-93`): "The tag every wire message carries — trace id,
  the sender's span id, and the keep flag decided at the trace root. Rides the encrypted
  envelope; a message minted outside any traced operation roots a fresh trace."
- **Witness** (`CONTEXT.md:134-135`): "The serving boundary's durability contract for guest
  writes: the write is journaled and group-committed before the reply — acked means
  durable. No unwitnessed byte can exist in a work volume; unacked bytes are work-bearing
  memory and die with the pod."
- **Seal** (`CONTEXT.md:137-138`): "The freeze of a work volume's journal epoch at increment
  submission: guest writeback drained, per-file edit ops derived, content chunked into the
  content store, manifest updated. What seals is exactly what the agent fsync'd; sealed
  content is immutable and cache-coherent by construction."
- **Pod** (`CONTEXT.md:57-59`): "The unit of agent placement — a microVM running one primary
  agent and its Scribe companion, one OCI container per agent loop, with its volumes and
  network identity." `_Avoid_: process, goroutine, "one agent per pod"`
- **Delta** (`CONTEXT.md:27-29`): "An immutable fact emitted after a ledger mutation commits
  — authoritative and self-sufficient, never a hint or a UI decoration." `_Avoid_: event,
  notification`
- **Participant** (`CONTEXT.md:31-32`) / **Principal** (`CONTEXT.md:163-164`): "Wire format
  and lifecycle never branch on the category." / "The kind is a read field on the record;
  wire format and lifecycle never branch on it."
- **Affordance** (`CONTEXT.md:34-35`): "... The response to an unmet affordance is inform or
  yield — refusal is reserved for structural invariants".
- **Ceiling** (`CONTEXT.md:166-168`): "A policy that only caps, never grants ... a decision
  must pass every applicable ceiling (they intersect)."
- **Grant** (`CONTEXT.md:173-174`): "A narrow, evaluated, epoch-stamped, durable
  authorization decision made durable ... The authoritative row lives in the authority
  plane; the Biscuit token is its portable projection."
- **Skill** (`CONTEXT.md:148-149`): "A typed, code-defined capability authored against the
  harness API and published over MCP as tools plus a `skill://` instructional resource.
  Never raw markdown at the source."
- **Registry** (`CONTEXT.md:145-146`): "The slow plane and the extension surface ...
  Never a runtime authority: it cannot route, gate a transition, or author claims."
- **Warden** (`CONTEXT.md:82-83`), **Hard block** (`:98-99`), **Merge gate** (`:123-125`),
  **Landing** (`:127-129`), **Conflict value** (`:131-132`) — hecate-specific, quoted in
  §8 where their inapplicability is discussed.

### 1.2 Spec-level vocabulary (not in the glossary, used everywhere)

- **Laptop degenerate / N=1** — the standing law, `GRILLING.md:17-19`: "**Maximal test**:
  every design examined for maximally correct/robust/performant/efficient at BOTH laptop
  and Meta scale; laptop is always the derived degenerate of one formula family — no modes,
  ever." Every mature spec ends with a "Laptop degenerate" section that reads like
  `STORE.md:553-559`: "`N=1`: one node; every shard's group is 1-voter ... Same code, zero
  modes." and a paired acceptance criterion "`N=1` ≡ fleet" (e.g. `STORE.md:621`, ST15).
- **Constants-from-data / derivation at the definition site** — `GRILLING.md:28-30`:
  "Constants derive from anchors with derivations at definition sites; floors ratchet from
  first CI baseline; observe-mode-first for any new influence." The mechanism is a probe at
  boot (`WAL.md:56-74`) and a **Derived constants** table per spec with columns
  `Constant | Formula | Anchors` (`STORE.md:510-523`).
- **Ratchet** — a performance floor established by the first measured CI baseline that may
  only tighten; "any regression >10% fails CI" (`RUNTIME.md:186-191`). `GAPS.md:503-504`
  itself flags the 10% as "one hand constant repeated four times".
- **Chokepoint law / boot classifier** — every cross-component access crosses a named,
  boot-registered boundary; "an unclassified transport path fails startup"
  (`WIRE_SECURITY.md:170-177`); the span roster *is* the chokepoint registry
  (`TRACING.md:141-155`).
- **Typed error / typed refusal / "loud"** — no string errors, no silent drops: "every silent
  drop is categorized and aggregated ... a drop with no signal is a bug"
  (`LEDGER.md:321-323`; `PROTOCOL.md:300-304`); a closed **refusal & loss taxonomy** per
  spec, e.g. `STORE.md:501-508`.
- **Masked / Degraded / Refused** — the three cell values of the failure×obligation matrix
  (`FAULTS.md:127-134`).
- **Structural / unrepresentable / physics vs policy** — the recurring test of a design:
  make the fault impossible to express (types, lints, kernel fd modes) rather than reviewed
  for. E.g. "cross-volume reads are unrepresentable in the type, not forbidden by policy"
  (`SERVING.md:38-39`); "**file** scopes are physics (the warden, at the fs boundary),
  **region** overlap is mathematics (the merge verdict), and **symbol/api/surface** scopes
  are judgment" (`LEDGER.md:36-39`).
- **Fast-forward multi-step operation** — "idempotent, self-checking steps that verify their
  own completion, so a crash mid-handoff resumes at the missing half"
  (`PLATFORM.md:73-74`; `LEDGER.md:400-402`).
- **Watermark / cursor / RESYNC** — every consumer holds a durable cursor; below retention ⇒
  typed `RESYNC_REQUIRED` and deterministic re-derivation (`LEDGER.md:366-369`;
  `PROTOCOL.md:282-286`).
- **Generational handle / arena / acquire-release counts** — the memory doctrine replacing
  `Arc` (`RUNTIME.md:96-107`), see §3.8.
- **Single-owner task / shard / move-only channels / thread-per-core** —
  `RUNTIME.md:15-38`, `CACHE.md:26-30`.
- **SIM vs REAL / Driver seam / nemesis** — the deterministic simulation discipline
  (`RUNTIME.md:40-80`; `FAULTS.md:82-125`).
- **Observe-mode-first** — a new signal/validator/trigger is computed and logged but drives
  nothing until its distribution is seen (`HEALTH.md:83-85`; `LEDGER.md:59-60`).
- **Receipts / THIN / rejected-alternative record / tripwire / one-way door** — the evidence
  vocabulary of the design dialogue (§7).
- **Access modes RWO / ROX / RWX-does-not-exist** — `VFS.md:100-108`.
- **Budget-charged isolation domain** — `VFS.md:110-120`; `CACHE.md:80-83`.
- **The single-surface law** — `SERVING.md:14-30`: "**No API in the system names a
  machine.** ... Any pod on any node may attach any volume version — locality is a property
  of caching, never of availability."

### 1.3 Dialogue vocabulary (the words the ledgers are written in)

- **Receipts** — primary-source evidence cited inline for a decision (rule 3,
  `GRILLING.md:16`). Used as a noun everywhere: "receipts on file", "the binding
  counter-receipt" (`VFS.md:30-31`), "cite-and-dismiss" (`GRILLING.md:1962`).
- **Priced** — a cost acknowledged, quantified and accepted rather than hidden: "the
  loss priced by OBJECT_TIER §3 formula" (`GAPS.md:64-65`); "N=1 loss = **Degraded**-
  priced, never silent" (`QUEUE.md:193-194`).
- **The cost ledger / owned wheels / wheel count** — `GRILLING.md:1262-1273`: "## Cost
  ledger (accepted burdens; check every new decision against these) — Owned wheels:
  runtime+SIM, claims protocol, hecate-wire codec, merge engine + verdict theorem, …
  Compounding: walking-skeleton first light is far behind the wheel count —
  acknowledged repeatedly, accepted under "we do not fear complexity."" Slates should
  keep such a ledger from its first decision.
- **Loud degenerate** — a collapse that is derived, never switched, but announced:
  "`R_eff = 1` loudly" (`SERVING.md:168-169`), "R_eff=1 LOUD" (`GRILLING.md:1087`),
  "cordon of the only node is a typed refusal" (`GRILLING.md:998-999`).
- **Physics vs policy / structural / unrepresentable** — see §1.2; the recurring
  question is whether a fault is made *impossible to express* rather than reviewed
  for.
- **Tripwire** — a measured threshold that reopens a closed decision (`GAPS.md:524-530`
  "Armed tripwires (metrics must exist from day one)").
- **Ratchet** — a floor set from the first CI baseline that may only tighten
  (`GRILLING.md:28-29`; `RUNTIME.md:185-190`).
- **THIN** — a source the author distrusts, flagged rather than hidden
  (`WIRE_SECURITY.md:68-69`; `OBJECT_TIER.md:334-337`).
- **Retired words** (grep-gated, rule 7 and `SESSIONS.md:235-236`): "workspace" (→
  lineage / session / work volume), "spawn" (→ summon), "event"/"notification" (→
  delta), "task"/"message"/"request" (→ claim), "sidecar" (→ companion), "orchestrator"
  (the Guide is the only orchestration authority), "failover" reserved for provider
  model switching only (`CONTEXT.md:111-112`). Slates should keep an `_Avoid_` line
  per glossary entry exactly as `CONTEXT.md` does.

---

## 2. `docs/specs/VFS.md` in depth (224 lines)

**Header** (`VFS.md:1-8`): "Status: presented for acceptance (grilling Branch 5; directions
ratified: 5a chunk-native unified store, 5b tool plane over it with the third store
deleted)." It is *direction-settled, not accepted*: `GAPS.md:29` lists "| VFS.md | presented
(Br 5) | V1–V11 | 8 | not stated |" and `GAPS.md:471-478` explains "**Genuinely
header-presented ... awaiting an explicit whole-spec verdict**: VFS, PODS, AGENTS_RUNTIME,
RANK, SCHEDULER". VFS.md is therefore the least mature of the storage specs and is amended
in place by later acceptances (SERVING 08-16, MERGE 08-18, CACHE 08-20, MONITORING 08-22).

### 2.1 The store (§1, `VFS.md:10-39`)

"One content-addressed chunk store per node. Everything on it: base images, green, pod
overlays, Designer volumes, tool blobs, guest images, registry content."

- Identity: "BLAKE3 — the single hash family, everywhere (chunks, manifests, content
  identity, schema hashes)." (`:15-16`)
- Chunking: "content-defined (FastCDC-family), boundaries stable under insertion; min/avg/max
  chunk sizes derived from measured device and workload anchors, with derivations at the
  definition sites. Files at or below the minimum are one chunk — the common
  small-source-file case pays one hash and no boundary scan." (`:17-20`)
- Placement in memory (`:21-31`, the passage most relevant to slates): "chunk bytes live in
  our own slab/mmap arena (off-GC-heap equivalent; budget-charged). **No disk spill exists.**
  Exhaustion is a typed retryable error plus pressure telemetry to the Guardian — never a
  hidden write. **Reconciliation with the node pack-volume store** (`OBJECT_TIER.md` §2/§5,
  amendment 2026-08-17): the arena and the pack-volume store are the RAM and NVMe **tiers
  of this one store** — movement between them is explicit lifecycle (flush-at-seal,
  fill-on-demand), never spill; "no disk spill" means exhaustion is typed at each tier, not
  that no NVMe tier exists."
- Lifetime (`:32-34`): "owner-managed acquire/release counts on generational handles (the
  memory doctrine's shared-immutable mechanism — refcounting as auditable data in the
  owner's state, not smart pointers). A stale handle is a typed error."
- Concurrency (`:35-37`): "hash-sharded across runtime shards by leading hash byte; each
  shard's store partition is a single-owner task. No locks, no cross-shard sharing —
  cross-shard chunk transfer is by handle message."
- Integrity (`:38-39`): "every read is hash-verifiable; corruption is detected at read and is
  a typed hard error naming the chunk — never silently served."


The architecture-level form of the same rule, `SUMMONING.md:113-115`: "VFS is in-RAM
with **no disk spill**, bounded by derived budgets (§8). Layers: disk (committed
baseline) → green (increment-validated, `LEDGER.md` §6) → per-pod overlays." That
sentence predates the 2026-08-17 RAM/NVMe-tier reconciliation quoted above and was
never re-worded — the two statements now disagree in strength, which is exactly the
"no *hidden* spill" weakening §8.4 warns slates not to inherit.

### 2.2 Manifests, layers, snapshots (§2, `VFS.md:41-60`)

"A **manifest** is the content-addressed unit of "a filesystem state": ordered `path →
(chunk list, mode, size)` entries, deterministically encoded (hecate-wire), hashed like any
content." The layer table (`:47-55`) maps base image → full manifest; green → "a
**versioned manifest chain**: each merge produces version N+1 as a delta over N; advanced
only by the merge serializer"; pod work-volume overlay → "delta manifest over its declared
green base"; tools → RO manifests.

Versions/snapshots: "Snapshot = manifest reference: O(1) to take, O(paths) to materialize a
listing, zero bytes copied." (`:56-57`); "Memory is **O(unique bytes) node-wide**: identical
content across pods, sessions, layers, and tools deduplicates at the chunk store by
construction." (`:58-59`); "Deterministic iteration order everywhere a manifest is walked
(runtime map rules)." (`:60`). `MERGE.md:39-47` gives the plain-English form: green "is
*not a disk anywhere*: it is a numbered series of **tables of contents** (manifests).
Version 43 is a small document naming blobs; it shares every unchanged blob with version
42 by name — that structural sharing *is* the copy-on-write." Merkle sharing is stated in
`SERVING.md:81-82`: "Merkle manifests share unchanged tree nodes: chain cost is O(changed
paths)."

### 2.3 Volume roles (§3, `VFS.md:62-83`)

Four roles, one store: **Work volume** ("per-pod RW overlay of its assigned work: writes
land server-side into the overlay's delta manifest; basis leases validated at the serving
boundary. ("Workspace" is retired from the design vocabulary ...)"), **Green** ("the
session's versioned, serializer-owned staging truth ... Extended by chain append, never
written in place; serving instances carry no write path"), **Tools** ("read-only
composition of the pod's resolved tool manifests"), **Scratch** ("pod-local, unwitnessed,
unjournaled, unmergeable; mounted at template/registry-declared redirect paths (`target/`,
`node_modules/`, caches) and freed at pod teardown. Exists so the witnessed overlay holds
source-tree mutations only").

### 2.4 The volume lifecycle: claims, attachments, access modes (§3b, `VFS.md:85-108`)

Added 2026-08-18 as "the cloud-shape correction: volumes are first-class objects with a
lifecycle, never ambient shared mounts." Verbatim:

- "**A volume is a declared object**: identity, role (§3), version domain, access mode. **A
  pod never sees a volume it didn't claim**: the summon manifest declares the pod's volume
  claims (volume + access mode + version policy); the scheduler reads claims for
  content-locality placement."
- "**The attachment** is the unit of control — one per (pod, volume), created at bind,
  destroyed at teardown: it pins the version, holds the lease, wires the warden's scope
  entries for the mount, runs the declared prefetch of the template's hot set (the
  cold-miss answer), and carries per-attachment accounting. Re-bind is a re-attach — an
  explicit lifecycle event; **a pod's view never changes without one**."
- "**Access modes by role**: work volume = RWO (one pod, one node, witnessed journal — the
  mutable case, never shared); green = written by no mount (the writer is the merge log;
  MERGE §2) with readers holding per-pod attachments to immutable versions — the
  container-image-by-digest sharing pattern, never a shared live mount; tools = ROX
  (immutable manifests); scratch = pod-local ephemeral; Designer = RWO."
- "Cross-node access is the read-through fill (SERVING §0): mount-anywhere with locality as
  caching. RWX does not exist in the system; nothing mutable is ever shared."

Attach/detach semantics elsewhere: `MERGE.md:270-283` (§6 "Green as a volume — objects,
claims, attachments": "version advance is the pod's next re-attach, an explicit lifecycle
event. **No mount writes green**: the writer is the log"); `MERGE.md:399-401` tests M16a–c
("Attach-denied: undeclared volume claim refused at bind", "Version stability: a pod's view
never changes without re-attach, fuzzed across concurrent merges", "Re-attach anywhere:
kill node, re-summon, attach same version elsewhere, byte-identical"); `MERGE.md:425-426`
AC 11 "Volume access is attachment-only". Detach ordering is *not* specified — see §2.14.
Handoff re-bind: `AGENTS_RUNTIME.md:92-96` "**The predecessor's work volume re-binds to the
successor** — volumes are host-side manifests that outlive instances by construction;
uncommitted overlay work transfers by re-bind, never by copy. A self-checking step: the
successor verifies the volume's manifest head against the claims' declared bases before
taking work."; `SERVING.md:249` FS13 "Volume re-bind: pod kill → successor binds same
volume — same inodes, no guest-visible discontinuity".

Why attachments (the rationale, from the grilling): the K8s frame was made law on
2026-08-18 — `GRILLING.md:1931-1936`: "**Kubernetes sets the abstraction bar; our
substrates are the mechanism — never the reverse.** Deliver K8s-proved ergonomics
(declare-don't-place, mount-anywhere, identity-not-location, reconcile-to-desired)";
and the receipts dossier `GRILLING.md:1952-1964`: "read-through local cache WINS decisively
for immutable content-addressed volumes: K8s PV access modes are per-node ATTACH
constraints for the mutable case (CSI = detach/reattach choreography), while K8s's own
immutable case (container images) is pull-by-digest to local store, IfNotPresent,
cache-forever — our side; EdenFS CONFIRMED on primary text as lazy local projection ...
AFS/NFS/Ceph coherence machinery exists ONLY because files mutate (caps/callbacks/
close-to-open) ... caveat: cold-miss latency real (EdenFS admits it) → manifest-driven
prefetch of hot sets."

### 2.5 Leases (as VFS sees them)

`VFS.md:67-68` "basis leases validated at the serving boundary"; AC 7 (`:207-208`) "Leases
validated at the serving boundary; correctness never depends on them (`MERGE.md` owns
conflict truth)." `LEDGER.md:230-232`: "**Leases guide, never guard.** An increment whose
basis proves its paths disjoint from green's movement skips transform work entirely. Lease
staleness informs; the conflict authority is the merge." The Sylk fault it answers:
`PLATFORM.md:142-145` "*No conflict detection at merge.* ... Hecate: canonical-rebase merge
with a deterministic conflict verdict (ADR-0005), leases as guidance only."


The third "lease" — ownership with fencing — is the one slates will actually need for
attachments: `CONSENSUS.md:219-227` "**Standing writers** … hold a **meta-tree lease in
Chubby's coarse-grained shape** (keepalives, grace period) **plus an epoch fencing
token enforced at the resource** — non-negotiable, because a paused-and-resumed writer
defeats any lease alone (§3b layer 3). Every write the resource accepts checks the
token; a stale token is a typed refusal." The Branch-27 charter phrases the rule as
"fencing tokens on every lease-holder effect" (`GRILLING.md:876-877`). Note that
hecate's *attachment* does not hold this kind of lease: RWO ownership is structural
(one serving instance per work volume), and the attachment "holds the lease" only in
the write-basis sense.

### 2.6 Materialization

hecate uses "materialization" in two senses. (a) Materializing a *listing/view* from a
manifest (`VFS.md:56-57`; stat/readdir answered "zero-fetch from the projection",
`SERVING.md:108-110`). (b) The lineage-level act of writing to a real target:
`SESSIONS.md:17-25` "The target is a local tree (laptop binding) or a source-control ref
(fleet binding) behind one port ... **The materialization lease is lineage-scoped and
single-holder**: exactly one session at a time may materialize to a given target; the lease
carries a fencing generation; supersession (a winner landing) bumps it and kills stale
holders at the chokepoint." Gate: "materialization to a real target defaults to prompt,
always, and requires zero unresolved conflict values" (`SESSIONS.md:131-133`).
(`MATERIALIZER.md` is the *ledger apply path*, unrelated to files.)

### 2.7 Prefetch

`VFS.md:96-97` ("runs the declared prefetch of the template's hot set (the cold-miss
answer)"); `SERVING.md:117-119`: "Prefetch: template eager-sets walk the manifest at bind;
sampled access logs derive per-template glob profiles (observe-mode first). Crawls degrade
to bounded cache-fill (SES7)." `GRILLING.md:248-250`: "prefetch = template eager sets at
bind + sampled-access-derived glob profiles (observe-first, EdenFS ~1500-glob receipt),
crawl detection → bounded cache-fill". Tests: `SERVING.md:248` FS12 and `SESSIONS.md:224`
SES7 "Crawl degradation: full-tree crawl in-guest degrades to bounded cache-fill
throughput, never per-file round-trips | the virtual-FS cliff". Prefetch traffic is
opportunistic-class by the purpose rule (`PROTOCOL.md:178-180`).

### 2.8 Accounting and quotas (§4, `VFS.md:110-120`)

"All derived at boot from physical anchors (system memory, summon allocations):
per-volume budgets from the summon; store arena budget from node memory; charge on acquire,
release on drop, reconciled transactionally with writes (a failed charge rolls the write
back with a typed error). Pressure telemetry streams to the Guardian — the primary consumer
— and to Scribes via the health plane. The **cache pools** (`CACHE.md` §2) are
budget-charged isolation domains of this doctrine — each bound to its session/dedup line,
charging on admit and releasing on eviction/drop, with typed exhaustion (never a reactive
OOM shrink)". The audit receipts for typed per-class budgets: `GRILLING.md:2095-2103`
(Seastar per-shard partitioning + bad_alloc; Scylla reader permits; cgroups v2 memory.min).
`SUMMONING.md:169-176`: "Every ceiling is **derived from physical anchors** ... Exhaustion
produces durable, typed signals (budget_exhausted is a counted outcome, not an error
string)". Volume *resizing* is not specified anywhere (see §8).

### 2.9 The OS bridge — what mount technology, per OS

Decided, but only for the **guest**: hecate's consumers are Linux microVM guests on all
three host OSes, so there is exactly one serving protocol. `VFS.md:122-129`: "Host-side
server per pod, speaking virtio-fs to the guest; one mount presenting the composed view:
work-volume overlay (RW) ⊕ green base (RO) ⊕ tools (RO). The serving cut is the path-based
namespace interface (stat/list/read/write + handle layer) proven in Sylk — now served over
virtio-fs instead of in-process FUSE." `SERVING.md:171-193` (§7 Device + DAX policy): "Own
FUSE-over-virtio protocol layer + backend trait, native to hecate-rt (`!Send` tasks,
arenas, io_uring, SIM-drivable), in-process in the libkrun fork. Multiqueue advertised
(Linux ≥ 6.10 guests; 5.5× receipt)"; mapping engine "one trait, two modes: `splice` ...
and `managed` ... KVM ships splice; HVF ships splice behind a boot capability probe with
managed fallback; WHP ships managed until splice is proven"; and the explicit advantage
(`:190-193`): "**Single-protocol advantage (recorded)**: one guest protocol (FUSE-over-
virtio into Linux guests) on all three host OSes. EdenFS's three-protocol matrix — and its
chmod-hack invalidation, fsck-every-boot, and un-veto-able writes — is structurally absent
from this design." The grilling record (`GRILLING.md:261-266`): "own FUSE-over-virtio
protocol layer + backend trait native to hecate-rt, in-process device in the libkrun fork
(vhost-user rejected: process model + sync trait + ENOSYS DAX)". Why in-guest at all:
`SUMMONING.md:109-112` "The guest mount **deletes Sylk's compensation machinery**: shebang
rewriting at extraction time, dynamic-linker interposers, and heuristic argv/env/shell
path translation existed only because processes saw host paths. In-guest, the canonical
paths are simply real."

Consequence for slates: hecate made **no decision** about FUSE/macFUSE/FSKit/WinFsp/
ProjFS/NFS-loopback on the host — those names appear zero (or, for NFS/ProjFS, once, as
EdenFS's pain record, `GRILLING.md:283`). Slates must appear "to ordinary host tools as a
normal path", which is precisely the three-protocol problem hecate designed *around*.

### 2.10 Serving details slates should keep (§5, `VFS.md:122-143`; `SERVING.md`)

- Writes captured server-side; RO layers "return EROFS-equivalent typed errors on write".
- "Range reads and streaming are first-class (no whole-file `Vec<u8>` transfers as the only
  verb — the Sylk §4.4 portability list, closed)." (`VFS.md:135-136`)
- Cache coherence (`VFS.md:141-143`): "manifest versions are the invalidation unit; the
  server advertises attr/entry validity derived from layer volatility (RO layers cache long;
  overlay entries invalidate on own-writes only; green base invalidates on version
  advance)." `SERVING.md:115-116`: "TTL posture: green/tools = infinite entry/attr TTLs +
  explicit invalidation; work volume = writeback cache mode (sole-writer coherence)."
  Receipt: `GRILLING.md:2104-2110` (libfuse recommendation verbatim, EdenFS "infinite
  expiry", virtiofsd cache=always).
- Inode identity law (`SERVING.md:103-107`): "inode = `(volume, path-entry)`, allocated
  monotonically at first lookup, stable for the *volume's* lifetime; the table serializes
  and re-binds with the volume ... Generation numbers guard reuse. Single-parent, no hard
  links."
- Digest xattr contract (`SERVING.md:112-114`): "BLAKE3 exposed as an xattr iff clean;
  absent while dirty; restored at seal."
- The two-representation law (`SERVING.md:32-42`): "Exactly two representations exist, and
  the set is closed: **Mutable** — the per-pod **work-volume overlay**: one logical WAL
  journal + one extent index per volume ... **Immutable** — **manifests-over-CAS**".
- Overlay = log-structured, "Crash recovery = index rebuild by segment header walk; content
  is never copied ... **no index checkpoint exists** — if measured rebuild breaches the
  pod-resume budget, that tripwire reopens the decision" (`SERVING.md:59-66`); "DAX for
  dirty reads is **rejected permanently**: group commit interleaves volumes in physical
  segments; mapping segment pages into a guest would leak foreign volumes' bytes."
  (`:72-74`)


Distribution and migration (§7, `VFS.md:165-176`), the passage slates' move/detach/
re-attach story should start from: "**Chunks travel by hash** (the Nix/Bazel/OCI
pattern): a node missing content cache-fills from a peer or origin and verifies
intrinsically. Immutable + self-verifying = the easy distribution problem; no ordering
or consensus applies to chunks." … "**Session migration** (colocation-unit move):
transfer manifest refs + lazy chunk fetch on demand; the WAL side is snapshot + tail
export (`WAL.md` §6). State transfer cost is O(manifest) up front, O(touched bytes)
over time."

### 2.11 The tests V1–V11 (§8, `VFS.md:178-192`), verbatim

```
| # | Test | Catches |
|---|---|---|
| V1 | Chunking determinism: same content ⇒ same chunk set and hashes, across platforms and runs | platform-divergent identity — cache poisoning by accident |
| V2 | Refcount balance fuzz: random acquire/release/GC interleavings ⇒ zero orphan chunks, zero premature frees (generational handle checks) | leaks; use-after-free-by-index |
| V3 | Isolation: pod A's server can never resolve pod B's overlay content, by construction test over the composed namespaces | cross-pod bleed |
| V4 | Dedup effectiveness: N pods with overlapping trees ⇒ store bytes ≈ unique bytes (ratcheted bound) | O(pods × bytes) regression |
| V5 | Budget: exhaustion ⇒ typed retryable + rollback of the failing write + pressure telemetry; memory bounded under sustained overload | hidden growth; silent refusal |
| V6 | Serving conformance: POSIX-subset syscall corpus (open/read/write/rename/stat/readdir/mmap-read) green through a real guest on Linux/macOS/Windows hosts | virtio serving drift; the REAL≠SIM class |
| V7 | RO enforcement: writes to green-base and tool paths from the guest ⇒ typed EROFS-equivalent; overlay writes land server-side with lease validation | write-authority leaks |
| V8 | Corruption: flipped chunk bytes ⇒ read fails typed, names the chunk; never served | silent corruption |
| V9 | Provisioner gates: unapproved source, hash mismatch, ungated build each refused with durable violation events; APPROVED_WITH_CAVEATS actually downgrades the sandbox config | half-wired governance (the Sylk substrate fate) |
| V10 | Migration: manifest-ref transfer + lazy fill reproduces byte-identical trees on the receiving node | migration divergence |
| V11 | Snapshot cost: manifest snapshot is O(1)/O(paths), measured, never O(bytes) | accidental deep copies |
```

Plus V12 (`VFS.md:218-224`, amendment 2026-08-22): "Test V12: intra-pod isolation — no path
exists from the Scribe's mount namespace to a work volume or the primary's upper
(structural walk + probe)."


Which of these transfer to slates as written: V1, V2, V3, V4, V5, V8, V10 and V11
carry over almost verbatim (rename "pod" → "agent handle"). V6 is the one to rewrite —
slates' conformance corpus runs through the *host* bridge (§2.9), not through a guest,
and must be green per host OS before that OS is supported. V7 becomes "RO-layer
enforcement at the host bridge". V9 (provisioner gates) and V12 (Scribe/primary
container isolation) have no analogue.

### 2.12 Acceptance criteria (§9, `VFS.md:194-211`), verbatim

```
1. **One store**: no second content store exists anywhere in the tree (architecture
   test — the Sylk two-stores-two-hashes fault is unrepresentable); BLAKE3 is the
   only content hash.
2. Memory is O(unique bytes): V4's ratcheted dedup bound holds in CI.
3. No disk-spill path exists in the store or overlays; exhaustion is typed and
   counted.
4. Every guest-visible byte is hash-verifiable; V8 gates every merge.
5. Serving conformance (V6) green on all three platforms before any agent work runs
   on that platform.
6. All three Guardian gates wired end-to-end with refusal tests (V9) before the
   provisioner accepts its first real recipe — capabilities ship wired or not at all.
7. Leases validated at the serving boundary; correctness never depends on them
   (`MERGE.md` owns conflict truth).
8. Budgets, chunk parameters, cache validities: derived, with derivations at
   definition sites; ratcheted perf floors (serve latency p99, chunking throughput)
   from first CI baseline.
```

### 2.13 Laptop-degenerate statement

VFS.md has **none** — `GAPS.md:29` "not stated", and `GAPS.md:340-342` Branch 38 charters
the sweep of "the inventory's not-stated column". The nearest statements are `SERVING.md:
168-169` "**Laptop**: inventory of one — same formulas, `R_eff = 1` loudly, empty exception
table, all tiers collapse local. No modes." and `MERGE.md:363-371` "One node: the session
group is one replica (self-ack), placement is a local write, every arrow in §7 is an
in-process call, attachments project from the one local store. Same code, same sequence,
no modes ... The fragile step is GROWING 1→2".


What a VFS laptop-degenerate statement must contain is best read off Branch 38's
charter, the whole-system collapse map (`GRILLING.md:1081-1110`): "(a) **the collapse
map, end to end** — meta tree → one group (root ≡ region, depth-1 failure-domain
tree); session groups → 1-replica self-ack (WAL path unchanged); placement/copysets →
R_eff=1 LOUD; green placement acks → self-ack; node-liveness fabric → self-support;
hecate-quic → loopback sessions for terminal attach + in-process short-circuit for
local delivery (router law); bare-UDP plane → loopback; **microVMs + wardens stay
REAL** … serving → one store, arena + local pack; scheduler shard=1; …" and "(e) **the
no-modes validation discipline** — architecture test: no code path branches on scale,
only on derived parameters; differential tests laptop ≡ fleet observable semantics
(the CN9 pattern generalized system-wide); every spec's laptop-degenerate statement
becomes a NAMED test". Slates should write its VFS spec with that section and that
named test from the first draft.

### 2.14 Open gaps around the VFS (from `GAPS.md` and GRILLING)

- Whole-spec verdict owed (`GAPS.md:471-478`).
- **Branch 36 "Pod↔volume attachment lifecycle mechanics"** — undesigned (`GAPS.md:334`).
  Charter `GRILLING.md:1038-1050`: "the bind sequence step by step (claim validation →
  serving-layer instantiation → version pin → lease acquisition → warden scope-entry wiring
  → prefetch execution → virtio-fs mount handoff → accounting open); the attachment state
  machine (binding/bound/re-binding/draining/detached + failure states — bind refused,
  lease lost, version withdrawn, node evacuating); re-attach mechanics at increment
  boundaries and at pod migration; detach ordering vs pod teardown (what flushes, what
  drops, what survives); concurrent attachment limits + budgets (derived);
  attachment↔handoff interplay ... laptop degenerate. Every step gets its message flow,
  failure rows, and tests."
- **Branch 37 "Volume provisioning lifecycle (both planes)"** — undesigned
  (`GAPS.md:335`). Charter `GRILLING.md:1051-1063`: "How a volume comes to EXIST, per role
  ... work volume provisioning at summon (journal allocation, extent index, budget charge);
  green provisioning at session create ...; the volume object's registry/directory home (who
  records that a volume exists — session directory?); version retention + withdrawal policy
  ...; volume deletion/teardown across both planes (crypto-erase interplay, D-3); quotas +
  accounting rollup; provisioning failure modes (budget refusal, placement failure)."
- **D-11 cross-node attach** — RESOLVED by dissolution (`GAPS.md:235-268`): "the only
  mutable volume (the work volume) is **RWO, single-pod, single-node, and never shared** ...
  so there is no multi-writer case to fence"; laptop degenerate = "identical sequence,
  in-process, µs" (MERGE §7).
- `GAPS.md:534-535`: "**libkrun fork**: three-platform DAX + splice (WHP the risk cell) —
  unbuilt, day-one required by PODS AC-4 / SERVING §7."
- `GAPS.md:506-507`: "SERVING.md `reclaim_headroom(20 ranges)` — the weakest derivation in
  that file by its own framing."
- Chunk-size constants are declared derived but no derivation is written anywhere in
  VFS.md; `TRANSFER.md:193-203` is the only place an anchor is named ("CDC average for text
  = derived from borg's index-cost model (~40–164 B/chunk) against our corpus and RAM
  anchors (restic/borg's 512 KiB–8 MiB envelope as sanity band)").

---

## 3. STORE, WAL, CONSENSUS, OBJECT_TIER, CACHE, ARCHIVE, TRANSFER — the decided designs

### 3.1 Consensus dialect and why (`CONSENSUS.md`, ACCEPTED 2026-08-17)

**The etcd challenge.** Branch 20's five decisions were "UNRATIFIED and **CHALLENGED
2026-08-17** — user: "IMMEDIATE and severe concerns with using ETCD or using ETCD as any
sort of example - it has well documented shortcomings and failure modes that do not scale
up well to Meta scale work."" (`GRILLING.md:160-165`). The answer was the **layer-
classification dossier** (`GRILLING.md:1500-1537`): "every challenged failure mode
classified by layer: (a) etcd-server/boltdb/watch, (b) etcd-raft-library, (c)
single-group-topology. FINDING: the famous record is overwhelmingly (a)+(c) — v3.5 silent
data inconsistency = server apply-loop watermark race ...; boltdb 8GB ceiling/mmap/blocking
defrag/freelist O(n) ... = backend; k8s stale-reads (#59848)/LIST OOM (KEP-3157)/OpenAI
Events-split = watch layer + one-keyspace topology; 5–7 voter + single-group ceiling =
topology the direction already rejects. The (b) record is real, short, enumerable".
Ratified form (`GRILLING.md:174-186`): "(2a) exemplar renamed "Raft, CRDB-lineage dialect,
pure core" (interface shape = etcd-raft as extended by cockroachdb/raft; tikv/raft-rs = Rust
portability proof; both exemplars never dependencies); (2b) the (b)-class + protocol bug
record ships as an executable conformance suite (named regression tests, never folklore);
(3-addendum) explicit conf-change activation semantics with #12359 countermeasures ...;
(5a) fault gate upgraded to deterministic whole-cluster simulation".

The decided core (`CONSENSUS.md:63-101`): "**One pure algorithmic core** ... **Exemplars,
never dependencies** — the core is ours, in our runtime, under our lint wall, with zero
etcd/CRDB/TiKV code." "**IO-as-data, deterministic**: the core is a state machine
`step(Message) → {outbound: Vec<Message>, to_append: Vec<Entry>, state_delta}` ... Time
enters only as logical `Tick` messages; election randomization is seeded and injected. No
clock, no channel, no IO inside the core". Two persistence laws (`:80-89`): "**Entries-then-
HardState**" and "**No message emission before covering durable state** ... The host's
applied/durability watermark advances **inside the same transaction** as the applied
effects — never a shared mutable index a background commit can race (the exact v3.5
mechanism, named)." Elections (`:103-121`): "**PreVote AND CheckQuorum together, always**";
"**Reads: ReadIndex only, v1.** ... **Lease reads do not exist**: they import a wall-clock
axiom the doctrine bans". Topology (`:20-61`): "the meta tree + N per-session groups";
"**Liveness is amortized to node level — no per-group heartbeats exist, ever.**" with a
disk-write-backed node-liveness fabric; and the **fabric law** (`:145-154`): "**the
node-liveness fabric is liveness-only; no safety property depends on it.**" AC 9
(`:413-417`): "**The fabric is deletable**". Split brain as four attacks each killed at a
named layer (`:134-143`). Single-writer roster (`:210-251`): CAS-first vs lease+fence,
"**The epoch-scoping law**", boot-validated. Laptop degenerate (`:352-361`) quoted in §1.2.
Conformance suite CS1–CS12 each citing its source issue (`:363-380`); CN1–CN16 (`:382-401`).

Paxos appears only as a reference (Spanner Paxos groups, `GRILLING.md:171`); the Delos
"VirtualLog" shape is adopted for reconfiguration-as-data (`CONSENSUS.md:39-41`).

### 3.2 WAL: logical logs over derived-ω physical streams, group commit (`WAL.md`, ACCEPTED 2026-08-17)

Model (`WAL.md:21-33`): "**Logical log** = per-group ordered record stream, `log_seq`
strictly monotonic, written only by that group's sequencer task ... **Physical stream** =
an append-only segment chain on disk. Records from many logical logs interleave, each
tagged `(log_id, log_seq)`. A stream has exactly one writer task". Content never enters
the WAL except an inline body "up to the derived `WIRE_FORMAT.md` §3c inline budget
(`frame_cap − AAD − record_header`)" (`:33-40`).

**Group commit and the ω derivation** (`WAL.md:56-74`): "At boot, measure the device: k
flushes on a preallocated probe file → `flush_p50`, plus per-stream submission overhead
`submit_cost` (measured, not assumed). Choose ω ∈ {1..N_shards} maximizing the closed-form
throughput model:

```
throughput(ω) = ω × batch_size(ω) / (flush_p50 + submit_cost)
where batch_size(ω) = expected arrivals during one flush at rate λ/ω (natural batching)
```

— cheap flush (PLP NVMe, µs-scale) drives ω → N_shards (parallelism dominates); expensive
flush (consumer NVMe ms-scale, macOS F_FULLFSYNC 17–24ms) drives ω → 1 (batch amortization
dominates). λ starts from the arrival-rate prior recorded on last run ... and ω is
re-derived per boot, never hand-set."

Commit path (`WAL.md:107-124`): "**Natural batching, no timers**: everything that queues
while a flush is in flight rides the next write+flush (etcd model). `commit_many` pipelines
a producer's burst as one message; entries are individually acked (a pipeline, not a
transaction)." One durability policy: "**always-full**: Linux `fdatasync` on preallocated
segments via **io_uring linked SQE (write→fdatasync)**; macOS `F_FULLFSYNC`; Windows
`FlushFileBuffers`. Ack is sent only after the durable return." Backpressure: "full
producer queue ⇒ typed retryable error with derived retry hint, counted — never blocking
the producer's shard, never unbounded growth." Record format (`:76-105`): 8-byte aligned
header `{len, crc, log_id, log_seq, stream_seq, kind, ver}`, **chained** CRC32C (ghost-record
defense), fixed-size preallocated zero-filled segments, torn-vs-corrupt discrimination by
512-byte sectors, "**No doublewrite**". AC 4 (`:196-197`): "One durability policy in the
tree: no barrier tier, no checkpoint tier, no config knob that weakens the ack contract."

**Replication groups and the 1-replica case** (`WAL.md:125-131`): "The sequencer appends
through the consensus-group API in every mode. Locally the group has one replica and the
leader's durable-append self-acks; distributed, the same call path runs 3-replica Raft with
pipelined appends (leader = sequencer). The physical stream stores the group's log entries
below consensus. No local-only commit shortcut exists." The law-level statement,
`LEDGER.md:386-391`: "**Replication is degenerate locally, real remotely — same code path.**
The WAL commit path is a consensus group from day one: locally a single-replica group where
every append is the leader voting for itself; distributed, the same group at three replicas
per session namespace. Distribution changes the replica count, never the commit code."
Later generalized to environment-derived durability, `QUEUE.md:99-115` and
`GRILLING.md:7697-7714`: "the DEFAULT is NOT a hardcoded 3-replica — it DERIVES from the
failure-domain tree ... Laptop (depth-1 tree, 1 node) ⇒ replica=1, single-node WAL-fsync
(power-cut safe; node-death NOT survivable — no 2nd node exists, can't beat the env)".

### 3.3 STORE: a WAL is not a store (`STORE.md`, ACCEPTED 2026-08-22)

The corpus-wide correction (`GRILLING.md:8052-8056`): "User caught the corpus-wide
conflation: **a WAL is a recovery log, NOT settled storage** (CONSENSUS §5 rejected boltdb
but never named a replacement ...). Need a WAL-fronted proper DB". `STORE.md:21-31`: "A
real store is a log **in front of a storage engine** — the log makes the engine
crash-safe; it never replaces it". Decisions: "**The consensus log is the only WAL.** The
backend keeps no write-ahead log and fsyncs nothing on the apply path" (`:186-190`);
**pluggable backend by declared workload property** (`:360-371`; the user overruled a
one-LSM recommendation, `GRILLING.md:8101-8113`); all-replicas-apply (`:203-213`); the DRAM
**window** as "a versioned hash index ... Concurrency by epochs, not locks" (`:314-341`);
range sharding with a meta-group **directory** as "the one splitter" (`:435-472`);
per-shard resolved-timestamp **watch** (`:404-433`); read-repair never by copying another
replica (`:393-402`); backpressure via `backend.lag()` (`:474-481`). This spec is the
canonical example of the "COLLECTOR bar" (§7).

Also decisive for slates: the storage-corpus finding, `GRILLING.md:3210-3219`: ""a
production, globally distributed authorization plane whose AUTHORITATIVE store is an
in-memory arena/heap with only log+snapshot durability" — NONE EXISTS. Every surveyed
authz/identity plane ... puts authority in a REPLICATED ON-DISK DB/engine and uses
in-memory structures STRICTLY as derived caches/indexes" — the user's overrule
(`GRILLING.md:3103-3110`): ""That doesn't even make any sense. Clearly we need some sort
of organized storage designed for extreme availability, low latency, and global scale.""

### 3.4 OBJECT_TIER: two planes, pack volumes, the durability ladder (`OBJECT_TIER.md`, presented; §9 and two-planes ratified)

- The factoring law (`:19-45`): one substrate ("one content identity (BLAKE3), one CDC
  chunking, one manifest encoding, one pack-volume engine (§2), one wire verb set"), two
  fleet disciplines — serving plane = weighted HRW (`SERVING.md` §6), durable plane =
  "**assignment-based copyset placement as published** (Cidon / Tiered Replication) over a
  small consensus-owned placement map". Rationale (`GRILLING.md:728-736`): "no coherent
  system needs both disciplines for the same data; the prior tension was a symptom of
  artificial unification".
- Pack volume format (`:59-113`): "one preallocated large file per volume, held-open fd
  ... framed chunks: `{header magic, blake3 (32 B), len, payload, footer magic}`, 8-byte
  aligned. **The BLAKE3 address IS the checksum** — no cookie, no separate CRC."; "in-RAM
  `blake3 → (volume, offset, len)`. **Rebuild-by-scan is truth**"; "**No fsck exists**;
  recovery is scan." Index-RAM formula with the CacheLib 8-byte escape hatch (`:94-101`).
  Rejected with receipts: LSM (ShardStore), raw-block (BlueStore), file-per-chunk.
- The four-rung durability ladder (`:153-162`): "1. **Witnessed** — journal group commit
  ...; 2. **Sealed** — CDC → BLAKE3 → arena → local pack volume; 3. **Placed** — R_eff acks
  ...; 4. **Referenced** — manifest commit, then ref flip ... **Placed strictly precedes
  referenced**: nothing reachable can be under-placed".
- Tier map T0–T5 (`:186-197`) and the RAM/NVMe reconciliation: "**The arena and the pack
  store are the RAM and NVMe tiers of one store.** ... movement between them is explicit
  lifecycle (flush-at-seal, fill-on-demand), never spill" (`:199-205`).
- Encryption × dedup (§9, `:275-348`): scope-salted convergent encryption with BLAKE3 keyed
  mode, four hardenings as law, crypto-erase per scope; tests OT16–OT19.
- Laptop degenerate (`:350-357`), verbatim: "Inventory of one: both planes collapse onto
  the same local pack volumes ...; the placement map is trivial; `R_eff = 1` loudly; scrub
  still runs on its derived cadence; hedged reads degenerate to no-ops; class labels
  persist inert, so a laptop corpus later joining a fleet re-places correctly from recorded
  classes. Same formulas, no modes."
- The no-cloud-pairing law (`SERVING.md:149-159`; `OBJECT_TIER.md:401-403`): "External cloud
  storage appears only as an optional, registry-declared import source behind Guardian
  staging — never a tier either plane depends on".

### 3.5 CACHE: coherence, and what "immutable" buys (`CACHE.md`, ACCEPTED 2026-08-20)

- Shared substrate §0 (`CACHE.md:19-43`) — the paragraph that most directly encodes the
  slates design rules: arena acquire/release fan-out "never a smart pointer (Arc/Rc
  banned)"; "**One single-owner task per shard** (thread-per-core): shard state is touched
  only by its owner ⇒ lock-free, race-free; a key maps to exactly one shard; cross-shard
  traffic is move-only over **bounded** channels"; "**N=1 collapse**: a depth-one
  failure-domain tree derives HRW ring cardinality = replica = shard = 1"; "no std
  `HashMap` in shard state; SIM is bit-reproducible".
- Coherence spine (§4, `:107-158`): within-node holder index (ValKey CLIENT TRACKING
  generalized, bound "derived from arena bytes (not ValKey's fixed 1M-key cap)"); cross-node
  "the key's **HRW owner holds the cross-node holder set** ... at-most-once notify + version
  backstop"; critical-read escape (TAO); and the opt-in content-addressed face where "the
  entire coherence machinery above becomes a **verified no-op**" (`:152-158`). `SERVING.md:
  27-30`: "Coherence machinery is absent because every shared object is immutable; the
  mutable work volume is single-pod, single-node by law — our RWO — and is never shared."
- Memory (§6, `:174-186`): slab-classed arena, `(slab, offset, gen)` **is** the generational
  handle, "No per-object refcount (ValKey's `refcount:29` is the Arc/Rc we ban)"; "**No
  background maintenance thread.**" — rehash/eviction/TTL/holder-bounds are "a bounded,
  time-boxed, cursor-resumable slice of work on the shard executor". The moka/tokio
  rejection (`GRILLING.md:6799-6810`): "moka + tokio are PRECEDENT-ONLY, NEVER DEPENDENCIES
  ... moka = tokio/Arc-based w/ BACKGROUND MAINTENANCE THREADS".
- Admission W-TinyLFU with a deterministic seeded sketch (`:160-171`); eviction pluggable
  per pool; TTL "O(due) (ordered timing wheel), never a sampled scan" (`:280`).
- Flash tier environment-derived (`:198-222`): "**Laptop** derives to **DRAM-only**".

### 3.6 ARCHIVE and "archive format"

hecate has no volume-archive *format*; the word "archive" names two things. (1) Ledger
retirement (`LEDGER.md:371-385`): "**Retirement bounds hot state — as a custody transfer,
never a loss.** ... moving **complete, by content identity, into the Archivalist's
archive** — content-addressed over the unified store, indexed at retirement time by scope,
agent, domain, lineage, and time ... Explicitly not tombstonic: no delete markers exist in
any read path". (2) The archive plane (`ARCHIVE.md`, presented 2026-08-22): per-session
memory — `ArchiveDoc` kinds (`:105-115`), an ingest QUEUE lane whose durable ack gates pod
teardown (`:229-249`), `ArchiveStore` = "content-addressed bytes in OBJECT_TIER's archive
storage class, session scope" (`:143-145`), `ArchiveIndex` = a STORE instance (`:146-149`).
The closest thing to "archiving a volume" is the sealed manifest + chunks placed to the
durable plane (`OBJECT_TIER.md:232-246` §6 lifecycle boundaries: "**Landing, archival,
generation publication, registry provisioning** are the durable-plane writes: the same
immutable chunks — identity unchanged — placed per the copyset map, then the ref flips")
and session `close` → `archived` (`SESSIONS.md:190-198`: "`close` (drain; pending
materializations resolved or abandoned-with-reason; retirement flush; archive finalize) ·
`archived`"). Destroy = crypto-erase of a scope key (`OBJECT_TIER.md:262-264`, `:314-319`).

### 3.7 TRANSFER: the transfer format (`TRANSFER.md`, ACCEPTED 2026-08-17)

- The scoping theorem (`:13-33`): "**Already-addressed content** ... every chunk is an
  independent, self-verifying, idempotent unit. Concurrency is free, resume is free
  (`batch_exists` skips what landed), ordering is irrelevant, and **no transfer state
  machine exists on this path**"; only **unaddressed content** gets the ingest state
  machine.
- Verified streaming (`:35-64`): identity `ContentRef { root, len, class }` with a
  chunking-independent BLAKE3 tree root; "Verification granularity = **16 KiB chunk
  groups** — a *derived* constant: the smallest group restoring full BLAKE3 SIMD batching
  (spec §7.1: 16-chunk batches restore peak), outboard overhead 64 B/group ≈ 1/256 of
  content, zero outboard for content ≤ 16 KiB"; "**The length rule is law**: `len` is
  untrusted until the final chunk group verifies".
- Upload (`:66-93`): "offer → missing-set → parallel verified streams → atomic commit";
  "for duplicate content **this reply is the entire upload**"; "A failed conditional commit
  **orphans nothing**"; "Mid-chunk resume is deliberately absent".
- Ingest state machine `OPEN → STAGING → COMMITTING → COMMITTED | ABORTED` with messages
  `TransferOpen{token (client-minted idempotency key), class_hint, declared_len, parts,
  scope}`, `PartRecord{offset MUST equal the part's watermark}`, `PartAck{durable_through}`
  (`:95-135`); chunking runs **at commit** so part boundaries cannot influence identity
  (`:152-172`, TR3/TR10).
- Bounds are formulas then carriers (`:174-203`); rejected magic numbers named: "S3
  5 MiB/5 GiB/10,000; GCS 256 KiB; gRPC 4 MiB".
- Migration of a volume (`VFS.md:169-173`): "transfer manifest refs + lazy chunk fetch on
  demand; the WAL side is snapshot + tail export (`WAL.md` §6). State transfer cost is
  O(manifest) up front, O(touched bytes) over time." (V10).

### 3.8 The runtime doctrine these all stand on (`RUNTIME.md`, ACCEPTED 2026-08-17)

Because slates' design rules (no Arc, lock-free/shared-nothing, allocation-aware) are
hecate's, the exact text matters:

- Shape (`:15-38`): "**N shards** ... One pinned OS thread per shard, each running a
  single-threaded executor: FIFO ready queue, hierarchical timer wheel, per-shard driver
  handle. **Tasks are `!Send`-capable and never migrate.** ... there is no work stealing
  ... **Cross-shard communication is move-only** over bounded SPSC/MPSC channels. No type
  containing `Arc`/`Rc` crosses (or exists — §4). **Every stateful component is one task**
  owning its state ... **The task-lifecycle law — no untracked tasks.**"
- Driver seam (`:40-80`): completion-shaped API; "Linux: **io_uring** primary, with linked
  SQE support (write→fsync chains) ... No epoll fallback"; macOS kqueue; Windows IoRing/IOCP;
  "**Cancellation is a request with guaranteed completion**".
- Determinism (`:82-93`): banned by CI-fatal lint — `std::time::{Instant, SystemTime}`,
  `std::thread::spawn`, `rand`/`getrandom`, tokio/async-std, unbounded channels, "**std
  `HashMap`/`HashSet` in component state**".
- Memory doctrine (`:96-107`): "**`Arc` and `Rc` are denied workspace-wide** (lint). The only
  exceptions are named FFI edge modules ... Intra-component references are **generational
  handles** (`u32` index + `u32` generation) into owner-managed arenas. A stale handle is a
  **typed error** ... Shared-immutable fan-out ... uses **explicit acquire/release counts
  stored as data in the owning arena** — visible in replay, single-threaded, auditable.
  Never a smart pointer. Buffers transfer by move; within a shard, loans (`&[u8]`) are fine;
  across shards, ownership moves or an owner-mediated handle is sent."
- No-panic law (`:109-136`): "we do not panic. Period. Ever." — clippy wall, `panic =
  "abort"`, no `catch_unwind`, "**Allocation exhaustion is typed by construction**".
- AC 6/7 (`:186-193`): ratcheted floors; "bare tuning literals fail review".

### 3.9 What `GAPS.md` says is still open in this area

- D-1 consensus CLOSED; rider closed; "Branch 27 narrows to the §6 writer-roster audit at
  build time" (`GAPS.md:52-76`).
- D-2 WIRE_FORMAT/TRANSFER CLOSED; "Branch 26 (multi-modal media) narrows to content
  **policy** only" (`:77-84`). D-3 encryption×dedup CLOSED (`:85-93`).
- D-14 IAM scope-keyspace split/reparent OPEN: "no range descriptor, no routing/descriptor
  cache, no split/merge record, no cross-group atomic-move transaction" (`:297-312`) —
  later partly covered by STORE §9 (2026-08-22).
- Undesigned (`:328-343`): "27 leader-election revisit · 28 secrets · 30 fleet fault
  detection/recovery · 31 replica handling · 35 git-compatible code hosting · 36 attachment
  lifecycle mechanics · 37 volume provisioning (both planes) · ... 38 the laptop collapse
  (whole-system scale-down map + no-modes validation + degenerate sweep of the inventory's
  not-stated column) · 32 node lifecycle · walking skeleton (final)."
- Unbranched: "the **provider gateway**" (`:403-408`).
- Residual magic numbers (`:497-513`): WAL acceptance multipliers 0.5×/0.8×/1.5×/20%; the
  shared >10% CI bar; RUNTIME provisional ≥1M cycles / ≤1µs p99; SERVING
  `reclaim_headroom(20 ranges)`; FOREST α = 0.15. "Disposition: each either gains a
  derivation at its definition site or is explicitly ratified as a shape constant".
- Fit-before-influence (`:515-522`): "Every ratcheted floor and commissioning-derived τ
  across the corpus assumes a **first CI baseline** that requires running code".

---

## 4. PROTOCOL, WIRE_FORMAT, WIRE_SECURITY — mechanisms and P1–P19

### 4.1 Framing and the two planes (`PROTOCOL.md`, ACCEPTED 2026-08-18)

"**There is no TCP anywhere in the mesh** (D-10(d), verbatim law: "no fallback. Period.
QUIC + UDP over TCP utilizing the standard(s) we just designed.")" (`:19-21`). The cleartext
rule: "**cleartext on the wire is only what is needed to find the key.**" (`:24-25`).

Control plane = stateless bare-UDP datagrams (`:27-56`): "consensus votes/terms/fencing
probes (Raft rides this plane — protocol-sound because Raft is loss-tolerant by design:
idempotent AppendEntries, leader retry), node-liveness fabric support claims, membership,
gossip, health piggyback, class-1 telemetry ... nothing here is ever retransmitted by the
transport (a stale heartbeat resent is anti-information)." Datagram layout:

```
cleartext prologue (the key-finding minimum):
  ver: u8
  key_hint: sender_id (u64) ‖ key_epoch (u32)   — or the opaque derived key-id form
  payload_len: u16
crypto:
  nonce: 12 bytes   = 64-bit per-(key, direction) counter ‖ 32-bit channel id.
                    Counter-based, never random; reuse refused + typed +
                    counted in every build (no assertion path — the no-panic law).
  ciphertext + 16-byte tag  (AES-256-GCM), containing the FULL envelope —
    kind, class, flags(must-be-zero), hlc, cluster_id, epoch, sender_term,
    src_pod, dst_pod, request_id, trace_ctx, schema_hash — followed by the
    hecate-wire payload.
```

Session plane = owned `hecate-quic` (`:81-98`): "RFC 9000/9002 dialect-as-exemplar ...
Private version + private Initial salt; Noise-IKpsk2 handshake in CRYPTO frames;
connection identity = host/enrollment identity; a term/epoch advance kills the session with
a typed reason." Four frame classes: `CTRL`, `SEALED_FRAME/claims`, `SEALED_FRAME/bulk`,
`DATAGRAM_SUPERSEDE`. Length-prefixing lineage: `LEDGER.md:269-272` "Length-prefixed frames;
the payload is length-prefixed *inside* the delimiter-parsed header so binary bodies can
never confuse the scan ... One derived frame cap, consistent at every layer".

Parser-resident enforcement order (`:100-114`): "length caps → prologue parse → **key lookup
(unknown sender = drop, zero crypto spent)** → AEAD verify + decrypt → envelope parse (flags
must-be-zero, version) → fencing check → replay window → admission (§3) → decode/unseal.
Enforcement points: host parsers (both planes), guest unseal paths, terminal clients —
identical order, identical categorized counters everywhere. **The HLC window serves
liveness only**".

### 4.2 MTU handling

`PROTOCOL.md:58-61`: "Payload budget derived at the datagram layer from **per-path MTU**
(loopback/jumbo included — the 1500 anchor is a floor derivation input, never a hard-code);
the builder rejects oversize before send." The hyperscale lesson, `LEDGER.md:257-259`:
"under one datagram budget — computed at the *datagram* layer, envelope and AEAD overhead
included (hyperscale budgeted the payload layer and could silently exceed MTU)". Test P12.
The laptop nit that forced per-path MTU: `GRILLING.md:1603-1604` "laptop degenerate
penalizes every UDP option (loopback MTU nit: §1.1's 1500 anchor must become per-path)".
Non-interference law (`:185-213`): "**The invariant: no class's latency bound contains any
term dependent on another class's object size or queue depth.**" — frame cap, per-class
partitioned queues, per-class credit pools, zero host crypto on bulk, class-separated
virtqueue pairs, per-class store IO queues (owed rider), per-class arena budgets; "**The
scale walk, as the permanent test — megabytes to petabytes**".

### 4.3 Request IDs, idempotency, dedup

- `LEDGER.md:281-283`: "**Request IDs correlate responses.** Hyperscale correlated by
  `(peer, handler)` and two concurrent same-handler requests could receive each other's
  replies; Hecate's correlation is explicit per request."
- `PROTOCOL.md:62-72`: "**Distinct from `request_id`**: `request_id` pairs a response with
  its request (transport); `trace_ctx` threads one operation's execution across hops
  (observability) — the two never merge." `TRACING.md:29-34` the three-id law table
  (`request_id` / `trace_id`+`span_id` / `caused_by`).
- Fencing on every message (`LEDGER.md:278-280`): "`(cluster_id, epoch, sender_id,
  sender_term)` — a stale or half-dead sender is rejected by every receiver ... Node
  identity is ephemeral per process start; a restart is never a rejoin."
- Dedup is structural per kind (`PROTOCOL.md:292-294`): "Dedup eligibility is a structural
  property of each message kind; probes/acks/votes are never content-deduped."
  Application-level idempotency keys: increments dedup by `hash(claim, base, post_state,
  ops_doc)` (`MERGE.md:123-124`); transfers by a client-minted `token` (`TRANSFER.md:110`);
  ledger exactly-once as three layers "cursor-exact stream resume → windowed dedup LRU →
  content identity" (`LEDGER_CORE.md:146-150`).

### 4.4 Encryption choice — Noise, not TLS

Decided: no TLS, no mTLS anywhere in the mesh. `LEDGER.md:294-299`: "**Encrypt-always, both
planes**: header-encrypted AES-256-GCM datagrams on the control plane (cleartext = the
key-finding prologue only); the Noise-IKpsk2/private-QUIC session plane with the seal-once
pipeline — one payload seal at the origin warden, one unseal at the destination; per-pod
summon-mint roots with labeled derivations, atomic key_epoch rotation at handoff. No mTLS
layer exists; there is no TCP in the mesh." `GAPS.md:140-148`: "**Handshake**: Noise-IKpsk2
owned handshake in QUIC CRYPTO frames (nQUIC blueprint; spec-named verified suite —
25519/AESGCM-256/SHA-256-or-BLAKE2s, NOT BLAKE3 in-handshake for proof fidelity; one-hash
law governs content identity only); private QUIC version + private Initial salt (RFC 9000
§7 sanctioned); IK message-1 replay rule + QUIC Retry/address-validation as law; 0-RTT =
closed list of replay-safe frame kinds". Rejected with receipts (`GAPS.md:189-194`):
"external-PSK TLS (rustls gap), RPK TLS (drags the TLS machine), bespoke non-Noise
handshake (gQUIC's own retirement), per-pod QUIC endpoints ... three-leg per-hop AEAD ...
egress-stamp variant (TOCTOU), unauthenticated-envelope ingress". Keys (`WIRE_SECURITY.md:
74-125`): HKDF per-workload roots, "**Epochs are explicit counters carried in the grant
protocol, never wall-clock** — deleting Kerberos's clock-skew failure mode"; nonces
"counter state is never persisted or resumed — any restart mints a new epoch and thus new
keys". Hop lanes (`:126-151`): GMAC-only on claims, bulk payload exempt under the
name-verify guard. Provider egress stays HTTP/2 over rustls (`RUNTIME.md:140-157`).

### 4.5 Backpressure and credits

`PROTOCOL.md:269-289`: "**One flow-control law, natively ours**: `hecate-quic`'s stream +
connection flow control *is* the ratified credit design — dual-level, absolute-offset
credits (idempotent under loss/reorder), windows derived as `k × frame_cap` per delivery
class and BDP-autotuned, the **never-whole-object-in-credit** invariant permanent." Admission
(`:258-267`): "Admission runs in the protocol callback before any task exists: per-class
caps + named admission groups with reserved slots (the health plane owns capacity the data
plane cannot touch); Control gets head-of-line scheduling ... classes 0/3/5 are never shed —
overload surfaces as backpressure (credit exhaustion or typed retryables), never silent
loss." Delivery classes (`:138-146`): `0 Control`, `1 Observation` (sheddable), `2 Phase`,
`3 Directed` (never shed), `4 ConsultRequest`, `5 ConsultResolved`, `6 StreamData`
(credit-governed), `7 EphemeralAtMostOnce`. Traffic archetypes (`:147-163`): "**Every
message kind declares exactly one traffic archetype** ... the archetype — never the
subsystem — determines carriage and lane." Bandwidth scheduling = mClock reservation +
weight + limit (`:165-183`). TRANSFER's four flow-control receipts (`TRANSFER.md:205-219`).

### 4.6 Versioning rules

- Envelope: `ver: u8` in the prologue (`PROTOCOL.md:41`); "**Protocol version in the
  envelope**, negotiated at connect — not discovered at registration after a wrong-major
  peer has already been talking." (`LEDGER.md:273-275`); AC 6 "Prologue layout
  compile-asserted; any change is a version bump."
- Payload (`WIRE_FORMAT.md:139-162`): "`#[derive(Wire)]` is **the single interpreter** of a
  type definition ... **`schema_hash`** = first 8 bytes of BLAKE3 over the canonical encoding
  of the reflection. The envelope carries it; the parser checks it before decode.
  **Evolution is append-only, compiler-enforced** ... removing, reordering, or retyping a
  field or variant is a compile error (trybuild-gated). Legal: appending
  `Option<T>`/defaulted struct fields, appending enum variants. **Cross-version decode by
  ancestor-hash matching, never tolerant reading** ... A major-version bump abandons the
  ancestor set; there is no decode-across-major path and never will be."
- The axiom (`WIRE_FORMAT.md:20-27`): "**One value, one encoding.** ... an accepted
  non-canonical encoding is a content-identity fork". Primitives (`:30-39`): LEB128 minimal
  varints, "`usize`/`isize` **do not exist on the wire**", canonical NaN, no −0; maps in
  canonical-encoding byte order; two length domains `FrameLen`/`ContentLen(u64)` as types
  ("**the codec must never be the binding constraint on content size**", `:52-59`);
  `ContentRef { root: [u8; 32], len: ContentLen, class: ContentClass }` (`:84-93`); the
  inline-vs-reference derive law (`:119-130`). Test vectors "never change within a major
  version" (`:179-181`). Scope: "hecate-wire is Rust-only; TS/Python SDKs speak MCP/JSON at
  the tool plane and never touch the claims plane" (`PROTOCOL.md:12-15`).

### 4.7 The tests P1–P19 (`PROTOCOL.md:317-339`), verbatim

```
| # | Test | Catches |
|---|---|---|
| P1 | Roundtrip property fuzz (via WF1) | codec correctness |
| P2 | Canonical-reject fuzz (via WF2) | content-identity break class |
| P3 | Garbage/truncation fuzz: no panic, bounded time/allocation at every enforcement point | untrusted-input DoS |
| P4 | Evolution: ancestor-hash decode; snapshot compile gates (via WF4) | silent wire breaks |
| P5 | Envelope statics: prologue layout compile-asserted; flags must-be-zero enforced post-decrypt | layout drift, dirty reserved bits |
| P6 | Parser-order instrumentation: stale epoch/term, replayed nonce, bad tag, unknown sender each rejected with counters at the specified stage, before any dispatch — at every enforcement point class | enforcement behind dispatch |
| P7 | Nonce discipline: per-(key,direction) counters monotonic; reuse refused + typed + counted in every build; SIM oracle flags reuse as harness defect | AEAD nonce catastrophe |
| P8 | Envelope tamper: any flipped ciphertext/prologue byte ⇒ tag failure / key-lookup miss, counted; **header-privacy vector: on-wire capture shows prologue fields only** | header malleability; metadata leakage |
| P9 | Admission reserve: data-plane flood in SIM; health-group probes still admitted; sheds counted | control-plane starvation |
| P10 | Stream kill/resume at arbitrary points: no gap, no duplicate at any cursor; RESYNC path exercised end to end | delta loss/dup; dead resync path |
| P11 | Credit stall: zero-credit halt, bounded memory; classes 3/5 never dropped | flow-control fiction |
| P12 | Per-path MTU: datagram builder rejects oversize (computed with prologue+nonce+tag); loopback/jumbo paths derive larger budgets | wrong-layer MTU budget; fragmenting groups needlessly |
| P13 | Fencing sweep: every kind × stale (cluster/epoch/sender/term) combination rejected, both planes | zombie senders |
| P14 | No-TCP structural: no TCP socket, listener, or dependency exists in the mesh (registry cannot construct one) | fallback creep |
| P15 | Clock strobe: HLC-window rejections typed+counted; every safety oracle green (FAULTS clock nemesis) | wall-clock in a safety path |
| P16 | Missing-set escape: stalled bulk stream ⇒ re-request by identity on a healthy stream/session within derived bound; zero duplicate store writes | HOL as a wait instead of a scheduling event |
| P17 | Incast: O(10k)-class fan-in in cluster-SIM ⇒ credit-bounded buffer occupancy, zero collapse | the storage fan-in class |
| P18 | Loopback ratchet: session-plane N=1 throughput ≥ ratcheted baseline | laptop regression |
| P19 | Archetype coverage: every message kind classified; boot check red on any unclassified kind | carriage chosen by subsystem instead of archetype |
```

Acceptance criteria 1–14 at `PROTOCOL.md:341-365` (e.g. 4: "Classes 0/3/5 show zero drops
across SIM overload sweeps; the drop taxonomy is exhaustive (unknown-drop = bug)"; 14:
"Cleartext on any wire is only what is needed to find the key"). Companion matrices:
WF1–WF10 (`WIRE_FORMAT.md:183-196`), TR1–TR10 (`TRANSFER.md:221-235`), the WIRE_SECURITY
phase plan P-a1..P-a5 with the "XSA-155 test" and the copy-once invariant
(`WIRE_SECURITY.md:48-57`, `:209-215`).

---

## 5. FAULTS, HEALTH, MONITORING, TRACING — fault model, health semantics, test conventions

### 5.1 The fault model (`FAULTS.md`, ACCEPTED 2026-08-17)

Scope, stated closed (`:13-42`): "**In scope**: crash-recover (kill at any instruction,
recover from durable state); process pause/resume of any duration (the GC/scheduler
stand-in — defeated by fencing, never by timing assumptions); network partition in every
shape ...; message loss, duplication, reordering; clock strobe/jump (only telemetry may
notice — no correctness path reads wall-clock); storage faults with detection (torn
writes, misdirected writes, detected corruption, disk-swap-on-reboot, stalled-device gray
failure); **region-scale events** ...; **internal-machinery misbehavior of liveness-only
components**". "**Out of scope, explicitly**: Byzantine participants ...; undetected
corruption past the checksum layer (BLAKE3-everywhere makes the undetected residue the
hash-collision probability, stated, not defended further)." One scoped Byzantine exception
for the IAM store (`:44-60`), entered "per the §7 acceptance rule ("new fault classes enter
by amending §1/§3, never by an ad-hoc test")".

Corruption dispositions (`:62-80`): "The one law: **never silent truncation** ... **Torn
tail** ... discard — it was never acked; **Detected body corruption** ... consensus log
entry ⇒ **rebuild-from-quorum** ...; N=1 or quorum-unavailable ⇒ **refuse loudly** (typed,
names the entry, operator-surfaced — never guess); content chunk ⇒ re-fetch by hash;
derived state ⇒ discard and re-derive (always legal; watermark-recovery invariant)."

Nemesis vocabulary (`:82-100`): "`kill` · `pause` · `partition` · `omit` · `dup/reorder` ·
`clock` · `torn-write` · `misdirected-write` · `corrupt` · `disk-swap` · `disk-stall` ·
`fabric-fault` · `region-partition` · `region-loss` · `wan-inflate` ... New fault classes
enter by amending this section, never by an ad-hoc test."

Simulation (`:102-125`): "**One simulation, cluster-scoped** ... **Seeded and replayable**:
every run is a seed; every failure is a seed + step count ... **Biased search,
BUGGIFY-style** ... **Budget as a ratchet, not a number** ... **The N=1 gate**". The
failure×obligation matrix (`:127-134`): "rows = §3 fault classes, columns = subsystem
obligations. Each cell is one of **Masked** (no observable effect), **Degraded** (typed,
bounded, surfaced), or **Refused** (loud stop, never guess) — and each non-trivial cell
names its test ... **boot-validated for coverage**: every subsystem × fault class has a
stated cell — an uncovered cell fails CI, not review."

### 5.2 Health semantics (`HEALTH.md`, ACCEPTED 2026-08-16)

"one consolidated per-agent signal plane ... health informs, agents act" (`:3-8`). Per-class
absence semantics (`:32-44`):

```rust
enum AbsenceIs {
    Degraded, // liveness, sensor: silence IS the signal (fails closed)
    Unknown,  // composite marks stale; the last value is NEVER frozen forward
}
```

"Consumers always receive `(value, freshness)`: staleness is data they judge, never a
hidden default (H7)." The content-free law (`:46-57`): "signals carry operational
measurements only — counters, rates, durations, byte/token counts, closed-vocabulary enum
codes, threshold crossings, and opaque references (UIDs, content hashes). **Never**: work
content of any kind — no code, no file paths, no free text ... Structurally: no signal type
contains an unbounded string or bytes field — the type walk is the test (H8)." No authority
(`:73-75`): "The health service itself has **no authority**: it cannot gate, author claims,
or trigger anything." Node roll-up: "Fan-in is bounded by node count, never pod count."
(`:63-64`). Tests H1–H8 (`:92-101`); "Acceptance: H1/H4/H5/H8 permanent".

### 5.3 MONITORING (ACCEPTED 2026-08-22) — what transfers

Mostly pod-interior machinery (§8), but three reusable items: (a) the three comms laws
(`:209-223`) as a pattern for a one-way observation channel — "**Observe-not-feed**",
"**Authority/enrichment split**", "**Context economy**"; (b) the flight-recorder ring
protocol (`:130-169`): "W4 The writer NEVER blocks and NEVER waits on the reader ... R2
COPY the sub-buffer out, THEN re-read {seq, commit} (acquire): changed during the copy ⇒
... discard the copy, count torn_read"; kernel-enforced fd modes (memfd O_RDONLY vs O_RDWR,
seals `SHRINK|GROW|SEAL`). The research behind it (`GRILLING.md:4744-4796`): "FOUR
independent kernel observability channels (BPF ringbuf / perf / ftrace / relay) all obey
one law: THE PRODUCER NEVER BLOCKS; consumer lag = counted/detectable loss, never stall";
measured tiers (`:4800-4805`) "shm/mmap 4.7–5.3M msg/s (~0.2µs RTT) ≫ pipes 162k / UDS 130k
(~6.2–7.7µs) > TCP-lo 70k (~14.2µs)"; io_uring `SINGLE_ISSUER` "(-EEXIST on violators = a
KERNEL assertion of the one-thread invariant)" (`:4727-4729`). (c) The lie-detector /
provenance-classed metrics (`:336-355`): "**cross-view divergence is itself a signal**".

### 5.4 TRACING (ACCEPTED 2026-08-22)

`TraceCtx { trace_id: [u8;16], span_id: [u8;8], flags: u8 }` on every message, codec-rejected
absence (`:89-95`); `SpanRecord` with a closed `ChokepointId`, node-local monotonic
`start/end`, closed `SpanStatus { Ok, Err(ErrorClass), Aborted, Parked }`, bounded tags
(`:97-118`). "**Those boundaries are the span points**, and the span roster is not a second
list — it *is* the chokepoint registry" (`:141-155`); the RAII `SpanGuard` pattern
(`:157-168`). Ids: "128 random bits from the seeded driver RNG ... The collision bound is
derived, not asserted" (`:172-178`); clocks: "monotonic locally, edges globally ... never
compared across nodes" (`:180-186`). Keep = class-aware head sampling, "`Always` classes
(100%-kept; all bounded-rate by construction)" vs "`Derived` classes ... `rate = kept-volume
budget ÷ measured class volume`" (`:216-245`). Park/handoff/death edges (`:188-214`).
Emission cost receipts (`:255-259`): "Dapper measured ~200 ns span create/destroy, 9–40 ns
per annotation, 426 B/span". TR1–TR12 (`:342-357`). Every subsystem spec then received the
same one-line amendment (e.g. `VFS.md:213-216`): "this subsystem's chokepoints emit
execution spans per `TRACING.md` §3; its chokepoint registry entries are the span roster
(boot-validated; an unregistered emitter fails startup)."

### 5.5 Test conventions across the corpus

- Every spec has a **numbered test matrix** with a per-spec ID prefix and a `Catches`
  column naming the failure class: V (VFS), FS (SERVING), W (WAL), CS/CN (CONSENSUS), F
  (FAULTS), P (PROTOCOL), WF (WIRE_FORMAT), TR (TRANSFER), T (RUNTIME *and* PODS), OT
  (OBJECT_TIER), ST (STORE), M (MERGE, families M13a–f), L (LEDGER_CORE), SES (SESSIONS), R
  (AGENTS_RUNTIME), S (SKILLS_API), H (HEALTH), MO (MONITORING), TR (TRACING — collides
  with TRANSFER's TR), IAM/IAMS (IAM), SCH (SCHEDULER), AR (ARCHIVE), LS (LEDGER_SUBSTRATE).
  `GAPS.md:482-485` C-8 flags the T collision and out-of-matrix IDs as "Cosmetic until
  cross-spec citations ambiguate".
- Test kinds are named by phrase: "architecture test" (a structural property checked by
  code walk/lint, e.g. `VFS.md:196-198`), "grep gate" (`SESSIONS.md:235-236`), "(trybuild)"
  compile-failure fixtures, "differential vs oracle" / "byte-identical", "fuzz" / "sweep" /
  "crash at every step boundary" / "kill at every point", "ratchet" (measured floor),
  "SIM" (seeded nemesis), "N=1 named" (the laptop configuration as a separate CI job,
  `CONSENSUS.md:356-361`, `FAULTS.md:123-125`).
- Later specs split the matrix into an **Acceptance criteria table** (`| # | Criterion |
  The failure it catches |`) and a **Test matrix (SIM)** table (`| Test | Asserts |`) that
  maps each test to criteria (`STORE.md:603-638`, `TRACING.md:342-371`, `ARCHIVE.md:440-470`).
- "permanent CI gate" / "permanent" marks the criteria that may never be relaxed
  (`SERVING.md:266`, `WAL.md:187-188`).

---

## 6. Skills: how capability is exposed to agents (`SKILLS.md`, `SKILLS_API.md`)

Architecture (`docs/architecture/SKILLS.md`): "A skill is a **typed, code-defined
capability** — never raw markdown at the source — published over MCP. Agents hold a small
handful of well-worded skills each, backed by a wide capability surface underneath."
(`:3-6`). The skill model (`:10-24`): identity, tools (typed operations with input/output
schemas), contract, instructional content ("authored as part of the typed definition,
rendered out as content"), declared capabilities ("inventory-from-declaration, statically,
before any handler runs"). "Everything about a skill that can be known without running it
is a static artifact."

**Two layers over MCP** (`SKILLS.md:26-43`): "Decision (Q17): Hecate skills are **typed at
the source, MCP on the wire** — compatible with the Skills-over-MCP working group direction
(SEP-2640) without inheriting its "skills are untrusted markdown" posture for our own code:
Each skill's **tools** publish as ordinary MCP tools (typed, schema'd, invocable). Each
skill's **instructional content** publishes as a `skill://` resource per SEP-2640 (SKILL.md +
supporting files, content-digested). The typed Rust definition is the single authoring
surface; the MCP projection is generated." For external servers "approval is
**content-bound** — a digest change to the resource set revokes prior approval. Digests
confirm consistency, not trustworthiness".

Built-ins: "compiled Rust ... There is no dynamic native code loading" (`:47-49`). User
skills: TS/Python declarations "against **generated, schema-first SDKs**" validated before
load (`:51-65`). Tool-surface discipline (`:70-86`): "The Sylk number to beat: its architect
surfaced **31 tool definitions in turn one** ... Each agent's default surface is a **small
handful** ... A façade is one well-worded skill with an action parameter, not ten sibling
verbs. The long tail exists behind **progressive disclosure**: a search-and-activate
mechanism ... **Omission is the strongest gate.** ... Activation state must be able to shrink
as well as grow".

The API spec (`SKILLS_API.md`, ACCEPTED 2026-08-16): one derive → five artifacts (`:33-38`):
"(1) the hecate-wire codecs and schema hashes; (2) the MCP tool projection (JSON Schema
generated from the same reflection — no serde, no schemars, no second interpreter); (3) the
`skill://` instructional resource (INSTRUCTIONS + supporting files, content-digested); (4)
the registry `Skill` document (name, domain, contract, **declared capabilities**); (5)
dispatch glue with input/output validation at the boundary." The Rust surface (`:14-31`):
`#[derive(Skill)]`, `type Action` = a closed `#[derive(Wire)]` enum ("the façade"), `const
INSTRUCTIONS: &'static str = include_str!("workspace.skill.md")`, `const CONTRACT`, `const
CAPABILITIES`, `async fn invoke(&mut self, cx: &mut SkillCx, a: Self::Action)`. Rules:
"**Façade-first**: one skill, one closed `Action` enum — never sibling verb families";
"`SkillCx` carries the claims façade, the store handles, and the emitters — skills reach
the world through it, never ambiently"; "**Statelessness**: built-in skill structs hold only
store handles and config"; "**Capabilities are a closed, harness-versioned bitset**".

The user's one-way door (`:56-63`): "**The law**: TS/Python are friendly, typed authoring
surfaces for defining Hecate extensions ... Their output is always a **canonical
document**; only documents ever cross into Hecate. **No Node, no Python, no interpreter
executes in any microVM.**" Declared-skill invocation "is composition, not code":

```rust
enum DispatchTarget {
    Facade { target: FacadeRef, map: ActionTemplate },   // existing typed harness façade
    ToolExec { recipe: RecipeRef, args: ArgTemplate },   // provisioned CLI via the tool plane
}
```

"Templates are **pure field mappings** — schema-validated parameter projection, literals
allowed, logic forbidden" (`:76-89`). Surface discipline (`:99-105`): "**Omission is
absence**: a capability a role must never hold is not registered in its bundle — invoking
it is *unknown tool*, not *denied tool*." Tests S1–S9 (`:109-119`; S7 "the same logical
declaration authored via TS, via Python, or as a direct document ⇒ byte-identical canonical
form and hash"; S8 "No-execution structural"). Related: the "façade set" for claims/peers/
history/self (`SKILLS.md:88-101`); the guest-side "MCP projection is the wire form;
in-guest invocation of a local skill is a direct call — same contract, no loopback theater"
(`AGENTS_RUNTIME.md:54-57`).

One drift to avoid copying: `AGENTS_RUNTIME.md:54-56` still reads "TS/Python skills
execute in-pod in their image-shipped runtimes behind the typed contract", which
contradicts the SKILLS_API law above ("No Node, no Python, no interpreter executes in
any microVM"); MONITORING's R1–R5 sweep lists it as a pending correction
(`MONITORING.md:473-475`: ":48–50 (TS/Py exec contradiction → SKILLS_API's law)").
The accepted law is SKILLS_API's.

For slates (which must offer MCP, skills-over-MCP, and raw skills): hecate's shape is
exactly one typed Rust definition → MCP tools + `skill://` resource (the "raw skill" is the
content-digested SKILL.md and supporting files) → generated TS/Python SDK types from the
same schema reflection; the SDKs "speak MCP/JSON at the tool plane" (`PROTOCOL.md:12-15`).
Note that hecate's SDKs are authoring bindings, not runtime clients; slates' async
Python/TypeScript *client* SDKs are a different artifact hecate never specified.

---

## 7. The documentation style — how hecate writes a spec

### 7.1 The status header

Every spec opens with `# SPEC: <name> — <subtitle>` then a `Status:` paragraph that is the
single source of truth (`GAPS.md:454-464`, the C-7 rule): "a spec file's `Status:` header is
the single source of truth for acceptance state. It reads `ACCEPTED <date>` only on an
explicit dated user whole-spec verdict, carrying the verdict quote; the GAPS §1 `Status
(header)` column mirrors it exactly; the GRILLING "SETTLED" table denotes
**direction-settled** (a design direction chosen) and is strictly weaker than
spec-accepted". The header also carries the verdict quote, the amendments folded, the
research on file, and the companions, e.g. `WAL.md:3-19`: "Status: ACCEPTED 2026-08-17
(whole-spec verdict "amend and accept" under the maximal audit — five amendments folded:
... **Amended 2026-08-20 (CACHE/QUEUE/FANOUT acceptance)**: ... References: TiKV
raft-engine (writer group, purge), Pebble/etcd WAL discipline, TigerBeetle batching, fsync
research on file (GRILLING.md)." States seen: "presented for acceptance", "ACCEPTED
<date>", "accepted-in-session", "PROVISIONALLY DIRECTED".

### 7.2 Section skeleton (the "COLLECTOR bar")

Early specs (VFS, WAL, PROTOCOL) are: numbered prose sections → `Test matrix (failure each
catches)` → `Acceptance criteria` → dated `**Amendment ...**` paragraphs appended at the end.
On 2026-08-22 the user raised the bar (`GRILLING.md:8345-8360`, user: "examine how detailed
our specs are and stop lying"): "the bar: unified §2 data model w/ ownership facts; §3
component+per-item state machines; step-by-step algorithm w/ derivations inline; §9
integration map; §10 worked example incl. a failure case". `STORE.md:3-6` restates it as
"the corpus spec standard: full data model, state machines, architecture map, networking,
lifecycles, failure matrix, worked example, integration enumeration". The canonical
skeleton (`COLLECTOR.md` headings; `STORE.md` and `ARCHIVE.md` follow it):

```
1. Role                                  (one paragraph; what it is NOT)
1a. The whole machine, in plain terms    (an extended analogy — bank branch, hospital
                                          records office, aircraft flight recorder,
                                          package tracking — ending with "The analogy
                                          carries the N least-obvious choices")
1b. Terms this document uses (reading guide)
2. Data model                            (Rust structs with inline comments; then
                                          "Ownership facts the types carry")
2b. (routing / placement functions)
3. <component> — lifecycle and the tick  (ASCII state machines; numbered tick steps;
                                          "Load-bearing orderings")
3a. Architecture map (what runs where)   (ASCII box diagram)
3b. Networking, hop by hop               (table: Hop | Transport | Plane/class | Security)
3c. Boot order, circularity, self-observation
4–10. mechanics, one section per concern
11. Failure & recovery matrix            (table: What dies | What is lost | Counted
                                          where | What recovers, from where)
12. Refusal & loss taxonomy (closed; an uncategorized refusal is a bug)
13. Derived constants                    (table: Constant | Formula | Anchors)
14. Worked example — ..., end to end (+ failures)
15. Laptop degenerate                    ("N=1: ... Same code, zero modes.")
15a. Integration (every companion touchpoint)
16. Amendments landing with acceptance (one coordinated sweep)
17. Acceptance criteria                  (table: # | Criterion | The failure it catches)
18. Test matrix (SIM)                    (table: Test | Asserts)
19. References (load-bearing few)
```

Prose conventions: **bold** for laws and named decisions; "(NEW settlement, flagged)" for
anything decided during the write-up rather than in-exchange (`STORE.md:9-11`, `:203`);
"(amended <date>, <X> acceptance)" inline wherever a later acceptance changed a sentence;
"Rejected-alternative record" / "Rejected with receipts" paragraphs (`CONSENSUS.md:156-167`,
`OBJECT_TIER.md:104-113`); "receipts" = primary-source evidence, "THIN" = a source the
author distrusts, stated honestly (`WIRE_SECURITY.md:73-74`, `OBJECT_TIER.md:334-337`);
"tripwire" = a measured threshold that reopens a decision (`GAPS.md:524-530`); "one-way
door" (`GAPS.md:492-495`); "the <Precedent> lesson/class" naming a bug family after its
origin (the v3.5 class, the Kleppmann class, the XSA-155 test, the SnapStart class); the
Sylk fault ledger as the negative reference (`PLATFORM.md:124-193`).

### 7.3 One representative test entry, verbatim (`VFS.md:186`)

```
| V5 | Budget: exhaustion ⇒ typed retryable + rollback of the failing write + pressure telemetry; memory bounded under sustained overload | hidden growth; silent refusal |
```

The shape: ID · what is exercised and the exact expected outcome (⇒) · the failure class
caught, phrased as the thing the test makes impossible. Newer specs add the criterion
mapping, e.g. `STORE.md:612`:

```
| ST6 | **Bounded lag**: firehose ⇒ `lag()` ⇒ admission throttle; window bounded by budget AND horizon under any load | unbounded window/replay |
```

### 7.4 One representative acceptance-criterion entry, verbatim

List form (`VFS.md:196-198`):

```
1. **One store**: no second content store exists anywhere in the tree (architecture
   test — the Sylk two-stores-two-hashes fault is unrepresentable); BLAKE3 is the
   only content hash.
```

Table form (`STORE.md:609`):

```
| ST3 | **State = pure function of the log**: crash-fuzz at every apply/age/flush/seal point ⇒ recover + tail-replay identical; N=1 named | nondeterminism; buffer-and-lie |
```

A criterion names the mechanism that enforces it (architecture test, lint, permanent CI
gate), the named configuration ("N=1 named"), and ends with the fault it excludes.

### 7.5 Laptop-degenerate statements

Always a dedicated section near the end, always the same rhetorical form: enumerate what
each fleet mechanism *derives to* at N=1, then "Same code, zero modes." Examples:
`CONSENSUS.md:352-361`, `OBJECT_TIER.md:350-357`, `CACHE.md:241-244` ("One shard, DRAM arena,
no flash region (derives to zero), in-process channels. Identical `get`/`put`/`publish`
code; ring cardinality 1."), `MERGE.md:363-371`, `STORE.md:553-559`. Paired with a differential
test "1-voter ≡ N-voter observable semantics for every client-visible API" (`CONSENSUS.md:
394`, CN9) and an AC "`N=1` ≡ fleet; every §N constant derived at its definition site".

### 7.6 Decision ledgers

Three ledgers, each with a defined role: `GRILLING.md` (the dialogue log: standing rules;
SETTLED/ON THE TABLE/OPEN BRANCHES; cost ledger of "accepted burdens"; dated research
reports), `docs/GAPS.md` (the gap ledger), and ADRs (`docs/adr/000N-<slug>.md`: a one-
paragraph decision, "## Considered Options", "## Consequences", sometimes "## Why not X" /
"## The tradeoff, honestly", dated amendments appended; `GAPS.md:490-491` notes "the five
ADRs carry no Status/Date headers"). Standing rules that govern the process
(`GRILLING.md:7-30`): "**One decision per exchange, worked to settlement.** No bundle
ratifications. Assent without a shown spec = direction only, spec owed. Specs are shown
IN-MESSAGE before any file is written"; "**Research before argument** for every substantial
decision; receipts inline."; "**Coined names are provisional** until mechanisms settle and
the glossary captures them."; and the same-change doctrine — companion amendments land in
the same commit as an acceptance (`ARCHIVE.md:412-438` "Amendments landing with
acceptance (one coordinated sweep)"; `IAM.md:597-609`).

### 7.7 The GAPS.md rubric

`GAPS.md:1-10`: "Rubric per the branch charter: research on file? spec exists? test matrix?
acceptance criteria? laptop degenerate stated? open decisions named? Classification:
`undesigned | designed-unspecced | specced-untested | decision-open | drift
(owed-and-forgotten)`. **A stale gap ledger is itself a gap**: every branch acceptance,
spec verdict, or tripwire firing updates this file in the same change." Its sections: §0
"The one global fact" (no code exists, so every spec is specced-untested); §1 "Component
inventory" table `| Spec | Status (header) | Tests | AC | Laptop degenerate |`; §2
"Decision-open (blocking decisions, named owners)" as D-1…D-14 entries carrying their
resolution history inline; §3 "Undesigned (open branches, charter only)"; §4 "Drift
(owed-and-forgotten) — corrected or to-correct" as C-1…C-10; §5 "Residual magic-number
surface (against the derivation doctrine)"; §6 "Fit-before-influence milestones"; §7
"Armed tripwires (metrics must exist from day one)"; §8 "External dependencies and port
hazards"; §9 "Blocking order toward the walking skeleton". The rubric extends in the
Branch-29 charter (`GRILLING.md:901-913`) with "fault-matrix cells? chokepoint coverage
boot-validated?".

---

## 8. What is inapplicable to slates, what hecate left undecided, and what looks wrong for an in-memory hermetic VFS

### 8.1 Inapplicable (and why)

- **MicroVM isolation and everything inside the pod** — ADR-0001/0006, `PODS.md`,
  `MONITORING.md` §§1–6, 9–10, `WIRE_SECURITY.md` §§2, 5 (the egress staging device, guest
  receive path), the warden/sensor, `hecate-init`, virtio-fs/DAX/mapping engine
  (`SERVING.md` §7), the two-container interior. Slates serves host processes; there is no
  guest, so "the VM boundary is the isolation guarantee" (`SUMMONING.md:37-39`) has no
  analogue and the entire "in-guest paths are simply real" argument (`SUMMONING.md:109-112`)
  inverts: slates *must* solve host-path presentation.
- **The agent roster, offices, rank, Scribe, Guardian judgment** — `AGENTS.md`, `RANK.md`,
  `PLATFORM.md` §§1–6, `HANDOFF.md`, the history ring. Slates has agents as *clients*, not
  as harness participants.
- **The claims ledger and its machinery** — `LEDGER.md`, `LEDGER_CORE.md`,
  `LEDGER_SUBSTRATE.md`, `MATERIALIZER.md`, `QUEUE`/`FANOUT` as ledger transport, summon-as-
  claim. Slates' provisioning is a request to a server, not "a claim ... executed by the
  scheduler" (`CONTEXT.md:53-55`). The *shape* (typed requests, typed dispositions
  `retryable | terminal`, `LEDGER.md:62-69`) is worth keeping.
- **Green, the merge gate, the landing engine, conflict values** — ADR-0003/0005,
  `MERGE.md`, `SESSIONS.md` §§5–7. These exist because hecate refuses shared mutable volumes
  and reconciles per-pod overlays through a serializer. If slates offers shared writable
  volumes it needs none of this; if it offers per-agent overlays it needs *some* merge
  story and should read ADR-0005's "Why reject at all" (`0005:24-33`).
- **IAM as a global authority plane** (`IAM.md`) — 614 lines of Zanzibar-class design for
  users/orgs/regions; slates needs at most a principal + scope + capability-atom model
  (`IAM.md:164-183` the `volume` resource actions `claim(role, access_mode)`, `attach`,
  `read`, `write`, `seal`, `snapshot_read` are a reasonable starting vocabulary).
- **Consensus, the meta tree, cross-region** — only relevant if slates replicates volumes
  across nodes. For a purely in-memory service, replication is the *only* durability, so
  this may be relevant after all (see 8.3).
- **Object tier NVMe tiers, pack volumes, scrub, EC, flash cache** — `OBJECT_TIER.md`
  §§2, 4–5, `CACHE.md` §8: all disk. Only the identity/manifest/ladder-ordering ideas
  transfer.
- **Provider egress, registry, forest/vector, scheduler bands** — no analogue.

### 8.2 Decided by hecate, directly reusable

The chunk-store doctrine (`VFS.md` §1), manifests-as-snapshots (§2), attachment-as-control-
object with pinned versions and explicit re-attach (§3b), typed budget exhaustion (§4),
range reads first-class, manifest-version invalidation units, inode identity law, the
runtime doctrine (§3.8 above), hecate-wire's canonical encoding and evolution rules, the
FAULTS Masked/Degraded/Refused matrix, AbsenceIs, the content-free law, chokepoint-span
tracing, the façade-first MCP skill model, and the documentation skeleton.

### 8.3 Left undecided by hecate — slates must decide

1. **Host-OS mount technology.** Zero decisions (§2.9). Per-OS candidates (FUSE / macFUSE or
   FSKit / WinFsp or ProjFS / NFS or SMB loopback) are exactly EdenFS's "three-protocol
   matrix" that hecate recorded as pain and avoided (`SERVING.md:190-193`; `GRILLING.md:
   283-284`). Whatever slates picks, hecate's cache-coherence posture (infinite TTL +
   explicit invalidation for immutable layers; writeback for sole-writer mutable) and the
   digest-xattr honesty contract are the transferable parts.
2. **The attachment state machine and detach ordering** — Branch 36 charter
   (`GRILLING.md:1038-1050`) lists precisely the questions: bind sequence, states
   (binding/bound/re-binding/draining/detached + failure states), re-attach on migration,
   "detach ordering vs pod teardown (what flushes, what drops, what survives)", concurrent
   attachment limits. hecate never wrote it.
3. **Volume provisioning, directory home, retention, deletion** — Branch 37
   (`GRILLING.md:1051-1063`). hecate has no "volume exists" record, no resize, no
   move/rename, no destroy protocol beyond crypto-erase of a scope.
4. **Dynamic sizing.** hecate budgets are fixed at summon ("Volume budgets derive from
   summon-time allocation", `SUMMONING.md:171-172`) and "resized only via handoff"
   (`PODS.md:24-25`). Grow/shrink of a live volume is unspecified.
5. **Page pinning, hugepages, NUMA, memory bandwidth as an anchor** — zero hits for mlock/
   hugepage. The only memory anchors named are node memory and object-size histograms
   (`CACHE.md:80-83`) and the guest-RAM DAX tax (`SERVING.md:183-185`).
6. **Sub-100 µs provisioning.** hecate's budgets are ms-class ("summon-to-ready ... a
   derived budget", `SUMMONING.md:228-233`; "Cold boot — the floor (~100–200ms class)",
   `PODS.md:118`). The only µs-class primitives are manifest snapshots (`VFS.md:56-57`, V11)
   and the laptop submission path ("identical sequence, in-process, µs", `MERGE.md:330`).
   A 50 µs volume-create path has no precedent in the corpus.
7. **Shared-mutable volumes.** hecate's law is "RWX does not exist in the system; nothing
   mutable is ever shared" (`VFS.md:107-108`). Slates' brief ("multiple agents concurrently
   creating/reading/writing ... volumes") must choose: RWO-per-agent + overlays (hecate),
   or a genuine multi-writer volume (which hecate's reasoning in ADR-0005 argues against
   for agents: "a silent interleave produces code nobody wrote or reviewed").
8. **Hard links.** "Single-parent, no hard links." (`SERVING.md:103-107`) was acceptable in a
   guest hecate controls; host build tools may not agree.
9. **Client SDK shape.** hecate's TS/Python are authoring-time bindings only
   (`SKILLS_API.md:56-63`); no async client SDK, no MCP *server* for a service like slates
   is specified.
10. **The laptop-collapse map for the VFS** — Branch 38 (`GRILLING.md:1081-1110`) never
    executed; VFS.md's laptop degenerate is "not stated".

### 8.4 Where hecate's decisions look wrong for an in-memory hermetic VFS (with reasons)

1. **"Witness" is defined by fsync.** `CONTEXT.md:134-135` and `SERVING.md:44-51` make
   "acked means durable" mean "journaled and group-committed"; `WAL.md:196-197` forbids
   "any config knob that weakens the ack contract". A never-touches-disk service cannot
   satisfy this; slates must redefine the ack (in-memory committed; optionally replica-
   acked) and price power-loss as a *designed* Degraded/loss cell in its FAULTS matrix
   rather than inherit hecate's zero-acked-loss AC (`WAL.md:187-188`). hecate's own
   "environment-derived durability" (`GRILLING.md:7697-7714`: "node-death NOT survivable —
   no 2nd node exists, can't beat the env") is the honest template.
2. **The arena/pack "one store, two tiers" reconciliation** (`VFS.md:24-31`) exists only to
   admit an NVMe tier. For slates the RAM arena *is* the store; keep "No disk spill exists.
   Exhaustion is a typed retryable error" and drop the tier language entirely — otherwise
   the "no disk spill" law reads as "no *hidden* spill", which is weaker than slates needs.
3. **Hashing on the write path.** hecate hashes at seal, off the hot path (`SERVING.md:67-71`;
   `MERGE.md:197-198` "The always-paid seal-time hashing of an earlier draft is deleted").
   Its chunk-store rules ("every read is hash-verifiable", `VFS.md:38-39`) are cheap only
   because content is already sealed. A 50 µs provisioning/first-write budget suggests
   slates should treat BLAKE3/CDC as lazy (at snapshot/archive/transfer), not per write,
   and hecate's V1/V8 should be scoped to sealed content.
4. **Chunk-size anchors are device anchors.** `VFS.md:17-19` derives min/avg/max from
   "measured device and workload anchors"; in memory the anchors are page size, cache-line
   and memcpy/hash throughput. The derivation must be re-done, not copied.
5. **DAX-style page sharing vs. hecate's rejection of DAX for dirty data.** `SERVING.md:
   72-74` rejects mapping journal segments into consumers because group commit interleaves
   volumes. If slates mmaps arena pages to host processes, the same cross-volume leak
   applies to any shared slab — per-volume slab isolation would be a precondition, which
   fights the O(unique bytes) dedup goal (`VFS.md:58-59`). hecate never had to resolve this
   because guests only ever saw sealed pages via DAX.
6. **The lease word.** Three meanings (§1.1). Slates needs attachment/ownership leases
   (liveness) far more than write-basis leases (merge aid); adopting hecate's glossary entry
   verbatim would mislead.
7. **Thread-per-core with no work stealing** (`RUNTIME.md:17-24`) assumes hecate owns the
   entry points. A FUSE/WinFsp/NFS server takes requests from kernel threads; mapping them
   to pinned shards without a hand-off copy is a design problem hecate solved only for
   virtio-fs multiqueue (`SERVING.md:173-176`).
8. **Encryption × dedup salting** (`OBJECT_TIER.md` §9) is at-rest machinery; for a hermetic
   in-memory store the threat model is different and importing the four hardenings would be
   cargo-cult. The one transferable rule is "an ID is never a bearer capability"
   (`:303-310`).
9. **The 16 KiB verification group and bao outboards** (`TRANSFER.md:41-47`) optimize
   network verification; intra-host transfers of in-memory volumes (fork/clone/move) are
   pointer or page-table operations, so slates' "transfer" of a local volume should be a
   manifest reference (V11's O(1) snapshot), reserving TRANSFER-style machinery for
   cross-node moves.

### 8.5 Things hecate got right that are easy to lose

"No modes, ever" with the laptop as the derived N=1 (`GRILLING.md:17-19`); typed exhaustion
instead of OOM; "a drop with no signal is a bug"; generational handles instead of `Arc`;
deterministic maps and a seeded driver so the SIM is bit-reproducible; "physics over
policy" as the design test; every constant carrying its derivation; the K8s ergonomics bar
"declare-don't-place, mount-anywhere, identity-not-location" with stronger native
mechanism underneath (`GRILLING.md:1931-1939`); and the honesty discipline of the specs
(THIN sources named, rejected alternatives recorded, tripwires armed).

---

## Appendix A — file index (hecate)

| Path | Lines | Status header | Tests / AC |
|---|---|---|---|
| `CONTEXT.md` | 208 | glossary (glossary wins) | — |
| `docs/architecture/PLATFORM.md` | 180 | law-level; §8 Sylk fault ledger | — |
| `docs/architecture/SUMMONING.md` | 233 | law-level; §5 VFS volumes, §11 local-is-distributed | — |
| `docs/architecture/SKILLS.md` | 101 | law-level; Q17 two layers over MCP | — |
| `docs/architecture/LEDGER.md` | 451 | law-level; §7 protocol summary, §8 durability | — |
| `docs/architecture/AGENTS.md` | 363 | law-level; ten-agent roster, rank matrix | — |
| `docs/adr/0001–0006` | 18–58 | no status headers (GAPS C-10) | — |
| `docs/GAPS.md` | 569 | Branch 29 gap ledger | — |
| `docs/specs/VFS.md` | 224 | presented (Br 5) | V1–V12 / 8 |
| `docs/specs/SERVING.md` | 282 | ACCEPTED 2026-08-16 | FS1–FS15 / 10 |
| `docs/specs/STORE.md` | 657 | ACCEPTED 2026-08-22 (COLLECTOR bar) | ST1–ST15 / table |
| `docs/specs/WAL.md` | 211 | ACCEPTED 2026-08-17 | W1–W11 / 9 |
| `docs/specs/CONSENSUS.md` | 433 | ACCEPTED 2026-08-17 | CS1–12, CN1–16 / 11 |
| `docs/specs/FAULTS.md` | 230 | ACCEPTED 2026-08-17 | F1–F8 / 6 |
| `docs/specs/PROTOCOL.md` | 365 | ACCEPTED 2026-08-18 | P1–P19 / 14 |
| `docs/specs/WIRE_FORMAT.md` | 213 | ACCEPTED 2026-08-17 | WF1–WF10 / 7 |
| `docs/specs/WIRE_SECURITY.md` | 239 | ACCEPTED 2026-08-18 | P-a1..a5 / 8 |
| `docs/specs/TRANSFER.md` | 272 | ACCEPTED 2026-08-17 | TR1–TR10 / 7 |
| `docs/specs/OBJECT_TIER.md` | 403 | presented (Br 24); §9 + two planes ratified | OT1–OT19 / 8 |
| `docs/specs/CACHE.md` | 290 | ACCEPTED 2026-08-20 | — / 10 |
| `docs/specs/QUEUE.md` | 219 | ACCEPTED 2026-08-20 | — / 10 |
| `docs/specs/ARCHIVE.md` | 484 | presented 2026-08-22 | AR1–AR12 / table |
| `docs/specs/MATERIALIZER.md` | 277 | ACCEPTED 2026-08-20 | M1–M10 / table |
| `docs/specs/LEDGER_SUBSTRATE.md` | 233 | ACCEPTED 2026-08-20 | LS1–LS8 |
| `docs/specs/LEDGER_CORE.md` | 246 | ACCEPTED 2026-08-16 | L1–L15 / 6+1b |
| `docs/specs/SESSIONS.md` | 248 | ACCEPTED 2026-08-16 | SES1–SES13 / 8 |
| `docs/specs/MERGE.md` | 436 | ACCEPTED 2026-08-18 | M1–M17b / 13 |
| `docs/specs/RUNTIME.md` | 203 | ACCEPTED 2026-08-17 | T1–T13 / 11 |
| `docs/specs/HEALTH.md` | 106 | ACCEPTED 2026-08-16 | H1–H8 / prose |
| `docs/specs/MONITORING.md` | 567 | ACCEPTED 2026-08-22 | MO1–MO13 / table |
| `docs/specs/TRACING.md` | 410 | ACCEPTED 2026-08-22 | TR1–TR12 / table |
| `docs/specs/SKILLS_API.md` | 126 | ACCEPTED 2026-08-16 | S1–S9 / prose |
| `docs/specs/IAM.md` | 614 | ACCEPTED 2026-08-18 | IAM1–36, IAMS1–6 / 16 |
| `docs/specs/SCHEDULER.md` | 320 | presented (+ amendment accepted) | SCH1–SCH21 / 10 |
| `docs/specs/PODS.md` | 326 | presented (Br 6) | T1–T24 / 13 |
| `docs/specs/AGENTS_RUNTIME.md` | 144 | presented (Br 7) | R1–R11 / 6 |
| `docs/specs/COLLECTOR.md` | 1330 | ACCEPTED 2026-08-22 (headings only read) | CL1–CL12 |
| `GRILLING.md` | 8861 | dialogue ledger | — |

## Appendix B — GRILLING.md ranges read (for follow-up)

L1–130 rules and SETTLED table · L130–200 open-branch headers incl. Branch 20's etcd
challenge · L200–300 Branch 21 serving decisions (a)–(g) · L540–800 Branch 24 object tier
(Tectonic lead reference, copysets×HRW withdrawn, two planes) · L850–1000 Branches 27–32 ·
L1000–1130 Branches 33, 35, 36, 37, 32-widened, 38, 39 · L1262–1300 cost ledger ·
L1500–1570 consensus re-analysis + cross-region receipts · L1600–1700 transport
determinations and the QUIC re-ratification · L1930–2060 K8s frame, volume/SMR receipts,
MERGE arc, A1–A4 re-audits · L2090–2130 VFS audit receipts · L3060–3140, L3200–3340 the
arena-reuse overrule and engine-choice dossier · L4620–4850 history-channel research
(memfd, io_uring, measured IPC tiers) · L6120–6140, L6500–6660 the laptop directives ·
L6795–6830, L7100–7130, L7550–7590 cache doctrine (moka/tokio rejection, slab arena, ValKey
study) · L7695–7740 environment-derived durability and flash · L8050–8140 STORE settlement
· L8320–8360 the COLLECTOR bar.

Verification pass (2026-09-03, consolidation): the ranges above that carry quoted
text — L1500–1537 (etcd dossier), L1603–1604 (per-path MTU), L1931–1964 (K8s frame +
volume receipts), L2095–2110 (VFS audit receipts), L3103–3110 and L3210–3219
(arena overrule + engine dossier), L4727–4729 and L4741–4805 (io_uring
SINGLE_ISSUER, the producer-never-blocks law, measured IPC tiers), L6799–6810
(moka/tokio), L7696–7714 (environment-derived durability), L8052–8056 and
L8099–8113 (STORE settlement), L8345–8348 (the COLLECTOR bar) — were re-read
directly and match the quotations. `MERGE.md` lines 39–47, 123–124, 197–198,
270–283, 330, 363–371, 399–401, 425–426 and `AGENTS_RUNTIME.md` 54–57, 92–96 were
likewise re-read. Grep census re-run the same day (case-sensitive, so counts differ
slightly from the header's case-insensitive census): etcd 24 · memfd 12 · tmpfs 8 ·
FUSE 48 · ProjFS 1 · NFS 20 · io_uring 7 · thread-per-core 2 · lock-free 9 ·
hermetic 0 · hugepage 0 · mlock 0 · "magic number" 0 (the phrase never appears; the
doctrine is always stated as "constants-from-data" / "derivation at the definition
site").

