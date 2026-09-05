# hecate's merge architecture, examined for slates: what carries over, what does not, and why

> **Current contract, A-9 (2026-09-05).** §4.16 requires a complete immutable green base, contributing-client barrier, retained inputs and placed-before-reference. The pure merge core is not the integrated service.
> See [the contract review](hecate-contract-review.md) and [the unified design](../SLATES_DESIGN.md).
> The rest of this file is dated research evidence; conflicting recommendations are superseded.

Status: complete (written serially by the architect, 2026-09-04, for amendment A-5). Every hecate
quotation was read directly from `../hecate/docs` on 2026-09-04 and is cited to file and section
(line numbers where the grep gave them). Evidence tiers as in `README.md`.

## 0. The question

Ada (2026-09-04): "bring in the merge architecture from hecate. Examine it, identify any gaps or
problems with integrating it with our existing architecture, and then work it into our design.
... maximally correct, robust, scalable, efficient, performant, and fast."

slates' design promised "clone + merge for optimistic parallelism; never last-writer-wins" (D-16)
and then declared merging a non-goal (Part 1.3). hecate has a fully argued merge architecture.
This file examines it, lists the integration gaps, and records the adaptations that D-27 and
§4.16 make.

## 1. hecate's architecture, as written (verbatim where it matters)

### 1.1 Green and the increment pipeline (`docs/specs/MERGE.md` §0-§1)

- "Green — the session's shared staging volume ... It is *not a disk anywhere*: it is a numbered
  series of **tables of contents** (manifests). Version 43 is a small document naming blobs; it
  shares every unchanged blob with version 42 by name — that structural sharing *is* the
  copy-on-write. Any pod on any node attaches to any version (§6); readers attach to immutable
  versions; **no mount writes green — its writer is the log** (§2)."
- Increments: "constant-size descriptor messages (never bytes; §4)": `{claim, base: GreenVersion
  (declared, never inferred), post_state: ManifestRef, ops_doc: ContentRef}`.
- Pipeline "inside the proposer, deterministic end to end": `dedup` (identity = hash(claim, base,
  post_state, ops_doc); "seen-in-log ⇒ ack-without-reapply (retries always safe)") → `fetch`
  (outside the pure core) → `map` ("position-map ops through canonical deltas (base..head]") →
  `verdict` → `apply` ("splice accepted ops into a new manifest (chunk-granular)") → `place`
  ("push the version's new blobs to the session-group members; await acks") → `commit` ("merge
  record {id, verdict, version, manifest_hash, term} ... a merge that cannot be recorded never
  applies; a version that is not placed is never referenced").

### 1.2 Writer authority (`MERGE.md` §2, line 137; `CONSENSUS.md` §6, lines 210-250)

- "**The merge proposer is a role of the session group's Raft leader.** The fence is the
  **term** — stamped on every merge record, enforced natively by Raft ... There is no second
  epoch". Succession is leadership; "Warm followers apply every record continuously (§5) and
  hold the placed bytes".
- Rejected alternative: "a proposer fenced by `SerializerOpen{generation}` markers, free-floating
  relative to leadership — is CockroachDB's pre-fortification leaseholder architecture: a
  separately-fenced in-group writer whose divergence from the Raft leader produced the documented
  'leader-leaseholder splits' (indefinite-outage variant), retired by the Leader Leases protocol
  change that merged the roles".
- The roster rule for everyone else (`CONSENSUS.md:219-227`): standing writers "hold a
  **meta-tree lease in Chubby's coarse-grained shape** (keepalives, grace period) **plus an epoch
  fencing token enforced at the resource** — non-negotiable, because a paused-and-resumed writer
  defeats any lease alone ... **The merge proposer is the exception: it is leader-fused**".

### 1.3 The verdict (`MERGE.md` §3, lines 179-205)

"A deterministic function of (canonical history since `base`, mapped increment, referenced
immutable content). Classes: **Accept** (all mapped ranges disjoint from intervening effect
ranges) · **AcceptIdentical** (same-range concurrent inserts with identical bytes — recognized,
not resolved) · **Conflict** (overlap/containment; edit anchored in a concurrent delete;
same-position differing inserts; rename/rename; create/create differing)." Pass one: "sweep-line
interval overlap on declared ops, O((n+m) log(n+m)) ... No diff inference exists anywhere — the
diff3 pathology family stays structurally unreachable." Fetch between passes. Pass two: "memcmp
results in hand ... Common path: zero content reads. Rare path: one range-compare". "Position
mapping is one-directional through canonical history and composes: `map(a..c) = map(b..c) ∘
map(a..b)` (M4). Leases fast-path, never guard." "Verdict purity is the engine's central theorem".

### 1.4 The ops document (`MERGE.md` §4)

