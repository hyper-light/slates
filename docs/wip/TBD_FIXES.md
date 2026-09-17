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

- [ ] **Terminal membership publication to other shards:** `fold_peer_state` ignores a refused
  `run_on` and relies on another active probe to repeat the fold. A terminal death can instead
  idle the probe forever. Establish a bounded, acknowledged/retried publication mechanism and
  test full destination admission followed by recovery. The new owner lookup reads the control
  shard, but that does not repair every other shard's stale membership copy.
- [ ] **Identity announcements across overlapping sessions:** review whether a delayed old-boot
  announcement can replace a newer learned identity. `classify_announced` treats a different
  random nonce as a restart; it cannot derive age from numeric ordering. Current shared-gossip
  filtering rejects a relayed alive report for a superseded identity, but that alone is not
  evidence about authenticated old sessions. Reproduce before assigning a new defect.
- [ ] Remove inaccurate comments claiming larger random boot nonces establish freshness,
  unrostered probes are answered, or discarding third-member gossip is necessary to prevent
  stale-alive flapping. Current authentication and incarnation ordering are the actual rules.

## 4. Audit findings still lacking recorded closure

The [September 14 audit](../bugs/2026-09-14_AUDIT.md) leaves these **eight** findings open
after its later recovery follow-up. Recheck each against current code and preserve its
required public-behavior regression; narrower subsequent repairs do not close the contract.

| Finding | Remaining correction | Required evidence |
|---|---|---|
| **AUD-01 — mount authorization** | Bind NFS access to an authorized consumer/attachment and volume rights; supplied AUTH_SYS UID and loopback reachability are not consumer authority. Filter enumeration. | Unbound caller, wrong consumer and forged owner UID cannot enumerate/read/write; authorized mount works. |
| **AUD-02 — FUSE coherence** | Deliver invalidations without waiting for another kernel request; retain the notification cursor when collection/delivery fails. | Warm the kernel cache, mutate elsewhere, then read/stat without an unrelated cache miss; recover an injected notification failure. |
| **AUD-06 — failed transaction publication** | Prevent an unpublished successful completion/effect from being returned on retry. Distinguish failure before durable append from maintenance failure after it. | Inject publication failure, retry the same id, restart; effect and completion survive together or remain explicitly uncommitted. |
| **AUD-08 — latest-state authority** | Gate current-state reads/service on confirmed owner authority, including pause/expiry and takeover. Shared host clocks and ReadIndex alone do not provide the integrated lease gate. | Isolated old owner refuses uncertain latest-state reads after successor takeover/write; explicitly pinned immutable reads keep their separate contract. |
| **AUD-11 — merge commit acknowledgement** | Do not publish committed submit acceptance before the required input closure and ledger-record quorum. | Withhold inputs and record acknowledgements separately; acceptance waits/refuses appropriately, and owner-loss retry returns the same committed result. |
| **AUD-14 — green ledger takeover** | Recover and serve the complete committed green prefix, inputs, catalog and original increment results; transfer safely to changed candidate sets. Generic single-head takeover is insufficient. | After owner loss, public clients read old/current versions, retry an accepted increment and submit the next one. |
| **AUD-15 — indirect SWIM probes** | Wire the direct → relay → suspicion stage into the daemon with bounded ownership and derived budgets. Shared gossip does not implement relayed probing. | A→B broken, A→C→B working: real indirect acknowledgement preserves B; losing both paths retires B. Require a relay-use counter. |
| **AUD-16 — merge memory admission** | Charge and bound rejected-result retention and full content retained by accepted edits, not merely encoded increments. | Conflict floods reach a typed derived bound with balanced accounting; small edits to large files have measured and charged resident growth. |

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
