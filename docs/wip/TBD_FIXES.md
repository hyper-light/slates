# Remaining fixes and verification

Updated: **2026-09-17**. Checkpoint after `c885d50`, including the current uncommitted
owner-location, shared SWIM gossip and editor-workload changes. This is the remaining-work
list for the audit/CI repair session; [GAPS.md](GAPS.md) remains the authoritative contract
ledger. Historical audit findings below need closure evidence against current source, not
blind reimplementation of their original baseline. No new test, install or deployment is
authorized merely by appearing here.

## 1. Immediate failure: fresh voter after a second loss — FIXED (2026-09-17)

- [x] Diagnosed and fixed
  `a_whole_ram_replacement_joins_as_a_fresh_voter_and_commits_after_another_loss`.
  **Root cause** (from the committed council log, captured by `council_debug`): after the second
  loss, the new leader retires the **live fresh voter** (`TakeOver{fresh}` committed at the
  re-election term) instead of the actually-dead victim, because SWIM transiently held the fresh
  voter `Dead` while its just-formed sessions churned through the re-election. The applied config
  becomes `{victim(dead), other}` — a voter set with a dead member and no live majority — so no
  leader can be elected and nothing more commits. The council took an **irreversible** consensus
  retirement on a **revocable** failure belief. Record:
  `docs/bugs/2026-09-17-council-retires-a-suspected-voter.md`.
- [x] Captured both survivors' committed/applied configurations, voter sets, terms, leadership,
  SWIM alive view and record links across the second loss (`council_snapshot` and `council_debug`
  in `crates/server/tests/fleet.rs`, `RegionalCouncil::debug_state`). The committed log named the
  exact bad command.
- [x] Fix, two parts: (a) `reconcile_alive(alive, dead)` retires only members whose death is
  **confirmed** (`Dead`), never merely `Suspect`; deterministic regression
  `config_group::tests::a_suspected_voter_is_not_retired_until_its_death_is_confirmed`. (b) a
  **death-confirmation window** — a member must be observed `Dead` continuously for the council's
  election-timeout base (the longest transient disruption a leader loss causes) before the
  irreversible retirement; the per-member watch (`ShardState::council_death_watch`) is advanced on
  every node every period so it is monotonic for a truly-dead member and survives a leadership flap.
- [x] Verified joining **and** a real post-loss commit, not a single sample: bounded Linux
  container `--memory=2g --cpus=4`, baseline 1 failure in 8; suspicion fix alone 2–3 in 30 (the
  re-election drove the voter to full `Dead`); a leader-only window fixed the second loss but
  stranded the first; the final all-node-watch + election-timeout window: **30/30**, plus a further
  confirmation run. macOS green throughout. The timeout was not raised.

Command (bounded Linux container, quantum image): `docker run --rm --network=none --memory=2g
--cpus=4 --cap-drop=ALL slates-quantum-proof:<tag> <test> --exact --test-threads=1`.

## 2. Finish the current uncommitted repairs

- [ ] **Shared SWIM gossip:** finish validation and documentation of the repair in
  `cluster/{gossip,detector,fleet}.rs` and `server/fleet.rs`. A detector's third-member death
  previously never reached the council's shared failure view; deaths also failed to travel
  over other live sessions. Keep timers scoped to the actual probed peer, use bounded shared
  dissemination, and preserve authentication, incarnation ordering and self-refutation.
  [Report](../bugs/2026-09-17-sparse-mesh-drops-membership-gossip.md).
  The wire regression failed before the fix in 0.81 s and now passes. Current results:
  **147 cluster units, 91 server units**, strict cluster/server/xtask Clippy; the five-node
  takeover/location/outage-retry history passes in **11.36 s**, and false-retirement rejoin
  passes in **5.23 s**. The failure in §1 prevents claiming the whole restart path is green.