"**The deriver emits the composed net op set**: witnessed ops compose by pure interval algebra
(Insert-then-overlapping-Delete cancel and split) ... **Composition of declared ops is arithmetic
on facts; reconstructing ops by comparing file states is the banned diff inference.** The deriver
does only the first." The ops document is "a hecate-wire header + a flat array of 32-byte records
`{kind, path_idx, at, len, src_off}`"; "No byte-bearing field compiles in any increment type";
"Message size is invariant in op count and content size"; retention: "merge-log-referenced
manifests and ops documents are ... GC liveness roots until log truncation".

### 1.5 Apply and replication (`MERGE.md` §5; `SERVING.md` §4)

"**Splice**: accepted ops rewrite only the chunks their mapped ranges touch". "**Placed strictly
precedes referenced**: the merge record referencing manifest `#abc` **does not commit until the
version's new blobs are replicated to the session-group members and acked**". "**Appliers
recompute — inputs are authoritative**: every session-group member applies each committed record
by **recomputing the full two-pass verdict from the record's inputs** ... The recorded verdict and
manifest hash are **cross-checks only, never an apply path**: a mismatch ... is fatal-and-loud".
"replicas compare green-head manifest hashes per log index — O(1), because the state IS a hash".
`SERVING.md` §4: "The merge gate **extends, never writes**; nothing mutates in place"; "Green
serving instances are compiled with no write path"; "Pods pin `green@version` (the lease basis);
re-bind happens pod-initiated at increment boundaries — manifest diff → targeted invalidations for
changed paths only."

### 1.6 Attachments, submission, conflicts, contention (`MERGE.md` §6-§10)

- §6: "Readers attach to **immutable versions** ... version advance is the pod's next re-attach,
  an explicit lifecycle event. **No mount writes green**".
- §7 submission: "resolver-cached direct submission to the session-group leader, with the
  **piggyback rule**: any NACK from a non-leader carries the current leader identity and term, so
  one retry suffices"; "resolver refresh is **single-flight per session**"; "The agent **parks**
  on submit"; failure semantics: "timeout ⇒ resubmit (identity dedup makes retries always safe)
  ... submitter dies after commit ⇒ the claim's state shows the increment applied". Latency:
  "single-digit ms intra-region, pipelined ... Laptop: identical sequence, in-process, µs."
- §8: "a `Conflict` carries per-path byte windows plus the refs of the colliding claims —
  evidence, not markers ... rebase against the window, resubmit (a rebased increment has a new
  identity; dedup never blocks it) ... Rejects are non-blocking; the proposer never stalls."
- §10: "same-scope work serializes above the engine (claim scopes ...); the engine's rejects are
  the residue"; tripwires "sustained rebase-retry rate on a region" and "p99
  intervening-deltas-per-merge ⇒ reopens the eg-walker branch (ADR-0005)".
- §11 laptop: "One node: the session group is one replica (self-ack), placement is a local write,
  every arrow in §7 is an in-process call ... Same code, same sequence, no modes."

### 1.7 The streaming gate (`docs/adr/0003-streaming-merge-gate.md`; `LEDGER.md:218`)

"Completed work does not land in one batch at testament close. The Engineer's work streams as
increments; each increment's own validations (Guardian safety, lint, conflict check) pass and it
merges into green immediately ... the claim's whole-work validations (tests green, Inspector
approval) gate the **disk commit**, not green entry. Failures fix forward via superseding
increments — green is never rolled back in place." Invariant: "**green = increment-validated
work; disk = claim-satisfied work.**" An "amber" pending layer was rejected.

### 1.8 Witnessed writes and the seal (`SERVING.md` §2-§3; `VFS.md` §5)

"every guest write arrives as `FUSE_WRITE`. The daemon appends a content-bearing record to the
volume's logical log — `{inode, offset, len, bytes, prev_version}`"; scratch scope: "`target/`,
`node_modules/`, caches ... mount a pod-local **scratch volume** — unwitnessed, unjournaled,
unmergeable". Seal: "drain guest writeback (`FUSE_FSYNC` sweep) → freeze epoch → derive per-file
edit ops by diff against `prev_version` (pure, version-pinned deriver ...) → CDC-chunk → BLAKE3 →
CAS → manifest update". `VFS.md` §5: "at seal, per-file edit ops are derived from successive
witnessed versions by a pure, version-pinned deriver".

### 1.9 Tests and acceptance (`MERGE.md` §12-§13)

