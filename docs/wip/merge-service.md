# §4.16 merge service — Green/Work roles, immutable bases, pinned attachments, the barrier, placement and holder recomputation

> Status (2026-09-13, branch `agent/merge-service`). GAP-A9-14's service layer is built on the single
> node and across the fleet, on the engine representation the pure core already used (an in-memory
> `Green` per green, an in-memory work journal per work): the roles enforced at every verb with typed
> refusals; a green's chain started from scratch or from a **complete immutable base** (never a live
> host directory — tested with a host edit after the create); version-pinned green attachments moved
> only by `advance`, with the invalidation set named; the submission barrier (an accepted submit
> consumes the work's journal, the bug it fixes on record) and every input to a verdict retained by the
> durable chain, not the work; merge records placed before they are referenced (inputs through the §4.10
> content exchange at `f + 1`, then the record on its own stream, in order per holder) and recomputed by
> every holder before acceptance, a mismatch fatal-and-loud for the green on that holder; and the CLI and
> MCP flows by use. **Owed** (each named at the end): the mounted work volume (T-6.15's "mutate via a
> mounted client") — a work is not a VFS volume yet, so mutation is through `edit`/`declare` (the SDK
> write the design names as the agent's preferred write); a green is not mountable at all, so the
> bridge's `EROFS` is unreachable rather than typed; the extent-backed green chain.

## 1. Inventory: what existed against what §4.16 says (before this branch, at `4f5deae`)

| Design requirement | What existed | Where |
|---|---|---|
| `Role { Plain, Work { green, base, .. }, Green { require_evidence, head } }` in the catalog | Present. `head_version` written at create and never updated (dead field; the engine's `head()` is the truth). | `crates/db/src/catalog.rs:152-172`; `crates/server/src/verbs.rs` `create_green` |
| Green written only by the merge task; SDK writes refuse `ReadOnlyVolume`; `NotGreen`/`NotWork` | **Absent.** `Edit`/`Declare`/`Submit`/`Rebase` looked the id up in `state.works`, `Versions`/`ChangedSince`/`CreateWork` in `state.greens`; a wrong role answered `NotFound`. No `ReadOnlyVolume`, `NotGreen`, `NotWork`, `UnknownBase`, `EvidenceRequired`, `ConsistentBaseUnavailable`, `ContentUnavailable`, `StaleEpoch` on the wire. `require_evidence` was recorded and never checked (a false success). | `crates/ipc/src/protocol.rs` `Refusal`; `verbs.rs` `edit`/`declare`/`submit`/`rebase`/`versions`/`changed_since`/`create_work` |
| A green is a volume: mounts answer `EROFS`, attachments pin a version, `advance` re-pins | **Absent.** A green (and a work) had no `VolumeSlot`: not in `by_id`, so `attach`, `status`, `destroy`, `snapshot`, `clone`, `land`, the base verbs and the NFS root listing all answered `NotFound` (a green could not be mounted at all). No `Advance` verb; `Attach` pinned nothing. | `verbs.rs` `find`, `attach`, `nfs.rs` `entries` |
| The chain starts from scratch or a complete immutable base | `CreateGreen { name, require_evidence }`: scratch only, `BaseRecord::Scratch`, `Green::new()`. No base option; a live host directory was unreachable only because no base was reachable. | `verbs.rs` `create_green` (was ~2491) |
| Submission seals behind the attachment barrier; inputs retained until the record commits | `submit` composed the work's journal into an increment, guard-checked `Op::GreenAdvanced`, ran the verdict, appended the record (the increment's doc and post-state inside it, durable). The work was left untouched after an accept: its journal still held the merged operations, so its next submit re-declared them against the old base and conflicted with its own bytes (`docs/bugs/2026-09-13-work-resubmit-self-conflict.md`). | `verbs.rs` `submit` |
| Fleet: the record sent to `2f + 1` holders, committed at `f + 1`, issued only when every identity is placed; holders recompute | **Absent.** The record plane shipped `HeadValue`s for volumes in `state.volumes` only; a green was never replicated, no merge record existed on the wire, no holder replica. | `crates/server/src/fleet.rs` `unplaced_heads`, `ship_head` |
| `slates.merge`, `edit`, the CLI verbs | `create_green/create_work/edit/declare/submit/rebase/versions/changed_since` over MCP and the CLI; no `advance`, no `read`, no base, no evidence on `submit`, `green` could not set `require_evidence` from the CLI. | `crates/mcp/src/lib.rs`, `crates/cli/src/args.rs` |
| Pure core (verdict, ops document, deriver, position map, splice, engine) | Complete and oracle-tested; not re-derived here. | `crates/merge` |

## 2. What landed, by commit

### `bcf15b5` merge: origin, head identity, invalidation span, evidence

- `crates/merge/src/origin.rs`: `Origin` — a green's version-0 state (files with bytes, dirs,
  modes, symlinks, hard links, xattrs), canonical encoding (tables sorted, length-delimited,
  little-endian, magic `GORG`), `decode` exact and hostile-checked (every count checked against the
  remaining bytes before allocation).
- `Green::with_origin` seeds every dimension at version 0 with a history entry, head stays 0.
- `Green::head_identity`: BLAKE3 of a tagged canonical encoding of every dimension — what a holder
  recomputes and compares. Golden vector `1fe971c080517dbe400bfbacca80db3df1fc451d8b4c934a9e947c7ed2ab50d8`
  for the fixed origin in `crates/merge/tests/origin.rs`.
- `Green::changed_between(from, to)`: the paths any dimension changed at a version in `(from, to]`,
  read off the histories (the last-changed index alone misses a path changed inside the span and
  again after it).
- `Increment.evidence: Vec<[u8; 32]>` persisted after the post-state, count-checked on decode.
- `cargo test -p slates-merge` — 130 tests (2026-09-13).

### `cd7e00d` db: `Op::GreenOriginated`

- Appended last in `Op`; guarded once-before-any-increment (`AlreadyExists`) and against the chain
  byte budget (`Capacity`); reclaimed on destroy; `GreenChain.origin` appended to the partition
  snapshot; `Partition::green_origin`. The crash-recovery model gained a `GreenOriginate` step.
- `cargo test -p slates-db` — 75 tests.

### `29fee69` vfs: snapshot completeness; `read_in` serves pinned base bytes

- `crates/vfs/src/coverage.rs`: `Volume::snapshot_is_complete(store, id)` — a scratch snapshot always;
  an overlay's only when every merged directory in the **frozen node** holds every name its base
  listing found (`BasePlane::listed_names`, a three-line addition in `base.rs`) and every base-backed
  file is witnessed, pinned whole and not lost. Stated on the frozen tree, so a listing or pin after the
  freeze cannot make an older snapshot complete.
- `Volume::read_in` gained the base-backed arm the head's `read` had: pinned extents serve, an unpinned
  range is `BaseUnavailable`, a lost entry `BaseDrift` — a frozen pinned file no longer reads as zeros
  (only VFS tests called `read_in` before; no user-visible path was affected).
- `cargo test -p slates-vfs --test coverage` — 2 tests, `--lib coverage` — 1.

### `9917982` server: the merge service, single node

`crates/server/src/merge_service.rs` (new; the join with the daemon is one field on `ShardState`, one
init line, and one-line guards at the verbs):

- **Roles.** `require_green`/`require_work`/`refuse_store_verb`: edit, declaration, write attachment,
  snapshot, resize of a green → `ReadOnlyVolume`; a work verb on a plain volume → `NotWork`; a green
  verb on a work or plain volume → `NotGreen`; clone/land/base verbs on a merge volume →
  `Unsupported` naming the verb. Destroy and status serve merge volumes (a destroyed green makes its
  works' submits `UnknownBase { green, version }`).
- **A complete immutable base.** `CreateGreen { base: Some(GreenBase { volume, snapshot }) }` routes to
  the base volume's owner shard, refuses `ConsistentBaseUnavailable` unless `snapshot_is_complete`,
  walks the snapshot into an `Origin` (the seed is bounded by the chain byte budget it is recorded
  against; cooperative slicing owed with the extent-backed green), records `GreenOriginated` before
  seeding the engine (guard-then-apply), and `rebuild_green` re-seeds the origin before replaying the
  chain.
- **Pinned attachments.** A read attachment of a green pins the head; `Advance { attachment,
  version? }` re-pins and names `changed_between`; `Read { volume, path, at: Head | Version | Attachment }`
  serves a green's head, a version or the pin, a work's or a plain volume's live tree (§4.12 `read`).
- **The barrier and retention.** `Submit { work, evidence }` refuses `EvidenceRequired` on a
  `require_evidence` green, `UnknownBase` for a destroyed green or a base past the head, and on accept
  moves the work to the new version with its journal consumed. Inputs are retained by the chain.
- Wire (`crates/ipc/src/protocol.rs`): `Refusal` gains eight variants **appended**; `CreateGreen.base`,
  `Submit.evidence`, `Attached.version` (fields); `Advance`, `Read` appended last in `RequestBody`,
  `Advanced`, `ReadBytes` appended last in `ReplyBody`. **Every schema hash of these bodies changes**;
  the integrator reorders the appended verbs against the digest agent's appended verb at merge.
- Client: `create_green_over`, `submit_with_evidence`, `advance`, `read` (+ `begin`/`spin`/`poll`);
  `create_green`/`submit` keep their signatures (the SDKs bind them).
- Tests: `cargo test -p slates-server --test daemon -- --exact
  the_merge_service_enforces_roles_pins_versions_and_seals_behind_the_barrier` — 4 scenarios, 4.45 s:
  `merge_role_scenario` (every refusal above; evidence required/carried), `green_over_base_scenario`
  (unpinned overlay snapshot refused; pinned one accepted; a same-length host rewrite after the create
  changes neither version 0 nor the head nor a work cloned from it), `green_attachment_scenario` (pins
  1, version 2 lands, the pinned read is unchanged and refuses the later file, advance → 2 with
  `["f", "g"]`, back to 1, past the head `UnknownBase`, detach), `submission_barrier_scenario`
  (submit, edit, submit → 2 with only the later edit; the work equals the green at 2; a third agent lands
  3; the work destroyed and versions 1 and 2 still read).
- Failing test first: with the journal-consumption block disabled the barrier scenario refused
  `[MergeWindow { path: "f", at: 0, len: 0, class: 4 }]` (create/create) on the second submit; with it,
  `version: Some(2)`.
- Lifecycle umbrella `the_daemon_serves_the_lifecycle_verbs_exactly_once_with_leases_and_typed_refusals`
  13.15 s; `-p slates-server --lib` 34; `-p slates-ipc --lib` 7.
- Recovery of a base-seeded green, by use (`crates/client/tests/client.rs`,
  `a_base_seeded_greens_origin_survives_a_daemon_restart`, `--exact`, 1.79 s): an overlay pinned whole
  and snapshotted, a green over it advanced once, the daemon restarted over the same segment — version
  0 reads the origin's bytes, version 1 the increment over them, a new increment lands as 2. Failing
  first: with the origin replay in `rebuild_green` disabled the recovered head is 0 — the one increment
  (an edit of an origin file) cannot replay over an empty version 0 at all.

### `f9e98dd` server: placed before referenced, recomputed by every holder

- Owner (`merge_service.rs` fleet half): `MergeRecordValue { version, increment, base, inputs,
  identity, evidence }` (canonical Wire; round-trip and every-cut tests); `enqueue_record` at create
  (version 0) and each accepted submit; at `f = 0` the local hold is the quorum and the record is
  placed at once (nothing pending on a laptop). `run_merge_period` (called from
  `fleet::run_record_period` after the seals and heads) works one version per green per period, the
  lowest not held everywhere: inputs (the chain's own bytes as a one-chunk archive; identity from the
  bytes alone) put through `put_content` to every remaining candidate until `f + 1` verify and hold
  them — counted `merge.inputs_unplaced` each period the record waits — then the record shipped over
  `MERGE_RECORD_STREAM` (12) with `commit_record_on` (the cluster's `commit_record` gained a stream
  parameter) only to candidates that acknowledged the version before it; placed at `f + 1`; the entry
  leaves once every candidate holds it. Pending state is bounded by the chain (no second copy of the
  bytes).
- Holder: `accept_merge_record` — inputs must be held (`merge.inputs_unheld`, the record waits), the
  replica at the version before (`merge.out_of_order`), the recomputation must accept as that version
  and reproduce the identity (`merge.recompute_mismatch`: counted, printed, the green refused for good
  on this holder, no replica served); a match accepts into the object's acceptor as a head record.
  `await placed(green, region)` answers from the placed records.
- Test hooks on `Daemon`: `merge_record_placed(green, version)`, `merge_holder_state(green)`,
  `inject_merge_fault(MergeFault { refuse_content_puts, corrupt_next_inputs })` (the fault arms are
  the only additions to `serve_peer_records`'s content path, three lines).
- Tests (each `--exact`, two nodes, `f = 1`):
  `a_merge_record_is_issued_only_once_its_inputs_are_placed` — version 0 places; with B refusing
  content puts version 1 stays unplaced over a 2 s window (owner `merge.inputs_unplaced` ≥ 1, holder
  `merge.content_put_refused` ≥ 1, B's replica at 0); the work destroyed meanwhile; the fault lifted →
  version 1 places, B's replica at 1, `await placed` true — 6.43 s.
  `a_holder_whose_recomputation_mismatches_refuses_the_version_loudly` — B's inputs corrupted one byte
  (the post-state's last byte; the document still decodes) → `merge.recompute_mismatch` ≥ 1, no
  replica, refused, version 1 never placed at `f = 1`, `await placed` false — 6.48 s.
  One run of the first test timed out a client wait (the harness's 5 s deadline, its one wall-clock
  bound) while the workspace clippy build ran alongside; alone it passed 3/3.
- `-p slates-server --lib` 37; `-p slates-cluster --lib` 124.

### `93176ff` cli, mcp: the flow by use

- CLI: `green NAME [--require-evidence] [--base VOLUME --snapshot N]`, `submit WORK [--evidence HEX]`,
  `advance ATTACHMENT [VERSION] [--json]`, `read VOLUME PATH [--version N | --attachment A]`;
  `attach --json` carries `version`. `cargo test -p slates-cli --bin slates` — 19. The
  `SLATES_TEST_CLI`-gated flow gained `json_merge_reader` (attach pins 1 → a second work lands 2 →
  the attached read exits 1 `NotFound` while the head streams → `advance --json` names `g.txt` → the
  attached read streams → detach); **written, not run here** (the validation limits exclude the gated
  CLI tests).
- MCP: `slates.merge.create_green { base_volume, base_snapshot }`, `slates.merge.submit { evidence }`,
  `slates.merge.advance`, `slates.fs.read`, `slates.attach.attach` reports `version`.
  `cargo test -p slates-mcp --test mcp` — 1.28 s, the reader flow inside the merge loop.
- Paths are canonicalized at the service boundary (`edit`/`declare`/`read` strip leading slashes) so
  a declaration made with a slash and the origin walk's `f.txt` key the same entry.

## 3. Siblings found (reported, not all fixed)

- `Role::Green.head_version` in the catalog is written once and never advanced (the engine's head is
  the truth; `Versions` reads the engine). Left as is; a dead field to remove or maintain.
- `refusal_of_db` maps `DbError::Capacity` to `BadRequest("green_chains at capacity")` rather than a
  typed capacity refusal; the design names `IncrementTooLarge{limit}` — not added (the per-record
  verdict-cost budget it derives from is unmeasured).
- `read_in` read zeros for a base-backed frozen file (fixed in `29fee69`; no shipped caller reached it).
- The fleet test harness's client deadline (`DEADLINE_NS` = 5 s) is wall-clock and sensitive to a
  concurrent workspace compile (observed once; `docs/bugs/2026-09-13-fleet-suite-robust-to-cpu-load-per-daemon-progress.md`
  made the coordinator waits period-driven, the client wait is not).

## 4. Owed (named, with the reason)

- **The mounted work (T-6.15 "mutate via a mounted client").** A work volume's content and journal live
  in the engine model (`WorkState`), not in a VFS volume, so a work cannot be mounted; the design's one
  declaration path — the VFS journal (`crates/vfs/src/journal.rs`) fed by the bridge's writes and by
  `edit` alike, composed at the seal — needs works (and greens, for their version snapshots) to be VFS
  volumes with the engine's effects projected into them. The CLI/MCP flows here mutate through
  `edit`/`declare`, the SDK write the design names as the agent's preferred write.
- **A green mount answering `EROFS`.** A green has no `VolumeSlot`, so the NFS root listing never
  shows it and a mount by name fails at `MOUNT`; the bridge's read-only refusal is unreachable rather
  than typed. Same dependency as above.
- **The extent-backed green chain** (§4.16 "Splice", D-27): the engine keeps full byte copies per
  version; the VFS-backed green shares extents O(1). The origin seed is one inline walk bounded by the
  chain byte budget, not cooperatively sliced, for the same reason.
- **Pipelining and hedging of merge records**: one version per green per period, inputs put to every
  remaining candidate at once (no first-round/hedge staging — the inputs are small).
- **Green takeover and a late-joining holder's catch-up**: a taken-over green's promotion epoch, and a
  candidate that joins after version 0 cannot recompute (it refuses `merge.out_of_order` for good);
  both follow the head takeover path.
- **`Read` of a large file** travels through the client's bulk area in one reply (typed `BadRequest`
  past it); a streamed read is §4.12's stream result.

## 5. For the integrator: the ledger rows and the status blockquote

**GAPS.md, Merge (4.16) row (line ~34)** — replace the summary column with:

> Pure verdict, deriver and splice/engine components; the Green/Work **service** is built on them
> (2026-09-13, `agent/merge-service`): roles enforced at every verb with typed refusals
> (`ReadOnlyVolume`, `NotGreen`, `NotWork`, `UnknownBase`, `EvidenceRequired`,
> `ConsistentBaseUnavailable`), a green's chain from scratch or a complete immutable base (a host edit
> after the create changes no version), version-pinned attachments moved only by `advance`, the
> submission barrier with every input retained by the chain, merge records placed before referenced
> (§4.10 content exchange at `f + 1`, then the record in order per holder) and recomputed by every holder
> before acceptance (mismatch fatal-and-loud), and the CLI/MCP flow by use. Owed: the mounted work and
> green (VFS-backed volumes with the VFS journal as the one declaration path), the extent-backed chain.

**GAPS.md, GAP-A9-14 row (line ~1180)** — replace the description column with:

> Service-level Work/Green roles, CLI/MCP flow, pinned attachments and distributed recomputation
> **built** (2026-09-13); the mounted work/green (T-6.15's mounted client, the bridge's `EROFS`) and
> the extent-backed chain remain: a work is not a VFS volume yet.

**SLATES_DESIGN.md §4.16 status blockquote** — add after the existing `> **Status (2026-09-05).**`
paragraph:

> **Status (2026-09-13).** The pure core is a green-volume *service* (`crates/server/src/merge_service.rs`;
> GAP-A9-14): every verb enforces the catalog role — an edit, declaration, write attachment, snapshot or
> resize of a green refuses `ReadOnlyVolume`, a work verb on a plain volume `NotWork`, a green verb on a
> work or plain volume `NotGreen`, a destroyed green's works `UnknownBase`, a `require_evidence` green
> `EvidenceRequired` — and a green's chain starts from scratch or from a complete immutable base:
> `CreateGreen { base }` walks a snapshot the volume core certifies complete (every merged directory
> listed into the frozen node, every base-backed file witnessed and pinned whole) into an `Origin` that
> seeds version 0 and is recorded durably before the chain, refusing `ConsistentBaseUnavailable` for a
> snapshot still served from the host directory (a host edit after the create changes no version, tested
> by use). A read attachment of a green pins the head; `advance` re-pins and names exactly the paths the
> span changed (`changed_between`, read off the per-dimension histories); `read` serves the head, a
> version or the pin. A submit seals what was declared before it — an accepted submit moves the work to
> the new version with its journal consumed (the resubmit self-conflict fixed) — and every input to the
> verdict is retained by the `GreenAdvanced` record, not the work. In a fleet the merge record
> (`MergeRecordValue`: version, increment identity, base, inputs identity, head identity, evidence) is
> issued only once its inputs — the chain's own bytes as one archive — are placed at `f + 1` through the
> §4.10 content exchange, then shipped on its own stream in order per holder and committed at `f + 1`;
> every holder recomputes the version into its replica from the placed inputs and compares
> `head_identity` with the record — a mismatch is counted, printed, and the green refused on that holder
> for good — before it accepts. At `f = 0` the append is the placement (R8). The CLI (`green --base`,
> `--require-evidence`, `submit --evidence`, `advance`, `read`) and MCP (`slates.merge.advance`,
> `slates.fs.read`, base and evidence arguments) drive the flow by use. Owed: the mounted work and green
> (a work is not a VFS volume; the VFS journal as the one declaration path, the bridge's `EROFS`), the
> extent-backed green chain, cooperative slicing of the origin seed, pipelined and hedged merge records,
> green takeover and a late holder's catch-up.