- [ ] **Remote owner lookup and forwarded completion ownership:** finish final validation of
  `server/owner_location.rs` and `verbs.rs`. Use actual held-object routes after takeover;
  never rank unrelated live members as substitute owners. Keep one route per client, resolve
  it on the control shard, preserve the original request id, and leave completion ownership
  at the executing owner. A temporary origin-side routing refusal must remain retryable.
  [Report](../bugs/2026-09-17-remote-lookup-guesses-outside-the-copyset.md).
  Red: foreign lookup failed in 42.83 s despite a working successor; the old retry test's
  forwarding counter also failed. The latest five-node history covers both corrected paths.
- [ ] **Editor workload:** validate the already-applied `backupskip=` change with real Vim saves
  in ordinary and temporary paths; compare edited **and backup** bytes. The regression is
  `conformance::workloads::tests::an_editor_save_preserves_its_backup_even_inside_tmpdir`.
  [Report](../bugs/2026-09-17-editor-backup-depends-on-scratch-path.md).
  CI supplies the original red result; a local real-save green is still owed. It needs an
  existing RAM directory via `SLATES_TEST_RAMDIR`, or explicit authorization for the proposed
  temporary 256 MiB RAM volume. Available cached Rust Linux images have no Vim. A skip is
  not proof; no installation or RAM-volume creation has been authorized.
- [ ] Recheck affected live histories after the final source change: fresh-identity restart,
  whole-RAM replacement, false-retirement rejoin, starved-live-peer retention, session redial,
  cross-region write retry and the scheduler-pressure proof. Run serially with bounded supervisors.
- [ ] Run final formatting, strict lint and `cargo xtask check`; update the three bug reports,
  design status/amendment and GAPS with exact results. The sparse-gossip and lookup reports
  still contain pre-green validation placeholders. Update the old takeover report's statement
  that the separate all-live-members lookup guess is still present.
- [ ] Review and commit the completed working tree under the repository's commit rules.
  These edits are **not committed or pushed** at this checkpoint.

## 3. Additional concrete follow-ups found during this repair

- [x] **Terminal membership publication to other shards** — assessed, no correctness impact
  (2026-09-17). `fold_peer_state` ignores a refused cross-shard `run_on`, so a terminal death's
  fold can be dropped. But a review of every `.fleet.membership()` read shows **no off-control-shard
  path consults the raw SWIM membership**: `verbs.rs` has zero membership reads; an owner shard
  routes and admits from the committed **configuration** (`fleet.configuration()`, `object_owner`),
  which `fan_configs_to_shards` re-fans every period (self-healing). So a dropped membership fold
  leaves a copy nothing reads. A heavyweight retry mechanism is therefore unwarranted; the
  inaccurate comments claiming `fold_peer_state` shares the config fan's self-healing discipline are
  corrected, and the publish is documented best-effort (D-7 uniformity, not load-bearing). If a
  future owner-shard path comes to read raw membership, add it to the periodic config fan.