M1/M2 verdict purity and zero false accepts/rejects versus an oracle; M4 mapping composition;
M6 apply/record atomicity under power cut; M8 rename matrix; M13a-f leadership races and the
leader-fused roster; M14a-h increment shape (no byte-bearing fields; same stream ⇒ identical
ops-doc ref; splice ≡ reference applier; identical edits cost one range-compare; GC roots;
constant message size; write-then-revise composition; merge-path p99 ratchet); M15a-e placement
before reference, applier convergence, promotion latency, lying-proposer injection; M16a-c
attachment-only, version stability, re-attach anywhere; M17/M17b submission-transaction fuzz and
resolver storms. Acceptance: "No LLM/clock/RNG/IO in either pure pass (lint + M12)"; "Conflict
windows byte-exact"; "Throughput and latency floors ratcheted from first CI baseline (merges/sec;
verdict µs p99; merge-path p99 incl. placement; promotion latency)".

## 2. Gaps and problems in bringing it into slates

1. **Journal granularity.** slates' journal records `{seq, op, path(s), inode, epoch}` (§4.5):
   path-level. hecate's verdict needs declared byte-range operations with a per-file version. Fix
   (§4.5): every write records `{at, len, prev_version}`; truncates record the new length; bytes
   stay in the extents, never in the journal.
2. **hecate contradicts itself on the deriver.** `SERVING.md` §3 and `VFS.md` §5 say ops are
   "derived ... by diff against `prev_version`"; `MERGE.md` §4 says "reconstructing ops by
   comparing file states is the banned diff inference. The deriver does only [composition]".
   slates follows `MERGE.md`: the deriver composes declared operations by interval algebra and
   never diffs. Consequence: a tool that rewrites a whole file (editors save by truncate-and-write
   or write-and-rename) declares one whole-file operation, which conflicts with any concurrent
   operation on that file unless the results are identical. That is the correct conservative
   answer; the ergonomic answer is an SDK `edit` verb that declares true insert and delete ranges
   so agents' edits are fine-grained by construction.
3. **POSIX writes are overwrites, not inserts.** `pwrite` replaces bytes in place and extends at
   the end; only SDK edits insert or delete in the middle. slates' op kinds therefore include
   `Overwrite`, `Extend` and `Truncate` beside `Insert` and `Delete`; overwrites never shift
   positions, size-changing ops do; the mapping and the sweep line handle both.
4. **Writer authority.** hecate fuses the proposer with a per-session Raft leader. slates has one
   pointer consensus group for all volumes (A-2) and partitioned single-writer execution (D-7);
   fusing every green's verdict work into the one group leader would centralize it. slates
   classifies the merge task as a standing writer with lease plus fence (hecate's own default
   class) on green's owner shard: leases are consensus-issued, expire in followers' clocks plus a
   drift bound, and the merge record commits only if its epoch equals green's current lease. The
   CockroachDB split hecate cites was a lease whose validity could diverge from leadership and
   stall; here the lease is itself consensus state, the fence is checked at commit, and PreVote
   with CheckQuorum is already in the Raft dialect. Recorded as a departure in D-27 with a
   tripwire (sustained `StaleLease` refusals on merge records reopen the decision).
5. **Who applies.** hecate's session-group followers hold the bytes and recompute. slates places
   sealed content on W holders chosen by rendezvous; those holders recompute the verdict and the
   manifest identity before serving a version, and compare head identities per version; on a
   laptop the owner is the only holder and no second computation exists (the derived N=1 case).
6. **Splice and chunking.** hecate re-chunks the affected region with CDC at splice. slates'
   chunks are page-multiple fixed sizes with sub-chunk extent references (§4.5), so the splice is
   extent-list surgery that copies nothing; CDC for the large class stays a background sealer job
   and never runs on the merge path.
7. **Placed before referenced.** slates already refuses a pointer that names unplaced content
   (§4.8). For an increment this means the work volume's seal must be `Placed(W)` before its
   merge record commits; in a fleet `submit` pays one placement round trip, pipelined; on a
   laptop it is local.
8. **Cross-shard chunk references.** Green's version references chunks sealed by the work
   volume's owner shard. slates already needs cross-shard references for clones; the increment's
   post-state snapshot stays pinned by the merge record, and one batched acquire message per
   increment records the references (§4.8 cross-partition rule).
9. **Hard links and symlinks.** hecate has neither ("Single-parent, no hard links"). slates has
   both. Operations are keyed by path; an inode with two names appears under each name in the ops
   document (conservative verdicts); link counts are derived, never merged; a symlink target
   change is a whole-entry replace.
10. **No Guardian, Arbiter, or claims.** slates has no validation pipeline. Increments carry
    opaque evidence references; a green volume's policy may require them (`require_evidence`);
    what counts as validation is the harness's. hecate's invariant becomes: green accepts
    increments by verdict; the disk is written only by a granted landing (A-4).
11. **Scratch scope.** hecate mounts `target/` and `node_modules/` as unwitnessed scratch volumes.
    slates journals everything for snapshots and excludes named subtrees from increments by a
    filter that is part of the increment identity.
12. **Long divergence.** hecate uses eg-walker at landing for hours-to-days divergence. In slates
    both merge sides have declared operations on one chain, and canonical deltas older than the
    retention fold into exact checkpoint deltas by the composition law, so any base version maps;
    eg-walker is not needed. The disk side of a landing has no declared operations, so the
    entry-level verdict of A-4 stays as it is.
13. **Version-pinned attachments** already exist in slates (an attachment pins a snapshot; bridges
    invalidate on snapshot swaps); the merge engine adds `advance` with targeted invalidations
    from the manifest diff.
14. **Submission topology.** On a laptop a submit is a cross-shard message; in a fleet the wire,
    with the placement-cached owner, the piggyback rule on NACK, single-flight placement refresh
    per green, exactly-once by completion records (RIFL) which slates already has.
15. **Latency budget.** hecate: "single-digit ms intra-region ... Laptop: µs". slates' merge-path
    budget is ratcheted from the first baseline; verdict cost is per op record, so the increment
    size budget is derived from the measured per-op cost and the budget.

## 3. Theory and precedents the design leans on

- Optimistic concurrency control: validation of write sets at commit [A: Kung & Robinson, ACM
  TODS 1981]. The increment is the transaction; canonical rebase is validation against
  intervening commits.
- State-machine replication with input logging: replicas apply inputs deterministically and
  outputs are cross-checks [A: Schneider, ACM Computing Surveys 1990; A: Thomson et al., Calvin,
  SIGMOD 2012; C: TigerBeetle's deterministic apply]. hecate's "appliers recompute — inputs are
  authoritative".
- Raft §5.3 follower apply; PreVote and CheckQuorum against paused leaders [A: Ongaro &
  Ousterhout, USENIX ATC 2014; C: the hecate Raft dialect already chosen in D-14].
- Why not symmetric operational transformation: TP2-requiring algorithms were repeatedly shown
  incorrect and TP1+TP2 is unsatisfiable for plain insert/delete on strings; under a central
  serializer rebasing replaces transformation [C: hecate ADR-0005's falsification record; A:
  Imine et al., "Proving correctness of transformation functions in real-time groupware", ECSCW
  2003 (the counterexamples), cited there].
- Why not an inferred merge: diff3 is unstable and can produce results neither side wrote [A:
  Khanna, Kunal & Pierce, FSTTCS 2007]; structural mergers silently miss real conflicts
  [C: hecate SESSIONS.md §5 receipts].
- Event-graph replay for long divergence [A: Gentle & Kleppmann, "Collaborative Text Editing with
  Eg-walker", EuroSys 2025]: not needed here because both sides have declared operations on one
  chain (§2 item 12); kept as hecate's tripwire target for the record.
- Interval overlap by sweep line in O((n+m) log(n+m)) [B: Preparata & Shamos, Computational
  Geometry, 1985; A: Bentley & Ottmann, IEEE TC 1979].
- Conflicts as first-class values and "a resolution is itself a change" [C: Pijul; C: jj].
- Fixed-layout records cast in place at memory speed [C: Cap'n Proto, FlatBuffers, Arrow, LMDB;
  C: hecate MERGE.md §4 receipts]; slates' wire already uses fixed-layout headers (D-15).
- Leases with fencing tokens at the resource [A: Gray & Cheriton, SOSP 1989; C: hecate
  CONSENSUS.md §6 standing-writer rule].
- Attach-by-version and invalidate-by-manifest-diff [C: EdenFS checkout and invalidation; C: OCI
  image-by-digest sharing; C: hecate SERVING.md §4].

## 4. What must be measured

Per-op verdict cost (pass one per record; pass two per candidate byte); mapping cost per
intervening delta and the checkpoint spacing where folding wins; merge-path latency (seal,
compose, submit, verdict, splice, commit, reply) on a laptop and across hosts; merges per second
per green; the base-lag distribution (versions between an increment's base and head) that sets
delta retention; the conflict rate by source (SDK edits versus whole-file tool rewrites), which
decides how loudly the skills push the `edit` verb; `StaleLease` refusals on merge records;
rebase-retry rate; increment size distribution; holder recomputation cost.

## 5. Risks

- Whole-file rewrites by tools inflate conflicts on shared files; mitigated by the SDK `edit`
  verb, by excluded subtrees, and by streaming submission (short divergence); measured, tripwired.
- The one pointer group is the fleet's commit bottleneck for merge records; D-O12's measured
  sharding applies, and the natural shard key is the green volume.
- Semantic conflicts (disjoint ranges, incompatible intent) are invisible to any textual engine;
  hecate answers with validation before green entry; slates answers with evidence policies on
  green and with the landing gate before disk.
- Holder recomputation adds work per version per holder; it is descriptor-sized and bounded by
  the increment size budget; a persistent mismatch is a bug signal, never reconciled.