- [ ] **Identity announcements across overlapping sessions** — latent defect **confirmed**, complete
  fix entangled with task #22 (2026-09-17). `classify_announced(known={A→N2}, boot_nonce=N1)` with a
  delayed *old-nonce* announcement (N1, already superseded by N2) returns `Restarted{old: id2}` — it
  would retire the newer id and revert to the older. Because boot nonces are random it cannot order
  N1 before N2 from the nonces alone. A robust fix needs a monotonic generation (the anchor's
  `SUP_GENERATION`, task #22 Piece 1 in the ephemeral-id plan) or a bounded per-anchor superseded-nonce
  history; a single prior-nonce guard fails across multiple restarts. Not fixed here (needs the
  generation work + a reproduction); no test currently hits the delayed-old-packet timing.
- [x] Inaccurate comments corrected (2026-09-17): the serve-probe comment claimed a "higher"
  boot_nonce marks a restart (a *different* nonce does; nonces cannot order) and that an unrostered
  prober "is answered" (the gossip rewrite gives it no ack); the third claim (third-member gossip
  discarded to prevent flap) was already corrected by the shared-gossip rewrite. The `fold_peer_state`
  and `fan_configs_to_shards` comments no longer claim a shared self-healing discipline.

## 4. Audit findings still lacking recorded closure

The [September 14 audit](../bugs/2026-09-14_AUDIT.md) leaves these **eight** findings open
after its later recovery follow-up. Recheck each against current code and preserve its
required public-behavior regression; narrower subsequent repairs do not close the contract.

| Finding | Remaining correction | Required evidence |
|---|---|---|
| **AUD-01 — mount authorization** — **DONE 2026-09-19** | Every volume is served over NFS only through a **mount capability**: the attachment id and a random 16-byte token minted by the access-list-checked `attach` (and the green's `attach_green`), stored on the `AttachmentRecord` with the granted rights (bounded by the intent), returned in `Attached.token`, presented in the `MNT` path `/<name>@<attachment_hex>.<token_hex>` (or `/@<capability>` for the scoped host root), stamped into the root handle and every derived handle (file handle v2), and validated on the owner shard on every request — no per-connection state, the uid never authority; a bare `/` lists nothing, a name without a capability mounts nothing. The attachment is the mount's (`Consumer::Bridge`): it outlives the attaching process and a daemon restart (recovery keeps it; the counter is seeded past recovered records) and ends with the kernel's `UMNT`, a `detach`, or the destroy. `slates mount ID PATH [--read-only]` attaches as a host mount (`Client::attach_mount`) and mounts under the capability. Modules `crates/server/src/nfs.rs`, `verbs.rs`, `crates/bridge-nfs/src/handle.rs`, `crates/cli/src/{mount,verbs}.rs`; records `docs/bugs/2026-09-19-nfs-bypasses-consumer-and-volume-authorization.md`, `docs/bugs/2026-09-19-mount-capability-attachment-dies-with-its-client-and-the-daemon.md`. | `a_consumer_private_volume_is_served_over_nfs_only_through_its_attachment_capability` (real NFS socket): an unbound client, a forged uid and a wrong token are refused at `MNT`, an unbound `ls /` is empty, the capability mount serves it, a file reads back on a connection that presented no token, the scoped root lists exactly it, and a `UMNT` of the mount path ends the attachment (the handle refused, the lease released) while a `UMNT` of the scoped root does not; the recovery crash sweep (15/15 crash points, a pre-crash handle resolving after each); the handle and parser hostile-input tests; the live CLI flow (`SLATES_TEST_CLI=1`: one attachment while mounted, still served after the command's client was reaped, zero after `umount`). |
| **AUD-02 — FUSE coherence** — **DONE 2026-09-19** | Invalidations are delivered at **every wake**, not only before a kernel request: the serve loop `poll`s the device and a `ChangeSignal` (an `eventfd` other mutation sources notify), and a `Change` wake runs a delivery round with no request. The round is a pure discipline (`crates/bridge-fuse/src/coherence.rs`): the cursor advances only past what was gathered **and** written; a seam refusal keeps it (counted) and the next wake delivers the missed changes; the transport's own request moves the cursor past its own records only when the round before it was delivered whole. `serve_step`/`wait` let an owner interleave its own changes (the daemon's shard serving verbs); `serve_blocking` is the loop over them. The real-kernel run found and fixed two more: attribute replies carried no file-type bits (every mount `EIO` at its first operation), and writeback cache made the kernel the size authority (an accepted invalidation ignored) — now refused at `FUSE_INIT`. Records `docs/bugs/2026-09-19-fuse-invalidations-wait-for-a-kernel-request-and-a-refused-gather-loses-changes.md`, `…-fuse-attribute-replies-carry-no-file-type-bits.md`, `…-writeback-cache-made-the-kernel-the-size-authority.md`. | `tests/coherence.rs` (every host, real volume bridge, recording sink): another attachment's change delivered, the transport's own not; a refused gather keeps the cursor across the request served under it and the next round delivers the missed change; a sink refusal re-delivers. `tests/coherence_mount.rs` (Linux, real `fusermount3` mount): the kernel cache warmed and proven warm (a second `stat` sends no `GETATTR`), another attachment's truncate seen through `stat` with no request having woken the loop (exactly one `GETATTR` forced), then an injected gather refusal under a request — the kernel still stale, the next wake delivering the change, the refusal counted once. |
| **AUD-06 — failed transaction publication** — **DONE 2026-09-18** | `Db::commit` keeps the two classes apart: a failure before anything is durable **rolls the transaction back** (the partition re-derived from the segment's durable state — effects and completion gone together; typed `DbError::Unpublished`, wire `Refusal::Unpublished`; the verb's in-memory objects for the vanished record released), a maintenance snapshot failing after a durable append is **deferred and counted**, the commit stands. Record: `docs/bugs/2026-09-18-unpublished-transaction-served-from-memory.md`. | db: `a_publication_refused_before_the_append_rolls_the_transaction_back_and_a_retry_re_executes` (completion `New` after the rollback, sequence unmoved, recovery agrees, the retried transaction durable) and `a_maintenance_snapshot_that_fails_after_the_append_is_deferred_and_the_commit_stands`; by use over one segment across two daemons: `an_unpublished_verb_is_refused_typed_a_retry_re_executes_and_a_restart_agrees` (refused `Unpublished`, rollbacks = 1, the same-id retry re-executes, the restart lists the volume once and answers the retried id from its completion record). |
| **AUD-08 — latest-state authority** — **DONE 2026-09-19** | An owner serves an object's latest state (a head read, the head version, a status, the mount's live tree) only under a confirmed **owner lease**: `f` of the object's other candidate holders acknowledged this node's probes within the horizon-derived bound (measured on the suspend-inclusive host clock from the probe's send time, less twice RFC 5905's clock tolerance) under the installed configuration version, and it is not superseded by a newer version a peer announced. The gate (`verbs::dispatch`, `crate::nfs` → `NFS3ERR_JUKEBOX`) refuses `LeaseUnconfirmed`; a holder's promotion of a departed owner's object waits for that owner's lease to have lapsed (quorum intersection). Pinned immutable reads (a green's version, an attachment) keep their separate contract. A bounded startup allowance (the horizon after a config install) spares a reachable just-formed owner a false refusal. Module `crates/server/src/lease.rs`; record `docs/bugs/2026-09-19-latest-state-served-without-a-confirmed-owner-lease.md`. | `an_isolated_owner_refuses_latest_state_reads_while_the_successor_advances_the_green`: green at version 3, A isolated both directions and its control shard paused across two lease bounds; A's lease lapses by the clock, the successor takes over and advances to version 4, and on A's original connection the head version and a head read refuse `LeaseUnconfirmed` while the pinned version-1 read still serves. |
| **AUD-11 — merge commit acknowledgement** — **DONE 2026-09-18** (the owner-loss retry lands with AUD-14) | At `f > 0` a submit's acceptance **waits** for its version's merge record to commit at the quorum: the verb commits its effects, records no completion and sends no reply; `resolve_accepted` records the completion and delivers the reply when the record places; a retry while waiting joins the wait; a cross-node forward polls the completion within the liveness budget, else refused retryable. `f = 0` unchanged (the append is the commit). Record: `docs/bugs/2026-09-18-submit-acceptance-before-fleet-commit.md`. | `a_submit_is_answered_only_once_its_record_commits_at_the_quorum`: inputs withheld → the bounded wait times out, one acceptance waiting, version unplaced; acknowledgements withheld (`MergeFault::refuse_records`) → unplaced and waiting across a hold window, the holder counting; both lifted → placed, resolved (counted), the holder at version 1, the same-request retry answered `Submitted { version: Some(1) }` from the record. |
| **AUD-14 — green ledger takeover** — **DONE 2026-09-18** (prefix transfer to a lagging successor stays with GAP-A9-7) | A taken-over green is **materialized** on the successor from its own accepted merge records and held inputs: the catalog record (the merge record value now carries the green's name, evidence policy and owner), the origin and every increment re-recorded durably, the engine rebuilt by the boot derivation and its head identity verified against the adopted record (mismatch fatal-and-loud), the head placed. Record: `docs/bugs/2026-09-18-green-takeover-left-no-servable-chain.md`. | `a_taken_over_green_serves_every_version_and_accepts_new_work_on_the_successor`: three versions on the owner, both holders caught up, owner dies → the successor's chain identities equal the owner's; through the client on the successor `versions` = 3, `f` reads version 1 and the head; a new work submits version 4 (committed with the remaining holder, recomputed there) and its retry meets the record. |
| **AUD-15 — indirect SWIM probes** — **DONE 2026-09-18** | Wired: a requester's timed-out direct probe posts ping-requests for up to `k` (derived) relays nearest the target; the relay's target-probe task answers and the answer rides back as the new `IndirectAck` wire message; the requester credits it before the suspicion tick. Bounded queues keyed by authenticated members; probe tasks woken on traffic. Record: `docs/bugs/2026-09-18-swim-indirect-probes-not-wired.md`. | `an_indirect_probe_through_a_relay_keeps_a_peer_the_direct_path_lost_and_losing_both_paths_retires_it`: A→B lost at B's serve side, A→C→B intact — B kept across a 100-period hold with `fleet.probe.indirect.acked ≥ 1` on A and `.relayed ≥ 1` on C; both paths lost → retired. Negative control (relays disabled): fails with acked=0. 14.22 s with the fix. |
| **AUD-16 — merge memory admission** — **DONE 2026-09-18** | The rejected-result cache is bounded in bytes (the derived green-chain cap), oldest evicted first, evictions counted; retained history is accounted (running total + recount oracle), **folded oldest-first only as far as the retention budget needs** and never past the oldest version a live reader names (a work's base, a pinned attachment; `advance` below the fold floor refused `UnknownBase`), and **charged** to the shard's budget as retention — secured before the verdict, refused typed `BudgetExceeded`, settled after. Record: `docs/bugs/2026-09-18-merge-rejected-results-and-retained-copies-unbounded.md`. | Engine: a 40-increment conflict flood under a 256-byte budget never exceeds it (retained + evicted = 40, evictions ≥ 1, head fixed; an evicted retry re-judges to the same verdict); eight 4-byte edits to a 64 KiB file retain exactly 8 copies (measured), fold to 0 at the head. By use: six edits to a 16 KiB file retain 96 KiB charged with `charged == history + rejected`, the lagging work's conflict cached and charged, its destroy folds and credits. |

## 5. Remaining integration and correctness contracts

These remain open in the later GAPS entries and subsystem records. Historical inventory rows
can lag subsequent implementations; use their acceptance criteria and current source together.

- [ ] **Configuration/ledger transfer (GAP-A9-7/-11, AC-8.18):** complete compacted Raft prefix
  transfer and full-message quotas; prove historical adoption values, quorum intersection,
  stale-writer fencing and safe application through message faults and configuration changes.
  Remembered copysets fix one takeover defect, not arbitrary state transfer. A-9 model
  refinement/revalidation remains separately owed; no checker installation/run is authorized.
- [ ] **Root availability and rolling changes:** establish the intended root-voter redundancy
  and committed-admission readiness barrier. A region's f does not establish root-quorum
  survival. Demonstrate safe rolling replacement without automatic rebootstrap.
- [ ] **Mounted snapshot barriers (GAP-A9-4):** one daemon-owned per-volume attachment registry,
  ready-device binding, writeback retrieval/flush and complete coverage before snapshot success.
- [ ] **Mounted Work/Green (GAP-A9-14):** VFS-backed work journals and green volumes, read-only
  enforcement, version-pinned attachments and the extent-backed retained chain; prove the mounted
  merge workflow, not just the service-level protocol.
- [ ] **Overlay/clone recovery (GAP-A9-2/-6):** independent overlay-clone host ownership, client
  open-handle handoff, complete immutable capture/remote clone coverage and changed-source refusal.
  Retained overlay witnesses/images are already implemented; do not list them as missing again.
- [ ] **Placement and repair (GAP-A9-8):** close verified reference-graph placement, real holder
  capacity, repair/healing and byte-complete cross-region/mirror service with time-based lag evidence.
- [ ] **Memory/QoS (GAP-A9-1/-11):** pressure-driven admission stop, Windows job-object bounds,
  guest/open-reference charges and bounded end-to-end large/unknown-length transfers, with
  cancellation and resource release across shard/device/transport limits.
- [ ] **Telemetry (GAP-A9-12):** cross-node trace propagation and fleet/archive emitters with
  typed absence, freshness and loss. Local observation receipts are already implemented.
- [ ] **Digests (GAP-A9-13):** cooperatively sliced hashing, sealed-content digests and SDK exposure.
- [ ] **Public/platform surfaces (GAP-A9-5/-9/-10/-15):** finish actual path readiness, attachment
  guarantees and schema/capability parity across the supported native/guest/container transports;
  verify each supported platform with the contract matrix rather than extrapolating from NFS.

## 6. CI and deployment evidence still owed

- [ ] Rerun native Ubuntu conformance after the effective-identity and workload fixes. The original
  job reported **6202 unexpected pjdfstest failures / 8798 cases**; correcting sudo invocation
  does not prove all failures were caused by it. Diagnose residual failures without widening
  expected-failure lists to hide them.
- [ ] Rerun hermeticity startup/tracing with the shared boot clock. The original lane repeatedly
  missed the first heartbeat and timed out after 60 s. The cross-process clock regression is
  green, but the corrected full tracer run is still owed. Anchor format 3 intentionally rejects
  incompatible format-2 retained state; record the upgrade constraint.
- [ ] Rerun affected CI on the final committed source. Local Linux compilation and targeted
  histories supplement, but do not replace, platform-specific mounted/driver gates.
- [ ] Rerun KIND whole-pod replacement on the final changes, including fresh IP, fresh voter,
  rejoin, quorum operation and mounted read-back inside a pod. The September 17 CI rejoin
  result (2.2 s; takeover 1.8 s) was on `a4fe23a`, before these local fixes.
- [ ] Complete the separately owed WAN/netem and safe rolling-upgrade histories under the
  existing authorization rules. Preserve laptop/bare-metal/VM/Kubernetes protocol equivalence.

## Already implemented — do not restart these tasks

Warm Raft retention, explicit reviewed-copy quorum-loss recovery, scoped unlisted enrollment
and retained overlay images have September 15 implementation records. Original KIND new-IP
session formation has passed historical gates. This session also committed:

| Commit | Correction already made |
|---|---|
| `169c355` | CLI leading-global parsing and explicit-bootstrap process fixtures. |
| `96bb82f` | Reserved pending-handshake capacity and authenticated per-peer session fairness. |
| `a26a5a6` | Live scheduler-quantum pressure proof: 899–964 ms measured delays. It proves dilation use, not a measured reduction in false retirements. |
| `490f0f8` | Linux fixture liveness-handle retention; holder-mismatch and inputs-placed timeouts. |
| `7599605` | Effective caller identity versus sudo availability in conformance invocation. |
| `a5eb419` | Shared OS clock domain across anchor/daemon generations and retained deadlines. |
| `c885d50` | Accepted-copyset retention, correct takeover candidates and original recovery quorum. |

Session artifacts are under
`/private/var/folders/1s/ldpdh04d7d7219qts5t19d7h0000gn/T/slates-followups-o5314nq6/`.
They are local diagnostic logs, not committed test evidence or portable tooling. Exact test names,
red/green results and reproduction contracts belong in the linked reports before closure.
