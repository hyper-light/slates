# The gap ledger (authoritative, kept current in the same change as any acceptance or tripwire)

> **A forward waits for the owner's session (2026-09-25, §4.8 Lookup).** A forward refused the client
> `HomedElsewhere` at once whenever a coordinator dispatch or a discovery page had the owner's session out;
> the copyset history failed about once in fourteen runs that way (CI run 36191789379). It now waits inside
> its deadline and counts the cause; a deterministic test holds the session out and fails on the old
> forward. Owed: the location round skips a peer whose session is out. The formation failures of the same
> test (CI runs 36191789379, 36199796152; two local full-suite runs) were the harness, not the product: the
> memory-bound fleet test ran unserialized and took the test's just-released serve ports; it serializes
> now (the full suite 50/50 on Linux). Owed: serve ports below the ephemeral range. Records:
> `docs/bugs/2026-09-25-a-forward-refused-while-the-owners-session-was-out.md`,
> `docs/bugs/2026-09-25-an-unserialized-fleet-test-took-another-tests-ports.md`.

> **The wake estimate learns after boot; a long poll is attributed (2026-09-25, A-31).** The boot mean
> was frozen for the process's life although it is ±20–30 % on a VM, and the long-step count read wall
> time, so every preemption of a correct poll counted as the task's bug. Now each shard and each client
> refine an online mean (seeded with the boot mean, weighted over 2^shift wakes from the probe's own
> spread) from the wakes they pay, counting only a sleeper woken: Linux shards confirm by the thread's
> voluntary switches, clients (Linux, macOS) by the daemon's wake finding a sleeper; macOS and Windows
> shards and Windows clients are bounded by the park's announcement, unconfirmed. The quantum, the
> spins, the idle windows and the destroy and archive slices read it live; the client region goes to
> layout version 2. A long poll is the task's (past the quantum on the CPU, or blocked in a call), the
> host's (runnable off the CPU), or unattributed (macOS off the CPU; Windows). Proven by use on both
> hosts (30/30 repeats each); suites green on macOS and Linux; fleet 48/48. Found on the way and owed:
> `ipc_bench`'s parked row times a park-setup race, not a wake (2–18 of 2,000 trips slept); a step costs
> O(ready set), which makes `observe.rs`'s full-arena fill quadratic (95–97 s, before and after); the
> archive walk's unit exceeds the quantum. Record:
> `docs/bugs/2026-09-25-wake-estimate-frozen-at-boot-and-preemptions-counted-as-long-steps.md`.

> **The wake probe measures one event and reports its mean (2026-09-25).** The profile's wake timed
> every park/unpark round trip, mixing an on-CPU handoff (about 0.45 µs) with a real wake (about 10 µs),
> so runs split between the two (median 416 ns or 10,041 ns on the same container cores), and it
> converged the median while consumers read a p99 that was often the maximum of 64 samples. Now: the
> waiter confirmed asleep, the pair placed as production is, the mean converged (the spin-then-park
> threshold is the expected cost of parking; a spinning waiter never saw the tail, a parked one did),
> five rounds compared by their medians. Quiet-host containers: median 8.9–10.8 µs over ten runs, mean
> 15.1–17.6 µs on four CPUs; macOS mean 2.03–3.27 µs over nine. Spin window, step quantum and timer
> tick read the mean; the inbound ring the p99 at its overflow target. The boot mean being quick on a VM
> and the long-step count reading wall time were closed by A-31 (the entry above).
> Record: `docs/bugs/2026-09-22-wake-probe-mixes-two-events-and-reports-an-unconverged-tail.md`.

> **Client ring sized by Little's law (2026-09-22).** A daemon's client seats followed one boot
> measurement of the wake tail: `slots_per_ring` was `wake.p99 / syscall.median`, and the bulk area
> scales with it, so pods of one image seated 1 to 1285 clients. A one-seat KIND pod refused the lane's
> `bootstrap` while its readiness probe held the seat; CI's macOS runner seated two and refused the
> restart test's third. The ring is now §4.7's Little's law, `requests_in_flight_per_shard` rounded to
> a power of two; the regression fails before with CI's exact message and passes after; workspace,
> server, fleet (48/48) and CLI-flow suites pass; every pod of a two-CPU fresh-cluster KIND run seats
> 330. Owed: the wake probe's mixed events and unconverged p99 still feed the step budget, the spin
> window and the runtime's inbound ring; the pressure hold follows host-wide memory.
> Record: `docs/bugs/2026-09-22-client-ring-sized-by-the-wake-tail-not-littles-law.md`.

> **KIND takeover: council seats (2026-09-22).** CI run 35615113353's KIND lane retired the killed
> owner but never took its volume over. The council's voters were the lowest member ids up to
> `2f + 1`: the owner's replacement, admitted beside its unretired predecessor, took the live
> leader's seat while the dead predecessor kept its own, and the replacement, which cannot observe
> its own predecessor's death, then led a council that never retired it. A sitting voter now keeps
> its seat while it is a member, and a freed seat goes only to a member the leader holds alive; the
> id is only the tiebreak. Reproduced one failure in eight on a CPU-pinned local cluster; the fixed
> image passed 6 of 6; two deterministic council regressions; 152 cluster unit tests and the fleet
> suite pass. Owed: the replacement's blind spot after two overlapping faults, and root-group
> representatives chosen by id order. KIND initial formation (run 35615970514) stays open and was
> not reproduced (14 local formations, each within 0.3 s).
> Record: `docs/bugs/2026-09-22-council-seats-follow-id-order-not-liveness.md`.

> **CI boundary audit (2026-09-22).** The shared NFS attachment cached MOUNT's uid and
> stamped later callers' creations with it. The registry now retains the catalog principal;
> Unix ownership rides each request separately. Nine daemon NFS tests, 21 shared-bridge
> tests and 42 NFS procedure tests pass, including reserved RMDIR components. Five client
> lifecycle tests now explicitly distinguish acknowledged refusal from replay after a lost
> reply; the previous fixture retried a released completion on small rings. Workspace Helm
> uses the KIND lane's pinned renderer; all four chart gates pass. The workload comparator's
> blanket `._*` exclusion is removed. Mounted AppleDouble behavior, tracer startup/lifetime,
> async acknowledgments, suite subprocess errors and KIND formation remain open in
> `docs/bugs/2026-09-22-ci-failure-pattern-audit.md` and TBD_FIXES. No all-lanes closure.
> The companion macOS NFS boundary report classifies the 117 additional pjdfstest
> failures; it adds no expected-failure entries and leaves mounted proof open.
> The fs_usage parser's malformed wait suffix is also guarded after a red regression
> (`2026-09-22-fs-usage-wait-suffix-panics.md`); tracer readiness remains a separate issue.
> The repair batch now passes the Linux workspace (1,564 functions; opt-in gates still
> need their dedicated runs), all 49 fleet histories, structural checks, and the full
> mounted NFS adapter sequence: nine identical workloads, no unexpected pjdfstest failure,
> and no unresolved or outside traced write. Linux Helm 4.3.0 now separately passes its
> four chart gates in 0.05 s after checksum verification. The dedicated native FUSE
> regressions also pass (0.39 s and 0.01 s), followed by the real CLI gate (10 functions;
> macOS-only branches skip), 50 conformance cases and six tracer lifecycle cases.
> macOS tracing remains a distinct verification obligation.
> The corrected macOS tracer captures 188 rows / 47,000 bytes, with empty stderr and no
> surviving task process. ENOSPC was the fixture's eight-inode limit: client metadata
> filled three extra slots. A measured page-based quota admits the full native workload
> and snapshot; the oracle now compares every mounted entry with the landed tree, including
> those metadata files, while retaining the known workload byte/kind checks. Linux passes
> the complete NFS/strace lifecycle with 220 writes, six paths matched and no unresolved or
> outside write. macOS still needs sound descriptor attribution; visible sidecars remain
> failures in the independent workload-equivalence suite. Records: the dated fs_usage and
> client-metadata fixture reports. No all-lanes closure.
> Task-scoped DTrace is now authorized; Terminal sudo authentication is pending. The
> replacement attribution must account for descriptor reuse: fs_usage suppresses dup/dup2
> rows in the selected mode, so later descriptor snapshots cannot prove earlier write targets.

> **CI fixture correction (2026-09-21).** Run 35604717581 exposed channel waiting-state
> allocation inside both allocator-counter fixtures and a status-paging fixture that shrank
> the report allowance to 1024 bytes on an eight-slot host. Stack-owned synchronization
> preserves the zero-allocation assertion; small pages now preserve CI's normal report credit.
> The memory binary passes 100 Linux repetitions, the sibling runtime failure reproduces
> independently and is corrected, and native status paging passes in 0.75 s. Full workspace
> validation passes 1,561 Linux cases under io_uring plus `xtask check`; the separate mounted
> conformance failures and subsequent corrections remain in progress in TBD_FIXES.
> Record: `docs/bugs/2026-09-21-ci-fixtures-count-local-work-and-shrink-status-credit.md`.

> **Instruction benchmark repair (2026-09-21).** Result checks reject all four negative
> controls; all 14 valid benchmarks pass. Explicit, non-inlined collection requests exclude
> teardown on Linux ARM64: eightfold CRC verification leaves 2,653 instructions unchanged;
> doubling header encoding increases 34 instructions to 58. Strict instrumented Clippy and
> xtask checks pass against isolated `2179cc2` plus this repair. Container-only libclang is
> authorized and installed; ordinary tests and Miri do not enable its benchmark feature.
> D-20's saved comparison baseline and enforced regression policy remain owed.
> See `docs/bugs/2026-09-20-callgrind-reports-refused-work-as-success.md` and TBD_FIXES.

> **Snapshot measurement correction (2026-09-20).** A same-tree experiment separated journal
> turnover (53 ns) from steady snapshot cost (28 ns); steady small/large trees measured 29/28 ns.
> The AC-1.3 benchmark now prepares equal journal state, retains its allowance and fails on
> refused operations. The separate destroy-slice overrun and the eager preparation walk outside
> its old timer remain open. Evidence: the dated snapshot benchmark report; no performance
> threshold was widened. The full local CI obligation remains open in TBD_FIXES.

> **CI repair in validation (2026-09-20).** A real one-core fleet status exceeded its 4-KiB
> reply slot. Bounded, client-owned status paging now preserves the complete report with
> charged retention and checked cursors (§4.14); native regressions pass, including the
> original fleet failure (1.58 s). A separate IPC fixture replaced an OS scheduling
> assumption with observable queued/armed-wait behavior (192/192 Linux executions).
> Records: `docs/bugs/2026-09-20-daemon-status-exceeds-a-reply-slot.md` and
> `docs/bugs/2026-09-20-ipc-ring-test-assumes-thread-scheduling.md`. Full local CI and the
> first root pjdfstest expectation review are tracked below; native Windows and complete
> native FUSE conformance are not yet proven by these results.

> **Additional local gates (2026-09-20).** The unchanged Linux NFS-adapter conformance run
> passes its reviewed gate: 6,970 pjdfstest passes, 1,800 expected failures, 28 TODO,
> no unexpected/stale exclusions; fsx, fsstress, nine workloads and hermeticity pass.
> The records remain LIMITED to that adapter. The million-operation VFS model passes.
> The RAM differential oracle dropped its root when selecting absolute VFS paths;
> correcting that translation passes the deterministic alias regression and 2,000 histories
> (0.29 s). The real-host digest test also assumed an earlier hash was inside the racy
> window; its final-read assertion now measures reuse relative to the immediately prior
> counters and checks the replacement bytes. Records: the 2026-09-20 differential-oracle,
> host-digest-test and pjdfstest-device-fixture bug documents.

> **Real landing gate (2026-09-20).** The Linux crash/resume run exposed the exchange's
> own ctime update being mistaken for an outsider edit. The engine now verifies the actual
> displaced name and hashes its witnessed content when ctime changed. The simulator models
> that update; 18 landing oracle tests and all five real Linux landing tests pass (0.06 s
> for the latter). Symlinks reported as ENOTDIR now receive the intended EscapesTarget
> refusal. The next CLI gate exposed a BSD-only mktemp template; its repair is in validation.

> **OCI source integration (2026-09-20).** The macOS CLI gate reaches a real capability
> mount but the OCI verifier still expected the former bare source. Its matcher now checks
> the exact volume and capability syntax, removing the bearer suffix from evidence and
> refusals. Eight pure checks pass; the real container workload is being rerun. Record:
> `docs/bugs/2026-09-20-oci-verifier-rejects-capability-mounts.md`.

> **Attachment lifecycle repair (2026-09-20).** OCI bindings now borrow their checked
> source mount, survive the issuing CLI and daemon restart, and end on explicit detach or
> parent unmount. The strict crash/rights lifecycle passes in 7.90 s; the current real Docker
> and CLI suite passes all 10 cases in 28.85 s, retaining exact detach assertions.
> The two-owner SIGKILL regression passes in 3.81 s after moving SDK retirement across all
> owner shards. Fresh client identities are now reserved durably before handoff: the crash
> probe exposed a new `Status` receiving an old `Created` completion. Linux workspace:
> 1,554 passed, zero failed, 14 ignored; macOS: 1,546 passed, zero failed, 14 ignored.
> The later last-id restart history passes on both hosts. Queued-forward
> and publication-refusal proofs remain owed. The corrected snapshot comparison passes;
> nine other macOS performance ceilings remain red, and the intermittent destroy failure
> remains open. Commands, scope and logs are recorded in TBD_FIXES.

> **CI coverage correction (2026-09-20).** The Linux CLI step now supplies `/dev/shm`
> for the portable operator-key recovery test. It previously returned without executing;
> with RAM supplied its unchanged real CLI history passes in 0.42 s. The native macOS
> SDK packaging commands pass with Ada's requested Python 3.14.3 (five Python, five direct
> Node and three packaged Node tests, no skips). Linux loom and shuttle pass too. Miri's
> Linux x86-64 interpretation passes 82 cases with 11 existing exclusions; memory and
> wire leak checks stay enabled. These results do not close instruction-count, KIND,
> Windows or performance obligations.

> **Shard-count correction (2026-09-20).** Choose an explicit shard count before deriving
> capacities. The old override left a one-shard SDK daemon with a three-shard memory split
> and allowed a six-shard daemon's content/metadata backing alone to exceed its 4 GiB bound.
> Two live-daemon regressions fail before the fix and pass after it (1.42 s, macOS).
> The unchanged Linux SDK packaging suite now passes all 13 cases with zero skips. The
> Linux workspace passes 1,557 cases, including all 49 fleet histories; strict Clippy,
> xtask, non-root FUSE and the real CLI step pass too. macOS passes 1,548 workspace cases,
> all 48 fleet histories, the real CLI lifecycle and 13 installed SDK cases; see
> [the diagnosis](../bugs/2026-09-20-explicit-shards-keep-the-automatic-memory-split.md).


Rubric per item: research on file? spec section exists? test matrix? acceptance criteria? laptop
degenerate stated? open decisions named? Classification: `undesigned | designed-unspecced |
specced-untested | decision-open | drift (owed-and-forgotten)`. A stale ledger is itself a gap.

## 0. The one global fact

> **Current repair checkpoint (2026-09-17).** [TBD_FIXES.md](TBD_FIXES.md) collects the
> remaining audit/CI fixes, unfinished validation and current uncommitted repairs. The shared-gossip
> five-node takeover/location history passes in 11.36 s. Neither result closes the broader
> configuration-transfer contract.
>
> **Whole-RAM replacement flake — FIXED (2026-09-17).** `a_whole_ram_replacement_joins_as_a_fresh_voter_and_commits_after_another_loss`
> failed ~1 run in 8 on Linux at the second-loss commit. Root cause (committed council log): after the
> loss the new leader retired the **live fresh voter** on a **transient** SWIM `Dead` belief (its
> sessions churning through the re-election) instead of the actually-dead victim, leaving a voter set
> with a dead member and no live majority — an irreversible consensus retirement on a revocable belief.
> Fix: the council retires only members whose death is confirmed (`Dead`, never `Suspect`) and stable —
> observed dead continuously for the election-timeout window (`ShardState::council_death_watch`, advanced
> on every node every period). Linux 45/45 after the fix; the timeout was not raised.
> Record: `docs/bugs/2026-09-17-council-retires-a-suspected-voter.md`.

> **Editor workload (2026-09-17).** Vim's temporary-path backup exclusion caused the Ubuntu
> host/mount discrepancy. The roster now fixes that policy explicitly and requires a readable
> backup. A real-save regression checks both path classes and exact old/new bytes; its local
> execution awaits supplied RAM scratch or authorization to create the requested RAM volume.
> The full native conformance lane remains unrerun. Record:
> `docs/bugs/2026-09-17-editor-backup-depends-on-scratch-path.md`.

A-9, 2026-09-05: the system has substantial component source and historical tests, but it
cannot yet offer the complete product contract. This status is based on read-only review of
Slates `a1059ed` and Hecate `103c078`, not a new test run. Fourteen source findings and their
triggers are in [the audit](../bugs/2026-09-05-system-contract-audit.md); §8i is the current
closure ledger. Design corrections are implemented in docs only. They do not fix the code.

Sections §8a–§8h preserve dated implementation records. An earlier "gated" claim applies only
to the named test and its original scope; it does not close the A-9 integration or correctness
gaps. The TLA runs in §10 are historical, bounded model evidence, not a proof of current Rust.


> **Remote owner location (2026-09-17).** The foreign-home lookup no longer ranks every live
> home-region member, which could select a node outside the object's actual copyset. A bounded
> read-only exchange asks home-region peers for their held-object routes; one hint per live
> client avoids rediscovery on ordinary requests. The executing owner retains the forwarded
> completion; an origin-side routing refusal cannot poison a retry. The five-daemon history
> failed remotely while the real successor served locally; the separate replay counter exposed
> that the original retry proof never reached the owner. These fixes do not close the broader
> lease, configuration state-transfer or native conformance gaps. Record:
> `docs/bugs/2026-09-17-remote-lookup-guesses-outside-the-copyset.md`.


> **Runner identity correction (2026-09-17).** Ubuntu job 105312670403 ran pjdfstest as uid 1001
> while reporting root: passwordless sudo availability was mistaken for the caller's effective
> identity, suppressing elevation. The invocation now derives elevation from the actual caller,
> and TAP classification, expected-failure selection and the result record use that invocation's
> identity. Other suites report their workload's identity independently of mount/tracing helpers.
> The dispatch regression fails before the fix and passes afterward; no native conformance
> rerun or closure of the 6202 reported failures is claimed. No expected-failure list changed.
> Record: `docs/bugs/2026-09-17-conformance-confuses-available-and-effective-root.md`.


> **Shared clock domain (2026-09-17).** HostClock now reads one OS monotonic boot/time domain
> shared by the anchor, daemon, shards and warm restarts; constructing a clock never resets time.
> Linux BOOTTIME, Darwin MONOTONIC and Windows precise interrupt time include suspend. Local
> lease deadlines retain their meaning after recovery, and heartbeat freshness uses comparable
> readings. Values are not comparable across hosts/time namespaces. Anchor format 3 refuses
> format 2 before recovery because its timestamps used per-instance origins. Two supervised
> child generations reproduce the old bug (0.12 s) and pass with the common clock. Record:
> `docs/bugs/2026-09-17-heartbeats-use-different-clock-origins.md`.


> **Takeover placement retention (2026-09-17).** Accepted held records retain their owner's bounded
> candidate set and quorum. Retirement selects among those candidates still in committed membership,
> never from a neighborhood rebuilt after a fresh replacement joined. Phase one uses that original
> quorum; adoption commits under the successor's current placement and records that exact placement.
> The deterministic counterexample chose an empty replacement (0.00 s before, passing after); the
> Linux restart and five-node takeover histories pass. This corrects a specific placement defect and
> does not close the broader AC-8.18 state-transfer obligation. Cross-region forwarding's independent
> all-alive-members owner guess is corrected by the owner-location exchange above. Record:
> `docs/bugs/2026-09-17-takeover-ranks-an-empty-replacement.md`.

## 1. Component inventory

| Subsystem (design §) | Current source status, 2026-09-05 | Open contract and acceptance |
|---|---|---|
| Machine/memory/runtime (4.1–4.3) | Foundation crates and historical measurements exist. **Admission/residency is integrated (2026-09-13, GAP-A9-1):** per-host admission is all-cost on one code path — content charged at its buddy block, snapshot retention charged from unpromised capacity by the retaining operation (refused typed before mutation, balanced through destroy and recovery), metadata laid out against the class and every volume's records reserved from a per-shard ledger; effective capacity clamped to the OS/job/cgroup bound; mapped and usable reported; an admitted claim protected through resize and recovery; proven by the charge oracle (150 histories) and AC-2.11's neighbour test; two defects found first (a retained inode version freed the chunks its successor shared — data loss on `destroy_snapshot`; the inline spill sized window 0 to the write's end). **Pressure signal DONE 2026-09-21** (a hold on each shard's byte budget, sampled from `memory_available_now` at the liveness cadence and fanned as the host shortfall's per-shard share; shrinks admittable only, never a committed claim, so a raised hold refuses a new create `BudgetExceeded` while an admitted volume's within-entitlement writes land — `crates/server/tests/nfs_mount.rs`, the pure budget unit; admission.md §5.5). Owed: the Windows job-object bound, guest and open-reference bytes (docs/wip/admission.md). AC-0.7's loom half is met (2026-09-13): the one-producer ring (157 bounded interleavings), the two-producer ring as a contention model (3,865) and a lapping model (26), the handle core (6) and the runtime's kick-if-parked protocol (27) pass every interleaving explored under the two-preemption bound and 224-branch cap held in `slates_mem::loom_bounds`, in CI's `miri-and-loom` lane; the parking model found a lost foreign wake on the protocol as it stood, fixed by a `SeqCst` fence on each side (`docs/bugs/2026-09-13-parked-shard-loses-a-foreign-wake.md`); Miri unchanged. T-1.6 and T-6.7 run in their shuttle forms on the nightly cadence (`shuttle-nightly`, 200 seeded schedules each). Record: `docs/wip/concurrency.md`. **The runtime registry carries a per-shard `Pulse` (2026-09-14)** — cache-line padded, `Relaxed` statistics, one plain store per step by the owning shard — reporting step, wait, spawn, completion, refused-admission and longest-step counts and the parked snapshot, readable from any thread with no shard round-trip, so an observer distinguishes a shard that is stepping-but-slow from one parked-with-no-kick or held in a long poll — the instrument a stall diagnosis needs when the shard will not answer a query. **The registry reclaims a shard's slot at shutdown (2026-09-14):** slots are generational (odd free, even live; one `compare_exchange` claims the lowest free one), a shut-down runtime gives every slot back after its threads joined (the kick descriptor closed, the entry retired until the slot's next holder replaces it, the next holder's task arena starting past the old one's generations so a stale waker can never name a live task), and the pair rings are owned by their source shard's entry — 1,025 runtimes start in turn where the 895th was refused, 64 two-shard cycles leak 0 descriptors where they leaked 212, a waker minted for a dead shard is refused by the slot's new holder (`crates/rt/tests/reclaim.rs`; `docs/bugs/2026-09-14-shard-registry-leaks-every-slot-for-the-process-lifetime.md`). **The fleet's task share (2026-09-14):** `DaemonConfig::with_fleet` derives the fleet's own task population — per peer its probe loop, its record link and the serve tasks of the sessions the demultiplexer holds for it on each plane; per plane a receive and an accept loop; the coordinator — and adds it to the shard's task and timer budgets (five peers add 35: 4,241 → 4,276 on this box), so a burst of peer re-dials under load fills the fleet's share and never a client's (the over-spec `adm_refused=4554` of the WAN tree was accept-side handshakes admitted out of the clients' budget); and every spawn the arena refuses — the fleet's loops and session serve tasks, the daemon's heartbeat and mount listener, a mount connection — is counted typed (`fleet.loop_spawn`, `fleet.serve_spawn`, `daemon.loop_spawn`, `nfs.serve_spawn`) or fails shard initialization typed (the serve and reap loops), where eight sites dropped the refusal in silence (`docs/bugs/2026-09-14-fleet-tasks-admitted-against-the-clients-budget.md`). **Contexts and per-shard singletons reclaimed (2026-09-14):** a shard's context (task arena, run queue, timer wheel — 2,032 KiB resident for a daemon-sized two-shard runtime) is owned by its registry slot and freed by its own thread when its loop ends, and per-shard singletons (a socket's demultiplexer, the fleet identity) live on the context (`ShardContext::keep`) and drop with it after its tasks; the fleet coordinator's progress count moved onto the registry pulse and the doorbell's stop flag became a channel — 32 start/shutdown cycles grow the process 752 KiB where they grew 56,016 KiB, and a stopped daemon's serve ports are free again (a restart binds the same addresses and re-meshes: `a_stopped_daemons_serve_ports_are_freed_so_its_restart_binds_the_same_addresses`). Freeing contexts exposed two faults in the slot protocol, both fixed: a foreign send spun forever on a full ring whose holder had left (now re-validated each turn, counted stale; a shard drains its own rings while it waits), and a reader could dereference an entry a re-registration had freed (now a counted reader the re-registration waits out under a SeqCst fence pair) — `docs/bugs/2026-09-14-shard-context-and-fleet-sockets-leak-per-boot.md`. **Admission refusals typed, ids never leaked, one doorbell per daemon (2026-09-14):** a connect past the client bound is answered `TooManyClients { limit }` on both rendezvous paths (Linux: a refusal handoff naming client 0 with the bound; macOS/Windows: a `REFUSED` claim slot carrying it) where the client read a short handoff or waited out its claim; the id is reserved at admission so a burst inside one accept round is counted (a bound of one admitted two); a seat the shard's arena or table refuses gives the id back through an `Admission` guard and is counted (`HANDOFF_LOST`), a handoff that fails after admission is reported with its id (`IpcError::HandoffLost`), the reaper's release shares the path, and `tasks_refused` reaches `slates status`; the doorbell flag is one per daemon (a table by control shard) — one process-wide flag let one daemon's poller consume another's ring under the parallel daemon suite (`docs/bugs/2026-09-14-refused-admissions-leaked-ids-and-were-never-told.md`). **Simulation leaks closed (2026-09-14):** a simulation's per-shard driver flags are owned by their registry entries (`RegisterKick::Sim`) and its clock by the runtime, the fabric reads the sending shard's clock — 32 daemon-sized simulations grow the process 288 KiB against a 2,032 KiB footprint (`crates/rt/tests/sim_memory.rs`); and a claim refused for any reason is answered on both rendezvous paths (the bound, or the daemon unavailable with the reason) instead of leaving the client to its claim wait. **2026-09-17:** a spawn submitted from another thread may carry an admission receipt (admitted as a named task, refused with the runtime's refusal, or terminated — a shard shutting down admits nothing new), and a submission can be pinned to the registration holding a slot so a reused slot refuses instead of admitting a stranger's task (`crates/rt/tests/admission.rs`, §4.3 status). Two runtime bugs found by those tests and fixed, failing test first: `Runtime::shutdown` dropped a `ControlFull` refusal of its own message and joined for good (`docs/bugs/2026-09-17-shutdown-send-lost-under-a-full-control-channel.md`); `drain_control` cleared its pending flag before one bounded batch, so a burst past a batch sat undrained until a later send (8 of 32 ran; `crates/rt/tests/burst.rs`, `docs/bugs/2026-09-17-control-drain-forgets-a-burst-past-one-batch.md`). The registry's unit tests now give every registration back (a leaked, undrained ring hung the interleaving stress test's neighbour wake for 10 min). | GAP-A9-1, GAP-A9-11; AC-0.10–0.11, AC-2.11 |
| Volume/namespace/base (4.4–4.5, 4.15) | **Extended attributes (A-32, 2026-09-26):** attribute inodes named from a per-inode table; quota, CoW, snapshots, recovery (image v5) and the model/differential oracles cover them; bridges, export, landing and the merge's attribute ops are owed. Core CoW, host seam and witnesses exist; every namespace and metadata mutation of an overlay now goes through the base plane from the shared bridge (witness copy-up for chmod/chown/utimes/truncate/link/rename, whiteouts with the listing reloaded, base names refused to create/mkdir/symlink/link; `991c84e`), a watcher hint marks the hinted directories stale for every attached transport and live base entries carry a bounded cache lifetime (`b4eeb2d`); snapshot coverage still incomplete. The clean-file digest is built single-node (2026-09-14): `digest` exports a verified current BLAKE3 of an untouched base file (identity and fingerprint checked before and after a windowed hash; typed `DigestNotClean` / `DigestUnverified`, never stale), kept in a bounded shard cache derived from the inode table and invalidated before every mutation, with watcher hints as revalidation triggers (docs/wip/clean-digest.md). | GAP-A9-2, GAP-A9-3, GAP-A9-13 (built single-node); AC-1.16–1.17 |
| Bridges (4.6) | **Mount hardening (A-34, 2026-09-26):** the macOS mount goes through `mount(2)` with the handle fetched over loopback, so no capability appears in `ps`, `mount` or `nfsstat -m`; UMNT is confirmed by the mount table; OCI binds are proven by the bound mount point. NFSv4.1/4.2 (one server, several versions): **the v4 front end is built (A-35, 2026-09-26)** over the v3 semantic layer — sessions with an exactly-once slot cache, bounded tables, lazy lease expiry, open state per (client, owner, file) with share reservations, the listener owning the state so a session survives its connection (`tests/v4.rs` 11, `tests/v4_session.rs` 8), and **served by the daemon** (v4 state on the listener's shard, each operation routed to the owner shard, bounds derived at boot; a v4.2 client writes a remote-shard volume and NFSv3 reads it back), and **mounted by the Linux kernel's v4.1 and v4.2 clients** (`tests/nfs_v4_kernel.rs`, Linux CI lane), **byte-range locks served** (POSIX semantics, model-tested; the kernel's `flock` conflicts across owners), **the v4.2 SEEK, READ_PLUS, COPY and IO_ADVISE** (the kernel's `SEEK_HOLE` answered by the server), **RFC 8276 extended attributes** (the kernel's `user.` attributes round-trip), **file state at the file's owner (A-36)** (two clients meet one lock table across shards), **durable across a restart (A-37)** (a held open and lock survive SIGKILL through the kernel client; the test fails with EIO without the restore), **`change` is the volume's counter and `change_info4`/`wcc_data` are real (A-38)** (a frozen clock still moves `change`; CREATE/REMOVE answer atomic change info; FIFOs, sockets and explicit times through the kernel client); **the session's negotiated sizes held, and the runtime's idle spin and io_uring harvest no longer sleep through requests (A-39, 2026-09-26)** (1 MiB writes through the kernel's v4.1/v4.2 clients; NFS bench, 256 stats: 782 → 6.5 ms over v3, 1,313 → 10.9 ms over v4.2, `docs/wip/BENCHMARKS.md`); owed: DEALLOCATE, CLONE (dedup), RPC-over-TLS, pNFS flexfiles, and the audit items in TBD_FIXES ("NFSv4 audit, remaining"). **AppleDouble views (A-33, 2026-09-26):** the NFSv3 bridge serves macOS `._name` sidecars as views of the attribute store (all eight workloads identical locally); FUSE/WinFsp attribute calls are owed. Linux FUSE codec/dispatch/transport/launcher; the NFSv3 loopback bridge (`bridge-nfs`, the macOS fallback + oracle) — now **serving a live mount**: `serve_connection` (`src/server.rs`) reads ONC RPC records off a stream and dispatches portmap `GETPORT`, MOUNT `MNT` and the NFSv3 procedures onto a `VolumeBridge`, the socket-and-mount half the codec was built to sit behind. A hand-rolled client mounts and reads a seeded file back byte-for-byte over a real socket in CI (`tests/loopback.rs`), and the `nfs_loopback` example serves a real `mount_nfs` client — no signing, no kext, no privilege beyond the mount itself (R10). This is the macOS live-mount path that needs no Apple entitlement, verified here. The **production async server** now runs on slates's own runtime, closing §4.6's "the production server multiplexes it on slates's runtime": `serve_connection_async` (`src/server.rs`) serves a connection over the runtime's async `TcpStream`, reads and writes awaiting the shard's driver (`write_all` awaits write-readiness, so a stalled client yields the shard rather than blocking it), sharing the RPC engine (`dispatch`) and record codec with the blocking form — one engine, two transport adapters. It rests on new async TCP in the rt (§4.3): `Driver::register_writable` (the `EVFILT_WRITE`/`EPOLLOUT` sibling of `register_readable`; kqueue, epoll, io_uring (a one-shot `PollAdd` since 2026-09-16, `docs/bugs/2026-09-16-io-uring-driver-carries-no-socket-readiness.md`) and Windows IOCP (its AFD reactor) all implement it, and the sim refuses only writability since its fabric sends never block) and `tcp::{TcpListener, TcpStream}` over one shared readiness future with `udp::UdpSocket` (rt unsafe budget 18→20, the two write-filter registrations). Because a volume is `!Send` (a `Box<dyn Clock>`), the serve loop reaches its shard by the daemon's own idiom (a `Send` boot task via `spawn_on`, then `futures::spawn` for the non-`Send` loop). Proven by use with no privilege: `tests/async_loopback.rs` mounts and reads a seeded file back byte-for-byte from the async server on the runtime (the same client as the blocking test), `crates/rt/tests/tcp.rs` the accept→read→write round trip on the runtime's own sockets; the `nfs_async` example serves a real `mount_nfs`. And the server now serves **many volumes**, not one: `MultiExport` (`src/multi.rs`) routes each request to the volume its file handle names — every served NFS procedure begins with a file handle, and the handle already encodes `(volume, inode, gen)`, so the router reads the leading handle's volume id and hands the untouched request to that volume's `Export` (a handle for a volume the server does not hold is `NFS3ERR_STALE`); `NfsService` is the seam the serve loop and `dispatch` now work over (a single `Export` or a `MultiExport`), so one server serves one volume or many with no transport change. And the **single root mount** the design calls for (§4.6 line 128, "the single kernel mount point per host under which volumes appear as directories") now works end to end: `MultiExport` serves a synthetic read-only root directory whose entries are the volumes — `MNT /` returns its handle, `GETATTR`/`ACCESS`/`READDIR`/`READDIRPLUS`/`FSINFO`/`FSSTAT` describe and list it, `LOOKUP` a volume's name returns that volume's own root handle (the same a direct mount gives), and every mutation is `NFS3ERR_ROFS` (a volume appears by a metadata operation, never a client `mkdir`, design line 129). And the serving core is now built to the **daemon's shape**, not the test's: a `VolumeSet` seam (`src/multi.rs`) supplies the volumes, and because a shard holds many volumes sharing *one* store, a volume is served through a *transient* `VolumeBridge` built per request (the design's "marshal each operation into the bridge queue of the owning shard", §4.6 line 1341; the shape `bridge-fskit`'s `MountSession` already takes). `MultiExport<V: VolumeSet>` does the routing and the synthetic root above the seam; the daemon implements `VolumeSet` over its `ShardState` (one store, a volume slab), a test over `OwnedVolumeSet` (one store, several volumes — the same shared-store shape). Proven by `tests/multi.rs` over a real socket against two volumes in one shared store (distinct inode prefixes, as a shard assigns). **And the daemon now serves it, end to end:** `crates/server/src/nfs.rs` binds a loopback listener at boot (its port on `Daemon::nfs_port`), serves it on the control shard, and `ShardVolumeSet` implements `VolumeSet` over the shard's `ShardState` — resolving a volume through `state::with_state` and a transient `VolumeBridge::attached` per request (the daemon's serve path; it lends the volume slot's base host and a fresh handle slab). Each connection is a detached task, so connections are concurrent (a request borrows the shard state only for the synchronous serve; the awaits are on the socket). Proven with no privilege by `crates/server/tests/nfs_mount.rs`: a single-shard daemon is started, a client provisions a volume through the real rendezvous, and then over the daemon's NFS port a client mounts that volume, **creates a file, writes bytes, and reads them back** — the bytes travel client → NFS → `ShardVolumeSet` → the shard's real volume and back. A real `mount_nfs localhost:PORT` would do the same over the kernel. And it serves volumes on **any shard**: the **cross-shard bridge queue** (§4.3, D-7 "bridge queues pinned to the owner") is built. A request naming a volume this shard does not own is routed (`route`, by the volume's owner *partition* from `verbs::owner_of` mapped to a shard) to run the same `serve_call` on the *owner* shard — spawned there exactly as the client path forwards a verb (`Control::Spawn`, §4.3) — and the owner spawns a task back on the accepting shard that hands the reply to the awaiting connection task through a per-shard, thread-local pending map (no new runtime primitive, no lock). Proven by `tests/nfs_mount.rs`: a single-shard daemon mounts a provisioned volume and writes then reads a file back byte-for-byte, and a **two-shard** daemon does the same for a volume on a shard *other* than the NFS listener's, over the bridge queue. Reaching a volume from the single host root works across shards too: a root `LOOKUP` routes by the looked-up name (which is a volume's id in hex), so `mount /` then `cd <id>` reaches a volume on any shard (`tests/nfs_mount.rs` `a_client_mounts_the_host_root_and_reaches_a_remote_volume_by_id`). And the host root's *listing* gathers every shard's volumes over the bridge queue: a root `READDIR`/`READDIRPLUS` scatters an entry-gather to each other shard (the same spawn/back-spawn the bridge calls use) and lists them all, so `mount /` then `ls /` shows every volume on the host (`tests/nfs_mount.rs` `the_host_root_listing_gathers_volumes_from_every_shard`); a remote volume's per-entry attributes are absent in READDIRPLUS (a client fills them with a `LOOKUP`, which routes across shards), the rest is complete. **So the whole browse — `mount /`, `ls /`, `cd <id>`, read/write — spans shards.** Each request runs as the **mounting user**, not always root: the daemon reads the uid from the call's `AUTH_SYS` credential (`nfs::subject_of` over bridge-nfs's new `auth_sys_uid`; `AUTH_NONE` falls back to root, §4.13), and the subject rides to the owner shard on a cross-shard call, so a request runs as the same user there. Tested by `crates/bridge-nfs/tests/auth.rs` (an AUTH_SYS credential's uid is read; AUTH_NONE names no user). And an object it creates is now **owned** by that user, not root: `VolumeBridge`'s three creating verbs (`create`/`mkdir`/`symlink`) stamp the new inode's uid from the request subject and its gid from the parent directory (the BSD/macOS create rule), at the shared seam so every transport inherits it — the fix for the *root:wheel mount bug* (`docs/bugs/2026-09-09-root-wheel-mount.md`; before it a created inode kept the volume core's born default uid 0, so a file an ordinary user made through the mount listed as `root`). And the group is the mounting user's own too (Ada's call, 2026-09-09): the request's `AUTH_SYS` **gid** is threaded beside the subject as one `Requester` (subject + group) through the daemon's NFS path — including across shards — and overlaid onto the created object via `OpContext::owner_gid`, so a file lists as e.g. `adalundhe staff`, exactly as a native NFS server stamps it (a mount with no credential group, `AUTH_NONE`, falls back to the parent's group — the BSD rule). The group is deliberately *not* in the uid-only §4.13 `Principal`; it rides as file ownership, carried on the `Export` (`set_owner_gid`, set per request) rather than the authenticated attachment, so `attach` and the 40 `Export::new` call sites stayed untouched. `auth_sys_creds` reads both uid and gid from one credential (`auth_sys_uid`/`auth_sys_gid` project it). Proven by `crates/bridge-core/tests/volume_bridge.rs` (`a_created_object_is_owned_by_the_mounting_user_and_its_parent_group`, failing-first: uid 0 where the subject's is required; and `a_created_object_takes_the_request_group_when_the_credential_names_one` for the credential-group half) and live on a real kernel mount (the `slates_mount` example now lists `hello.txt` as `adalundhe staff`, was `root wheel`). Minor remaining: the volume *root* directory (`.` at the mountpoint) is still `root wheel`, created 0/0 at provisioning and shared across mounts — the mount root's own ownership, not a file created through the mount (the reported, fixed defect); stamping it needs the provisioning user's group carried into `Volume::create`, a separate change. The served NFSv3 procedure set now includes **COMMIT** (`fsync`) and **LINK** (a hard link), which had fallen through to `PROC_UNAVAIL`, breaking real workloads over the mount. COMMIT (RFC 1813 §3.3.21): every slates write already lands `FILE_SYNC` (synchronously durable before its reply), so it is a no-op that reports the file stable and returns the same `writeverf3` a WRITE does — a client's `fsync`, which the kernel issues as COMMIT, now succeeds instead of failing, so the git/sqlite/editor workloads (§6 test set) that fsync work. LINK (§3.3.15): the volume core supports hard links (`Bridge::link`), so `ln a b` makes a second name for a file — both names resolve to the same object and its link count rises — instead of failing. Proven by use in `tests/procedures.rs`: `a_commit_over_the_export_reports_the_write_stable` (with `a_commit_of_the_host_root_is_a_no_op`, so `fsync` on the `mount /` root is fine) and `a_link_over_the_export_makes_a_second_name` (both names name one object, nlink 2; a LINK on the read-only synthetic root is `NFS3ERR_ROFS`). A routing-level stale/bad-handle COMMIT and LINK are framed in their own failure shapes (`status_only_or_wcc`). So every NFSv3 procedure a real program depends on is served (create/lookup/read/write/remove/rename/mkdir/rmdir/symlink/readlink/link/setattr/access/readdir(plus)/getattr/fsstat/fsinfo/commit). **MKNOD** (device/FIFO/socket nodes) is refused with the typed `NFS3ERR_NOTSUPP` — a RAM CoW filesystem does not create special nodes — not `PROC_UNAVAIL`, honoring the project's typed-refusal rule (an uncategorized refusal is a bug); a client's `mknod` gets a proper NFS error framed as the directory's `wcc_data`, and on the read-only synthetic root it is `ROFS` like the root's other mutations (`tests/procedures.rs` `a_mknod_over_the_export_is_notsupp`). And **PATHCONF** is served: it reports the volume's POSIX limits from the volume's own policy — the name maximum and case behaviour from `statfs` (the neutral `FsStat` gained a `case_sensitive` field, filled from the volume's `NameEquivalence`, the additive completion of the info surface that already carried `namelen`, and the *only* `Bridge` impl is `VolumeBridge`), the link maximum as the `u32` counter's range, names refused-not-truncated (`no_trunc`), and chown unrestricted (`chown_restricted` false — `Volume::chown` imposes no privilege check). **So every RFC 1813 NFSv3 procedure is now handled**: a client's `pathconf` gets real answers, an exact-name volume reporting case-sensitive and a case-folding one case-insensitive (`tests/procedures.rs` `pathconf_reports_the_volume_limits`, `pathconf_reflects_a_case_folding_volume`), and the synthetic root reports uniform limits. Only MKNOD (unsupported) is a typed refusal; nothing falls through to `PROC_UNAVAIL`. And a volume now appears under its **friendly provisioned name** (§4.6 "Chosen path"), not its id in hex: the shard's `VolumeSlot` carries the name (set at create/clone/recovery), `ShardVolumeSet::entries` lists it, and a root `LOOKUP`/`MNT` of a name routes across shards by `owner_of_name` — the same partition the create routed to and the id encodes, so a name reaches its volume with no global index (D-14) — where the owning shard resolves it against its own slots (`MultiExport` matches the name in `entries`, already name-agnostic, so bridge-nfs needed no change). Proven end to end by `tests/nfs_mount.rs`: `mount /vol`, `mount /vol-N` on another shard over the bridge queue, `cd rv-N` from the host root across shards, and `ls /` listing every volume by name. The self-describing hex scheme is replaced, not layered (no dual path). Owed here (minor, situational refinements): And the volume now **mounts**: `slates mount <id> <path>` reads the volume's name and the daemon's NFS port (`StatusReport::nfs_port`, a process-global word the daemon sets at boot) and runs `mount_nfs -o vers=3,tcp,port=P,mountport=P,noresvport,soft,intr,locallocks,nosuid,rdirplus,actimeo=1 localhost:/<name> <path>` (`crates/cli/src/mount.rs`) — no privilege (`noresvport`, R10), no kernel extension, no Apple entitlement, the same signing-free mechanism sylk mounts over FUSE-T (itself NFS-backed). The attribute-cache `actimeo` is a documented short value (loopback GETATTR is sub-millisecond so revalidation is cheap; sylk uses the same trade-off for its 100 ms FUSE timeout). **Proven by a real mount on macOS in-sandbox**: after `slates mount`, `mount` shows `localhost:/<name> on <path> (nfs, ..., mounted by <user>)`, a file written through the mount reads back byte-for-byte, and the arg construction is unit-tested (`mount.rs` `the_mount_arguments_target_the_daemon_port_and_the_named_export`). The mount **lifecycle and capability detection are adapted from sylk's cgofuse mount** (`core/purevfs`): a pure, predicate-injected `classify` (mount_nfs present → the NFS loopback backend, testable on any host with no live mount, the way sylk factors `classifyDarwinFUSEBackend`), a probe that refuses with a message naming what is missing rather than a raw `mount_nfs` failure, and `slates unmount <path>` (`umount`, the counterpart of sylk's cgofuse `Close`; no daemon needed, so a stale mount unmounts even after its daemon has gone). **Proven live in-sandbox**, now by a repeatable by-use test (`crates/cli/tests/cli.rs`, gated `SLATES_TEST_CLI=1`, loud-skip without `mount_nfs`) driving the real binary — `slates mount` → a file written through the mount reads back byte-for-byte → `slates unmount` → the mount table clean — and by a runnable example (`cargo run -p slates-cli --example slates_mount`) that drives an in-process daemon to the same live kernel mount. The **anchor-held listener** (§4.6 line 509, restart survival) is now built: a supervising anchor binds the NFS loopback listener and hands its descriptor to every daemon it spawns (`slates-cli`'s `hold_nfs_listener` + `Supervisor::hold_nfs_listener`, inheritable across the spawn), and the daemon adopts it (`daemon.rs` `nfs_listener` over `slates_rt::tcp::TcpListener::{from_fd,into_fd}`) rather than binding a fresh ephemeral one, so the loopback port is stable across a restart — a live mount survives it; a standalone daemon (tests) still binds its own (Unix only: NFS is the macOS/Linux bridge). Proven by use: `crates/rt/tests/tcp.rs` (`a_listener_handed_over_by_descriptor_serves_on_the_same_port` — a listener reduced to a descriptor and re-adopted serves on the same port) and `crates/anchor/tests/anchor.rs` (`the_supervised_child_inherits_the_held_nfs_listener` — a real spawned child finds the held listener at its bound port on the inherited descriptor, the first proof here that the `Command` descriptor hand-off works, since macOS hands the segment over by name); rt unsafe 20/20 unchanged (both new constructors are safe), slates-server 4→5 for the daemon's `from_raw_fd` on the inherited number. The root-listing gather now fans out to the shards in parallel. The §4.6 differential oracle (line 1368) is **not** an owed refinement here but gated with item (1): it mounts the same volume via FSKit *and* via NFS and compares the abstract states — two real kernel mounts — so it needs the FSKit mount and hence the Apple Developer entitlement this sandbox cannot hold; a synthetic FUSE-dispatch-vs-NFS-dispatch stand-in would be vacuous (both legs dispatch onto one `VolumeBridge`, R5). The **macOS FSKit bridge** now has its Rust half **complete** (`crates/bridge-fskit`, A-1/D-O9): the shim wire codec for the whole `Bridge` operation set (read/write/handles/dir-enumeration/namespace/links/refs/metadata/root — 21 operations) dispatched onto the one seam, a golden vector pinning the wire, 10 by-use tests against a real `VolumeBridge` (round-trip, hostile-input, a directory lifecycle, symlink→readlink, create→rename→lookup, a chmod+truncate setattr, the real root object, a NotFound reply). Codec-first, exactly as the NFS codec. The Swift half now **compiles against the real framework** (macOS 15.4+ SDK): `ShimWire.swift` is the codec (a pure library, cross-checked byte-for-byte against the Rust golden vector), and `SlatesVolume.swift` is the `FSVolume` + `FSUnaryFileSystem` handler — it conforms to FSKit's real `FSVolume.Operations`/`ReadWriteOperations`/`OpenCloseOperations` and `FSUnaryFileSystemOperations`, translating every operation to a shim request; `swiftc -emit-library` builds a dylib exporting `SlatesVolume`/`SlatesItem`/`SlatesFileSystem` with `@objc` conformance thunks over the real FSKit signatures (built on macOS 26 here; the CI step skips loud on a pre-15.4 runner). `HandlerTest.swift` drives the real handler by use against FSKit with no mount (constructing `SlatesVolume` and a mock `ShimChannel`), asserting the shim requests it emits and the FSKit objects it builds — including that two opens then two closes of one item release both daemon handles (the per-inode handle stack fixed a leaked open-reference the earlier single-handle map had), and that a NotFound lookup surfaces an error, not a crash). Owed (the Phase 4 mount spike, now just the transport and the live run): the app-group ring behind the `ShimChannel` seam, the `Slates.app` bundle + FSKit entitlement + `UnaryFileSystemExtension` `@main`, and the live mount that exercises the `FSItem` lifecycle. The earlier `SPIKE:` guesses are resolved and by-use tested: the root object id (`OP_ROOT` learns the real `compose(prefix, 1)`), the time unit (Unix nanoseconds) and the open/close handle accounting (a per-inode handle stack that releases every handle, not the single-handle map that leaked one); and `probeResource` now recognizes a `slates://` URL resource by its scheme and refuses others, rather than accepting unconditionally. `HandlerTest.swift` drives all of these against the real handler with no mount (20 checks). And the transport is no longer only a stub: the `test-harness` feature builds the crate as a cdylib exposing a C ABI over `serve` (`src/ffi.rs`), and `InProcessTest.swift` links it to drive the real handler through `serve` over a real `VolumeBridge` on a real scratch volume — the whole handler↔codec↔bridge stack end to end in one process (create→lookup→write→getattr→remove, the root proven to be `compose(prefix, 1)`). What remains is the production transport reaching the daemon's real volumes, and the live mount — and the daemon-side serve is now **built**: `VolumeBridge::attached` lends the bridge an *external* open-handle map instead of owning one (a non-breaking addition — FUSE and NFS keep `new` and its owned slab), and `bridge-fskit`'s `MountSession` owns that map per mounted volume, building a transient bridge per request over the shard's store and volume. That is the Rust shape of a garbage-collected mount handler (the per-mount state lives in the session; the volume access is threaded per call — the borrow checker's price for no GC, and a small one). It is proven by use: `a_mount_session_persists_open_handles_across_requests` drives create→open→unlink→read→release→read across *separate* requests and shows the content reclaimed only after the last handle is released — which requires the map to have persisted (§4.8 unlink-while-open). The serve handles both scratch and overlay volumes: `attached` also takes an optional *borrowed* host (a `HostRef::Borrowed`, the overlay analogue of the borrowed handle map — `OsHost` is not `Clone` and its reads take `&mut`, so the transient bridge borrows it as it borrows the store), so `MountSession` lends the shard's `OsHost` per request and a base entry the empty overlay does not hold is served from it (`a_mount_session_serves_an_overlay_base_through_the_borrowed_host`, over the repo's crates tree read-only, no RAM disk). What remains is genuinely external: the app-group ring that delivers requests (needs the signed bundle) and the mount-session/attachment lifecycle (§4.13). The in-process form is also proven end to end against a scratch volume (`InProcessTest.swift`); the production transport form is the spike's choice (§4.6). The 20th op, `OP_SETATTR`, is wired end to end: `setAttributes` carries the fields FSKit marks valid (chmod/chown/truncate/utimes) to the bridge's `setattr` and returns the new attributes (`serve_sets_attributes_through_the_bridge`). The 21st, `OP_ROOT`, fixes a real correctness bug the handler carried: `activate` learns the volume's true root object (`compose(prefix, 1)`, per-volume prefixed) from the daemon instead of assuming inode 1, which was wrong for any prefixed volume (`serve_returns_the_real_root_object` proves it against a prefix-1 volume whose root is provably not 1). Two former `SPIKE:` guesses are now verified against the daemon's code: the object generation is a stable 0 (D-4) and the shim times are Unix nanoseconds. Both ops round-trip on the Swift side. The **WinFsp bridge is now built end to end** (`crates/bridge-winfsp`, 2026-09-09) — the Windows mount, the last owed OS-integration path. Two parts: the refusal taxonomy `ntstatus(&VfsError) -> Ntstatus` (the analogue of the FUSE errno and NFS `nfsstat3` edges), host-buildable and tested on any host; and the **mount host** (`host.rs`, Windows-only) — the `FSP_FILE_SYSTEM_INTERFACE` vtable and the `FspFileSystem*` FFI **hand-transcribed from winfsp's `winfsp.h`/`fsctl.h`** (`ffi.rs`), the exact discipline the rt AFD reactor uses over the WDK, so it is a faithful model, not a guess (the earlier "winfsp's header-defined protocol needs the headers on Windows" framing is overturned: reading the real headers and transcribing the `#[repr(C)]` structs — which carry the header's own `static_assert` sizes — is the same move that built `afd.rs`). The 16 implemented callbacks (`GetVolumeInfo`/`GetSecurityByName`/`Create`/`Open`/`Overwrite`/`Cleanup`/`Close`/`Read`/`Write`/`Flush`/`GetFileInfo`/`SetBasicInfo`/`SetFileSize`/`CanDelete`/`Rename`/`ReadDirectory`) dispatch onto the shared `Bridge` over a **single owner thread** that holds the `!Send` volume — WinFsp's dispatcher threads send each op over a bounded channel and block for the reply (D-7's "sharing is a move over a bounded channel"; no `Mutex`, no `Arc`, R2), serialized on top by WinFsp's COARSE operation guard; a reference "allow Everyone" security descriptor (one SDDL string) satisfies the FSD's access checks. The whole crate cross-lints clean for `x86_64-pc-windows-msvc` from macOS (`cargo check`/`clippy -D` — no linking needed to type-check the FFI) and is CI-wired on the `windows-latest` runner: `choco install winfsp`, clippy the crate, and a **live mount test** (`tests/mount.rs`, gated `WINFSP_TEST_MOUNT=1`) that mounts a slates volume at a free drive letter through the real kernel FSD and creates/writes/reads/lists/deletes a file through the Windows filesystem, then unmounts — the Windows analogue of the macOS FSKit and `mount_nfs` live mounts. unsafe budget 0→86 (all Win32/WinFsp FFI with `// SAFETY:` lines, no `unsafe impl Send`/`Sync`). Owed: the daemon transport that reaches a *provisioned* volume's shard (the WinFsp callback forwarding to the owning shard, the analogue of the NFS `ShardVolumeSet` — the mount serves a directly-owned volume today), the reparse-point (symlink) and security callbacks left `None`, and the live-mount *runtime* proof depends on the Windows runner (the CI job runs it). **The virtio-fs guest device is built and wired (2026-09-13, `crates/bridge-virtiofs`, `crates/server/src/virtiofs.rs`; GAP-A9-5's device half): a sans-io split virtqueue over a bounded guest-memory seam with every §4.6 A-9 check made before access (chain length, loops, indirect refused, overflow, buffers outside or straddling guest memory or aliasing the rings, readable-before-writable, derived byte caps), the FUSE-over-virtio cycle through the FUSE codec onto the shared `Bridge` (byte-identical with direct dispatch over 15 requests; the hiprio queue; INIT/DESTROY; DAX never advertised), admission that authenticates the consumer before any queue, mapping or tag with per-chain credits derived from the shard's admission limit and the §4.9 window and an owned terminal step (T-4.14: revoke with requests pending → refusal before access, references swept, credits whole), and the device loop as a perpetual task on the volume's owning shard over `slates-rt` woken by the seam's doorbell; the daemon attaches a guest device to a provisioned volume on its owner shard and a guest's file reads back over the NFS port byte for byte (`crates/server/tests/virtiofs.rs`). The `VmmSeam` models the in-process and inherited-descriptor forms; the in-process form is served, the inherited-descriptor binding is refused typed `AttachmentUnsupported{InheritedDescriptor, BindingNotBuilt}`. Owed: the real libkrun/vhost-user bindings, the guest form's durable attachment record and the transport report on the `attach`/`status` wire, a live guest for AC-9.7, and the OCI namespace handoff (the container half). Record: `docs/wip/virtiofs.md`. Codec siblings fixed on the way: `FUSE_DESTROY` now sweeps and `FUSE_BATCH_FORGET` is served (`docs/bugs/2026-09-13-fuse-destroy-and-batch-forget-unserved.md`).** **GAP-A9-3/-4 sweep (2026-09-14, `991c84e`..`1ebd7da`, record `docs/wip/base-fuse.md`):** the bridge's mutating verbs route through the base plane (8 of 10 T-1.21 cases failed before); the FUSE edge resolves `UTIME_NOW` by the volume clock, honours `KILL_SUIDGID` and `CTIME`, refuses unknown `valid` bits and uncarried rename flags with `EINVAL` (5 of 7 failed before) and reports the true change time; 18 vectors transcribed from `include/uapi/linux/fuse.h` (7.46) found `flags2` read from a non-existent padding word and `INIT_EXT` never echoed — no second-word capability had ever negotiated — fixed; statfs reports the capacity the shard budget can honour (a dynamic volume showed 1 GiB against 16 MiB); invalidations are produced from the journal and watcher hints and written by the FUSE loop before each request, `HAS_EXPIRE_ONLY` negotiated, live base entries cached only for the base filesystem's timestamp granularity; the mount-helper handshake is portable, deadline-bounded and reaps or cancels its helper (real processes, 5/5); open/close beyond the 65,536-slot arena is bounded and generation-checked; attachments carry generations with in-flight pins and `barrier(volume)` refuses `BarrierIncomplete` over a consumer lost mid-request. Owed: the Linux lane's first compile of the serve loop and `mount()` (Docker could not start here), the mounted conformance run, a per-volume registry for the snapshot verb's barrier and the writeback flush, base entries' owner fields, a ready-device attachment binding, `RENAME_EXCHANGE`. **The attachment capability report and the OCI form are built (2026-09-14, `71cd63f`…`33fb52f`; GAP-A9-5's report and container halves): `attach` and `status` report every transport with the six facts of §4.6 A-9 — `TransportReport {os, kernel, oci_runtime, capabilities}` on `StatusReport` and the form's `AttachmentCapability` on `Attached` — each fact read from the machine (`uname`, the bound listener, the `PATH` probe) or stated from what the tree holds, a refused transport carrying its typed `UnsupportedReason` and no evidence; a request for a form the host cannot offer refuses `AttachmentUnsupported{transport, reason}` before the lease or the record. The OCI form (`AttachRequest::Oci{source, destination}`, `crates/bridge-oci`, `crates/server/src/oci.rs`) verifies the host path against the kernel's mount table without touching the mount (`getfsstat(MNT_NOWAIT)` / `/proc/self/mountinfo`; a `statfs` of the path would deadlock a single-shard daemon), records `AttachForm::Oci`, and returns the runtime-specification `mounts` entry (`type: bind`, `rbind` + `ro`/`rw` by the attachment's policy) with `HostMountEvidence`; an unbound path is refused `ChosenPathUnavailable{NotAbsolute | NotAMountPoint | ForeignFilesystem | NotThisVolume | ...}`. T-4.13 runs by use on macOS over Docker Desktop (`crates/cli/tests/cli.rs`: the same workload on the `slates mount` path and in `docker run -v` over the daemon's entry; byte and name/size agreement; the container's edit is the host's and the host's delete the container's; the read-only bind refuses a write; `/private` refused typed) with a CI Linux variant over a real FUSE mount (`crates/bridge-fuse/tests/oci_container.rs`). Measured and now reported typed (`SharingSemantics.delete_while_open`): Docker Desktop's share of the host path holds files a container touched open past the container's life, so an in-container delete over the NFS-loopback mount is silly-renamed to `.nfs.*` (not released within 150 s), blocking `rmdir` and the plain unmount. The guest transports carry the virtio-fs device's own report fact for fact (`VirtioFsInProcess` offered with `GuestTag`, DAX not mapped, `SimulatedGuestDriver`; `VirtioFsInheritedDescriptor` refused `BindingNotBuilt`), and a guest form over the ring refuses `SeamNotOnWire`. Owed: the daemon-served FUSE mount (then the bind on Linux), the runtime-specification words verified against the published text, a live guest for AC-9.7. Record: `docs/wip/oci-handoff.md`.** Conformance measured over the live NFS mount (2026-09-14, `docs/wip/conformance.md`): fsx 10,000 ops and fsstress 500×4 pass; the workload suite exposes two declared limits of the NFS fallback — every created entry gets an AppleDouble `._` sidecar (no xattr store over NFSv3) and SQLite refuses WAL on an `nfs`-typed mount — and pjdfstest confirms fifo/socket creation surfaces as `EIO` (the typed `NFS3ERR_NOTSUPP` as the macOS client shows it); the FUSE bridge is served by no daemon transport, so the Linux lane records LIMITED. **POSIX access control at the NFS edge (2026-09-15):** every procedure now applies the POSIX permission rules to the caller its `AUTH_SYS` credential names — uid, primary and supplementary groups — before any effect (`crates/bridge-nfs/src/access.rs`, a pure unit-tested module; `ACCESS` reports the same class verdict; `chown` restricted, the sticky bit, the set-id hygiene), the analogue of `default_permissions` at the FUSE edge. Before, the export answered `ACCESS` from the owner's bits whoever asked and enforced nothing else: 5,336 of pjdfstest's 8,686 root-run cases on the macOS lane (`docs/bugs/2026-09-15-nfs-export-enforces-no-posix-permissions.md`). The volume root is now owned by its provisioning user at `volume create`, closing the root:wheel sibling of 2026-09-14 that the enforcement had made a hard refusal. The live pjdfstest run then found and closed two timestamp faults: `drop_link` marks the inode's `ctime` when a name is dropped (`docs/bugs/2026-09-15-dropping-a-link-leaves-the-inodes-ctime.md`), and `LINK`'s `linkdir_wcc` is read after the link (`docs/bugs/2026-09-15-nfs-link-reply-carries-the-directorys-pre-link-times.md`). The unprivileged run's needs-root rule is now decided per case from the caller's identity (a `chown` to another owner, or an expectation of one, is root-only), and `cargo xtask conformance tally` reviews kept outputs — the CI root artifact included — by command (docs/wip/conformance.md §3.4). The archive now carries ownership (format minor 2, A-20): every node's uid/gid and the root's own metadata, so a takeover successor's rebuild reproduces the origin's owners rather than `0:0` (which the enforcement would have shut the owner out with). | GAP-A9-3–5; AC-3.10–3.12, AC-4.11–4.12 |
| IPC/local database (4.7–4.8) | Rendezvous, metadata replay and completion transactions exist; daemon-restart content recovery implemented and proven over the NFS transport (barrier publish + crash-at-every-step oracle, AC-2.12, 2026-09-14); an exactly-once ack-keying bug from the ephemeral-id split fixed (docs/bugs/2026-09-14-ack-keyed-under-ephemeral-member-id.md). Base-plane recovery and same-uid isolation (GAP-A9-9) owed. | GAP-A9-6 (content closed; base plane owed), GAP-A9-9; AC-2.12–2.13 |
| Registers/configuration (4.8) | Pure register, ledger, mirror and reconfiguration simulations; BUG-12 fixed in `d9cb6e5`, broader BUG-13/protocol evidence open. Cluster plane now built in `slates-cluster` (A-10): SWIM/Lifeguard detector (live over sim UDP), the hecate Raft dialect complete in its mechanism set (election/replication/PreVote/CheckQuorum/ReadIndex/joint consensus with log-integrated transition/snapshot compaction + install-snapshot, sans-io, plus a multi-node conformance suite), the config group folding its committed log into the `Configuration` (reconcile + takeover), register-commit live over authenticated sim UDP, Vivaldi coordinates (live over the wire), progress extension. The Raft dialect also rides the transport (a `RaftMessage` codec and a live election+replication proof over sim UDP). Phase-one promotion now exists in the transport-driven register too (`register.rs`: `Prepare`/`Promise`, `install_authority`, `prepare`, `promote_over_holders`) and rides the transport (`serve_promotion`/`promote_record`/`promote_under_configuration`, a live `f=1` takeover adopting the committed head in `promote.rs`), oracle-tested for Continuity and StaleNeverCommits — single-value register takeover. The **ledger's committed-prefix adoption now rides the transport too**, the multi-entry generalization of that single-value takeover (`ledger::{LedgerAcceptor, LedgerPromise, adopt}` + `cluster::{serve_ledger_promotion, promote_ledger_record}`): a new owner ships a phase-one prepare, each holder raises its fence and reports its **whole log**, and the owner adopts per position the identity under the highest epoch across the quorum. Proven live over sim UDP in `tests/ledger_promote.rs` (an `f=1` takeover adopting the committed prefix `[r0, r1]`, recovering `r1` which after the owner's death only the surviving holder still holds — Continuity), the `LedgerPromise` wire codec hostile-input tested, the `f=0` degenerate observably identical (R8). The owner-runtime composition now exists too (`fleet.rs`: `FleetNode` composes membership + config group + the owner acceptor, keeping the acceptor authority in step with the configuration version; `ConfigGroup::new(owner, quorum)` is its f-parameterized constructor), with the design-mandated N=1≡fleet differential (R8) as a named test and a head commit driven live over the transport (`fleet_live.rs`) — refuting the earlier "gated on scatter width/hardware" reading (the register core is f-parameterized; fleet semantics are testable in-process over the sim). The register object is now the creator-routable 128-bit `ObjectId` (high half = creator host), unifying with the catalog `VolumeId` and enabling routing-by-id with no catalog (§4.8 "Lookup"; `d580be0`), replacing the `object: u64` shortcut. That change surfaced and fixed two real bugs: **placement collision** — `placed_state` truncated the 128-bit `VolumeId` to its high 8 bytes (the creator alone) as the u64 object, so every volume of one creator collided to one placement object; now the object is the full `VolumeId`. **Rendezvous avalanche** — plain FNV-1a left the last fed byte unmixed, so object ids sharing a creator half and differing by one low byte all picked the same holder; added the MurmurHash3 fmix64 finalizer so rendezvous spreads. **Fleet integration built (2026-09-10, Ada's "build all of fleet"):** the **object→owner routing view** (`cluster::routing::Routing` — per-object `object→owner`, no directory (D-14), `take_over` via `rendezvous_first`, `db04059`); it is **composed into `FleetNode`** so `observe` returns `Observed { config_changed, takeovers }` and a death drives takeover (`69f09c6`); the **detector→fleet bridge** `sync_membership` (folds confirmed deaths → takeover and alive joins, `6d2cdad`); and the whole **membership → takeover path proven live over the transport** (`tests/membership_takeover.rs`: a silent peer's real probe timeout → the detector declares it dead → `sync_membership` → the survivor takes over its objects, `fae1aa1`). **`FleetNode` is wired into the daemon** (`bf427c4`): each shard's `ShardState` holds one (`FleetNode::solo` at boot), and the daemon's placement authority (place/region_placed/await_placed/host_epoch) now flows from `fleet.configuration()` — the register/placement path runs the fleet's configuration group, degenerate at N=1 (R8). The **commit and promotion dispatch now consume the progress extension** (§4.8 "late work"): `CommitBudget::with_extension` seeds a `DeadlineExtender` + `ProgressWitness` into the collection loop (`collect_acks`/`collect_promises`), so a commit whose quorum is still filling as it passes its deadline is extended and commits, while a stalled one (no new acknowledgement within the stall window) is left to time out uncertain — one shared `DispatchWait` step replacing both fixed-deadline tasks, proven live over sim UDP (`tests/extend.rs`: a progressing `f=2` commit runs past its base deadline and places, a stalled one expires uncertain within the extension budget), with the `CommitBudget::hard` degenerate reproducing the fixed deadline exactly (R8). The **daemon config now carries the fleet membership**: `DaemonConfig.fleet: Option<FleetMembership>` (the quorum + peer hosts) drives the `FleetNode` boot construction (`init_shard` builds `FleetNode::new(host, quorum, peers)` when set), the laptop leaving it `None` (solo — the `f = 0` degenerate, the same code path, N=1 unchanged with every daemon test green). Owed: the daemon's control shard running the live probe/gossip *loop* over real peer connections (peer-address directory + connection management + the perpetual task driving `sync_membership` and each takeover's phase-one recovery/serve); the **cross-node commit path** (daemon head commit → `FleetNode::commit_head` over peer endpoints, unifying with the local `db` register); the live probe/gossip loop on the control shard over real peer connections and a multi-node daemon test. **`Endpoint::accept` now accepts a peer without knowing its address in advance** — it adopts the source of the first datagram it hears (mutual-TLS-gated by `allowed_clients`), proven by use in `session.rs` (a server that was never told the client's port replies over the session it learned) — which unblocks a **two-node** fleet: each node accepts its one peer on its advertised socket and dials the peer from a separate socket, so the two sessions are cleanly one-directional (no bidirectional-request deadlock). A node serving **many** peers on one socket still needs the connection-ID demux the endpoint marks owed (`endpoint.rs`: the destination connection ID is zero-length). **The two-node daemon membership loop is now BUILT** (`crates/server/src/fleet.rs`, boot step 6): `FleetTransport` (this node's fleet TLS identity, advertised address, and peers) is handed to `Daemon::start_with_fleet` and drives a control-shard loop — a probe task (owns the `Detector`, dials the peer with a bounded-retry handshake since the transport does not retransmit, probes each period, and folds the converged view into the shard's `FleetNode` via `sync_membership`) and a serve task (accepts the peer via `Endpoint::accept` and answers its probes). Proven live by two in-process daemons (`crates/server/tests/fleet.rs`): they form a fleet over real loopback UDP with mutual TLS, and when one is stopped the survivor detects the probe timeouts, ages the suspicion to death, and **retires** the dead peer (`Daemon::fleet_members` observes it) — a transition only the loop can make (the seeded config would hold it alive forever), so the proof is non-vacuous. The **cross-node commit path is now BUILT** (`crates/server/src/fleet.rs` `ship_records`, boot step 6): each node dials its peer's record address at boot alongside the probe session and, each period, commits its unplaced volume heads to the peer holder over that session (`commit_record` — the owner's local hold plus the remote holder, committed at `f + 1`), recording the acknowledging `Placement` in `ShardState.placed_heads` so the verbs' `region_placed`/`await_placed(Region)` report the head placed. Proven live by two in-process daemons over real loopback UDP + mutual TLS (`crates/server/tests/fleet.rs` `a_provisioned_head_replicates_across_the_fleet`): a volume provisioned on one node replicates to the peer holder and reaches the `f = 1` quorum, non-vacuous (at `f = 1` a solo head is not region-placed — only the replicated commit places it). Building it closed a real transport gap: a single-shot request/reply over a **real** datagram socket needs loss recovery, because a dropped packet or acknowledgement leaves no later ack to expose the gap (the probe path tolerated this by retrying each period; a single commit cannot). The transport now drives its tail-loss probe from the estimated PTO — every reliable exchange (`Endpoint::{request, serve_once, send_stream, recv_stream}`) waits for the next packet only up to the probe timeout (`Endpoint::receive_or_probe`, RFC 9002 §6.2.1) and on a timeout retransmits the oldest in-flight packet (`Connection::probe`, already built), and the handshake seeds the RTT estimator (`establish`, RFC 9002 §5.1) so that timeout reflects the real path from the first packet rather than the coarse initial RTT. The fleet frame cap is now derived from the RFC 9000 §14.1 minimum datagram (a whole fleet message in one frame), replacing a value copied from a transport test. The membership loop is now the **general N-peer form**: `run_membership` iterates the transport's peers, binding one serve socket per peer per plane (since `Endpoint::accept` pins one peer per socket) and spawning a probe/serve/ship set per peer, each peer's own detector folding into the shared `FleetNode` via the new **peer-scoped** `cluster::fleet::sync_peer` (which touches only the peer it tracks — the whole-view `sync_membership` would let one peer's detector re-join a peer another has retired, flapping it; `sync_peer` removes that, unit-tested). Proven live at N=2 by the two tests above (the two-node fleet is the single-peer degenerate). **N-node mesh formation is now reliable (2026-09-10):** a fleet of N forms all N·(N−1) probe/record handshakes over `Endpoint::accept` — proven by the new `three_daemons_form_a_full_mesh` (all six three-node sessions establish; 20/20 in measurement), asserted per node by the new `Daemon::fleet_meshed` (every configured peer *actually probed*, distinct from the optimistically seeded membership `fleet_members` reports from boot, recorded in `ShardState.formed_probe_peers` as each `probe_peer` dial completes). Three transport fixes to `Endpoint::establish` made it solid: **(1) handshake confirmation** — TLS 1.3 finishes the client the instant it *sends* its final flight but the server only when it *receives* it, so a dropped final flight stranded the server forever; `confirm_as_client`/`confirm_as_server` add a 1-RTT confirmation (a fresh-numbered `Connection::emit_confirm` packet) the client waits for and the server resends while it still sees the client's flight retransmits (RFC 9001 §4.1.2 / RFC 9000 §19.20 HANDSHAKE_DONE in spirit). **(2) Fast establish retransmission** — the retransmit interval backs off exponentially from the timer granularity rather than a flat two-thirds of a second, so a peer slow to bind its socket during the boot race (a fleet forms as its nodes boot one after another) is reached within milliseconds, capped at the conservative initial PTO (`handshake_probe_ceiling` = `RttEstimator::initial_pto`, not the tiny loopback-seeded estimate a flat cap would collapse to). **(3) Flight dedup** — the aggressive retransmit races the peer's reply, so a re-sent flight identical to one already fed to `read_hs` is recognized and skipped rather than faulting the handshake stream. The N=3 **retirement** test (`three_daemons_..._retire...`) is now **un-ignored and reliable (measured 27/27, and the whole 4-test fleet suite 5/5 with 0 ignored)** — a survivor's SWIM probe of the dead node reliably times out, ages it to death, and both survivors retire it. Reaching that took a **runtime timer-wheel bug fix** on top of formation: the wheel's `cancel` (`crates/rt/src/timer.rs`) unlinked a timer by bare slot index *before* validating the id's generation, so a **stale cancel** — one whose slot had already fired and been reused by a *later* timer (the SWIM probe cancels its deadline task the instant a healthy probe is acked, and the fired slot is reused constantly under a fleet's load) — spliced the *reused* live timer out of its slot list, **orphaning it** in the arena so it never fired. That stranded the next probe's `sleep`, so a survivor's probe of the dead node hung past its deadline. Fix: remove from the arena first (which validates the generation and refuses a stale id) and only then unlink, using the removed entry's own recorded position — regressed by `a_stale_cancel_does_not_orphan_the_timer_that_reused_the_slot`. This was NOT a "noisy machine" flake: a three-node fleet's own task/timer churn triggered it every time (0/12 before, 27/27 after). The formation test waits for the real direct mesh (`Daemon::fleet_meshed`) before the kill so the survivors are retiring peers they actually probed. **Durable held records are now BUILT (2026-09-10, takeover slice 1):** a node backing a peer as a candidate holder now keeps each accepted record in a **durable per-object acceptor** in `ShardState.holder_records` (keyed by `ObjectId`, authority owner = the socket's TLS-authenticated peer), replacing the task-local acceptor that discarded everything past the serve task — so a survivor's phase-one recovery has the newest committed record to read. `crates/server/src/fleet.rs` `serve_peer_records` now serves the peer's commits into that hold via `accept_held_record`, which also tracks the object in the routing view (`FleetNode::track_object`), so the owner's death hands `sync_peer`'s takeover computation the object. One acceptor **per object** (each object has one owner, so one `Authority`) keeps every acceptor within the register's "one authorized owner per generation" model and lets a promotion route to its hold by object id regardless of the peer socket that carried it; the *per-object authority* a single acceptor would need to serve several owners at once stays the owed refinement. Proven by use (`crates/server/tests/fleet.rs` `a_holder_durably_holds_the_owners_replicated_head`): after a head replicates in a two-node fleet, the holder reports it durably holds the owner's head (owner + value) via the new `Daemon::fleet_holder_head` — non-vacuous (the holder holds nothing until the record commit reaches it, and this is distinct from the owner's `fleet_head_placed` quorum view). **The takeover drive is now BUILT (2026-09-10, takeover slice 2):** on a death, the survivor's probe loop records the objects `sync_peer` reassigns to it (`ShardState.pending_takeovers`) and brings every held acceptor's authority into step with the routing view (`reconcile_held_authority` — `install_authority` to the successor, so a holder can answer the new owner's prepare and accept its re-commit); the record-ship task then **drives phase one** over the surviving candidate holder (`drive_takeover`: `promote_record` over the object's durable hold at a bumped epoch, `Promotion::adoption_record` re-committed under the new epoch via `commit_record`, the placement recorded in `placed_heads`); the serve loop dispatches a `Prepare` (fixed 40 bytes) vs a `Record` (longer) on the shared record socket, answering the promotion from the same hold (`serve_held_promotion`). Proven by use (`crates/server/tests/fleet.rs` `three_daemons_take_over_a_dead_owners_head`): a volume provisioned on the node that then dies is taken over by the survivor rendezvous ranks first, which serves the head region-placed **under its own ownership** (`fleet_head_placed`) with the value preserved — non-vacuous (a mere holder has no `placed_heads` entry; the seeded config never reassigns ownership), 8/8. **The takeover is now the general `f > 1` form (2026-09-10, `360582f`, corrected by the same-day review below):** the record plane is one **coordinator** task per node (`run_record_plane`) dispatching over **every** candidate holder's client session — kept up by per-peer link tasks (`establish_record_link`) in `ShardState::record_sessions` and borrowed per dispatch — replacing the former per-peer ship tasks; so a head is committed to **all** its candidates in one `commit_record` (the design's "records sent to all candidates at once", not N independent single-holder commits) and a takeover is promoted over **all** surviving candidate holders (`drive_takeover` borrows every holder session via `take_sessions`/`return_sessions`). This is what lets an `f > 1` promotion assemble its `f + 1` promise quorum over the several survivors one object needs — a per-peer ship task held only its own peer's session and could reach a one-holder (`f = 1`) quorum only. Proven by use (`crates/server/tests/fleet.rs` `five_daemons_take_over_a_dead_owners_head_over_a_multi_holder_quorum`, 8/8 + green in the full suite under load): a five-node `f = 2` fleet, a volume provisioned on the node that dies, the successor takes it over by promoting over a **three-promise** quorum (itself plus two other holders) and serves it region-placed under its own ownership with the value preserved — non-vacuous, and impossible for the old per-peer drive (one remote holder could never reach quorum three). The coordinator survives the owed connection-ID demux unchanged (only the socket count beneath it falls from O(N) to one). **Review of the coordinator (2026-09-10, same day; `docs/bugs/2026-09-10-swim-stale-ack.md` "Review addendum"):** reading the dispatch code rather than trusting the green 5-node run found five defects — three introduced by the consolidation (a straggler's late acknowledgement discarded on a non-quorum round, so it re-shipped forever at `f > 1`; an early quorum cancelling stragglers and dropping their sessions, which the per-peer-socket mesh cannot re-establish; one slow link's handshake blocking the whole record plane) and two pre-existing ones the sweep exposed (the verbs' `status`/`await placed(region)` never read the recorded acknowledgements — `Configuration::place` is the owner alone — so a fleet's head was reported **unplaced forever** at `f ≥ 1`, contrary to what this ledger claimed; and the record plane's owner acceptor was frozen at the boot generation, so after any join or retirement a newly provisioned head failed its own local hold `ForeignGeneration` and never placed). All five fixed and tested by use: `record_acks` merges every round's acknowledgements; the cluster dispatch no longer cancels stragglers but hands them back (`Stragglers`, on `Committed`/`Promoted`/`LedgerPromoted`; `crates/cluster/tests/extend.rs` proves a recovered session is live by placing a second commit only through it); per-peer link tasks keep the sessions in `ShardState::record_sessions` and the coordinator borrows them; `verbs::committed_placement` reads `placed_heads` (the two-node replicate test now also requires the owner's `await placed(region)` verb to answer `placed: true`); the coordinator re-installs the configuration authority each period (the three-node retire test now provisions on a survivor after the retirement and requires it to place). Also from the sweep: a peer whose serve sockets fail to bind/accept at boot was silently skipped — now counted as a `fleet.bind`/`fleet.accept` status refusal, tested. **Content replication and the takeover's content serve are now BUILT (2026-09-10, A-12):** each owned volume's newest snapshot is exported to the D-17 archive by a **resumable, budgeted walk** (`slates_vfs::export::SnapshotArchiver`, its slice derived from the profile's measured BLAKE3 throughput and the shard step budget — `DaemonConfig::archive_slice_bytes`; deterministic, restore-byte-identical, a base-backed entry refused rather than archived as zeros); the archive is put to the content candidates over the record session's own content streams by **missing set** (`slates_cluster::content`: `Offer`→`Missing`, `Put`→`Ack`, `Fetch`→`Have`, hostile-input tested), the first round to `f + 1` and later rounds hedged to the rest; a holder **verifies before it holds** (`ContentHold`: every chunk against its identity, the manifest against its hash, refused unless every referenced chunk is held — §4.10 "placement closure") and acknowledges bound to the object, sequence and manifest; only then does the head naming the manifest and the acknowledging holders ship (`HeadValue`, the head register's value — carrying the catalog essentials too, the split into a distinct catalog register class owed), and the snapshot is recorded placed durably (`SnapshotIdentified` + `SnapshotPlaced`), which `status` and `await placed(snapshot, region)` answer from. After a takeover the successor **serves the content**: it materializes the volume under its original id and name from the archive it holds, or fetches it by identity from a recorded holder (`verbs::materialize_taken_over`). Proven by use over real loopback UDP + mutual TLS + the daemon's real NFS port (`crates/server/tests/fleet.rs`): `a_sealed_snapshots_content_replicates_to_the_holder_and_places` (a file written over NFS, sealed; `await placed(snapshot, region)` true at `f = 1`; the holder holds the manifest whole) and `a_takeover_successor_serves_the_dead_owners_content_over_nfs` (three nodes; the file written on the owner reads back byte for byte over the successor's NFS port after the owner dies). Building it found and fixed a pre-existing reader bug: a volume's **first** snapshot has the id the catalog uses for "no snapshot" (slab slot 0, generation 0 → `SnapshotId { value: 0 }`), so `status`/`await placed` reported the creation head's placement for every first snapshot (`docs/bugs/2026-09-10-first-snapshot-id-is-the-none-sentinel.md`; the discriminator is now the volume's epoch). **The record plane now serves every owner shard (same day; D-7 "one owning shard per volume"):** the control shard alone holds the peer sessions and probes, so it hands each peer state it folds to every other shard's `FleetNode` (all copies of the configuration advance identically — `cluster::fleet::apply_peer_state`) and reaches every owner shard each period through the new `server::xshard` cross-shard call (a typed, deadline-bounded generalization of the spawn-and-spawn-back the verbs and the NFS bridge already use): the seal walk and the head values run on the owner shard, the archives and heads move to the coordinator by value, and the acknowledgements and durable placements are recorded back there; a taken-over volume is materialized on the shard its id routes to (`verbs::owner_of`), carrying the takeover's `PlacedHead` — sequence, **promotion epoch** and holders — so the successor's next seals of the object are written at the epoch the holders fenced it at (a head at the successor's lower host epoch would be refused `StaleEpoch`; proven by `reseal_places` in the takeover test). Runtime shard ids are process-global and never reused, so a partition index is not a shard id: every partition-addressed send maps through the daemon's shard list (the verbs' dispatch and the NFS bridge already did; the fleet's materialization and observers now do). Proven by use with two-shard daemons: `a_volume_on_a_non_control_shard_replicates_its_content_and_places` and `a_takeover_successor_serves_a_volume_on_a_non_control_shard` (the volume placed on the non-control shard by its name's routing, asserted). Owed in §4.10: content-defined chunking and the compress-or-not cost model (D-17; chunks are raw at the CoW chunk size), the hedge trigger from a measured p95 (the round deadline is the trigger), anti-entropy and the healer, erasure coding, remote attach and prefetch, live shipping, migration and mirroring. **The N-node probe was also made robust (2026-09-10, `docs/bugs/2026-09-10-swim-stale-ack.md`):** two defects that flaked the two-node retirement/formation (no gossip redundancy to mask them at N=2) are fixed — (1) the SWIM probe now carries a per-probe **nonce** the acknowledgement must echo, so a stale acknowledgement the reliable transport redelivered on the reused probe stream (a dead peer's buffered ack) no longer passes for a fresh one and keeps the peer looking alive; (2) `probe_once` now drives the request/reply **inline** and **returns the session whatever the outcome**, so a single missed probe (a lost packet, scheduling jitter, a nonce-rejected reply) no longer drops the unrecoverable session and retires a *live* peer — the session is re-probed, a still-live peer refutes the suspicion (SWIM incarnation refutation), and only sustained silence ages a peer to death across the window. Regressed by `crates/cluster/tests/swim.rs` `a_stale_nonce_acknowledgement_is_rejected...`; the two-node test, ~3% flaky before / ~8% with the nonce alone, is 90/90 after both. **The three-node takeover is now reliable under load (2026-09-10, 30/30 full-suite under load, was ~8% flaky):** three more defects closed on top of the probe fix (all in `docs/bugs/2026-09-10-swim-stale-ack.md`) — (a) the **record dispatch** dropped a straggler's session on a commit/promotion timeout (the collection loop cancelled it), which the per-peer-socket mesh cannot re-establish; each holder request now rides `request_within`, a deadline-bounded exchange that returns the endpoint **whatever the outcome**, so a load-timed-out commit or promotion retries over the same warm session (`CommitBudget::max_deadline_ns` bounds it); (b) the **ship session** now drives its handshake on **one persistent socket** (`establish_session`, since also shared by the probe plane — see below), retried each period so the peer's pinned `accept` completes rather than a fresh-port re-dial being ignored; and — the actual root — (c) a head was shipped **only until `f + 1` quorum, not to every candidate**: `unplaced_heads` gated on the object's overall placement, so once one per-peer ship task placed a head every other skipped it and a co-survivor never received it (failing the takeover's "both survivors hold the head" precondition and starving its promotion quorum). The gate is now **per holder** (`unplaced_heads` returns each head with the candidates that have not acked, and the placement's acked set is **merged, not overwritten**), so a head reaches all candidates — the design's "records are sent to all candidates; committed at `f + 1`." The fleet tests also `yield_now()` between poll checks instead of `spin_loop()`, yielding cores to the daemons rather than starving them. **The probe plane now shares the record plane's persistent-socket establishment** (2026-09-10): `probe_peer` previously dialed with a single `establish()` attempt (`dial`) and returned on failure, stranding a probe task for a peer slow to come up (a real scale-up-join gap); it now uses `client_for` + `establish_session` (one handshake attempt per period on the same socket, retried until it completes), the detector ticking only when a probe is actually sent, and `dial` is removed (fully replaced). **Final validation (2026-09-10): the whole fleet scales up and down flawlessly** — the full six-test fleet suite serialized as real `cargo test` (`--test-threads=1`, one process) ran **25/25 green under heavy load** (15 runs under 8 CPU spinners, then 10 oversubscribed with 24 on an 18-core box), `three_daemons_form_a_full_mesh` **60/60 sequential**, on top of the two-node retire/formation 90/90 and the three-node takeover 30/30 under load. A ~2.5 % `three_daemons_form_a_full_mesh` failure seen only when the test binary is run as **several concurrent OS processes** is a **test-harness artifact**, not a fleet defect: separate processes reuse released OS ephemeral ports and reset the process-local `unique()` host-id counter, so their fleet sockets collide and an `accept` pins the wrong source — conditions that cannot arise in real operation (distinct addresses) or the real suite (fleet tests serialize; one process). Documented in the bug doc, deliberately left as-is (a parallel-process-safe harness would need the daemon to accept pre-bound sockets — test-infra scope beyond the fleet). **Consensus/takeover reliability under CPU starvation and across the full in-process suite (2026-09-12):** two fixes closed the residual flake. (1) The fleet consensus ran on the hard `f=0` budget — `CommitBudget::with_extension` (the §4.8 late-work progress extension) had **zero callers** — so under CPU starvation a round whose replies arrived late-but-progressing was declared uncertain and re-dispatched every period, a false-timeout storm (`docs/bugs/2026-09-12-fleet-consensus-hard-budget-under-load.md`, `3aa6b85`). (2) `broadcast` then waited for **every** child, so a dead-but-not-yet-retired voter's extended ~1.1 s deadline gated each consensus round (measured: 39 rounds × ~1100 ms in one run) — a slow-retirement vicious cycle; now **progress-aware** via `DispatchWait` (return at all-reported or stall, fold-and-re-ship; `docs/bugs/2026-09-12-broadcast-waits-out-dead-voter.md`, `68f3d1a`). A leaked-in-process-socket hypothesis was disproved by direct experiment first. The suite — ~1 flake in 7 on an **idle** machine before, always the largest 5-daemon consensus/takeover test (17/17 in isolation) — is **13/13 green and ~30 % faster** (~93 s vs ~120–140 s) after. Built at the transport since: received-packet-number dedup (`4dd4e6e`-follow-up — RFC 9000 §12.3, the deeper cause of the redelivered ack; `Connection::handle_incoming` discards a packet number already processed, `AckGenerator::is_duplicate` recognizing one still tracked or below the ACK-of-ACK-confirmed floor, since a sender's numbers only increase; the SWIM nonce is now belt-and-suspenders) and the re-establishable session (the connection-ID demux replaces a re-dialing peer's session). Owed beyond this: reconnection after a mid-run session loss (a link task re-establishes on a fresh socket, but the peer's pinned accept side rebuilding is owed with the transport's other reconnection work); the loom/shuttle concurrency pass. **Connection-ID demux BUILT (A-14, 2026-09-10):** every 1-RTT short header carries an 8-byte connection id both ends derive from the TLS exporter (no wire negotiation; `Endpoint::connection_id`); `crates/transport/src/demux.rs` owns one socket per plane and routes raw handshake datagrams by source and 1-RTT packets by id to per-session bounded inboxes, opens a server session per new dialer for an `accept()` consumer, closes a peer's old session when it re-dials under the same certificate (reconnection after a mid-run loss, counted `replaced`), and counts unknown-id/overflow drops; the fleet loop binds two sockets per node (probe, record) instead of `2(N−1)`, spawns a serve task per accepted session (the record side resolves the peer from its authenticated certificate), and closes a retired peer's sessions; the manifest's port block is now `base`/`base+1`. Proven in `crates/transport/tests/session.rs` (two clients on one socket each get their own reply; a stray packet naming no session is dropped and counted while the live session serves on; a peer that re-dials replaces its old session, whose serve loop ends `Closed`), the twelve-test fleet suite, and the three-process CLI deployment test — **12/12 under eight CPU spinners** after three latent session-plane defects the extra hop exposed were fixed (`docs/bugs/2026-09-10-abandoned-request-retransmit-lockstep.md`: a forgotten stream's frames were still retransmitted and its late reply read by the next exchange — a one-behind lockstep that retired live peers; idle sessions spun on the estimated PTO; no PTO backoff). The in-process content-placement tests then exposed two more, one a runtime-level fairness bug: a continuously busy shard never harvested driver I/O (a client that never idles re-queues its serve loop every step, so the loop never parks and the demux receive task starves), and an abandoned content exchange left its stream open on the reused session and was folded into the next request — both fixed (`docs/bugs/2026-09-10-busy-shard-never-harvests-io.md`, `docs/bugs/2026-09-10-abandoned-content-stream-poisons-reused-session.md`), the in-process fleet suite 12/12 and both content-placement tests 5/5. **Rejoin (A-15, `4dd4e6e`-follow-up):** a retired peer that returns is re-admitted by SWIM refutation — the probe serve side echoes this node's belief so the peer self-refutes and is folded back (scoped), and the probe/record loops idle-not-end on retirement so they resume on rejoin; no death tracker or bump (slates keeps the death incarnation in its membership and `refute` supplies the bump), authority stays the configuration group. Proven `a_falsely_retired_peer_rejoins_by_refutation` (5/5), fleet suite 13/13. The serve-socket counters are exported in `status`. **Multi-process deployment BUILT (A-13, 2026-09-10):** one shared manifest per fleet (`slates daemon|anchor --fleet PATH --node NAME`; JSON: the TLS name, `f`, and each node's name, advertised address and DER certificate/key paths); `crates/server/src/deploy.rs` derives every node's member id from its certificate (leading 8 bytes of BLAKE3) and its serve sockets from its advertised address (originally a block of `2N` ports; two since A-14), refusing by name a manifest that could never commit, repeats a name or certificate, overflows the port range, or carries an identity the TLS stack cannot use (checked at boot by building the server side once) (unit-tested pairwise: every dial address equals the other side's serve bind); `FleetMembership` carries the node's own member id (`host`); `slates status` reports `fleet_host`, `fleet_f`, `fleet_host_epoch`, `fleet_members`, `fleet_peers_probed` (text and `--json`, the MCP schema too). Proven by real processes (`crates/cli/tests/cli.rs`, `SLATES_TEST_CLI=1`): three `slates daemon --fleet` processes form an `f = 1` fleet (every process reports the same three certificate-derived members), a snapshot sealed on one places across processes, its peers do not serve the volume, the owner is `SIGKILL`ed and both survivors retire it, the successor serves the volume under its id, and where `mount_nfs` exists the payload written on the dead owner reads back through a kernel mount of the successor. **The distributed configuration council now runs live over the daemon transport (D-14, #3, 2026-09-11):** each node's `RegionalCouncil` (the region's configuration master — a multi-voter Raft producing the agreed `RegionalConfiguration`) is driven from the one record-plane coordinator (`server::fleet::drive_config_council`) over the same record sessions, its Raft riding a distinct `CONFIG_STREAM` (7) served by one arm in `serve_peer_records`: each period a leader heartbeats every voter (holding the term, carrying the commit index), a follower ages a jittered `[T, 2T)` election timeout (Raft §9.3, made determinism-clean) and on lapse campaigns through the full pre-vote then real vote (§9.6), and the sole voter self-elects (R8). Driving it from the coordinator makes its voter-session borrow **sequential** with the record ships — no contention and no third socket (the demux keys one session per peer per plane); the election/replication fan-out (`broadcast`) is the concurrent, session-preserving shape of `commit_record`, reusing the shared `request_within` (now `pub`), each child **joined once terminal** so a per-period round accumulates no task slots (banned item 8). Proven live by three daemons whose councils **elect a single stable leader over the real transport** and whose leader's heartbeats keep the followers in contact (`three_daemons_elect_one_stable_council_leader_over_the_transport`: exactly one leader, holding across a stability window, a follower's leader-contact counter advancing — a non-vacuity counter), on top of the sans-io council (`config_group.rs`) and its sim-UDP proof (`config_group_live.rs`); full fleet suite 14/14, cluster lib 101/101, clippy/xtask/fmt clean. A sibling finding recorded on the way (**FIXED 2026-09-12**): the four dispatch drivers (`commit_record`, `promote_over_holders`, `promote_ledger_over_holders`, `content::dispatch`) spawned joinable straggler children under the perpetual `run_record_plane` and neither joined nor detached them, so each completed straggler's arena slot lingered until the never-ending parent finished — unbounded task-slot growth on the record path (banned item 8). Fixed by **detaching** every dispatch task (after the collection round in the three `lib.rs` drivers, whose `tasks` vector was already held for the spawn-failure path; at spawn in the `content.rs` helper, which keeps no such vector), so each slot is reaped on the task's termination. Detach does not cancel: the early-quorum and timed-out stragglers still complete and hand their sessions back over the channel `Stragglers` drains, so recovered-session behaviour is unchanged (the `extend`/`commit`/`promote`/content tests stay green). Failing-test-first: `commit::a_commits_dispatch_task_slots_are_reaped_under_a_perpetual_parent` (an `f=1` commit whose owner then parks forever — `live_tasks` was 3, now 1). Bug doc `docs/bugs/2026-09-12-straggler-task-slot-leak.md`; the council/root fan-out `broadcast` already **joins** its children once terminal, so it was never affected. **The council also maintains regional membership now (2026-09-11):** the leader reconciles the `RegionalConfiguration`'s members from its own SWIM view each period (`RegionalCouncil::reconcile_alive`, gated on the log being **caught up** so a change in flight is not re-proposed — the near-zero commit rate the design makes a tripwire), proposing admits/retires that commit over the transport and apply on every voter; only the leader proposes (it probes every member, so a follower's own detection need not). Proven by use when a member dies (`a_council_commits_a_membership_retirement_over_the_transport`: three daemons, a *follower* killed so the leader keeps quorum, the leader detects the death via SWIM and the retirement **commits over the transport** so every survivor's regional membership drops it — the full propose→replicate→commit→apply path through the daemon's own sessions, not just an election) and by a sans-io reconcile unit test (admission + the non-leader and caught-up gates); full fleet suite 15/15, cluster lib 102/102, clippy/xtask/fmt clean. **The authority switchover is now BUILT (2026-09-11):** `FleetNode` no longer holds a per-node `ConfigGroup` — it holds the current `Configuration` **installed from the council** (`FleetNode::install_configuration`), so every `place`/`region_placed`/`await_placed`/`host_epoch` the verbs read comes from the configuration the council committed (D-14, one configuration group per region). `observe` now only advances the SWIM failure view (the council leader reconciles the region from it, `reconcile_alive`); the record-plane coordinator installs the council's committed configuration into the `FleetNode` each period (`sync_config_from_council`, version-gated so most periods are a no-op at the near-zero config commit rate), and a retirement it commits reaches the placement neighbourhood and hands the survivor the departed owner's objects to take over (`routing.take_over` over the new neighbourhood → `pending_takeovers`; the phase-one `drive_takeover` unchanged). Takeover triggers on a member **leaving the region** (a retirement/death — the members diff), not a neighbourhood re-ranking, so it stays correct once the scatter width exceeds the member count. Proven end to end: `a_council_commits_a_membership_retirement_over_the_transport` asserts a committed retirement drops the dead member from BOTH the council's regional membership AND the survivor's placement neighbourhood (`Daemon::placement_neighbourhood`) — the switchover is non-vacuous — and the full fleet suite (placement, retirement, 3- and 5-node takeover, content, rejoin) is 15/15 with the council as the sole configuration authority; cluster 103 lib + integration green; clippy/xtask/fmt clean. The cluster `FleetNode` tests were reworked to the council model (the membership→configuration reconcile they used to drive through `observe` is the council's now, proven in `config_group.rs`); the R8 differential (`the_owner_runtime_has_identical_semantics_at_n1_and_in_a_fleet`) now installs the council's configurations and degenerates the fleet to the laptop's, the solo council self-electing so the laptop path is the same code. Owed next: the explicit `ConfigCommand::TakeOver` per-host **epoch fence** (A-9 — a resumed zombie is currently fenced by the advanced configuration version, `ConfigurationStale`; the per-host `StaleEpoch` fence over the holders is the refinement); **a small voter set + non-voter learners are now BUILT (2026-09-11):** the council votes with a small set — the members up to the candidate floor `2f+1` by id (`daemon::council_voters`, deterministic so every node agrees) — and the rest are **learners** that do not vote. A learner fetches the committed regional configuration from a voter (`CONFIG_FETCH_STREAM`; the `RegionalConfiguration` wire codec `raft_wire::{encode,decode}_regional_configuration`, hostile-input tested) and adopts a newer one (`RegionalCouncil::{is_voter,adopt}`); the drive loop's learner branch (`drive_learner_fetch`) does this **reactively** — only when the learner has evidence its configuration is behind the region (§4.8 the piggyback rule): a `ConfigurationStale` refusal it received to a record it sent, a record it accepted naming a newer generation (both flag `config_refresh_wanted`), or its own SWIM view diverging from its installed membership (`membership_diverges_from_config` — the quiescent takeover successor's only cue, since it receives no records for the dead owner's objects); an idle learner whose view matches its configuration sends nothing. The fetch still carries the learner's version so even a triggered-but-caught-up fetch costs an eight-byte request and an **empty reply**, not a full transfer (the config-commit rate is near zero). The adopted configuration is installed into placement by `sync_config_from_council` exactly as a voter's committed one. Proven by use: `a_learner_fetches_the_councils_committed_configuration_over_the_transport` — a five-member `f=1` fleet (three voters, two learners); a learner never leads; and when a member's retirement is committed by the voters, the observed learner (which cast no vote) drops it from both its regional membership and its placement neighbourhood, learned only by fetching (the death is injected into the voters, as the rejoin test injects, since real SWIM detection under the five-node + learner-polling load is slow and orthogonal to what this proves). Full fleet suite 16/16, cluster 105 lib + integration green, clippy/xtask/fmt clean. The **reactive piggyback is now BUILT (2026-09-11):** the register acceptor splits its configuration-generation gate by direction — a record from an *older* configuration is refused `ConfigurationStale{version}` naming the holder's newer version (a record from a *newer* one stays `ForeignGeneration`, the holder-behind case), so the sender learns which way it is out of step — and that refusal rides back on the record wire (`register::{Refusal, encode_refusal, decode_refusal}`, a nine-byte tagged reply that `Ack::decode` can never mistake for its 72-byte ack; round-trip + hostile-input tested). `commit_record` surfaces the newest stale version as `Committed::stale_version` (over a `FnMut` collector so the future stays `Send`), and the record-plane coordinator flags a refresh from it (`ship_head`) and from a received newer-generation record (`accept_held_record`), then the learner branch fetches once when flagged or when its SWIM view diverges — replacing the per-period poll (banned item 7: replace, don't layer), so an idle learner sends nothing. Proven by use: `a_holder_on_a_newer_configuration_rides_its_version_back_and_the_owner_does_not_place` (cluster) surfaces the exact newer version and does not place; the learner test now learns reactively (its own injected SWIM death is the cue, the fetch the learning). `StaleEpoch`-carry-back on the record path is shadowed by this gate (a superseded owner is at an older generation, refused `ConfigurationStale` before the fence is reached), so the design's "the latter refusal ends the sender's authority" is realized through the version refresh installing the configuration that reassigns the object away. **The ROOT GROUP across regions is now BUILT (2026-09-11 — §4.8, D-14 "a root group across regions holds region membership and cross-region promotions"):** the cross-region counterpart of the `RegionalCouncil`. `slates_db::register::RootConfiguration` holds the regions, the moved-volume homes (`homes: ObjectId→RegionId`, moved volumes only) and the region promotions (`promotions: RegionId→RegionId`, a lost region → its mirror); `home_of(volume, creator_region)` resolves where a volume is served — its moved home else its creator region, then follows any region promotion to a fixed point (bounded by the promotion count, cycle-safe). `RootGroup` (`cluster::root_group`) is the multi-voter hecate-Raft producing it, mirroring `RegionalCouncil` exactly (`new`/`answer`/`fold_reply`/`propose`/`caught_up`/`adopt`; self-elects at one voter = the laptop's sole region, R8); its voters are the hosts that carry the root group, its committed configuration names regions. Log commands `RootCommand::{AdmitRegion,RetireRegion,PromoteRegion{lost,mirror},MoveHome{volume,to}}` (encode/decode round-tripped, malformed → no-op); wire codec `raft_wire::{encode,decode}_root_configuration` (bounded counts, a lying count refused — hostile-input tested). Proven by use: in-process — a region-membership change and a region promotion each commit at a majority and apply on both voters (`root_group.rs`); over sim UDP — `a_root_group_commits_a_region_promotion_across_the_transport` (`tests/root_group_live.rs`: a pre-vote election then a cross-region promotion committed and applied at leader and voter). db 31 register / cluster 103 lib + integration (incl. root_group_live) green; clippy -D warnings, xtask, fmt clean. **The cross-region DAEMON DRIVE is now BUILT (2026-09-11):** the daemon holds the root group on the control shard (`ShardState::root`, built at boot from the fleet's regions — the members' distinct regions, one representative host per region as the voters, via `daemon::{region_of,fleet_regions,root_voters,build_root_group}`; `FleetMembership::regions` maps each host to its region, absent = the sole region 0) and drives it over the transport from `run_record_plane` (`drive_root_group` — the cross-region parallel of `drive_config_council`: the root leader reconciles the region membership from its own alive view mapped to regions (`RootGroup::reconcile_regions`, `alive_regions`), replicates, and a follower elects; `drive_root_replication`/`drive_root_election` on `ROOT_STREAM`=9, served by `serve_root`). Observers `Daemon::{root_leads,root_regions}`. Proven by use over the daemon transport: `the_root_group_commits_a_region_retirement_over_the_transport` (three daemons, each its own region and root voter, elect one root leader; a follower's region is lost on a kill and the surviving leader commits its retirement — every survivor's committed root region membership drops it). Full fleet suite 17/17, db 31 register, cluster 104 lib green; clippy/xtask/fmt clean. The default (no declared regions) is a single region, so the root group is the degenerate self-leading group (R8) and the existing single-region tests are undisturbed. The operator per-node **region declaration** in the manifest is now BUILT (2026-09-11), mirroring the failure-domain declaration: an optional `"region"` integer per node in the fleet JSON manifest (`cli::fleet` `NodeText`, refused typed if non-integer and named by path) → `FleetNodeEntry::region: Option<RegionId>` → `deploy::plan` builds `FleetMembership::regions` (keyed by the certificate-derived member id, a node absent = the sole region 0), which `init_shard` feeds the root group. Proven by use: `a_declared_region_parses_and_a_bad_one_is_named` (cli parse + typed refusal) and `the_plan_carries_declared_regions_to_the_membership` (deploy — the declared region reaches the membership under its member id); documented in `docs/cli.md`. So a real multi-region fleet is now declared entirely from the shared manifest. **Root learners are now BUILT (2026-09-11):** a region member that is not its region's representative does not vote in the root group; it fetches the committed root configuration from a root voter over `ROOT_FETCH_STREAM`=10 (`serve_root_fetch` returns the config only when newer than the learner's version — a caught-up fetch is an empty reply) and adopts the newest (`drive_root_learner_fetch`, `RootGroup::adopt`, `raft_wire::{encode,decode}_root_configuration`), **reactively** — only when its own alive view of the regions diverges from the root configuration it holds (`root_diverges`), so a converged learner fetches nothing (the cross-region parallel of the config learner). Proven by use: `a_root_learner_fetches_the_committed_region_membership_over_the_transport` (four daemons, three regions — region 0 has two hosts so one is a pure learner; a single-host region is lost, the surviving root leader commits its retirement, and the learner — a non-voter — drops the region from its committed root membership only by fetching; the victim's death is injected for a deterministic SWIM cue as the council learner test does). Fleet suite 18/18. (This rests on the all-to-all mesh where a node's SWIM sees every region's hosts; a region-scoped mesh would need a version-carrying signal instead — owed with region-scoped SWIM.) **Region-loss promotion is now BUILT (2026-09-12), operator-initiated for split-brain safety.** Each region's mirror is declared fleet-level in the manifest (`mirrors: { region: mirror }` → `FleetManifest`/`FleetMembership::region_mirrors`; `cli::fleet::mirrors_text`, typed-refused/path-named). The reconcile is now mirror-aware: a lost region **with a mirror** is **not** auto-retired — auto-failing-over a merely-partitioned region would promote its mirror while it is still serving (a second owner, split-brain), so it stays in the membership awaiting a deliberate operator promotion; a **mirror-less** lost region has no failover target, so its confirmed loss is a clean auto-retire as before (`RootGroup::reconcile_regions` gained a `mirrors` gate). The operator promotes on the root leader — `Daemon::promote_region(lost)` looks up the mirror and proposes `PromoteRegion{lost, mirror}`, which commits over the transport; every node then re-homes the lost region's volumes to the mirror (`RootConfiguration::home_of`, which follows promotions). Proven by use: `root_group::a_lost_mirrored_region_is_not_auto_retired` (the mirror gate), `an_operator_promotes_a_lost_regions_mirror_over_the_transport` (server — a mirrored lost region is not auto-retired, then an operator promotion commits over the transport and every survivor re-homes to the mirror via `Daemon::region_home`), `cli::fleet::declared_region_mirrors_parse_and_a_bad_one_is_named`; fleet suite 19/19. **The `home_of` lookup guard is now BUILT (2026-09-12, cross-region routing slice 1 — §4.8 "Lookup"):** a request for a volume homed in another region — moved there, or failed over there by a region-loss promotion — is refused `Refusal::HomedElsewhere{region}` naming the home region so the caller re-routes to it (the refusal-driven redirect the design's lookup uses, the data-plane counterpart of the configuration-version piggyback). `verbs::home_redirect` is the pure decision (a single-region fleet short-circuits before any map lookup, so local and laptop deployments pay nothing; else the volume's `ObjectId` is homed via `RootConfiguration::home_of` from its creator region, and a home other than this node's region is the redirect), `homed_elsewhere` its wrapper reading the shard's committed root configuration and node→region map, and the guard sits in `serve` before owner routing. `Refusal::HomedElsewhere{region}` is a forward-compatible IPC wire variant. Proven by use: `verbs::tests::{a_cross_region_volume_is_redirected_to_its_home_region, a_promoted_regions_volume_redirects_to_its_mirror}` (the redirect names the moved/promoted home; a same-region and a single-region volume are not redirected). Fleet suite 19/19 (the single-region fast path leaves every existing test unchanged). **OWED:** cross-region routing slice 2 — the caller (or a forwarding node) actually following `HomedElsewhere` to the home region's daemon over the cross-region data transport (a real subsystem, not a wire-in); and a CLI verb over `Daemon::promote_region` — now BUILT (2026-09-12): `slates promote-region REGION` sends `RequestBody::PromoteRegion`, which `verbs::serve` routes to the control shard (`promote_region_on_root`) to propose on the root group (§4.8, D-14, §4.12 — "the operator issues it (a CLI verb over this)"); the `Client::promote_region` method, the `Refusal::NotRootLeader` and `Unsupported` (no declared mirror) refusals, and the parse. **Forward-to-leader is now BUILT (2026-09-12), so the operator may issue `promote-region` on ANY node, not only the root leader:** a follower forwards the operator's `PromoteRegion` to the leader it knows over the fleet transport (`FORWARD_STREAM`) and relays the reply. Foundation `d17edb0`: `RaftNode` tracks a leader-hint (`RootGroup::leader`) — a redirection hint only, never a safety input (a stale hint costs a retry; the target proposes only if it is in fact the leader; `PromoteRegion` is idempotent). The forward is a general node-to-node primitive: `fleet::forward_over_leader_session` borrows the peer's record session (`take_sessions`/`return_sessions`, so it does not corrupt the coordinator's use — whichever misses retries) and rides `slates_ipc::protocol::{encode_body,decode_body}` (a `RequestBody` request → `ReplyBody` reply); `verbs::serve_forward` runs the forwarded verb on the peer. Proven by use: `a_client_on_a_follower_promotes_a_region_by_forwarding_to_the_leader` (a client on a **follower** promotes a region; the follower forwards to the leader and every node re-homes — verified non-vacuous: forwarding disabled, a follower cannot promote). Only the operator command is forwarded today; general **volume-verb** forwarding (a request reaching a remotely-homed volume's owner, relaying the requester's principal) reuses this primitive. **Cross-region READ forwarding is now BUILT (2026-09-12, slice 2 first form):** the `serve()` `homed_elsewhere` guard, for a forwardable read (`Status`/`Versions`/`ChangedSince`) of a volume whose owner is its creator, **forwards** it to the creator over `FORWARD_STREAM` instead of refusing; the owner runs it on its owner shard under the relayed principal (`ForwardedRequest { principal, body }`; `verbs::serve_forward` async, served through `Endpoint::serve_once_async`, `xshard` to the owner shard — no completion record, reads are idempotent), and the reply relays back. **Cross-region WRITE forwarding is now BUILT too (2026-09-12, task #29):** the same guard forwards a forwardable **write** (any volume-scoped mutation) to the owner, which runs it through `run_forwarded` — a completion keyed by `(origin, client, sequence)` where `origin` is the **mutual-TLS-authenticated** forwarding peer (unforgeable, and globally unique where per-node client ids are not), so a retried forward is **exactly-once**. `ForwardedRequest` now relays the origin request word and the client's acknowledgement watermark; the owner prunes the forwarded client's completions up to that watermark (`prune_forwarded`), the same ack-based bound a local client gets, so they do not grow unbounded (banned item 8). The completion key was widened with the origin host across the DB (`CompletionRecord`/`ClientCompletions`/`Op::CompletionsAcknowledged`, RAM-only format), a local client's origin being this node's own host (one path, R8). Tests: `a_client_writes_a_cross_region_volume_by_forwarding_to_its_owner` (a retry returns the same snapshot id — exactly-once; `HomedElsewhere` without the fix) and `completions_from_distinct_origins_do_not_collide`. Boundary **now CLOSED by task #22** (`cefb159`/`bd3a5f5`): the member id is ephemeral per boot (`deploy::member_id(anchor, generation)`, the generation from the anchor's `SUP_GENERATION`), so a restart is a **join under a new id** — the design's §4.8 "Recovery" — and its old objects are taken over by neighbours (rendezvous, #30). The RIFL completion origin keys on the **stable cert-anchor** (`host_id_of_certificate`, in `ShardState::origin_anchor`), NOT the ephemeral member id, so a retry meets its completion record **across a daemon restart** (proven: the single-node restart test passes under the split — the member id changes, the origin does not). Fleet peers learn a restarted node's new id from its authenticated SWIM `from` (`serve_peer_probes`), retire the old (`probe_and_apply` mismatch → ages out), and `reconcile_alive` admits the new member + takes over the old (`a_restarted_peer_rejoins_under_a_new_member_id_and_the_old_is_retired`). RAMCloud's `(index, generation)`: the cert-anchor is the stable index for auth + idempotency, the generation ephemeral for membership/ownership. **This required fixing a foundational bug the build exposed:** `fresh_volume_id` put the owner **partition** in the id's high bytes (a Phase-8 placeholder), not the **creator host** — so the id named no creator host, and slice-1's guard misrouted a real volume (a non-region-0 node redirected even its own volumes; `region_of(creator)` defaulted to region 0). Fixed: the high 8 bytes now carry the creator host (fleet member id, `ObjectId::creator`), the partition moves to bytes 8-9 (`owner_of`), the low 6 bytes a per-host counter (bug doc `docs/bugs/2026-09-12-volume-id-lacks-creator-host.md`). Proven by use: `a_client_reads_a_cross_region_volume_by_forwarding_to_its_owner` (a client in region 1 reads a volume created in region 0; forwarded to its owner and served — failed before the id fix, passes after). **Both are now BUILT:** cross-region **write** forwarding (task #29), and moved/promoted-volume owner resolution (task #30, 2026-09-12) — the `homed_elsewhere` guard no longer refuses `HomedElsewhere` for a volume whose owner is no longer its creator; it resolves the volume's **current** owner in the home region (`verbs::current_owner_in_region` → the pure `owner_in_region`) and forwards read or write to it. The current owner is the creator while the creator is alive **and** still lives in the home region; otherwise it is the survivor rendezvous ranks first among the home region's alive hosts — the same successor a takeover assigns — so the forward reaches the node that now holds the shard after a home move or a region-loss promotion to the mirror, not a dead or moved-away creator. An empty home-region view falls back to the creator, so the forward degrades to `HomedElsewhere` and the caller re-routes rather than dropping the request (a self-forward finds no session to itself and degrades the same way — `forward_over_leader_session` returns `None`). Proven by use: `verbs::tests::{owner_in_region_is_the_creator_until_it_fails_then_its_takeover_successor, owner_in_region_of_a_moved_volume_is_a_home_region_host_not_the_live_creator}` cover the owner-selection decision for every case — live creator, takeover successor, moved/promoted with a still-alive creator in its old region, and the empty-view fallback — order-independent and non-vacuous (removing the elected successor hands over to the next); the forward *mechanism* to a resolved `HostId` is proven end-to-end by `a_client_writes_a_cross_region_volume_by_forwarding_to_its_owner`, which lands on the creator-is-owner branch over the identical `forward_over_leader_session(owner, …)` path a successor takes. Proven by use: `a_client_promotes_a_regions_mirror_over_the_promote_verb` (a client connected to the root leader promotes a region and every node re-homes to the mirror — the client → IPC → serve → control-shard → root-group path) and `cli::args::the_grammar_parses_promote_region`; documented in `docs/cli.md`. **The root configuration now reaches every shard (2026-09-12, `sync_root_to_shards`):** the lookup guard runs on whatever shard a client lands on, so a configuration installed on the record-plane (control) shard alone left a multi-shard daemon's other shards reading a stale home (a single-shard/laptop daemon has only the control shard, so it was unaffected — which is why slice 1's tests passed). Found building the guard, fixed failing-test-first: `a_committed_promotion_reaches_every_shards_lookup_view` (three daemons, two shards each; a promotion the control shard re-homes *and* the non-control shard does too — verified non-vacuous with the fan-out removed, only the non-control assertion then failing). The fix fans the control shard's committed root configuration to every other shard each period, each **adopting** it (`RootGroup::adopt`, version-gated, so a re-fanned unchanged configuration is a no-op at the receiver; the same shape a root learner adopts over the wire, here a same-process copy over the existing `xshard` `run_on`). The fan (both the placement and the root configuration, in one cross-shard message per shard — `fan_configs_to_shards`) is re-attempted **every period**, not only on change: `run_on` is refused when a shard's control channel is momentarily full, and a fan dropped on the one period a configuration changed would leave that shard stale until the next change, so re-fanning heals it next period (the idempotent-retry discipline `fold_peer_state` uses for the SWIM view; the configurations are bounded, so the per-period cost is bounded at every scale). Bug doc `docs/bugs/2026-09-12-root-config-not-propagated-to-non-control-shards.md`. **The placement configuration now reaches every shard too (2026-09-12, `fan_configuration_to_shards`):** the sibling of the root-config fan-out — the council's committed placement configuration was likewise installed on the control shard only, so a non-control **owner** shard read a stale `Configuration` after a membership change (`place`/`region_placed`/`await_placed`/`host_epoch`; partially masked because `committed_placement` reads `placed_heads`, recorded per owner shard). `sync_config_from_council` now returns the committed configuration and members, and fans them to every other shard, each installing them **for reads only** (version-gated, idempotent). It is *not* entangled with takeover after all: peer records — and so the held-record fences and the routing that drives takeover — live on the control shard, so a non-control shard tracks no held object and its `install_configuration` returns no reassignment to drive; takeover stays correctly centralized. Failing-test-first: `a_committed_retirement_reaches_every_shards_placement_view` (three daemons, two shards each; a committed retirement drops the dead member from the non-control shard's placement neighbourhood too — verified non-vacuous with the fan-out removed). The **ε-gated durability check is now BUILT (2026-09-11)** — the flagged follow-on of the copyset-count/loss check (`c`, task #25): the operator declares a fleet-level durability policy in the manifest, `durability: { accepted_loss, coincident_failures }` (`cli::fleet` parse — typed-refused and path-named when malformed, e.g. an accepted loss outside `[0,1]`) → `FleetManifest`/`FleetMembership::durability: Option<DurabilityBound>`, and every configuration change is checked against it at install (`daemon::record_durability` at boot and in `sync_config_from_council`); a breach is **surfaced** as the `DURABILITY_BREACHES` health signal, never a silent over-scatter (a breach is a recovery-vs-durability conflict for the operator to resolve — raise `f`, the re-replication bandwidth, or the failure-domain granularity — §4.8, D-14). `DurabilityBound::breached_by` reads the existing `Configuration::within_loss_bound`. Proven by use: `config::a_durability_bound_surfaces_a_breach` (a zero-loss policy is breached by any redundant configuration; an any-loss policy never is), `cli::fleet::a_declared_durability_policy_parses_and_a_bad_one_is_named`, `daemon::a_breaching_configuration_moves_the_durability_signal` (the non-vacuity counter moves on a breach — not without a policy, nor within the bound), `deploy` region/domain plan tests; documented in `docs/cli.md`. **The per-host epoch fence is now BUILT (2026-09-11, A-9 mechanism):** a SWIM-confirmed death is committed as a `Reconfiguration::TakeOver` (not a clean `Retire`), so `RegionalConfiguration::take_over` bumps the failed host's fencing epoch in the committed configuration ("Host failure increments the host epoch", §4.8 line 1730); when a holder installs that configuration (`server::fleet::sync_config_from_council`) it raises its per-object fence to the departed owner's committed epoch (`Acceptor::raise_fence`, monotonic) **before** re-owning the object to the successor, so a resumed stale owner's records under the old epoch are refused `StaleEpoch` at once across every object it owned — the design's "every holder raises its fence for that host to the new epoch" (line 1702). Additive on top of the existing configuration-generation fence (`ConfigurationStale`), and consistent with the takeover promotion's own epoch (`highest_held+1` = the committed bump), so it strengthens fencing without breaking the successor — proven by the 3- and 5-node takeover tests still green, plus unit tests (`register::a_raised_fence_refuses_a_record_under_the_old_epoch`, `config_group::the_leader_takes_over_a_failed_member_bumping_its_epoch`). Full fleet suite 16/16; cluster 106 lib + db 29 register green; clippy/xtask/fmt clean. **The FencedRegister TLA+ revalidation the design mandates for A-9 (proving TotalOrder/Continuity/StaleNeverCommits/ReadSafety on the corrected per-host model, §4.8 lines 1710-1712) remains owed** before the modeled safety result formally applies — the code mechanism is built, the formal revalidation is a separate architecture task (CLAUDE.md item 13). The single-owner per-node `ConfigGroup` and the per-object `ConfigCommand::TakeOver{object}` (the retired per-node forms the switchover replaced) are now **removed** (2026-09-11): `ConfigGroup`, its `TakeoverError`, and its eight unit tests are gone; `ConfigCommand::TakeOver` is per-host `{ dead }`; the `config_group` module doc, `routing.rs` and `register.rs` references point at `RegionalCouncil`. Cluster 98 lib + fleet suite 16/16 green after the removal. **Consensus voters outside the bounded record neighbourhood fixed (2026-09-13).** The council and root group ride the per-peer record sessions, but the link tasks kept a session only to the owner's copyset (`select_neighbourhood` at the scatter width — at the candidate floor the owner plus `2f` holders) and idled every other peer as retired; a consensus voter outside a node's copyset was unreachable from it by construction. An election still won through whichever voters were in-copyset (self + 1 of 3), so it hid until the in-copyset voter was the one killed: the root leader then ran **0 replication rounds in 1516 periods** — `take_sessions` over its voters empty every period, its live follower never once appended to and campaigning forever, the learner diverging every period with nothing to fetch. Fixed by `keeps_direct_contact_with` (`server/src/fleet.rs`): the dial set is the neighbourhood **or a council/root voter** — everything `take_sessions` is ever asked for; `a_root_learner_fetches_the_committed_region_membership_over_the_transport` converges in 5.37 s under 12 busy-spin processes (learner learned 0.3 s after the kill) where it capped before. Four hypotheses were measured and rejected first, recorded in the bug doc: an RTT-derived election timeout (SWIM RTT p99 17 ms, consensus broadcast p99 33 ms — inert on one host, owed for a WAN only, the 10× heartbeat ratio kept); two Raft Figure 2 follower timer-resets (fixed as siblings, unit-tested, 4 tests); and a real `broadcast` defect misattributed as the cause — it dropped its stragglers' replies and endpoints at its progress-aware stop, orphaning a late voter's session for good — now fixed as the record plane's shape (`broadcast` hands back `Stragglers`; every consensus/fetch round is a `Dispatch` with a `LateReplies` policy that folds a late `AppendReply`/`VoteReply`, standard Raft). Also fixed: the daemon's test-facing `observe`/`observe_peer_dead` swallowed `ControlFull` under load (`spawn_admitted` retries; the injection now reports and the test asserts it landed). Follow-up, same day: `check_quorum` is now **driven** on the election-timeout cadence (the leader's `idle` ticks it every `ELECTION_HEARTBEATS` periods; a leader that heard from no majority steps down — by-use unit tests in both groups, no false step-down in the suite at rest or under load), and SWIM now **probes consensus voters directly** (`keeps_direct_contact_with`: one predicate for the record link, the probe's resume and retire-on-fold, and the formation gate — the first cut left `fold_peer_state` on the neighbourhood alone, so the probe tore down an out-of-copyset voter's record session every period; caught by a 1-of-3 cap under load and fixed before commit). Third commit, same day: every test-facing observation accessor is `Option<T>` (`None` = could not observe: stopping, no shard, or unresponsive for the observe budget) and the fleet tests treat `None` as never satisfying a predicate — a poll keeps waiting, a hold fails rather than passes, "no longer contains" holds only over an observed membership; `promote_region`/`fleet_head_manifest` ride `observe` and `observe_peer_restart` rides `spawn_admitted` and reports landing. **Fixed the same day (2026-09-13):** the flap measured on a box spike to load 41 — SWIM declaring a *live*, CPU-starved voter dead and the root leader retiring/re-admitting its region in a loop (config `version 0→4`, `regions 3→2→3→2→3`, no victim killed) — had four causes, all closed: the probe deadline is now derived ("detection timeout for membership from RTT p99 × k": the transport's RFC 9002 probe timeout over the peer's measured probe round trips, floored at the heartbeat as the scheduler quantum, doubled per consecutive miss, capped at the liveness budget — `ProbeTiming`); a suspected member stays in the probe rotation (SWIM §4.2; it had dropped out, freezing the Lifeguard window at four periods and discarding its acknowledgements); every ping to a suspected peer carries the suspicion (Lifeguard's buddy system, `Detector::ping_gossip`); and a stale acknowledgement re-sends the probe within its deadline instead of counting as a miss. By use: a peer whose control shard is held busy for 3 s (`Daemon::starve_control_shard`) is kept where it was retired at ~1.2 s (`a_starved_but_live_peer_is_not_retired`); a truly dead peer is declared after six backed-off misses (≈ 4 s at rest) instead of 1.2 s. The gated three-process deployment test, run for this, also corrected the same day's contact predicate: a dead consensus voter stayed probed and linked (Raft's voter set does not shrink on a committed retirement), so `keeps_direct_contact_with` now excludes a peer the membership holds dead. Record: `docs/bugs/2026-09-13-swim-fixed-probe-deadline-kills-a-starved-live-peer.md`. The ε-gated durability refusal is wired (2026-09-13): the operator's `durability` policy (`accepted_loss` ε, `coincident_failures` F) is measured against every installed configuration (`DurabilityBound::shortfall`, boot/council/fan), kept on each shard, and a write that would commit a new head or seal (create, clone, snapshot, submit) is refused `DurabilityUnmet { coincident_loss, accepted_loss, coincident_failures }` while the configuration's loss is above ε — reads, destroys and status continue, `status` counts it, the laptop is never short. By use: `a_write_the_declared_durability_cannot_cover_is_refused_typed`, `a_fleet_refuses_a_write_its_configuration_cannot_hold_to_the_declared_durability` (+ the within-policy contrast). Record: `docs/bugs/2026-09-13-durability-refusal.md`. Sibling fixed on the way: `acknowledge` pruned completions under the ephemeral id (`docs/bugs/2026-09-13-acknowledge-prunes-under-the-ephemeral-id.md`). Task #22 learn-on-contact landed (`ca0577a`): the daemon generation rides the SWIM ping/ack and is validated against the certificate's anchor (`classify_announced`/`learn_member`: forged and stale announcements refused and counted; a higher generation is a restart — old id folded dead and taken over, new admitted and **probed**, region carried over); wire-level proof `a_restarted_peer_is_learned_on_contact_under_its_new_generation` (before: mesh never formed to the restart, 34 s FAIL; after: 5 s). Record: `docs/wip/ephemeral-id.md`. Raft membership change DONE (2026-09-13): the council's voter set follows the committed membership (`council_voters`, lowest ids to `2f+1`) and the root group's the representatives of the committed regions, moved by the leader through the core's joint change one at a time (`reconcile_voters`); a retired voter leaves every majority, the next member is promoted, a removed leader steps down at `C_new`, a non-voter never campaigns — proven sans-io, by conformance, and live over the transport with a dead voter. Found on the way and fixed: a holder's acceptor born stale by a refused first record never placed a head provisioned just after a retirement (`docs/bugs/2026-09-13-holder-acceptor-born-stale-never-placed.md`) — the signature the probe-cadence rejection rested on; re-measure it. Task #22 learn-on-contact is complete (`ca0577a`, `7c847bf`): the generation rides the SWIM wire and is validated, a restart is a new member whose old id is taken over, its admission carries the node's declared failure domain (`Admit { host, domain }` in the council log; retire prunes the map), stale and forged announcements are counted and unanswered (`a_stale_or_forged_announcement_is_refused_and_counted`, 8.9 s), the RIFL completion origin is the rostered anchor on every plane, and with the Raft membership change merged the voter set follows a restarted voter's new id too. The rest of §4.10 content replication landed (`35a0ac1`, `162de28`, `6ee4887`): the hedge to the remaining candidates fires on the **measured p95** put latency (`PutLatency`; a round's collection stops at the hedge delay, a slow holder's late acknowledgement is folded — a starved first-round candidate is masked in ~350 ms, not 3 s); **anti-entropy and the healer** re-offer one placed snapshot per healer period at a cadence derived from the measured put-failure rate (`heal_period_ns`), the `Offer → Missing` exchange being the Merkle diff — a holder that lost content is re-put exactly what it lacks (13.5 s; before: never noticed); and the **D-17 cost model** is a pure decision over the profile's measured codec points (`slates_archive::codec`), applied per chunk by the archiver, at neutral storing a copy read once raw (measured ~50× cheaper to move than to compress). Owed: probation (a council action), the archive-class `value_of_byte` and the live pressure/load signals, FastCDC (gated on the walk's measurements, now recorded), the regression's online update. Record: `docs/wip/content-replication.md`. Integrating it found two defects, fixed: the hedge widened on the count of rounds that *placed*, not on the clock, so a first round whose only holder's session was unavailable re-aimed at it for the whole hold (3.2 s against 3 s, 1 in 3 singly) — now `hedge_targets` on the clock, 10/10; and a `ProgressWitness` was born "advancing", granting an extension to a round with zero acknowledgements at its lookahead — now `None` until the first real advance (`docs/bugs/2026-09-13-hedge-keyed-on-placed-round-count-never-widens.md`). The Lifeguard probe-cadence dilation is wired (`probe_period_ns = beat × health_multiplier`, capped 3×). **Correction, same day:** its first wiring was recorded as measured-and-rejected on a 495 s hang of `three_daemons_form_a_fleet_and_the_survivors_retire_a_dead_node` — that attribution was **wrong**: the hang was a holder acceptor created by a refused first record and pinned at a stale generation (`docs/bugs/2026-09-13-holder-acceptor-born-stale-never-placed.md`, found by the Raft membership work); the dilation was merely the only line in the diff. Re-measured on the fixed tree: the same test passes 3/3 at 11.3 s with the dilation and `a_starved_but_live_peer_is_not_retired` at 8.8 s. Still owed: A fresh stream id per exchange landed (RFC 9000 §2.1, `f7651dc`): the kind rides the low `STREAM_KIND_BITS`, a per-connection sequence above; a request behind an abandoned exchange's unacknowledged reply is served, not deduplicated, and its late reply is discarded below the floor (credit returned); the server yields an abandoned reply to a newer request; the SWIM re-send loop is removed and an answered probe no longer ages its suspect to death that period (`a_starved_but_live_peer_is_not_retired` 9.12 s). Several frames per packet landed (`ae3eab1`, oracle-tested under loss and reorder). Still owed: per-path MTU discovery (DPLPMTUD, RFC 8899) — built, measured and withdrawn (stall at 1350; the separated budget/cap design must gate congestion on the next frame), record `docs/bugs/2026-09-13-reused-stream-id-collides-behind-an-unacked-reply.md`; ingesting the first 1-RTT datagram during server confirmation — **fixed 2026-09-14 with the WAN timing** (below). **The RTT-derived election timeout is built and proven on a WAN profile (2026-09-14, `agent/wan-timeout`):** the council and root group derive their timing each period from one measured path estimate per peer (`slates_cluster::timing`: the transport's RFC 9002 estimator over the SWIM probe's and every consensus round's round trips; `base = ⌈10 × max(tail, heartbeat)/heartbeat⌉` periods, the span the same over the variation), the coordinator's round budget from the same tail (`round_budget`), and the daemon exposes them (`Daemon::council_timing`/`root_timing`); the fabric now models a path's latency (`SimDelay`), and the fabric proof (`crates/cluster/tests/wan_election.rs`) shows the daemon's previous rule never electing across a published 162 ms P50 pair (Japan East → East US: 56 campaigns, 0 leaders in 300 periods) because its one-period round budget expired every pre-vote round before a reply — the actual WAN-blocking defect — while the derived rule elects at 3.62 s and holds (base 22–25 periods on tails of 213–243 ms), replaces a killed leader in 6.37 s, and at a GEO-class profile begins no spurious campaign where the fixed timing begins 59 in 180 s; LAN histories are identical under both rules and a loopback fleet derives the floor from measured samples (`a_loopback_fleet_derives_its_election_timing_at_the_measured_floor`). Found and fixed on the way: the transport sampled round trips on the wall clock while its timers ran on the runtime clock, dropped a session's first 1-RTT datagram, and re-stamped the handshake seed on retransmit (`docs/bugs/2026-09-14-transport-rtt-sampled-on-the-wall-clock.md`). Owed: the KIND lane on a real path (RTT distribution, loss, handshake ceilings above ~330 ms one way); the membership lease (no mechanism exists to derive it for); record-commit and learner-fetch round trips feeding the path estimate. **The fleet suite is traced and load-classified on the WAN tree (2026-09-14):** 35/35 under one CPU burner per hardware thread (210.43 s, cool start) and 35/35 at normal load (204.15 s), `adm_refused=0` throughout; a per-poll daemon-time trace and a direct-read per-shard pulse (steps/waits/spawns/completions/refused-admissions/longest-step) distinguish a slow-but-progressing fleet from a wedge; two over-spec failures (~2.5–3.5× oversubscription) are each isolated by the alone-vs-in-suite discriminator (the same test passes alone under the same load in ~12 s), so they are accumulated-suite-state and bounded accept-side handshake waits, not the load regime and not the daemon's liveness at the acceptance load (`docs/wip/fleet-under-load.md`). **The accept side's bound, proven by use (2026-09-14):** a burst of one more re-dial than a peer's session slots against a daemon's record socket is refused typed past the slots (9 refused, 3 replaced, every dial establishing in turn), 301 client verbs run through it with none refused, and the serve tasks return to the pre-burst count (16 → 15: one per live session, never one per dial) while the fleet stays meshed (`a_peers_re_dial_burst_is_held_to_its_session_slots_and_never_refuses_a_client`, 7.50 s); the accept-side task budget the load record left open is now the derived fleet share above. **KIND lane (2026-09-14):** The fleet deploys on Kubernetes through the Helm chart `deploy/helm/slates` — a Parallel StatefulSet with no PersistentVolume (RAM only, R1), a headless Service giving each node its per-pod DNS name (resolved by the daemon at every dial), the fleet manifest as a ConfigMap rendered from the replica count with f = ⌊(replicas−1)/2⌋, one certificate Secret per pod, Guaranteed memory QoS, node anti-affinity as the failure domain, and readiness from `slates status`. The KIND lane proves it on real multi-node pods: the fleet forms (every pod probes both peers, one council leader with measured timing), a volume places at f + 1 across pods, and the owner's pod deleted (SIGKILL) is retired by the survivors while the first-ranked successor serves the volume (`docs/wip/kind-lane.md`). The task-budget defect the lane found (a fleet node's tasks outside `tasks_per_shard` left one pod of five unable to admit a client) is the fleet task share landed on main the same day (`5de244d`); its failing test is green on the merged tree. Owed: the five-replica scale and the netem timing numbers on the merged image; a whole-pod restart rejoining the mesh (the replacement forms no probe session to its peers — a session-formation failure below the membership layer); and a byte-level read-back through a mount inside a pod. **Roster-sized handshake flight fixed (2026-09-14):** the merged lane's 38-peer test exposed that a server's handshake flight grew with the roster it admits (rustls's certificate-authority hints: 694 bytes at one peer, 3,024 at 64, against a 2,048-byte receive buffer) and was truncated at the dialer, which faulted forever; the server now sends no hints (`RosterVerifier`), an oversize flight is refused typed at the sender (`FlightTooLarge`), and the datagram bound is one shared constant — a server admitting 64 peers handshakes like one admitting two (`docs/bugs/2026-09-14-servers-handshake-flight-grows-with-its-roster.md`). **KIND lane measured on main (2026-09-14 18:24–18:47, `cargo xtask kind all`, 19 min 55 s):** image 26.6 s, cluster 25.4 s, 3 replicas rolled out in 8.5 s, formation 0.2 s, placement at f + 1 in 1.1 s, the deleted owner retired by both survivors 7.0 s after `kubectl delete pod --force` and the successor serving 7.1 s after; **five replicas install and form in 7.5 s** (`f = 2`, every pod `peers_probed 4`, one leader — the step defect 5 cut); and the **netem timing numbers the WAN status owed**: under 80 ms ± 20 ms the election base derives to 20 periods on 195–200 ms measured tails, under 1 % loss to 24–25 on 232–245 ms, under a 350 ms one-way ceiling to 77–79 on 763–785 ms with the fleet still forming in 4.9 s — 18 samples over 183 s per profile, zero leader changes on all three (`docs/wip/kind-lane.md`). Still owed: a whole-pod restart rejoining (open: the replacement forms no probe session to its peers though all pods are published endpoints — a session-formation diagnosis, not the incarnation design tension first recorded; `docs/wip/kind-lane.md`) and an in-pod mount read-back. **Fixed (2026-09-17):** `a_whole_ram_replacement_joins_as_a_fresh_voter_and_commits_after_another_loss` flaked about one run in three on `b2f1ef7` because a survivor's discovery exchange to the dead incarnation was unbounded and held the record link that alone re-dials the replacement (271 replication attempts, no append; `docs/bugs/2026-09-16-discovery-await-strands-a-replacement-raft-voter.md`); discovery exchanges are now bounded by the measured round budget and invalidated on a peer change, a borrowed session returns only to the slot it left, and the history forces the interrupted phase (§4.8 status, 2026-09-17). **Observation delivery typed (2026-09-17):** every test- and operator-facing observation returns `Result<T, ObserveError>` naming the stage it reached (submission, admission, execution) and what ended it there, under one absolute budget with only capacity refusals retried; the fleet harness's waits take a verdict of each ask (observed, unavailable, terminal) and charge the wait per daemon from its own start (`crates/server/src/observe.rs`, `tests/observe.rs` — six histories; `docs/bugs/2026-09-17-observations-are-typed-to-their-stage.md`, §4.8 status). | GAP-A9-7; AC-8.18, AC-8.20 |
| Wire/distribution (4.9–4.10) | Framing/canonical bodies and protocol primitives built. **The session-plane transport (§4.10a) is now built end-to-end and runs over the real `rt` UDP driver** (`crates/transport`): the RFC 9000/9002-shaped QUIC dialect — TLS 1.3 handshake (`rustls::quic`, pinned certs), ordered multi-stream delivery, packet-number assignment, **multi-range ACKs** (RFC 9000 §19.3), reorder-threshold loss detection + retransmit + probe, **ACK-of-ACK** state bounding (§13.2.4), the **dual-level `MaxStreamData`+`MaxData` credit law**, and **NewReno congestion control** (RFC 9002 §7). `Endpoint` binds a `slates_rt::udp::UdpSocket` and drives handshake + streams + request/reply over the wire, proven end-to-end over loopback UDP (`tests/session.rs`) and by sans-io oracles over any loss + reorder (`connection.rs`). Only empirical *tuning* (initial window, CUBIC-vs-Reno, pacing, ECN, an RTT-derived probe timeout) and fleet-level pieces (connection IDs, MTU coalescing, multi-node placement/routing) remain — Phase 8. See `docs/wip/fleet-transport.md`. **2026-09-14: the handshake's pending flight now survives across `establish` calls** — a dialer that spent one retransmit budget against a peer not yet listening resends the flight on the next period's call instead of waiting a second budget in silence (the flight was a local of one call, so the socket could never establish again: the N-node mesh stalled forever once a peer was starved past one budget under load, seen once in the admission validation); after two budgets or any protocol fault the fleet dials afresh from a new port. Failing test first: `a_dialer_that_outwaited_an_absent_peer_completes_the_handshake_once_the_peer_listens` (`docs/bugs/2026-09-14-handshake-retry-forgets-its-flight.md`). **Handshake flights fragmented (2026-09-14):** a handshake flight larger than one path-floor datagram — a mutual-TLS server flight with a certificate chain — crosses as fragments the peer reassembles (`crates/transport/src/flight.rs`, RFC 9000 §19.6 in the owned dialect: one cumulative stream per direction, a flight placed by its stream start so a stale fragment of an earlier flight can never write into the next; deterministic, so a retransmit resends identical fragments; every receive-loop arm bounded), replacing the single-datagram flight that a receiver truncated and a router could drop. Proven by an oracle over 2,000 random flights and delivery orders with duplicates, hostile fragments dropped typed, and by use: a server with a wide certificate (a ~2.7 KiB flight) serves a dialer, its flight crossing as three fragments (`a_server_flight_larger_than_a_datagram_crosses_as_fragments`). The MTU item's coalescing (several frames in one datagram) stays owed. **Session admission (2026-09-16):** the demultiplexer's pool is shared — `SESSION_RESERVE_PER_PEER × fleet_peer_capacity` slots per plane, one derivation the transport and the task budget both read — with capacity exhaustion and a setup fault counted apart (`sessions_refused` / `setup_refused`, `high_water` against `capacity`), and exhaustion, release, replaced-slot retention and the setup category proven at the seam over the simulated fabric (`crates/transport/tests/session.rs`). **Authenticated per-peer fairness implemented 2026-09-17:** C pending handshakes are separate from two authenticated endpoints per certificate across C identities (3C total slots per plane). A full certificate or identity reservation is refused before replacing a live session; charges survive closure until owner drop. Three simulated transport histories and the live re-dial history prove isolation/reclamation. Anonymous flooding is bounded by its own reservation, with no identity fairness claim before TLS authenticates it ([report](../bugs/2026-09-17-authenticated-session-fairness.md)). | GAP-A9-8, GAP-A9-11; AC-7.7, AC-8.19–8.21 |
| Archive/compression (4.11) | Pure archive/raw-format and missing-set helpers; separate pre-existing archive edits outside A-9 review. | GAP-A9-8, GAP-A9-11; AC-7.7; compression/dedup rest remains planned |
| Agent surfaces (4.12) | Rust client and CLI subset; MCP in `slates-mcp`. The **CLI's read verbs now emit JSON** with a global `--json` switch — `status` (daemon), `status ID`/`volume stat` and `volume list` — reusing the MCP serializers (`slates_mcp::{status_json,summary_json,daemon_json}`, now public) so the CLI and MCP surfaces share **one** schema (§4.12 "consistent JSON", part of GAP-A9-10); the shared `status_json` gained the `nfs_port` field it had been missing. Proven by use in `crates/cli/tests/cli.rs` (`the_verbs_emit_json_with_the_json_flag`, gated `SLATES_TEST_CLI`): each verb's `--json` output is a JSON object/array carrying the volume's real fields, over a live daemon. A **failing** verb under `--json` now emits a structured error too — `{"error": {"kind", "message"}}` on stderr, same exit code — so a harness gets JSON on the error path as well as the success path (GAP-A9-10 "consistent JSON errors"). **`--json` now covers every client verb**, not the read verbs alone (Ada's steer, 2026-09-09: a human scripting the CLI wants every verb to speak JSON — `id=$(slates volume create x --bounded 8MiB --json | jq -r .id)`): the read verbs, the merge queries (versions/changed-since) and outcomes (submit/rebase), and the **volume-lifecycle verbs** — create/snapshot/clone (`{"id"}`, the one key across every creating verb), placed (`{"placed","mirror_age_ns"}`), attach (the MCP `attachment_json` schema), pin (`{"pinned"}`), rewitness (`{"paths"}`), grants and audit (arrays of objects), land (the MCP `slates.land.materialize` schema), and the outcome-only verbs resize/destroy/detach/destroy-snapshot (a uniform `{"ok":true}`). The lifecycle attach/land JSON reuses the MCP serializers (`slates_mcp::{attachment_json,outcome_json,landing_summary_json}`, now public) so the two surfaces stay one schema. The one exception is `base read`, which streams a file's raw bytes with or without `--json`. Each verb is extracted into an `emit_*` helper so `serve()` stays a branch-free dispatcher under the cognitive-complexity gate. Proven by the same live test (each lifecycle verb driven under `--json`, every id captured from its own JSON). Cursors (pagination) are the remaining `--json` gap. And the **MCP surface gained `slates.merge.declare`** — the namespace operations (unlink/rename/mkdir/rmdir/set_mode/symlink/link/set_xattr/remove_xattr) as one tool dispatching `WorkOp` by `op.kind`, so MCP's merge surface now matches the SDKs' (it had content `edit` but not the namespace dimension); proven in `crates/mcp/tests/mcp.rs`'s merge loop, where a work builds a directory tree with a rename, a mode change, a symlink, a hard link and an xattr, then submits cleanly over a real daemon and merge engine. The **Python SDK** now exists (`crates/sdk-python`, Phase 5, D-19): a PyO3 extension over the typed client, imported as `slates`, binding connect/create/snapshot/client_id/reconnects with typed refusals mapped to a `SlatesError` exception; tested by use (stdlib unittest — the module loads and a missing-daemon connect raises the typed refusal); maturin builds the cp39-abi3 wheel. The **TypeScript/Node SDK** now exists too (`crates/sdk-node`, D-19): a napi-rs addon over the same client (the `.node` Node loads), binding the same verbs with typed refusals mapped to a JS `Error` and every FFI integer range-checked; tested by use (Node's built-in `node:test` — the addon loads and a missing-daemon connect throws the typed refusal). napi's GC-owned binding `Rc` is D-8 exception 1, marked in place. **Both SDKs now bind the volume-management, merge and namespace verbs — `status`, `list`, `resize`, `destroy` (§4.4); the merge workflow `create_green`/`create_work`/`edit`/`submit`/`rebase`/`versions`/`changed_since`; and the namespace operations `unlink`/`rename`/`mkdir`/`rmdir`/`chmod`/`symlink`/`link`/`set_xattr`/`remove_xattr` (§4.16)** — alongside the earlier connect/create/snapshot. `submit` and `rebase` return a uniform outcome (`ok`, the new green `version`, and any `conflicts` windows); `edit` is a content splice whose bytes cross as Python `bytes`/a Node Buffer; the namespace verbs are ergonomic wrappers over the design's `declare(WorkOp)` (the `WorkOp` enum stays inside the SDK, never crossing the FFI). `status` returns the volume's placement, byte accounting, attachments, overlay drift and NFS port (the fields `slates status` prints); `list` returns the volumes as id/name/accounting/overlay records; `resize` and `destroy` change and reclaim a volume. The Python SDK returns dicts, the Node SDK `#[napi(object)]`s with napi's camelCase keys, every `u64` range-checked to a JS-safe integer. **Both daemon-spawn round-trip harnesses are built and proven in-sandbox**: `tests/test_sdk.py` and `tests/sdk.test.mjs` each spawn a real anchor+daemon, connect the SDK, and drive create → snapshot → status → list → resize → destroy → list-gone, then the merge loop create_green → create_work → edit → submit → versions → changed_since → rebase, asserting the outcome of each over a real daemon and merge engine (R5). Each spawns the anchor in its own process group and tears the whole group down on teardown, so the supervised daemon goes with the anchor at once (no daemon outlives the test); both still skip loudly where no `slates` binary is present, so the default run stays green off-box. **Both SDKs are now async-primary** (R6, D-19), each an async client peer to the sync one (the thin blocking facade), exposing every sync verb's async counterpart — the volume lifecycle (create/snapshot/status/list/resize/destroy), the whole merge workflow (create_green/create_work/edit/submit/versions/changed_since/rebase), and the namespace operations (unlink/rename/mkdir/rmdir/chmod/symlink/link/set_xattr/remove_xattr) — each an async verb a real event loop drives to completion by the completion fd's readiness — never blocking the loop, no `tokio` and no `pyo3-asyncio`/thread, resting on the `slates-client` async core (`begin`/`spin_reply`/`poll_reply`, one reader multiplexing every in-flight request by id). **Python** (`crates/sdk-python`): `AsyncClient`, a PyO3 pyclass, resolves each verb on the running `asyncio` loop via `loop.add_reader(completion_fd)`. **Node** (`crates/sdk-node`): `AsyncClient`, a JS wrapper (`async.mjs`) over the addon's low-level primitives, returns a Promise per verb resolved by the completion fd wrapped in a libuv-polled `net.Socket`; because `net.Socket` adopts and closes its fd, the addon hands it a **dup** it owns (`enable_async_completion_dup`, a safe dup of the bridge's owned read end), leaving the client's fd intact. Proven by use over a live daemon on this macOS host (`tests/test_sdk_async.py`, `tests/sdk_async.test.mjs`): each awaits the create → snapshot → status lifecycle and drives eight concurrent creates (`asyncio.gather` / `Promise.all`), each returning a distinct id — concurrent awaits multiplexed through the one reader. The async SDKs now have **full verb parity with the sync ones** — every sync verb has an async counterpart, `land` (§4.15) included (it resolves to the landing outcome or the grant-required dict, issuing no grant itself, R10) — proven by an async by-use suite that drives the sync suite's whole surface (lifecycle + merge loop + a namespace tree built and submitted, all awaited). Owed (both SDKs): only the Windows async *bindings* — the transport they rest on, the Windows completion socket, is now built (`completion.rs`'s `CompletionBridge` gained a Windows arm: a client-local loopback `TcpStream` pair whose thread parks on the named Event and nudges the socket on an armed reply, exposed through `ClientEnd`/`Client` as a `RawSocket`; D-10 "asyncio on Windows needs a socket"), lint-clean on the native Windows target and CI-tested by the `windows-latest` `ipc` `rings` lane (`the_completion_socket_becomes_readable_on_an_armed_reply`, `WSAPoll` from quiet to readable at the armed reply). And the **Node/Python async bindings now consume that socket on Windows too** (2026-09-09, the "alongside the SDK bindings that use it" half): the Node addon's `completionFd` returns the completion `SOCKET` as a JS-safe integer that the same `net.Socket({ fd })` reader in `async.mjs` adopts (Node/libuv accept a socket there, so one reader serves both platforms); the Python addon caches the handle as an `i64` and registers it with `loop.add_reader`, which on Windows needs a `SelectorEventLoop` (the Proactor default has no `add_reader` — the README shows the one-line `WindowsSelectorEventLoopPolicy` a caller sets). Both addons cross-lint clean for `x86_64-pc-windows-msvc` (a `pure-hash` feature forwards blake3's pure backend so the addon cross-checks from a host with no MSVC assembler; the CI napi/maturin build uses the SIMD C path), macOS/Linux unaffected. The addons' Windows *runtime* — `net.Socket` adopting a raw `SOCKET`, the selector loop polling it — is the CI/hardware-owed proof (the native Windows runner), the same completion-socket contract the `ipc` `rings` lane already exercises at the transport level. This rests on **slates-rt now building on Windows** (`e3a8ccd`): the IOCP driver gained real socket readiness through an AFD reactor (`afd.rs`, `\Device\Afd`/`IOCTL_AFD_POLL` — the wepoll/mio mechanism), and the datagram socket the QUIC fleet transport rides is cross-platform through a `netsys` seam (rustix on Unix, Winsock 2 on Windows); TCP stays macOS/Linux (the NFS mount server's — Windows mounts via WinFsp, and the fleet is QUIC-over-UDP). So the outward-facing publish is the only SDK piece left. **The packaging is now built** — usage READMEs (`crates/sdk-{python,node}/README.md`, the Python one referenced from `pyproject.toml`; both cover install + connect + the async and sync workflows); the **Python** wheel via maturin (`pyproject.toml`, name `slates`, dynamic version, asyncio classifier, repository url); and the **Node** package (`crates/sdk-node/package.json`, name `slates`, a napi addon with per-platform `optionalDependencies` — `slates-<triple>` for all nine release targets, the dirs generated by `napi create-npm-dirs`), a hand-written data-driven `index.js` loader (picks the local `slates.<triple>.node` or the platform package; napi's own generator is a 3.x CLI against a 2.x crate, so the loader and `index.d.ts` types are maintained by hand), an `index.mjs` ESM entry exporting `Client` + `AsyncClient`, and `async.mjs`'s `connect` made dual-mode (a string instance loads the addon itself; an explicit addon serves the sandbox tests). The loader and ESM entry are verified to load the addon and expose both clients here; the `.node` binaries are built per-platform in CI (gitignored). Only the **PyPI/npm publish itself remains** (outward-facing, Ada-authorized). | GAP-A9-10; AC-5.9–5.11 |
| SDK publishing (§2.4, 4.12) | **Built (2026-09-14, publish lane).** Tag-triggered `publish-python.yml` (cp39-abi3 wheels for manylinux and musllinux x86_64+aarch64, macOS x86_64+arm64, Windows x64; the sdist; `twine check --strict` before upload; PyPI trusted publishing from the `pypi` environment — a pending publisher, no token anywhere) and `publish-node.yml` (napi addons for all nine `napi.targets`, musl in `node:24-alpine`; every platform package must hold its binary or the publish is refused; npm trusted publishing from the `npm` environment after the one-time 0.0.0 stub publish per name that npm requires, npm/cli#8544, the stubs derived by `cargo xtask npm-reserve`), both behind the shared `version-guard.yml`. **One version:** `[workspace.package] version` is the only hand-edited one; the wheel derives it through maturin (`dynamic = ["version"]`, a static one refused); `cargo xtask version` (in `cargo xtask check` and the CI gates) refuses any drift in the npm copies — the main package, the nine platform packages, the nine `optionalDependencies` pins, the platform READMEs, and each platform package's `os`/`cpu`/`libc`/`main`/`files` against napi's rule for its triple — and `--write` derives them (pure audit under nine unit tests; the tree run refused 36 findings after the rename and `--write` cleared them). **Names:** the unscoped `slates` on npm is an unrelated package (`slates@1.0.0-rc.23`, found 2026-09-14), so the Node SDK is `@hyper-light/slates` + nine `@hyper-light/slates-<platform>` (§2.4 amended); PyPI `slates` was free. CI's `sdk` job installs the wheel into a fresh venv and the two npm tarballs into a fresh project and runs both suites plus the packaged smoke test (the loader resolving the addon through the optional-dependency package) over a live daemon on Linux and macOS, every push. Proven by use on this host 2026-09-14 (commands and numbers in `docs/publish.md`). Sibling defects fixed on the way: the main package's `exports` hid `package.json` from tooling (`ERR_PACKAGE_PATH_NOT_EXPORTED`); both SDK READMEs — the PyPI/npm long descriptions — still said the async form was owed; the Windows classifier was missing; the sdk-python manifest's lib comment named the wrong import name. | Owed: the ten stub publishes (a maintainer's login; `docs/publish.md`), the PyPI pending publisher and the npm trusted publishers, then the first tag. Tripwires: a tag whose version differs from the workspace's is refused by the guard; a platform package without its binary refuses the npm publish. Design gaps kept honest: §4.12's `abi3-py312` + `cp314t` wheels (PyO3 0.22 cannot build free-threaded; cp39-abi3 today); Windows arm64/ia32 wheels; the sdist carries no toolchain pin — maturin refuses `..` in `include`, so an older default rustc gets the typed MSRV refusal `requires rustc 1.98` (README states the requirement); 22 workspace crates lack `repository` metadata (one maturin sdist warning each). |
| Security (4.13) | OS credential and channel checks; enrolled consumer/human issuer boundary incomplete. **Consumer enrollment built (2026-09-13, `596bfb0` + `e0d6877`):** the grant issuer is a capability the daemon verifies (a per-start 256-bit issuer secret in the anchor segment, keyed BLAKE3 proofs with domain separation, constant-time compare; `slates grant`), and consumers are enrolled under a host account by the same authority — `Enroll` mints a consumer id and a capability shown once; `Attest` binds a channel to it by a proof keyed over that channel's client id, read from the partition the id names and verified pure (`verify_attestation`); `Revoke` records durably and marks every shard's bound slots **before** acknowledging, so every later verb refuses `ConsumerRevoked` at a local gate; `Share` sets `Rights { read, write, admin }` per principal. Two consumers under one uid hold only the rights shared with them (`distinct_consumers_under_one_uid_hold_only_the_rights_shared_with_them_until_revoked`, a two-shard daemon, ok 1.0 s; forged proofs refused and counted). Owed: the CLI verbs `enroll`/`revoke`/`share`, the fleet leg (consumer scope over the authenticated transport), MCP roots consulting the access list, and the capability-delivery channel (a decision: inherited descriptor recommended). Record: `docs/wip/enrollment.md`. | GAP-A9-9; AC-2.13, AC-5.10 |
| Observability (4.14) | Partial signals/counters; typed absence and causal context not established end to end. The shard **health signals are now a closed registry** (`HealthSignal` in `slates-ipc`, GAP-A9-12): the report is built by mapping `HealthSignal::ALL`, so an unregistered signal cannot be emitted and a registered one cannot be silently dropped — the "nine spans were called seven" miscount is now a compile error, and a doc-truth test (`the_registry_is_closed_and_its_names_are_unique`) pins the canonical names; the daemon still emits them (the CLI flow test asserts `catalog.volumes`). The **chokepoint-span roster is now closed** too (`Chokepoint` in `slates-wire::observe`, GAP-A9-12): a closed enum of the design's nine spans (§4.14 "Span roster"), pinned by a doc-truth test — so the "nine spans were called seven" drift is now a compile/test failure — with the distinct three-id types (`SpanContext` = `RequestId` (routes, deduplicates) + `TraceId` (128-bit) + `SpanId` + optional `CausedBy`, kept separate types so a trace field can never carry the authority a `RequestId` does, the A-9 correction that trace fields never authorize effects). The **span emission foundation is now built** (`slates-wire::observe`): a completed `Span` (the three-id `SpanContext` + a **content-free** bounded dimension code + monotonic start/end), a bounded **shed-first `SpanSink`** — a ring that keeps the most recent spans and **counts every shed span** explicitly (§4.14 "bounded rings report dropped spans"), never growing unbounded (ban 8) — and the `ChokepointRegistry` **health-plane gate** (§2.6): fail-closed, it opens only once every chokepoint in the roster has registered its emitter. The gate is **wired into the daemon boot** — `Daemon::start` builds the roster via `registered_chokepoints()` (each line naming the subsystem that owns the emitter) and refuses with a typed `ServerError::ChokepointsUnregistered { missing }` *before acquiring any resource* if the roster is incomplete, so a daemon never serves with a silently missing span source; proven by `the_daemon_declares_every_chokepoint_so_the_gate_opens` and by every live daemon test still serving (the gate opens in the real boot). By-use tests in `observe` pin the gate (eight of nine keeps it shut and names the one missing; nine opens it), the shed-first sink (five spans into a sink of three keeps the three most recent, the drop count exact — the non-vacuity witness a silently-lossless sink would fail), and the span's own duration (saturating a backwards clock to zero). The **emission path is now live for six chokepoints** (`shard.op`, `log.append`, `ring.request`, `merge.verdict`, `bridge.request`, `land.entry`): each shard owns a bounded `SpanSink` in its `ShardState` (per-shard, thread-local — no lock, R2; capacity = one client ring's depth, `config.region.slots`). `run_recorded` emits a `shard.op` span (the whole verb, content-free read/mutation label) and a `log.append` span (the durable `Db::commit` within it, partition label) around every verb; `serve_client`/`retry_deferred` emit a `ring.request` span (ring read → reply written) for a synchronously-served reply, threading the read time through `Deferred` so a reply deferred by a full ring is still timed; the submit handler emits a `merge.verdict` span (one increment judged), stamped with the request the shard is serving via a per-shard `current_request` context (set in `run_recorded`, so a fine-grained span deep in a verb needs no request id threaded through every handler); and `crate::nfs`'s `serve_local` emits a `bridge.request` span (one NFS bridge call from arrival to reply, procedure label), reaching the shard's clock and sink through `with_state` at the call's edges (outside `serve_call`'s per-operation borrows, so no re-entrancy) — a bridge call carries the default request id (it is not a RIFL-replayed verb). `land.entry` arrives through the **cross-crate span seam** now built: the land engine's `Observer` trait gained an `after_entry(start_ns, end_ns)` callback (primitives only, so `slates-land` stays wire-free), the engine calls it per entry, and the server implements it with a bounded shed-first collector (`SpanObserver` — a large landing never grows it unbounded, `SpanSink::record_dropped` folds its loss into the sink) drained into the shard's sink after the landing, once the landing's borrows release. This is the reusable seam pattern for the remaining lower-crate chokepoints; `SpanObserver`'s bound is unit-tested and the engine's `after_entry` call site is exercised by the land oracle (its own by-use apply test is the Linux `/dev/shm` lane). Each span carries the real `RequestId`, a per-shard span id, and a trace seeded from the request word until cross-boundary propagation is wired; emission is a sink push (no await, no lock) and a full sink sheds the oldest and counts it. The counts (`spans_held`, `spans_dropped`) ride `ShardReport` to `slates status` (text) as `shard N telemetry:`. Proven live by `telemetry_scenario` (`crates/server/tests/daemon.rs`): the held count **moves up** after running verbs — the non-vacuity witness that the registered emitters actually emit (a dead path would keep it at zero while the gate still passed). The emit is within the design's ~200 ns/span budget and a fraction of the 50 µs floor; the AC-2.1 ratchet gates it on a quiesced machine (unmeasurable under this session's load, where the bench is load-dominated at ~237 µs p99). The six live chokepoints are **every one active in the single-node daemon path**. Still owed, but gated on their subsystems being live rather than on the seam (which is built): `ship.record` and `consensus.step` — a laptop runs no replication or consensus at f=0 and `slates-cluster` is not a daemon dep, so they land with fleet integration (§4.8); `archive.chunk` — the archive is not wired into the daemon and its codec is Phase 7, so it lands with §4.10 — each then through the same cross-crate seam (instrumenting them before their subsystems run would be untestable code, R5). Also owed: `ring.request` for a **forwarded** reply (its origin span crosses the shard boundary — `read_ns` 0 marks it owed rather than timing it wrongly); the cross-shard aggregation of the per-shard sinks into the single control-shard sink (the `Control::Spawn` path the bridge queue uses); real cross-boundary trace propagation; and `(value, freshness)` on the daemon-level counters. **Typed absence on the shard health signals is now done** (A-9): `Signal.value` is `Option<u64>` with an `AbsenceIs` (`Unknown`/`Degraded` per signal, `HealthSignal::absence()`), so an absent sample is never conflated with a measured zero — a live `catalog.volumes: 0` renders `0`, a genuinely absent signal `absent/<meaning>`; the six current shard signals are all measurable (`Some`), the `None` path is for a future genuinely-absent signal (a mirror age at f=0, a non-reporting shard), pinned by `every_signal_types_its_absence` (ipc) and a `signal_value` render unit test (cli). **2026-09-13 (`e18619a`): the registries are the single source of their documentation** — `Chokepoint` and `HealthSignal` declare dimension, absence meaning, expected producer, observer and freshness horizon/basis and render the tables `docs/wip/observability.md` carries; doc-truth tests compare them byte for byte (`--ignored regenerate_*` rewrite) and parse the design's own roster sentence (count word, names, dimensions == the registry; "nine called seven" is now a failing assertion against the design text) and health catalog (drift list: `shard.clients`, `shard.deferred`). **Causation is enforced by type**: a `Span` exists only by ending an `OpenSpan` a shard `Tracer` opened — a root from its `RequestId`, a child from its cause (same request and trace, `Cause::Span(parent)`), or unlinked (`Cause::Missing`, the explicit missing link); trace ids are minted, never the request word. Proven by `telemetry_scenario`: one `Create` followed through `ring.request` (Root) → `shard.op` (caused by the ring span, on the other shard) → `log.append` (caused by `shard.op`, within it), one trace ≠ the request word. **The export is built**: the `Telemetry{partition}` verb drains one shard's ring bounded to what one 4096-byte chunk holds (derived at boot: 679 B fixed, 68 B per span, 50 spans), with typed loss markers (`shed_before`, `dropped_total`, `remaining`, `missing_links`) and per-chokepoint freshness against the failover-SLO horizon (typed absence, never a stale value); `slates status`, `status --json` and MCP `slates.status` drain every shard through one gather, and the JSON now carries every shard's signals and telemetry (it carried only a shard count). Overflow proven: 256 verbs on a 256-slot ring → `shed_before` 258, ≤ 50 per batch, 6 batches drain 266, `remaining` → 0. Still owed: cross-node trace propagation, the three gated emitters, a deadline on the status scatter, paging of the 4 KiB `DaemonReport`. | GAP-A9-12; AC-0.11 |
| Landing (4.15) | Engine, server records and Unix control/write integration; CLI issuance and trusted issuer incomplete. | GAP-A9-9–10; AC-5.10 |
| Merge (4.16) | Pure verdict, deriver and splice/engine components; the Green/Work **service** is built on them (2026-09-13, `agent/merge-service`): roles enforced at every verb with typed refusals (`ReadOnlyVolume`, `NotGreen`, `NotWork`, `UnknownBase`, `EvidenceRequired`, `ConsistentBaseUnavailable`), a green's chain from scratch or a complete immutable base (a host edit after the create changes no version), version-pinned attachments moved only by `advance`, the submission barrier with every input retained by the chain, merge records placed before referenced (§4.10 content exchange at `f + 1`, then the record in order per holder) and recomputed by every holder before acceptance (mismatch fatal-and-loud), and the CLI/MCP flow by use. Owed: the mounted work and green (VFS-backed volumes with the VFS journal as the one declaration path), the extent-backed chain. | GAP-A9-14; AC-6.13, AC-8.19 |

Research is indexed in [README.md](README.md); the canonical subsystem sections and original
acceptance criteria remain in [SLATES_DESIGN.md](SLATES_DESIGN.md). New acceptance rows supplement
them rather than renumbering or weakening the original gates.

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
- D-O15 (opened 2026-09-04) The landing fallback where the target filesystem lacks an atomic exchange (verify-then-rename-over with a reported window until 2026-09-29; since A-43 the old entry moves aside and the new one in by renames that replace nothing, the name's absent window reported) versus a staging-directory strategy — Phase 1 measures the window; Phase 4 measures per platform.
- D-O16 (opened 2026-09-04) Landing filters — DECIDED 2026-09-04: the confirmation surface offers suggestions the human toggles (ignore-file-aware); the agent's filter stays explicit; a toggle produces a new manifest hash (§4.15 step 2). Phase 5 builds it.
- D-O17 (opened 2026-09-04) Conflict rate from whole-file tool rewrites versus SDK `edit` operations: if the measured share of conflicts caused by whole-file rewrites on shared files exceeds the operator SLO, reopen the ergonomics (a declared-edit bridge path for editors; stronger skill guidance) — Phase 6 measures.
- D-O18 (opened 2026-09-04) Merge proposer authority: slates departs from hecate's leader-fused proposer (one shared pointer group; partitioned execution) and uses a consensus-issued lease with an epoch check at commit; CLOSED 2026-09-04 by A-6: the owner is the distinguished proposer of its own registers under its host epoch, so no leader-versus-leaseholder split can exist; a resumed stale owner is refused at the first holder (model-checked as StaleNeverCommits and Continuity).

## 3. Undesigned (charter only)

- Security spec — A-8 defined account credentials; A-9 adds enrolled consumers and protected human issuer authority (§4.13). Implementation remains open in GAP-A9-9.
- Observability spec — A-9 corrects trace identities and adds typed absence/freshness (§4.14). Implementation landed single-node 2026-09-13 (`e18619a`); cross-node propagation and the fleet/archive emitters remain open in GAP-A9-12.
- Skills content (the seven SKILL.md documents, including `slates-landing` and `slates-merge`) — owed in Phase 5 and Phase 6.
- The confirmation-surface contract for harnesses other than the terminal (the request stream a harness renders, answered only by a human-operated process through the control channel) — owed in Phase 5 with the terminal surface as the reference.
- Operator documentation (fleet configuration: failure-domain tree, regions and mirror regions, neighbourhood sizing inputs, certificates) — owed in Phase 8.

## 4. Drift (owed-and-forgotten)

A-9 corrects documentation drift: the remote delta-only clone, exclusion of virtio-fs,
quota-as-reservation language, implicit whole-tree snapshot claims, uid-as-consumer authority,
missing writeback barriers, overstated recovery and protocol proof, and CLI grant availability.
The corrected design is still ahead of implementation; every item remains open in §8i until
its observable regression and integration gate pass. Historical source/measurement records
are retained with their scope; no documentation edit is an implementation acceptance.

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
- Configuration commits growing with ordinary write traffic → fail the D-14 control-path invariant; D-O12 remains closed because there is no pointer group.
- FSKit spike fails its go criteria → NFSv3 remains primary on macOS and D-2 is reopened next macOS release.
- NFS fallback coherence test fails at the derived `actimeo` → reopen the fallback's cache posture.
- Hashing backlog persistent → reopen D-6 (hash-on-seal policy).
- Drift checks per second exceeding the measured `stat` capacity of a base (stat storms) → reopen the check cadence in §4.5 (hint-driven checks only, or a coarser listing fingerprint).
- Watcher overflow rate above the operator SLO on a base → reopen the watcher strategy (fanotify mount marks on Linux; the USN journal on Windows).
- Large-class copy-up cost or descriptor use beyond its derived budget → reopen the copy-up class boundary (D-6, §4.5).
- Landings taking the exchange fallback (no exchange; A-43's two renames that replace nothing) above a measured fraction → revisit D-O15 within the granted target; replacing its ungranted parent is forbidden.
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
- loom (2026-09-13): AC-0.7 is met for loom on the ring and handle cores and the runtime's parking
  protocol — five models explore 157 / 3,865 / 26 / 6 / 27 interleavings under `slates_mem::loom_bounds`
  (`RUSTFLAGS="--cfg loom" cargo test -p slates-mem -p slates-rt --lib --release loom -- --nocapture`,
  0.12 s after the build); loom found a lost foreign wake (a deadlock at its first interleaving) that a
  `SeqCst` fence on each side of the write-then-read closes; T-1.6 and T-6.7 have shuttle forms (200
  seeded schedules each; `docs/wip/concurrency.md`).
- Instruction counts (D-20): `benches/callgrind.rs` in `mem`, `rt` and `wire` under iai-callgrind
  0.16.1, run by CI's `callgrind` lane on Ubuntu with valgrind (authorized 2026-09-05); valgrind
  has no port for macOS on Apple silicon. The authorized Linux ARM64 container now runs all
  14 benches with successful-work and collection-boundary controls (2026-09-21, report above).
  Use `cargo bench --workspace --bench callgrind --features
  slates-mem/instruction-counts,slates-rt/instruction-counts,slates-wire/instruction-counts`.
  The feature isolates Valgrind's binding-generator dependency from ordinary tests and Miri.
  The lane still needs a saved comparison baseline and an enforced regression policy.
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
1.7, 1.8, 1.9 as named tests; T-1.6 in both forms: the one-shard generated interleaving
(`tests/clones.rs`) and the shuttle form over two agent threads and the store's owner
(`tests/shuttle_clones.rs`, 200 seeded schedules, 2026-09-13).

Task 14 (the deriver) landed 2026-09-05: the interval algebra as a pure module
(`crates/vfs/src/algebra.rs`: a content map of base and new runs, every declared operation a
splice, hunks as the unique minimal edit against the surviving base runs, 2,000 generated
histories against a byte-level reference applier, disjoint operations proven to commute); the
SDK `edit` on the volume (delete then insert with true positions, journaled as such; the tail
is rewritten, the zero-copy splice is Phase 6's); the deriver (`crates/vfs/src/derive.rs`: the
journal since the base snapshot folded into per-inode maps, the touched paths resolved in both
trees, a state delta over paths with base references, sorted, encoded little-endian, identified
by BLAKE3); a file inode's home (parent and name hash) so a written inode's path costs no walk;
`readdir_in`, `lookup_in`, `resolve_in`, `stat_in`, `readlink_in` reading a snapshot as it was
(`lookup_in` had resolved to the head's node: a latent bug, fixed). Gated: T-1.18 (300 random
histories over random bases: net-apply reproduces the head's files, symlinks and directories
byte for byte; every hunk inside its sources; deriving twice gives the same bytes), T-1.19
(truncate-and-write and write-and-rename give one hunk of the base length and the new length
and byte-identical documents), AC-1.15 (a fixed history's identity
`11476fb81b5bc32d474f28cd2afd2f65b1609c7dc070e1ade41f3c5df4d955c4` pinned for every lane). The
document keys content by post-state path with an explicit base reference rather than by inode,
so a rename over a base path and a rewrite in place read the same; hard-linked inodes are
listed at every path (conservative, as D-27 says).

Task 10 (the base plane) landed 2026-09-05: the read-only host seam (`crates/vfs/src/host`)
with opaque handles, bulk listings carrying fingerprints, `O_NOFOLLOW` opens and watcher hints;
`SimHost`, an in-memory host with outsider edits, a controllable clock and watcher overflow, the
disk leg of the (disk, overlay, witnesses) oracle; the volume's base plane (`crates/vfs/src/base.rs`):
merged lookups and listings validated by the directory's fingerprint on every use, base
entries given inodes on first touch and dropped when the disk loses them, copy-up by size class
with the racy rule, whiteouts and redirects journaled, drift checked first on what the held
descriptor serves (an in-place change marks the body lost and reads refuse with `BaseDrift`)
and then on the path (deleted, replaced, retyped: reported, still served from the held inode),
`read_base`, `rewitness`, `pin`, `status`, hints and overflow re-checks, the diverged set over
loaded nodes; and `slates-base` (`crates/base`), the operating-system host: descriptor-relative
rustix calls on Unix, inotify on Linux and `EVFILT_VNODE` on macOS behind the seam, and
handle-relative `NtCreateFile` opens on Windows (AUD-29-62, 2026-10-01; until then a path-relative
standard-library form). Gated: AC-1.9 (one open and one node at 10^3,
10^5 and 10^6 files, and over the workspace's own tree), AC-1.10 and T-1.10 (150 generated
histories of agent and outsider moves over random bases, the diverged set, the drift list and
every readable file compared after each step), AC-1.11, T-1.11, T-1.12 (40,000 entries), T-1.13;
the host's own tests over `crates/` and, in the Linux lane, over tmpfs (descriptor semantics,
`O_NOFOLLOW`, hints). Baselines in BENCHMARKS.md (Phase 1 baseline: the base plane).

Deviations and owed items from task 10:
- The Windows host reports no watcher (fingerprints alone, the failure matrix's Masked cell);
  `ReadDirectoryChangesW` is owed. Its timestamp granularity is the table's coarsest until the
  volume is queried through the handle. The directory-handle form is built (AUD-29-62,
  2026-10-01): retained handles, `NtCreateFile` relative opens with `FILE_OPEN_REPARSE_POINT`,
  the reparse check on the opened object, listings enumerated from the handle; run on the
  native Windows lane (`cargo test -p slates-base --test host`).
- Listings on macOS use `getdents` plus one `statat` per entry (3.4 µs per entry measured);
  `getattrlistbulk` is the design's bulk call for the platform and its gain is owed as a
  measurement before Phase 3's bridge, where listings sit on the `readdirplus` path.
- A large-class copy-up hashes the whole file for its witness identity (6.8 ms measured on a
  file of a few megabytes); D-6's tripwire on large-class copy-up cost stands, and a lazy
  identity (hashed at seal or landing) is the change it would trigger.
- `slates-base` carries two `unsafe` sites (rustix's `kevent`), budgeted.

Tasks 11–13 (the landing) landed 2026-09-05: `slates-land` (`crates/land`), the only crate
that links a write-capable syscall (the structural test's allow-list): the manifest with its
canonical encoding and BLAKE3 hash (`manifest.rs`: creates, replacements and deletes with their
witnessed base, directory renames as one rename, directory creates, recursive removals, a
`Clear` for a base directory the overlay removed and recreated opaque, symlinks; the filter;
the summary), the pure verdict of §4.15's table (`verdict.rs`, every row and every conflict
class in one table test), in-process grants and the single-holder lease (`grant.rs`; a session
grant covers later landings of the same volume into the same target), the online ramp policy
(`ramp.rs`), the state machine (`engine.rs`: present with a preliminary verdict pass, grant,
lease, capability probe inside the granted target, validate, sweep, write by class, sync,
advance, report, with an audit ring), and the write seam over the operating system (`os.rs`:
`O_TMPFILE` linked through `/proc/self/fd` on Linux and hidden-name temporaries elsewhere,
`renameat2(RENAME_EXCHANGE)` and `renameatx_np(RENAME_SWAP)`, `fdatasync` and
`F_BARRIERFSYNC`, `F_FULLFSYNC` as the media barrier, `futimens`, `fchmod`, containment by
`O_NOFOLLOW` per component with the ownership check). The seam gained the write verbs
(`LandFs`) and `SimHost` implements them with crash injection at every write instruction,
a switch for the exchange and one for unnamed temporaries, and a count of every seam call.

Gated (`crates/land/tests/oracle.rs`, over `SimHost`): the worked example of §4.15 with both
of its failures (a conflict refused with nothing written, then `read_base`, rewrite,
`rewitness`, a new manifest; a compare-and-swap lost to an outsider, exchanged back, `Undone`,
the report `Partial`, the outsider's bytes kept, the entry still in the overlay); AC-1.14 (the
sixteen entries take the same seam calls over 10^3 and 10^5 base entries); T-1.14 (eight
seeded runs of random outsider rewrites in both forms, every loss detected at the swap, none
silently applied); T-1.15 and AC-1.13 (a crash at every one of the writer's 96 write
instructions over a delta with every action class: every path old or new after each, the
resume with the same landing id sweeps the siblings, reaches the reference disk, and a
further plan is empty); T-1.16 (no exchange: verify-then-rename — since A-43, 2026-09-29, the old entry
moved aside and the new one in, the name's absent window measured —, the window in the outcome,
`NoExchange` reported, an outsider edit still refused at the verify); T-1.12 (the 40k-entry
directory: one `Clear` and two creates, exactly two entries after); a scratch landing into an
empty target (1,010 entries, the scratch volume an overlay after, reads then following the disk;
whole-target staging removed on 2026-09-19 because it wrote outside the grant); a populated target in place with `CreateCreate`;
grant mismatch, held lease, consumed and session grants, the audit log. Over a real
directory (`crates/land/tests/os.rs`, Linux lane on `/dev/shm`, loud skip elsewhere): the
worked example's shape on the disk, containment refusals, stable target identity, and T-1.15's real `kill -9`
(a child lands round after round until killed; every file is a whole round; the parent resumes
with the child's landing id and sweeps). Baselines in BENCHMARKS.md (Phase 1 baseline: the
landing).

Found by the landing oracle and fixed as rules (each with its test): a merged directory's link
count ignored its base subdirectories, so removing a base subtree bottom-up drove the parent's
count to zero one step early and its own `rmdir` refused `NotFound` (the count is now two plus
the subdirectories, overlay and base, at listing load); a cleared directory renamed aside then
recreated left the name absent between the two steps (now a fresh directory exchanged with the
old one, verified, the displaced tree removed); a resumed landing met its own fresh directory
and its finished rename and called them conflicts (the verdict now knows a directory holding
only what the manifest creates beneath it, and a rename whose destination holds the witnessed
directory); a crash inside the directory syncs reported `Done` (a failed sync now aborts, an
aborted landing advances nothing, and every entry's directory is synced on the resume); the
simulated host's `st_mode` lacked the type bits a real `stat` carries, so a removed directory
planned as a file delete.

Deviations and owed items from tasks 11–13:
- Entries run one at a time; the ramp records the depth it would have chosen (`ramp_depth` in
  the report). Concurrent entries arrive with the runtime's pool in Phase 2; the linked
  io_uring chains and arena-page writes with Phase 4's Linux bridge (bytes are read from the
  volume into a buffer and written through the seam until then).
- Whole-target stage-and-exchange is rejected (2026-09-19): the CI tracer observed four
  writes outside the granted target. The writer now receives no parent-directory authority.
  Per-entry exchange remains inside the target, whose identity is preserved. The scratch
  directory omission and the zero-write hermeticity false pass have dedicated regressions;
  validation is recorded in `docs/bugs/2026-09-19-linux-conformance-first-complete-run.md`.
- Reflinks are not used (`LandCapabilities.reflink` is probed as `false`); `FICLONE` and
  `clonefile` arrive with the manifest's own hash index of identical files.
- The directory sync strategy is one `fsync` per touched directory; the `syncfs` alternative
  waits on the measured per-directory cost the report now carries (`Durability.dir_sync_ns`).
- Windows has no OS writer yet (`FileRenameInfoEx` with `POSIX_SEMANTICS`, the sharing-mode
  verify): the Windows bridge of Phase 4; the crate compiles there without the `os` module.
- `TargetIsVolume` is not checked: there is no mount table before Phase 3.
- The real-target tests and the OS rows of the bench run in the Linux lane; on this machine
  they skip loudly (no RAM disk authorized), so their first numbers are the lane's.
- `slates-land` carries three `unsafe` sites (the macOS libc calls rustix does not wrap:
  `F_BARRIERFSYNC`, `F_FULLFSYNC`, `renameatx_np`), budgeted.

Open in Phase 1: none. AC-1.2's harness (`crates/vfs/tests/differential.rs`)
and policy (`docs/wip/EQUIVALENCE.md`) are written; it runs in the Linux lane against
`/dev/shm` (2,000 histories) and skips loudly elsewhere, since a macOS RAM disk is a
system-state change Ada has not authorized; its first run is the lane's, not a local one.

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

## 8d. Phase 2 record (2026-09-05)

Task 1 (the anchor) landed 2026-09-05: `slates_mem::SharedObject` (`crates/mem/src/shared.rs`),
the shared memory object of §4.7 created without a filesystem entry (`memfd_create`, `shm_open`
under the 31-character limit, a `Local\` section), handed to another process by an inherited
descriptor or a name, mapped whole, with atomic views of the words two processes touch; and
`slates-anchor` (`crates/anchor`): the segment layout (a header with the magic, version,
machine identity, generation and the persisted geometry; a supervision block of atomic words;
the profile, the per-partition log rings, two snapshot slots per partition, the audit ring and
the landing slots, every region page-aligned and every size a derivation the daemon passes in),
create and attach with the seqlock rule (a torn header or payload is refused, a foreign
identity is refused with both hashes, a wrong length is refused), publish and read of payloads,
and the supervisor (start with the handoff in the child's environment, non-blocking `step`,
restart on exit, a restart bound derived from the recovery budget and the measured daemon start
p99, the crash loop recorded in the segment as the health plane's `daemon.alive` input).
Gated: `crates/anchor/tests/anchor.rs` (create, attach through the handoff, the payloads, the
refusals; a real child process, the test binary re-invoked, attaches from its environment,
beats, exits, is restarted three times and refused the fourth, with every step visible in the
segment). Owed from task 1: the profile is published by the daemon's boot (task 2 wires
`MachineProfile` in); `slates anchor` as a CLI command arrives with task 5; held descriptors
(the FUSE fd, the NFS socket) with Phases 3 and 4.

Task 2 (the database) landed 2026-09-05: `slates-db` (`crates/db`): the records of §4.8 with
one canonical `Wire` encoding each (`catalog.rs`: volumes with policy, base path, head, epoch,
accounting, state, lease, owner and access list; snapshots with placement; lineage edges;
attachments; completion records; grants; landing leases; landing records; audit records);
the operations (`op.rs`, 24 kinds, every local mutation); the adaptive radix tree of the
indexes (`art.rs`: four node shapes growing and shrinking, path compression, values at inner
nodes so keys need not be prefix-free; model-tested against an ordered map over 400
histories); the partition (`partition.rs`) with the guard-then-apply split: `check` refuses
what the log must never record (a duplicate name, a missing record, a held or stale lease, a
stale completion) and `apply` is the unconditional transition both the live path and replay
run, so a recorded operation applies the same way forever; leases indexed by holder with the
runtime's timing wheel for expiry; the log record over the segment's ring (`record.rs`: a
32-byte header with magic, length, sequence, CRC32C over sequence, schema and body, the
schema hash; records wrap; the tail is released after the bytes; replay verifies each and
stops at the first that fails); recovery (`replay.rs`: the newest valid snapshot slot then the
records after its sequence, the torn tail cut and overwritten, the replay timed) and the
snapshot cadence derived from the recovery budget and the measured replay throughput
(`SnapshotPolicy`, bytes per microsecond), with a full ring snapshotting and retrying rather
than refusing.

Gated (`crates/db/tests/model.rs`): 60 generated histories of every operation kind with the
database dropped and recovered at random points and random snapshot cadences, the recovered
partition equal to the live one after every crash and refused operations never recorded
(AC-2.3's durability half); the torn tail (a byte flipped in the last record: cut off, the
sequence reused, the next mutation lands over it); four hostile record shapes (a length of
`u32::MAX`, a foreign magic, a bad checksum, a truncated header) refused with everything
earlier intact; lease fencing and expiry through the wheel with epoch + 1 for the next holder
and the wheel rebuilt by recovery (AC-2.4's core); completions exactly-once across recovery;
snapshots trimming the log and recovery restoring a snapshot plus its tail. Baselines in
BENCHMARKS.md (Phase 2 baseline: the database): recovery of 10^4 volumes from 10^6 records in
96 ms against the 1 s budget (AC-2.7), 206 ns per mutation, 95 ns per replayed record, the
tree at 53 ns per insert and 18 ns per lookup at 10^5 keys.

Found by the model test on its first run: a completion recorded at a sequence the client had
already acknowledged (a stale retry) was retained live but released when the state was
restored from a snapshot, so a recovery differed from the live partition. The guard now
refuses it (`StaleCompletion`) and the window itself drops such a record.

Owed from task 2: the register and held-record tables of §4.8 arrive with task 7 (f=0) and
Phase 8; the `put_wal` with Phase 8; the chains, deltas and last-changed index with Phase 6;
the recovery budget is a ratified default (GAPS §5) until the CLI takes the operator's value.

Task 3 (the IPC) landed 2026-09-05: `slates-ipc` (`crates/ipc`): the 64-byte slot (sequence
word, kind, length, request word, 40 payload bytes) and the single-producer single-consumer
ring of slots with per-slot sequences (`slot.rs`; a hostile kind or length is a typed refusal
and the slot is released, so a bad message never wedges the ring; an oversized payload is
refused before the ring is touched); the client region over a shared object (`region.rs`: a
header with the geometry the daemon derived, the wake word, the client's and the daemon's
parked flags, the doorbell, the command and completion rings, the bulk area); the wake word
per OS (`wake.rs`: a shared futex on Linux; `os_sync_wait_on_address(SHARED)` on macOS, two
`unsafe` sites budgeted; Windows waits on the named Event of Phase 4); the two ends
(`endpoint.rs`: the client spins for the published window, sets its parked flag, re-checks the
slot to close the race, and waits on the word; the daemon bumps the word per reply and wakes
only a parked client; the client rings the doorbell per request while the daemon's shard is
parked); and the rendezvous per OS (`rendezvous.rs`: Linux, an abstract-namespace socket named
from the uid and the instance, `SO_PEERCRED` refusing another uid and counting it, the region
descriptor and the completion eventfd sent with `SCM_RIGHTS`, the socket kept as the control
channel; macOS and Windows, a bootstrap object with claim slots taken by compare-and-swap, the
object's per-user name and mode as the authentication, the region's name and length written
into the slot and released to the waiting client). Discovery: `SLATES_ENDPOINT`, then
`default`.

Gated (`crates/ipc/tests/rings.rs`, two mappings of one region on two threads): the round trip
while spinning (no park, no wake), the late reply (one park, one wake), the ring's credit
(`RingFull`, nothing dropped, the order kept), the hostile slot (refused, released, the ring
flows), the deadline, the doorbell; (`crates/ipc/tests/rendezvous.rs`) the test binary
re-invoked as the client connects through the real rendezvous, receives its region, completes
a round trip and exits 0, and a client with no daemon is refused `DaemonUnavailable` quickly.
Both branches lint clean for Linux and Windows from this machine (the Linux tests run in the
CI lane). Baselines in BENCHMARKS.md (Phase 2 baseline: IPC): 278 ns per spinning round trip,
1.05 µs per parked-and-woken round trip.

The completion-fd mechanism is now built end to end and proven on macOS (`endpoint.rs`): the
daemon end nudges a completion fd on a reply to a *parked* client, under the same parked check
as the futex wake, so a spinning client pays for neither the wake nor the nudge; the client end
carries an owned completion fd it exposes as `completion_fd` (a raw fd an async SDK loop polls)
and `drain_completion` (a non-blocking read that clears its readiness), no `Arc` and no lock —
one owner per end, dup'd once at accept. `a_reply_to_a_parked_client_nudges_the_completion_fd`
drives it over a socketpair on this macOS host: the fd is quiet before the reply, readable after,
the reply waiting in the ring. The Linux rendezvous dups its `SCM_RIGHTS` eventfd into the daemon
end and hands the client its own (`rendezvous.rs`: `Accepted::completion_dup`,
`Connected::take_completion`); `daemon.rs` sets it on the daemon end at accept. The **macOS
completion fd is now live too**, by a different mechanism the design forces there (D-10 forbids
the Linux one on macOS — Mach messages and filesystem-named sockets are both refused, so no fd can
cross the `shm` rendezvous): `completion.rs`'s `CompletionBridge` is a client-owned thread that
parks on the very wake word the daemon already signals and, on an *armed* reply, writes a
client-local self-pipe the async SDK polls (§4.7 "signal the completion fd (eventfd / pipe /
socket)"). The daemon is unchanged (its macOS completion stays `None`) and the fast path is
untouched: a disarmed reply — taken during the spin, the client never parked — wakes neither the
word nor the pipe, so the async fast path adds no event-loop wakeup (§4.7 worked example). One
thread per async client, owned by the `ClientEnd` and joined on drop (the `slates-server`
`DoorbellThread` pattern; the stop flag a `Box::leak`'d `&'static AtomicBool`, no `Arc`, R2).
`ClientEnd::{enable_async_completion, arm_async, disarm_async}` are the uniform seam over both
mechanisms (Linux eventfd written by the daemon, macOS pipe written by the bridge);
`the_completion_bridge_signals_only_an_armed_reply_and_stops_clean` drives it on this macOS host —
the fd quiet for a disarmed reply, readable for an armed one, the thread joining clean on drop,
and the Linux branch lints clean cross-target. The **Rust async core the bindings drive is now
built** (`slates-client`, splitting the sync round trip so a host event loop drives the wait):
`Client::begin` sends without waiting and returns the request id; `spin_reply` is the fast path (a
reply taken within the daemon's spin window, no event loop — §4.7's worked example); `poll_reply`
takes a reply by id once the completion fd signals, buffering another request's reply so a reply
that arrives out of order (a deferred verb) or unawaited (an acknowledgement) never blocks
another's — the buffer bounded at twice the ring (item 8, no unbounded growth); `take_ready` drains
every ready reply for a pump that serves every in-flight request through one reader (a loop allows
one reader per fd); and `enable_async_completion` / `arm_async` / `disarm_async` /
`drain_completion` are the completion-fd seam. Proven by use over an in-process daemon
(`crates/client/tests/async_core.rs`): the create's reply taken by the spin fast path, the
snapshot's by the completion fd (armed before the send, so the daemon signals the fd exactly as for
a request parked on the loop), and two in-flight requests each routed to their own reply by id.
The **Python `asyncio` binding is now built on this core** (`crates/sdk-python`): `AsyncClient` (a
peer to the sync `Client`, which is the thin blocking facade) exposes each verb as an `async`
method a real event loop drives — the fast path returns a ready awaitable without touching the loop
(§4.7 worked example: it "returns the reply without ever touching the event loop"), the slow path
arms the completion signal, registers the fd with `loop.add_reader`, and resolves the awaiting
future when the fd fires; one reader per client serves every request in flight, replies matched to
requests by id, and `begin_ack_if_due` keeps the daemon's records bounded (§4.9). No external
runtime — no `tokio` (banned), no `pyo3-asyncio`: the loop is the user's, the readiness is the
completion fd's. Verbs bound async: create/snapshot/status (the rest follow the same three-line
shape). Proven by use on this macOS host (`crates/sdk-python/tests/test_sdk_async.py`, over a live
daemon): the awaited create → snapshot → status lifecycle resolves on a real `asyncio` loop, and
eight `asyncio.gather`-ed creates each return a distinct id — concurrent awaits multiplexed through
the one reader. The **Node `uv_poll` binding is now built too** (`crates/sdk-node`): `AsyncClient`
(a JS wrapper, `async.mjs`, over the same low-level primitives on the napi addon) returns a Promise
per verb, resolved by the completion fd wrapped in a `net.Socket` that libuv polls — the fd become
readable fires `'data'` and the pump resolves the awaiting Promise, never blocking the loop; the
socket is `unref`'d so it never holds the process open. Node's `net.Socket` *adopts and closes* the
fd it wraps (unlike `asyncio`, which only polls it), so the addon hands it a **dup** it owns
(`ClientEnd::enable_async_completion_dup`, a safe dup of the bridge's owned read end) — the client's
own fd is untouched, no double close. Proven by use on this macOS host
(`crates/sdk-node/tests/sdk_async.test.mjs`, over a live daemon): the awaited create → snapshot →
status lifecycle and eight `Promise.all`-ed creates, each a distinct id. On Linux the completion fd
the rendezvous passes is the shared eventfd, which Python's asyncio polls directly but Node's
`net.Socket` cannot adopt (`ERR_INVALID_FD_TYPE`: libuv classifies an eventfd as `UV_UNKNOWN_HANDLE`),
so `enable_async_completion_dup` on Linux starts a completion bridge that converts the eventfd's
readiness to a pollable self-pipe (`completion.rs`'s Linux arm) — the plain path keeps handing Python
the raw eventfd — closing the one platform where the Node async lane was dead, now proven over a live
daemon under io_uring (`docs/bugs/2026-09-16-node-async-sdk-cannot-poll-the-linux-completion-eventfd.md`).
The **full volume lifecycle
is now async in both SDKs** — create/snapshot/status/**list/resize/destroy** — proven by use to the
same shape as the sync suite (create → snapshot → status → list → resize → destroy → list-gone, then
the concurrent creates); the unit verbs (resize/destroy) carry a `bool` "done" sentinel through the
typed poll so the async pump tells "done" from "not yet" and resolves them to `None`/`undefined`.
The **merge workflow core is async in both SDKs too** — create_green/create_work/edit/submit — proven
by use (a green, a work, a content edit, a clean submit), so the async merge outcome (`{ok, version,
conflicts}`) and the tuple `{id, base}` both cross correctly; the sync verbs were refactored to share
the same shape builders (`submitted_to_py`/`submit_outcome`, `work_dict`/`work_volume`). The **whole
merge loop is now async** — versions/changed_since/rebase too (`extract_versions`/`changed`/`rebased`
+ `rebased_to_py`/`rebase_outcome`) — so both async by-use tests drive the sync suite's exact merge
loop (create_green → create_work → edit → submit → versions → changed_since → rebase, each awaited).
The **namespace operations are async too now** — unlink/rename/mkdir/rmdir/chmod/symlink/link/
set_xattr/remove_xattr, each declaring one `WorkOp` (built in the SDK, never crossing the FFI) and
resolving to `None`/`undefined` — so the **async SDKs have full verb parity with the sync ones**:
both async by-use tests drive the sync suite's whole surface (lifecycle + merge loop + a namespace
tree built and submitted). And **`land` is async too** now (§4.15) — resolving to the landing outcome
or the grant-required dict, the SDK still issuing no grant itself (R10) — so **every sync verb has an
async counterpart**: the async surface is complete. The **Windows cross-process wake is now built**
(`wake.rs`, `region.rs`, `endpoint.rs`): a named auto-reset [`Event`] per client, derived from the
region's object name (`{name}-wake`) so both ends `CreateEventW` the one Event with no handle
passing — `WaitOnAddress` on the wake word is process-local (D-10), so the endpoint's late-reply
wake signals and waits on the Event on Windows (`ClientRegion::wake_signal`/`wake_wait`), the wake
word still carrying the spin and the parked flag; the auto-reset semantics carry a signal made before
the wait, closing the park race the word's value re-check closes on Linux/macOS. It **lints clean on
the native Windows target and is now CI-tested**: the `windows-latest` job runs `slates-mem` and the
`ipc` `rings` test (the two-thread park/late-reply round trip — no daemon or rendezvous needed), so
the Event wake is exercised on a real Windows runner, not just linted. The **Windows completion
transport is now built too** — the socket the async loop the Event wake was the last piece of needs
(D-10 "asyncio on Windows needs a socket"): `completion.rs`'s `CompletionBridge` is now paired
`#[cfg]` (module gate `any(macos, windows)`), the macOS self-pipe arm byte-identical under its cfg
and a Windows arm beside it — a client-local **loopback `TcpStream` pair** (a Windows `SOCKET` that
libuv's `uv_poll` and a Python selector both poll), whose bridge thread parks on this same named
Event (`ClientRegion::wake_wait`, since `WaitOnAddress` on the word is process-local) and, on an
*armed* reply, writes the socket — the daemon's `reply()` already signals the Event on a parked
client, so the async fast path adds no wakeup exactly as on macOS. `ClientEnd`/`Client` gained the
Windows `completion_socket`/`enable_async_completion[_dup]`/`drain_completion` returning a
`RawSocket` (the `dup` a safe `try_clone` for Node's socket-owning consumer), the uniform seam over
all three mechanisms (Linux eventfd, macOS pipe, Windows socket). The bridge adds **no `unsafe`** (it
is all safe `std::net`). It **lints clean on the native Windows target and is now CI-tested**: the
same `windows-latest` `ipc` `rings` lane runs `the_completion_socket_becomes_readable_on_an_armed_reply`,
which drives a real region and a real reply and `WSAPoll`s the loopback socket from quiet to readable
exactly at the armed reply (the transport's whole contract), the reply then taken id-matched off the
ring. Still owed on Windows: the **SDK bindings that consume this socket** (the Node/Python async
methods on Windows — the transport they rest on is now here, so this is the "alongside" half Ada
sequenced) and the full Win32 rendezvous (Phase 4) that a client connects through; and separately the
runtime's async TCP/UDP (`slates-rt`'s `tcp.rs`/`udp.rs`) are still Unix-only, so the *daemon* on
Windows awaits the IOCP driver — a concern of the server, not this client-side completion transport. The Rust client parks on the word and needs
none. The Windows named Event per client (Phase
4, with the section-and-Event rendezvous compile-checked now); the doorbell thread that turns a
client's wake of a parked macOS shard into the driver's kick, and the heartbeat slot that
tells the daemon a client died where no socket closes, both with the server's integration in
task 4; the bulk region's use by streams (Phase 5); ring depth and spin window are the
daemon's derivation at rendezvous (task 4 wires the profile in).

Task 4 (the server) landed 2026-09-05: `slates-server` (`crates/server`): the daemon's
configuration as derivations from the profile (`config.rs`: clients per shard from Little's
law, the shard reserve, the table and store caps, ring slots and the spin window, the segment
geometry; every derivation logged with its inputs); the shard state in a thread-local cell on
its shard's thread (`state.rs`: volumes with their base host and reservation, the partition,
the store, the clients, the reserve, the deferred replies, the listings in flight); the verbs
of §4.4 (`verbs.rs`: create scratch and overlay with the bounded reservation or the host's
live memory as the dynamic quota's pressure source, snapshot, clone, attach with the lease
taken or renewed under D-16's epoch rule and a read intent needing none, detach releasing the
holder's last lease, resize moving the reservation, destroy in cooperative slices of half the
step budget, status with the drift list, list as a scatter-gather over every shard, base
verbs, acknowledgement, and the grant kind refused by channel and counted); the completion
record of every reply appended before it is sent (RIFL); the rights of §4.13 checked per verb;
a volume-bound verb from a client on another shard forwarded to the owner shard named by the
id's first bytes and the reply routed back (route by id, no index); the daemon (`daemon.rs`:
the segment created or attached from the anchor's environment, one mapping per shard, the
profile published, each shard's partition recovered and its state installed by a task on that
shard, the server loop as a poller of its clients' rings that idles otherwise and marks its
clients' regions parked, the control shard's rendezvous loop woken by the doorbell thread and
handing a client to its shard as a spawned task, the heartbeat at a tenth of the liveness
budget, stop joining everything); the doorbell thread (`doorbell.rs`: Linux waits on the
listening socket's readiness; macOS and Windows wait on the bootstrap object's word and kick
every shard; the value last acted on is what the wait compares against, so a ring during a
kick is never lost). The runtime gained pollers (`ShardContext::register_poller`: a task woken
by the loop whenever its ring says so) and `futures::idle` (yield without re-queue); the IPC a
shared protocol (`protocol.rs`: the bodies with one schema hash each, inline or through the
bulk chunk the slot's ring index owns) and the doorbell handed at rendezvous (the shard's kick
descriptor on Linux; the bootstrap word elsewhere); the volume core a `resize`.

Gated (`crates/server/tests/daemon.rs`, a two-shard daemon in the test process over a fresh
segment, one client at a time through the real rendezvous): the lifecycle (create, the
duplicate refused with the original's id, snapshot, clone, attach with epoch 1, status, list,
detach releasing the lease, resize, destroy completing in slices with the clone surviving);
exactly-once (a retry under the same id returns the retained reply without executing, an
acknowledgement releases it and a later retry is a stale duplicate); leases (a read attachment
takes none; the same principal renews with its epoch); AC-2.8 (the grant kind refused on the
ring and counted); a 200-byte name and an overlay over this crate's own source tree through
the bulk area, `read_base` reading the disk, `pin`, and a missing base refused
`BaseUnavailable`. Linux and Windows branches of the new crates lint clean from this machine.

Found by the daemon's tests and fixed: a task spawned from a task is joinable and stays in
the arena after it ends, so a daemon's perpetual tasks held its shutdown (they are detached at
spawn now); the doorbell thread re-read its word before each wait and lost a ring that landed
while it was kicking (it compares against the value last acted on); a client handed to a shard
whose loop already idled found the parked flag clear on its fresh region and never rang (the
hand-off wakes the loop, which marks the new client before idling again); a volume-bound verb
from a client on another shard was refused `NotFound` (forwarded to the owner now).

Found by the first cross-target lint of the Phase 1 base crate (a lane the Phase 0 crates had
and Phase 1's did not): on Linux rustix's `stat` nanosecond fields are unsigned and the
fingerprint's widening refused to compile; on Windows the fingerprint used unstable
standard-library metadata (`windows_by_handle`, `windows_change_time`). Both fixed in the
same change, and the cross-target lane now lints every shipped crate.

Owed from task 4: the file verbs over the ring (Phase 5's SDKs; until then content is
reachable in-process only); the Windows named-Event wake (Phase 4); one principal per uid
until the fleet's certificates (Phase 8); the clients-per-shard and ring-depth derivations
re-derived from measured rates at the first `status` (task 6 measures); `TargetIsVolume`
and the bridge path in `Attached` (Phase 3); the health signals of §4.14 exported through
`status` (task 6 with the histogram).

Task 5 (the Rust client and the CLI) landed 2026-09-05: `slates-client` (`crates/client`):
`Client::connect` through the rendezvous, one request in flight (the body framed inline or
through the slot's bulk chunk, the client spinning for the daemon's published window and then
parked on the wake word), request ids `(client id, sequence)`, and the two uses of exactly-once
(§4.9): a reply stalled past the reply deadline with the daemon found gone (`Liveness`, §4.7's
"control channel reset": the Linux control socket's peer end, or the bootstrap object's start
stamp on macOS and Windows) makes the client reconnect under its own id and resend, so the
retry meets its completion record; and `Session` lets a later process resume the id and the
sequence. Deadlines are derived (`Deadlines::derive`: the reply deadline is the anchor's
liveness budget, the reconnect budget the recovery budget plus one reply). Every verb of §4.4
is a typed method; refusals are the wire taxonomy as `ClientError::Refused`; the channel's
own refusals are `Stalled`, `DaemonGone` and `SessionTaken`. The rendezvous gained the wanted
id (`connect_as`; the daemon's `accept_pending` takes an in-use predicate and honours a free
id), the typed `TooManyClients` at the daemon's derived client bound (AC-2.6), and the
liveness check. `slates-cli` (`crates/cli`, the `slates` binary): `anchor` (the profile
measured, the segment created, the profile published, `slates daemon` supervised with the
segment and the anchor's pid in its environment; a daemon that never beats inside the recovery
budget or whose heartbeat lapses is killed and the policy decides; the restart bound
re-derived from the longest measured start), `daemon` (attaches and reads the published
profile, or measures and creates a segment when run alone; leaves when its anchor dies:
`PR_SET_PDEATHSIG` on Linux, a parent watch at the heartbeat cadence, a job object on
Windows), `profile`, and the client verbs with a stable plain output (one `key: value` per
line, one record per line for `list`) and exit codes for the taxonomy (0 done, 1 refused, 2
usage, 3 no daemon, 4 failed); a hand-written grammar with every flag listed once
(`args.rs`). The server gained `SegmentSource::Handoff` (an anchor in the same process), the
control channel held for a client's life, the client → shard mapping by the id's residue over
the partitions (a reconnect lands on the partition holding its records), routing by the
persistent partition index rather than the runtime's shard id, the name's owner partition by a
stable hash (`owner_of_name`, FNV-1a; every create of one name lands on one partition, so
uniqueness is that partition's to keep, with no global index), and the rebuild of recovered
volumes at start (`rebuild_recovered`: a live tree again, the reservation retaken, local-only
snapshots and attachments reconciled out of the catalog as recorded operations). `slates-mem`
keeps the object's name on macOS for opened objects, so an attached process can hand the
segment on.

Gated (`crates/client/tests/client.rs`, 2 tests, 1.2 s): the typed verbs over a two-shard
daemon (create, the duplicate refused with the original's id, snapshot, clone, attach with
epoch 1, status, list, detach, resize, destroy in slices, acknowledge; parks never exceed
replies); and a session outliving a daemon restart over one segment with the test as the
anchor: the first daemon stopped, a second started over the same handoff, the client's next
call stalling, finding the daemon gone, reconnecting under its id (one reconnect counted) and
served by the restarted daemon with the volume rebuilt, the retry of its earlier create
answered from the replayed completion record with the same id and no second volume, the
local-only snapshot reconciled away, new work continuing under the session's sequence, and a
second client refused the live session. (`crates/cli/tests/cli.rs`, 2 tests, 1.2 s): a real
`slates anchor` supervising a real `slates daemon`, the binary driven through create (the id
and the path line), the duplicate refused with exit 1, list, snapshot, clone, stat, attach,
status with and without `--drift`, detach, resize, destroy, the usage refusals with exit 2, a
missing volume with exit 1, then the anchor killed with SIGKILL and the daemon leaving so the
instance answers exit 3; `profile --quick` and the usage. The grammar and the value formats
have unit tests. All gates green on 2026-09-05 (`cargo xtask ratchet`: 82 rows, 0
regressions).

Found by the tests on their first runs: (1) routing used the runtime's shard id, which is
process-local (a second runtime in one process numbers its shards after the first's), so a
restarted daemon could reach none of its recovered volumes; volume ids and client ids now
route by the partition index, which recovery keeps (`ShardState::partition`); (2) a volume's
name was unique per shard only: two clients on different shards created one name twice
(T-2.1 across clients), which is what the CLI does on every invocation; (3) on macOS a shared
object opened by name refused to hand itself on, so a daemon attached from the anchor could
not map the segment on its shards and exited, which the anchor restarted and then refused as a
crash loop, exercising that path for real; (4) `detach` found a holder's other attachments by
encoding the whole partition (`to_snapshot`), replaced by `attachments_of`.

The dead-client reclaim landed 2026-09-05 (the daemon's side of §4.7's failure matrix, T-2.3):
the rendezvous carries the peer's process id (`SO_PEERCRED` on Linux; the claim slot's pid
elsewhere); every shard runs a sweep task at the liveness cadence (`reap_loop`, the same
budget the anchor allows the daemon's heartbeat) that expires leases by the wheel with nobody
asking and asks about every client silent for the budget: `peer.rs` (paired `#[cfg]`) peeks
the control socket on Linux (end of stream is the kernel closing the dead client's end),
probes the pid with signal 0 on macOS (`ESRCH` dead, `EPERM` reused by another user), and
waits on the process handle with a zero timeout on Windows. A gone client's attachments leave
the catalog as recorded operations, its deferred replies are dropped, its region and control
channel close with its slot, and its id returns to the control shard's live set (a thread-local
on that shard, reached by a spawned task: sharing by move); its leases keep their terms and
expire by the wheel, since a paused client is not a dead one and the term is the fence (D-16).
The operator's failover SLO moved into `DaemonConfig` (`failover_slo_ns`, ten seconds until
`slates anchor` takes a value; `with_failover_slo` for tests). The clock is read once per serve
round to mark the clients served in it. Gated (`crates/client/tests/reap.rs`, 3.8 s): the test
binary re-invoked as the victim connects, attaches for writing (epoch 1), prints its id and
parks; the parent kills it with `SIGKILL`; the attachment is reclaimed inside the lease term
(observed within two liveness budgets), the lease still shows epoch 1 after the reclaim, the
daemon's reaped counter moved by one, a session under the victim's id resumes (the id is free
again), the lease then expires by its three-second test term with nobody asking, and the
observing client never reconnected. Gotcha kept in the test: a re-invoked test binary prints
libtest's banner on stdout before the role runs, so the victim's line is tagged.

Owed from task 5: the CLI's grant surface
(`slates grant`, `grants`, `land`, `audit`) with task 8; a daemon-wide `slates status` with
task 6's health signals (`CLIENTS_REFUSED`, `RECOVERY_SKIPPED`, `HANDOFF_LOST`,
`INIT_FAILURES` are counted now and printed nowhere); the daemon start p99 for the restart
bound is the longest start measured in this anchor's life until a histogram of starts exists;
the Windows console handler, job object and section-and-Event paths are lint-checked from this
machine and run first in the Windows lane.

Task 6 (the provisioning histogram, exactly-once as a durable atom, admission and the wake
strategy) landed 2026-09-05. `crates/client/examples/provision_bench.rs` (R9, AC-2.1, T-2.6):
a volume created from the Rust client through the real rendezvous and rings against an
in-process daemon, sampled p50/p99/p999/max in a spinning form (the client spins for the 50 us
floor, so the reply lands without a wake when the daemon meets it) and a parked form (paced
past the shard's park, so each pays the doorbell and two wakes), at 1, 8 and 64 concurrent
clients. The 50 us floor is gated on the single-client spinning p99 (the latency claim); every
runnable concurrency's rows are recorded so the ratchet catches regressions; a run with more
client threads than the machine has cores past its shards is informational (it measures the
scheduler, not the path). Baselines (Apple M5 Max, macOS 26.4.1, best-of-3, all shown): one
client p50 9 us / p99 25 us / p999 31 us against the floor; eight clients p99 34-45 us;
sixty-four (oversubscribing thirteen runnable cores) p99 about 2 ms, not gated; the parked form
p99 about 250 us; a status round trip p99 about 9 us. The ratchet holds 102 rows.

Exactly-once became a durable atom: a verb's effects and its completion record now go into one
log record (`Db::begin` opens a transaction over the partition, `mutate` inside it applies and
queues, `commit` writes one `LogEntry` of every queued operation, a full log snapshots
instead), so `kill -9` between the effect and the record can no longer leave one without the
other (AC-2.3). A forwarded verb records its completion at its owner partition, and the reply
travels back already recorded; an acknowledgement is a scatter over every partition that may
hold the client's records; the client acknowledges on its own every half ring of replies, so
the daemon's retained records stay bounded without the caller (§4.9).

Admission and backpressure (AC-2.6): `clients_per_shard` is the client share of the shard's
reserve over a region's bytes (not the request rate, which sizes only what one client holds in
flight); the task arena and control channel are sized from that times the cross-shard traffic
per client plus the shard's own loops; a connect past the daemon-wide bound is refused
`TooManyClients`; a full owner-shard control channel makes a forward wait in the bounded
`pending_forwards` (retried each round) and, past the clients' credit, refuses
`Overloaded{shard}` without starting the verb. The daemon raises its descriptor soft limit to
the hard one at start (no privilege). A daemon-wide `slates status` (a scatter-gather like
`list`) reports each shard's counters and the health signals of 4.14 and the anchor's view.

The wake strategy was completed (4.7): a runtime shard sets a parked flag before it waits and
re-checks its inbox, and a sender kicks only a parked shard, so a message to a spinning shard
costs no syscall (the flag and the message are sequentially consistent, so a lost wake needs
both to miss, which the total order forbids); the server loop keeps polling for a derived idle
window after its last work, so an active client's next request never pays a wake; the client
can spin for a latency floor of its own (`Client::spin_for`).

Found under the histogram: (1) the create verb's completion record and its effect were two log
records, a `kill -9` between them a durability hole, closed by the transaction; (2) a shared
object whose name exceeded the macOS 31-character limit was truncated, so two clients' regions
could collapse onto one object under the bench's long names, now hashed when they would not fit
(`crates/mem/src/shared.rs`); (3) `status` and `detach` walked the whole partition
(`to_snapshot`) to count a volume's attachments, replaced by `attachments_of`; (4) a burst of
concurrent connects exhausted the bootstrap object's claim slots, so the client retries the
rendezvous on `RingFull` as on `DaemonUnavailable`.

Owed from task 6: the histogram runs on the reference machines in CI (this baseline is the
laptop's); the write-tracer hermeticity assertion (AC-2.2, T-2.9) and the simulation crash at
every instruction (AC-2.3's simulation half) arrive with the chaos harness; the cross-uid
security test (T-2.7) needs a second uid, gated on CI (the rendezvous refuses and counts it
now); a completion fd for parked SDK event loops is Phase 5.

Task 7 (the register protocol at f=0) landed 2026-09-05: `crates/db/src/register.rs`, the pure
core of §4.8 parameterized by the fault tolerance `f` so the laptop is the degenerate of one
formula (R8), never a mode: `Quorum` (`2f+1` candidates, commit at `f+1`, `f=0` giving one
candidate and a commit of one, the local append); rendezvous (highest-random-weight) candidate
selection, owner-first and deterministic, so every host computes the same holder set from an
object id with no directory; `Fence`, a holder's monotonic authority for a host (a record under
a host epoch below the highest seen is refused `StaleEpoch`, so a resumed stale owner never
commits); and `Configuration`, the one-voter oracle (`solo`: version 0, one member, `f=0`, no
mirror; `check_version` refuses a stale version with the current one; `await_placed(scope)`
returns for the region — the local append at f=0 — and refuses the absent mirror `Unsupported`).
Every reply carries the placement from the first version so Phase 8 changes no interface: the
wire gained `PlacedState` (`region`, `mirror_age_ns`, `host_epoch`) on `StatusReport`, the
`Scope` enum, and the `AwaitPlaced` request with the `Placed` reply; the server holds a
`Configuration::solo` per shard built from the machine identity's host id, records a snapshot's
placement through it (placed at f=0), reports the head's placement and the host epoch in
`status`, and serves `await_placed`; the client has `await_placed` and the CLI `slates volume
placed ID [--snapshot N] [--mirror]` with the placement fields in `status`.

Gated (`crates/db/src/register.rs` tests): the commit rule is the same code at f=0 and a
simulated f=1 (one candidate vs three, both `placed`), the observable differing only by the
quorum's own count (AC-2.5's register slice); a stale host epoch is refused at every f
(`StaleNeverCommits`); a stale configuration version is refused with the current one; `await
placed(region)` returns and the absent mirror is refused; rendezvous placement is
deterministic, owner-first and spread. The client and CLI tests assert the f=0 placement over
the real rings: `status` shows `placed=true`, `host_epoch=1`, no mirror; `await_placed(region)`
returns `(true, None)` and the mirror is refused `Unsupported`.

Found by the CLI test on its first run: `detach` carried only an attachment id and ran on the
detaching client's shard, but the attachment record lives on the volume's owner shard; it had
passed only because earlier client ids happened to land on the owner shard, and task 7's extra
clients shifted them. Attachment ids now carry their owner partition in the high 16 bits
(`attachment_id`/`owner_of_attachment`) and `detach` routes to it, like a volume id (§4.8
'ids route to owners, no global index'); a latent cross-shard `detach` bug closed.

Owed from task 7 (updated 2026-09-05): the holders, commit at `f+1`, takeover and phase-one
adoption are now implemented as the pure fenced ledger register simulation (§8h); still Phase 8
are the server put path (hedged placement over the real holders, recorded holder sets in the head
record), the healer and probation, mirroring and `await placed(mirror)`, migration on a
write-intent attachment, and the SWIM membership (the register core is f-parameterized so they
raise `f` without a new shape); the host epoch is persisted only as the constant 1 until the
takeover path is wired into the server (Phase 8); a chain is a register written in sequence,
which arrives with the merge engine (Phase 6).

Task 8 (grants and landings through the server) — the durable records, the ring verbs and the
CLI landed 2026-09-05; the control-channel grant transport and the write execution are in the
Linux lane. `crates/server/src/landing.rs` wires the Phase 1 landing engine (`slates-land`)
through the server: a `RequestBody::Land` plans the manifest from the snapshot's diverged
entries and, without a grant, replies `GrantRequired` with the manifest hash, its summary and
the preliminary conflicts, recording a `LandingRecord` (AwaitingGrant) and a `LandingPlanned`
audit record; a grant that binds the manifest lets the landing take the target's lease (one
holder per target, AC-2.9), validate and write through `OsLand`, after which the landing
record, the consumed grant and the audit trail are persisted (all §4.8 ops, so the
accountability replays after a crash, AC-2.10). The grant is never created on the ring or MCP
(R10, AC-2.8): the ring's grant kind stays refused, and `issue_grant` (the control-channel
entry) binds the manifest a human saw, refusing `GrantMismatch` when a re-planned landing's
hash differs. The wire gained `Land`/`Grants`/`Audit` requests, `GrantRequired`/`Landed`
replies, the `LandingSummary`/`LandingOutcome`/`GrantSummary`/`AuditEntry` shapes, the `Filter`
and `GrantScope`, and the refusals `TargetUnavailable`, `LandingConflict`, `LandingLeaseHeld`,
`GrantMismatch`, `GrantInvalid`. The client has `land`/`grants`/`audit`; the CLI has `slates
land ID TARGET`, `slates grants`, `slates audit`. The landing execution and the `os` writer are
Unix-only, so the write path's test runs in the Linux CI lane; the daemon suite here checks the
off-ring refusal, a landing into a target that cannot be opened (refused `TargetUnavailable`
with no write), and the empty grants and audit reads.

Found while wiring: `detach`'s cross-shard routing bug (task 7) had a sibling — a landing id, a
grant id and an attachment id all need to route to the partition that holds their record;
attachment ids now carry the owner partition (task 7), and the landing/grant records live on
the volume's owner shard, reached by the volume-bound `Land` request. A bench-harness flake
surfaced under the omnibus ratchet on a loaded machine and was fixed: the size-independence
check (`vfs_bench` ac-1.3) compared the median growth against the bare timer resolution, so a
lucky-fast small-size sample read as per-file scaling; it now allows the two measurements' own
bootstrap-interval widths (a real per-file term over three decades still fails). The IPC
parked-round-trip bench asserted the client parked exactly once per trip; a spurious futex
wakeup can add a park, so it now asserts at least once per trip.

Owed from task 8: the control-channel grant transport (the Linux control socket reader in the
daemon and `slates grant`/`slates grant --watch`; the socket is the rendezvous control channel,
Unix, with macOS and Windows on Phase 5's control socket) and the Linux landing execution test
(the full plan-grant-write flow into `/dev/shm`, AC-2.9's two-session serialization, and
AC-2.10's audit replay after `kill -9`); the per-entry audit records (`EntryWritten`,
`EntryRefused`) beyond the plan and the terminal record; the grant scatter for a daemon-wide
`slates grants`/`slates audit` (served on the shard now); the write-tracer hermeticity
assertion (AC-2.2, T-2.9) with the chaos harness. The provisioning histogram
(`crates/client/examples/provision_bench.rs`) was pulled out of the omnibus `cargo xtask
ratchet` (it spawns a daemon and needs a quiescent machine; back-to-back with the microbenches
its p99 measured contention, not the path) and is its own recorded command / CI lane for
AC-2.1; its rows were removed from `ratchets.toml`.

## 8e. Phase 3 record (2026-09-05)

Phase 3 (the Linux FUSE bridge) task 1's first piece landed 2026-09-05: `slates-bridge-fuse`
(`crates/bridge-fuse`), the FUSE ABI codec — the pure, transport-free layer. It parses the
kernel's `fuse_in_header` and the opcode-specific bodies slates serves (`request.rs`,
`abi.rs`: the opcode set as `#[repr(u32)]` discriminants that are the wire values, so an
unserved opcode is a typed miss the daemon answers `ENOSYS`), encodes the daemon's replies
(`reply.rs`: `fuse_out_header`, `fuse_attr`, `fuse_entry_out`, `fuse_attr_out`, `fuse_open_out`,
`fuse_write_out`, and a bounded `readdir` buffer), and computes the `FUSE_INIT` negotiation
(`init.rs`: the intersection of the flags slates wants — writeback cache, parallel dirops,
readdirplus, explicit data invalidation, big writes — and the kernel's, the minor version
bounded to slates' 7.31 floor, and the sizes it will use). The writeback-cache flag's advertised
bit was corrected to the Linux ABI value `1 << 16` (`<linux/fuse.h>`); it had been `1 << 8`
(`FUSE_SPLICE_MOVE`), so writeback never negotiated — the source audit's BUG-6, fixed with a
kernel-vector test. Every field is read and written in
order through a bounds-checked sequential reader/writer (`wire.rs`), so no byte offset is a
literal and a truncated or oversized message is a typed refusal, never a panic or an
out-of-bounds read; the crate holds no `unsafe`.

Gated (`crates/bridge-fuse/tests/codec.rs`, 12 tests, on every host — the codec is pure): a
`LOOKUP` parses to its header and name; an unserved opcode is `None` not a panic; hostile
headers (truncated, a length below the header, a length past the buffer) and hostile bodies (an
unterminated name, a short read, a write whose declared data runs past the body) are refused
without a panic (§4.9); read and write bodies parse; error and success replies encode to the
exact wire bytes with an undersized buffer refused; the `readdir` buffer packs 8-byte-padded
entries and stops before it exceeds the request's size; `FUSE_INIT` keeps the flag intersection
and handles a version mismatch and a short body. Golden byte checks stand in for kernel vectors
until the transport test runs a real mount.

Owed from Phase 3 task 1 (the rest of the driver, all Linux-only, CI lane): the `/dev/fuse`
transport (request read, reply write, notifications), `FUSE_DEV_IOC_CLONE` per shard and the
io_uring command path with the read/write fallback, mount establishment (the new mount API when
permitted, `fusermount3` otherwise) with the fd held by the anchor and the restart handoff, the
`Bridge` trait implementation over the volume core with inode `(no, gen)` and invalidation on
every mutation, `slates exec` (the launcher), the conformance suites (pjdfstest, fsx, fsstress)
and the workload harnesses, and the base-files read path through the mount. These are Phase 3
tasks 1b–8; the codec is the foundation they build on.

The bridge dispatch landed 2026-09-05 (still Phase 3 task 1, pure): `bridge.rs` defines the
`Bridge` trait (§4.6's methods, one real implementation to come in the daemon over the volume
core) and `dispatch(message, bridge, out)` — the seam between the wire and the semantics. It
parses a request, calls the matching method, and encodes the reply or the errno: `INIT`
negotiates, `LOOKUP`/`GETATTR`/`OPEN`/`OPENDIR`/`READ`/`WRITE`/`READDIR`/`CREATE`/`RELEASE`/
`FLUSH`/`FORGET` reach the bridge, a `Result::Err(errno)` becomes the kernel's negated errno, a
parse failure is `EIO`, and an unserved opcode is `ENOSYS` without reaching the bridge. Gated
(`crates/bridge-fuse/tests/dispatch.rs`, 3 tests, every host): a mock one-file bridge driven
through the dispatch — LOOKUP and its ENOENT, READ returning a slice and WRITE mutating the
file, READDIR packing an entry, INIT negotiating, FORGET reaching the bridge with no reply, and
an unserved opcode answered ENOSYS. The transport (the `/dev/fuse` read/write loop) is the thin
Linux-only layer over this dispatch.

The Bridge implementation over the volume core landed 2026-09-05 (still Phase 3 task 1, pure):
`volume_bridge.rs` (`VolumeBridge`) borrows a `Volume` and its `Store` and turns the kernel's
requests, by FUSE node id (the inode number, node id 1 the root), into volume operations —
lookup, getattr, open/opendir, read, write, readdir, create, release, forget, flush — with a
small file-handle table naming the inode a handle was opened on, and the volume core's refusals
mapped to the Linux errno the kernel expects. The volume core gained by-inode-number wrappers
(`root_inode`, `lookup_no`, `readdir_no`, `create_file_no`) so the bridge speaks inode numbers,
not handles. Gated (`crates/bridge-fuse/tests/volume_bridge.rs`, 2 tests, every host — a scratch
volume is pure RAM): a whole FUSE round trip through the real volume core (CREATE a file, WRITE
to it, LOOKUP it, GETATTR its size, OPEN and READ the bytes back, READDIR the root lists it) and
the typed errnos (a missing name is ENOENT, a stale handle is EINVAL). The metadata ops followed the same day: the Bridge trait, the dispatch and the VolumeBridge
gained mkdir, unlink, rmdir, rename (and rename2), symlink, readlink, setattr (size → truncate,
mode → chmod, by the `valid` mask) and statfs, with the volume core's by-inode-number wrappers
(`mkdir_no`, `symlink_no`, `unlink_no`, `rmdir_no`, `rename_no`) and the setattr/rename/statfs
codec. Gated (two more tests in `volume_bridge.rs`): mkdir then a file inside it, rmdir refused
non-empty (ENOTEMPTY), unlink then rmdir; and rename moving a file, setattr truncating it, and
statfs answering. The FUSE bridge's semantic surface is now complete and pure-tested; the
`/dev/fuse` transport (Linux) is the only remainder of task 1. A pre-existing anchor
test-isolation bug was fixed in the same change: `supervised_child` set process-global env vars
that raced into its own in-process test thread under a concurrent `cargo test --workspace`; it
is now `#[ignore]`d and the parent spawns it with `--ignored`, so cargo never runs it in
process. The `/dev/fuse` transport landed 2026-09-05 (Phase 3 task 1b, Linux): `channel.rs` (Linux-only)
owns the device descriptor and turns it into the request/reply stream — `FuseChannel::open`
opens `/dev/fuse` (a character device, structurally allowed as a non-disk-file open), `from_device`
adopts the fd the anchor hands back on a restart, `read_request`/`write_reply` are the device
I/O (a disconnect is `ENODEV`, typed), and `serve_blocking` is the fallback loop the design
names: read a request, `dispatch` it to the bridge, write the reply, until the kernel unmounts.
No `unsafe` (rustix's I/O-safe wrappers over the owned descriptor). It compiles and cross-lints
for Linux from this machine; the serve loop runs against a real mount in the CI Linux lane. Owed
with the rest of the driver: the io_uring command path, `FUSE_DEV_IOC_CLONE` per shard and the io_uring command path (which drops the request copy
the blocking loop makes), generation-tracked node-id reuse after `forget` (§4.6 `(no, gen)`),
`link` and xattrs, the kernel invalidation notifications, `slates exec`, and the conformance and
workload suites.

Mount establishment landed 2026-09-05 (Phase 3 task 2, Linux): `mount.rs` mounts a slates
connection through `fusermount3`, the OS-shipped setuid helper, so no privilege is required
(R10, D-2): the daemon makes a socket pair, spawns `fusermount3 -o default_permissions,fsname=…`
with one end in `_FUSE_COMMFD`, and receives the `/dev/fuse` descriptor the helper sends back
with `SCM_RIGHTS` (the same fd-passing rustix path the rendezvous uses), returning a `Mount`
whose `FuseChannel` serves it; `unmount` tears it down with `fusermount3 -u`. No `unsafe`. It
compiles and cross-lints for Linux here; the handshake runs against a real `fusermount3` in the
CI Linux lane. Owed: the new mount API (`fsopen`/`fsconfig`/`fsmount`/`move_mount`) where the
daemon has `CAP_SYS_ADMIN` in its user namespace, and the anchor holding the fd across a
restart (§2.6 step 4).

The invalidation notifications landed 2026-09-05 (Phase 3, §4.6 "Cache posture"): `notify.rs`
encodes the unsolicited messages the daemon writes to `/dev/fuse` to drop kernel cache on a
mutation — `inval_inode` (an inode's attributes and a data range), `inval_entry` (a cached
name → node mapping) and `delete` (an entry removed). They are pure encoders (the header with a
zero unique and the notification code in `error`, then the body), tested on every host with
golden byte checks (4 tests). The driver writes them before it acknowledges a mutating request,
so a second process never reads stale attributes after the mutating call returns (AC-3.3); wiring
them into the mutating dispatch paths is the driver's, with the transport.

Base files through the bridge landed 2026-09-05 (Phase 3 task 8's read path, §4.6 "Base files",
AC-3.9): `VolumeBridge::with_base` holds the overlay's read-only `OsHost`, and lookup, getattr,
readdir and read route through `Volume::with_host` (the overlay path that serves untouched base
entries from the disk), the volume core gaining `Overlay::lookup_no`/`readdir_no`. Gated
(`crates/bridge-fuse/tests/base_overlay.rs`, on any Unix host — the base is a real read-only
directory, this crate's own `src`): an overlay over `src`, served through the bridge, lists its
base files, looks `lib.rs` up, and reads it back byte-identical to reading the file straight
from disk, writing nothing. Owed: a standalone `LOOKUP` of an unlisted base entry loads the
directory's base listing on demand (today the listing loads on `readdir`, which the kernel does
first; the realistic sequence is verified); `mmap` of a base file and the splice reply path (both with the
transport). Base writes' copy-up through the bridge is done and verified: a write to a base file
routes through `Overlay::write`, copies the base up into the overlay's RAM, and leaves the base
directory on disk untouched (`base_overlay.rs` second test — the write is visible on a later
read, the rest is the base bytes, and the disk file is byte-for-byte unchanged, R1).

The launcher `slates exec` landed 2026-09-05 (Phase 3 task 4, Linux): `crates/cli/src/exec.rs`
makes a volume visible at a caller-named path for one command, in a new user and mount namespace,
without privilege (D-2, R10) and without writing disk (AC-3.5): it enters `CLONE_NEWUSER |
CLONE_NEWNS`, maps its own uid and gid, makes the mount tree recursively private, bind-mounts the
volume's directory (under the daemon's root, `SLATES_ROOT`) onto the chosen path, and execs the
command — so the parent shell's view is unchanged and the bind lives only in the command's
namespace. An unsatisfiable path is refused with the exact missing directory, never created. One
budgeted `unsafe` (`unshare_unsafe`, sound in the single-threaded pre-exec launcher). The `--`
splits the flags from the command; the parsing is unit-tested on every host, and the launcher
runs against a real daemon mount in the CI Linux lane. Owed: the daemon publishing its root
mount so `SLATES_ROOT` need not be set by hand, and the AppArmor-profile detection with the exact
remedy message (a generic hint is given now).

The xtask unsafe counter was made word-boundary aware in the same change: it counted the
substring `unsafe` inside identifiers like `unshare_unsafe`; it now counts the `unsafe` keyword
as a whole token. The end-to-end CLI flow test (`crates/cli/tests/cli.rs`) is gated behind
`SLATES_TEST_CLI` and runs as its own CI step: it spawns a real anchor and daemon whose shards
spin, and a busy parallel `cargo test --workspace` starves them; on its own (the CI step passes
`--test-threads=1`) it is reliable.

The bridge seam became the one shared operation layer 2026-09-05 (§4.6 "Bridge trait (one VFS operation layer)", D-2): the `Bridge` trait, its neutral data types (`NodeAttr`, `DirEntry`, `FsStat`, `SetAttr`, `RenameFlags`) and the single `VolumeBridge` implementation moved from `bridge-fuse` into a new `slates-bridge-core` crate (in the `bridge-*` lint family), so every OS transport — FUSE, the now-first-class virtio-fs (GAP-A9-5, RQ-20), the macOS FSKit module and its NFS fallback, WinFsp — dispatches its wire protocol onto one seam and encodes the neutral results back, never a second parallel operation layer (Part 2 item 7). The trait is neutral by construction: addressed by real inode numbers (the FUSE "node id 1 is the root" convention resolved at the FUSE edge through `Bridge::root`, so NFS, which mints its root handle the same way, shares the implementation), returning neutral attributes, and refusing with the volume core's own `VfsError`, which each transport maps to its wire error (a Linux errno, an `nfsstat3`). `bridge-fuse` keeps the FUSE ABI codec and becomes the wire edge over the seam (`bridge.rs`: node id to inode, `VfsError` to errno, neutral to `fuse_attr`), re-exporting the trait so its tests' import paths hold; all 26 of its tests pass unchanged, evidence the move preserved the FUSE behavior byte for byte.

Three A-9 bridge findings were fixed in the same change, at the seam so every transport inherits the fix instead of the flagged narrow signature (the audit names `Bridge::rename` and "the reduced Bridge signature" directly; Ada's steer, 2026-09-05). BUG-8: `setattr` carries every POSIX field (`SetAttr { size, mode, uid, gid, atime, mtime }`) and the volume core gained `chown`/`set_times`, so a requested field is applied, never an ignored field acknowledged (§4.6 "never acknowledge an ignored setattr field"). BUG-10: `rename` carries the `renameat2` flags (`RenameFlags`), honoring `RENAME_NOREPLACE` (an existing destination is `EEXIST`) and refusing `RENAME_EXCHANGE` with `EINVAL` — the errno `renameat2` itself returns where a flag is unsupported (D-26) — never silently downgrading to a replacing rename. BUG-4: open handles live in a bounded generational `slates-mem` slab, so a released handle is stale (never a wrong inode) and its slot is reused, and the table refuses at its bound (`EMFILE`) instead of growing without end. Gated (`crates/bridge-core/tests/volume_bridge.rs`, 5 tests, every host, no mount): a `setattr` of ownership and times takes effect while a later mode-only `setattr` leaves them alone; NOREPLACE refuses an existing destination and takes a free one; EXCHANGE is refused, not downgraded; a thousand open/release cycles leak no handles; and — the later *root:wheel* fix (2026-09-09, `docs/bugs/2026-09-09-root-wheel-mount.md`) — a created object is owned by the mounting user (the subject's uid) with its parent's group, not the born `root:wheel` default, across all three creating verbs. The mounted verification (independent kernel vectors, real setattr/rename semantics under writeback, `UTIME_NOW`/`UTIME_OMIT` resolution, and open/close beyond the arena bound) is owed to AC-3.10/AC-3.12, which need a real mount; the remaining bridge findings — BUG-5 base-aware lookup, BUG-7 READDIRPLUS/FSYNC/LINK dispatch, BUG-9 truthful statfs — stay open under GAP-A9-3. `slates-bridge-core` unsafe budget 0; clippy (all targets), literal, structural (19 crates), cross (linux, windows) and fmt gates clean.

## 8f. Phase 6 groundwork (2026-09-05)

The merge engine's deterministic verdict — the pure core of §4.16 (D-27) — landed 2026-09-05 as
`slates-merge` (`crates/merge`), ahead of the rest of Phase 6, because it is a self-contained
pure function that needs none of the fleet or the green-volume integration to be correct and
directly confirmable. `range.rs` is the byte range and the per-path `RangeSet` (sorted,
non-overlapping, with the half-open overlap rule that an insert at the very edge of a range does
not conflict). `verdict.rs` is the two passes: `path_verdict` sweeps the increment's ranges
against the intervening deltas' effect ranges on one path — disjoint ranges `Accept`, an
identical span becomes a candidate for pass two, an insert anchored inside a change or two
inserts at one point are `SamePositionDiffering`, any other overlap or containment is `Overlap`;
a structural class the sweep cannot see (rename/create/type/meta/delete-vs-modify) is passed in
and returned directly. `compare_bytes` is pass two (a memcmp: equal bytes `AcceptIdentical`,
else a conflict). `fast_path` is the whole-increment shortcut: every touched path last changed
at or before the base means `Accept` with no range work, and it never decides a conflict. The
sweep is a linear merge of two sorted lists; it does no I/O, reads no clock, draws no randomness,
and its hot comparison allocates nothing (a lint and a no-alloc test to pin that are owed).

Gated (`crates/merge/tests/verdict.rs`, 9 tests, every host — the verdict is pure; the hecate
M-matrix as range cases): disjoint accepts, no-intervening-change accepts, overlap and
containment conflict, an identical span becomes a candidate that pass two resolves to
identical-or-conflict, an insert inside a change conflicts while one at the edge accepts, two
inserts at one point are resolved by pass two, every structural class is returned directly, and
the fast path accepts an untouched basis.

The ops document — the canonical serialization of an increment's declared operations, whose BLAKE3 is half the increment's identity (§4.16 "Data model", "its identity is the test") — landed the same day (`ops_doc.rs`). It is the fixed op kinds (§4.16 `OpKind`, wire values 0–14; `from_wire` reads them off the kinds' own list, so no arm is a bare number), a per-document path table that interns paths in insertion order while building and sorts them canonically at `canonicalize` (remapping every op's path index), and one fixed little-endian record per operation (kind, flags, path index, offset, length, source offset). Bytes are never in the document; content an operation adds is named by a source offset into the post-state chunks the splice resolves later. `encode` is a sequential writer, never a struct transmute, so no host-dependent padding enters; `identity` is the BLAKE3 of that encoding. Gated (`crates/merge/tests/ops_doc.rs`, 5 tests, every host): the same operations declared in different orders produce byte-identical encodings and one identity (the determinism gate), encoding is stable and the identity is its hash, different work has a different identity, every op kind round-trips through its wire value, and interning is stable while canonicalize sorts the table and remaps the ops; and a golden vector pins the identity of a fixed document (`67613f8e…`) so a change to the canonical encoding is caught across versions, not only within a run (the identity is what an increment's id is built on). A caller canonicalizes before taking the identity; the deriver's terminal step will, and that property is what the determinism test pins.

The content deriver — the pure core of "Composition at seal" — landed the same day (`derive.rs`). One path's declared content operations (`ContentOp`: overwrite, extend, truncate, insert, delete), in journal order, compose by interval algebra into the canonical net op set relative to the base content, never by comparing bytes (D-27's never-diff clause: the deriver reads no content). Composition tracks the file as a piece list in current-file coordinates — runs of surviving base bytes and runs of new bytes — that each operation splits and rewrites; a single readout pass then emits the net ops in base coordinates, naming added content by its offset in the sealed post-state (the final file, `slates`' ground truth for bytes), which the splice resolves later. The canonical form is minimal: overlapping overwrites merge into one, an equal-length in-place replacement is one overwrite, an unequal one is a delete then an insert (the whole-file-rewrite shape), an addition past the base end is an extend and within it an insert, a removal to the base end is a truncate and within it a delete. Gated (`crates/merge/tests/derive.rs`, 9 tests, every host): the four worked cases (overlapping overwrites merge, a truncate cancels operations beyond it, an insert then an overlapping delete cancels, a whole-file rewrite is a delete then an insert), an append is an extend, an empty journal is the identity, and three proptests over random in-bounds journals — the deriver oracle (applying the net ops to the base, drawing added bytes from the post-state by source offset, reproduces the post-state exactly, so composition is correct without any byte comparison), determinism (the same journal composes to the same ops), and the net set is ordered by base offset. The oracle draws base bytes and added bytes from two different position-varying patterns, so a misplaced op, a wrong length or a wrong source offset diverges. A `Vec` piece list makes a front split O(pieces); the journal a submit composes is bounded, and a balanced structure is the measured replacement if a length benchmark shows the piece count dominating (owed, not guessed).

Position mapping — "canonical rebase" (§4.16 "Position mapping") — landed the same day (`map.rs`). An increment declared against a base version is mapped forward through the canonical deltas of every version in `(base, head]`, one direction, per path, before the verdict and the splice. For one delta: an intervening operation that turns `old_len` base bytes at `at` into `new_len` shifts every later position by `new_len - old_len`; a range entirely before it is only shifted; a range that overlaps a touched span is returned as `Overlaps` for the verdict to classify (the mapper does not decide conflicts); an insert at the very edge of a range does not overlap it (the range's edge rule), so a range whose neighbourhood only grew or shrank maps cleanly. Maps compose: mapping through `(base, head]` feeds each delta's shifted range into the next. Content effects come from the ops' kinds (overwrite touches its span with zero size change, insert and extend add, delete and truncate remove); namespace ops have no byte-coordinate effect. Gated (`crates/merge/tests/map.rs`, 9 tests, every host): an insert before a range shifts it and after it leaves it, a delete before shifts it left, an overwrite hitting it overlaps, an insert at the edge is clean, maps compose across two deltas, and a later delta meeting the shifted range overlaps — plus a provenance oracle (apply the deltas to a vector recording each byte's base origin, kept at every version; a range maps cleanly exactly when its bytes stayed intact and contiguous at every version, and then to their head position) over random single-op delta sequences, and determinism. **2026-09-30:** the oracle had judged the head alone, so an insert strictly inside a range that a later delete removed read as untouched while the mapper — rightly, by the per-delta rule — handed it to the verdict; CI run 36655388624 drew that history. The oracle now judges every version, the case is a named test, and the property holds over 200,000 histories (`docs/bugs/2026-09-30-the-position-mapping-oracle-judged-only-the-head.md`). The checkpoint fold below must keep every touched span, not only the net size change, or it stops being exact. Owed here: folding old deltas into checkpoint deltas so a distant base maps in O(log) lookups (the same composition applied ahead of time; the measured optimization when a base-lag benchmark shows the raw walk dominating).

The whole-volume deriver — composing a work volume's journal of content, create and unlink into one ops document — landed the same day (`increment.rs`, replacing the content-only assembler). Per path the journal is a state machine (`VolumeOp`): a create makes a fresh empty file, an unlink removes it, content operations accumulate against whatever file is present. Composition follows §4.16: a create then an unlink of a new file cancels (nothing declared); a base file unlinked is one `Unlink`; a base file edited in place is its content net ops; a base path whose content was replaced (unlinked then recreated, or created over) is a delete of the base and the new content, with no create because the path existed at base; a new file that survives is one `Create` and its bytes as inserts. It then groups the paths, composes each with the content deriver, and lays the post-state out in sorted-path order (each path a contiguous region, an op's source its per-path offset plus the region base), so the identity is independent of the journal's declaration order. An operation the journal could not have produced on a valid volume (content on a missing file, a create over an existing one, an unlink of a missing one) is a typed `DeriveError`, never a panic. Gated (`crates/merge/tests/increment.rs`, 6 tests, every host): create-then-unlink cancels, unlinking a base file is one unlink, creating-and-writing is a create and an insert, recreating a base file replaces its content (a delete and an insert, no create), content on a missing file refuses, and a whole-filesystem oracle over three paths with random valid journals — reconstructing the filesystem from the increment (creating, removing, and applying content drawing from the post-state) reproduces the model the journal produced, base bytes (0-127) and added bytes (128-255) disjoint so any wrong path, offset, source or missing create/unlink diverges.

Rename composition landed the same day, extending the deriver to a moving-entity model (`increment.rs`): each file is an entity carrying its origin, base length and content ops; a create makes one, an unlink kills one, a rename moves one and content accumulates on whichever entity is at a path. §4.16's rename cases: a base file renamed to a fresh path is one `Rename` whose source is the base path, plus its content; a base file renamed over another base file is one `Rename` that replaces the target; a new file renamed over a base file is the write-and-rename pattern — it composes to the destination's content being replaced (a delete and the new bytes), not a rename. A gone base path is unlinked only when no surviving file occupies it, so a rename or replacement onto a path covers its removal and a base file recreated to the same content leaves no trace. The rare rename onto, or create at, a base path already consumed this increment is a typed `Unsupported` refusal (owed). The oracle now generates renames too and reconstructs them (a renamed path's base comes from the rename's source), skipping the owed refusal; 7 tests including worked rename cases (base to a fresh path, write-and-rename replacing a base file's content).

The splice landed the same day (`splice.rs`): once the verdict accepts a path's net ops, the new version's extent list is built from the base version's extents with each op's range replaced by a reference into the post-state chunks — no byte is copied, an unchanged run keeps pointing at its base chunk and an added run points at the post-state chunk (`src`). It is a single walk over the base extents and the (base-ordered) net ops: copy the base extents up to the next op, then place the op's replacement; `Source` is `Base` or `PostState` (a chunk store and offset). Gated (`crates/merge/tests/splice.rs`, 5 tests, every host): an overwrite splits the base and keeps the rest base-sourced (a non-vacuity check that unchanged bytes are not copied), no ops leaves the base extents untouched, a truncate to zero leaves none, and two proptests — the splice oracle (chunk the base into extents, splice a random journal's net ops, read the extents back from the base and post-state buffers, reproduce the final content exactly) and well-formedness (the extents cover exactly the final length, none empty).

Directory composition landed the same day, extending the deriver's input to a `Base` snapshot that knows the base's files and directories (`increment.rs`). Directory operations compose independently of files (a path is a file or a directory, never both at once): a base directory removed is one `Rmdir`; a new directory that survives is one `Mkdir`; a mkdir then an rmdir cancels; a base directory removed then recreated is unchanged (directories have no content). A path used as both a file and a directory this increment, a mkdir over a present directory, and an rmdir of an absent one are typed refusals (`PathIsFileAndDirectory`, `MkdirOverExisting`, `RmdirMissing`). Gated (`crates/merge/tests/increment.rs`, now 15 tests, every host): seven directory worked cases (make, remove, mkdir-then-rmdir cancels, remove-then-recreate is nothing, the file/directory collision, the two refusals) and a directory oracle over random valid mkdir/rmdir journals (applying the document's Mkdir/Rmdir to the base directory set reproduces the set the journal produced). The file deriver's seven tests are unchanged (no regression).

Mode composition landed the same day (`SetMode`, extending `Base` with the base mode per path). A file or directory's mode composes independently, last-write-wins, emitting a `SetMode` (the new mode carried in the op's `len`) only where the mode differs from the base and the path is present at seal — a surviving file (including an untouched base file) or a present directory. A mode set to the base mode declares nothing (minimality); a mode on a path present nowhere is `SetModeMissing`; chmod then rename of one path is the owed `Unsupported` refusal. Gated (`crates/merge/tests/increment.rs`, now 22 tests, every host): setting a base file's mode, setting it to the base mode (nothing), last-write-wins, a base directory's mode, a new file's mode, the missing-path refusal, and the chmod-then-rename refusal.

Symlink composition landed the same day (`Symlink`, composed independently like directories, with `Unlink` on a symlink path routed to it; the target is interned in the path table and named by the op's `src`). A new or retargeted symlink is one `Symlink`; a base symlink removed is one `Unlink`; a symlink created then unlinked, or a base symlink removed then recreated to the same target, is nothing; retargeting a base symlink (unlink then symlink) is one `Symlink` because the survivor rule suppresses the removal. A symlink over a present symlink, and a path used as conflicting kinds (file/directory/symlink), are typed refusals (`SymlinkOverExisting`, `PathKindConflict`). The base grew a `symlinks` list (path to target). Gated (`crates/merge/tests/increment.rs`, now 30 tests, every host): create, symlink-then-unlink cancels, base-symlink removal, retarget, recreate-to-same-target (nothing), symlink-over-existing, symlink at a base file path, and a write on a base symlink. This work reused the file oracle, which surfaced a latent write-and-rename bug: a new file renamed over a base file that had itself been renamed to that path was wrongly treated as a content replacement of a non-existent base path; the fix restricts the content-replacement to a base file at its own path, so the other case is a new file at the destination with the clobbered base file's original name unlinked (the analogous path in `apply_create` already checked the own-path case). The oracle now passes across repeated proptest seeds.

Xattr composition landed the same day (`SetXattr`/`RemoveXattr`, composed like `SetMode` but against the base value). Per `(path, name)` the final state is the last set value or removed; a `SetXattr` is emitted only where the value differs from the base and a `RemoveXattr` only where a base xattr is removed, and only where the path is present at seal. The op names the file in `path` and the attribute name (interned in the path table) in `at`; a set xattr's value is laid out in the post-state after the file content, in sorted `(path, name)` order, and named by the op's `src`/`len`. The base grew an `xattrs` list `(path, name, value)`. An xattr on a renamed-away path is the owed `Unsupported` refusal; one on a path present nowhere is `XattrMissing`. Gated (`crates/merge/tests/increment.rs`, now 36 tests, every host): setting to a new value, setting to the base value (nothing), removing a base xattr, a new xattr's value laid in the post-state, set-then-remove of a base xattr, and the missing-path refusal.

Hard link composition landed the same day (`Link`, composed like symlinks: an independent per-path state machine with `Unlink` routed to it, the target file named in the path table by the op's `src`). A new or retargeted link is one `Link`; a base link removed is one `Unlink`; link-then-unlink cancels; a base link removed then recreated to the same target is nothing. Whether the shared content survives an unlink is the merge's concern at apply time, not the composition's; a write through a hard link is refused as a kind conflict (owed). A link over an existing link, and a link sharing a path with a file/directory/symlink, are typed refusals (`LinkOverExisting`, `PathKindConflict`). The base grew a `hardlinks` list. **With this, the deriver composes every §4.16 declared operation kind** — content (overwrite/extend/truncate/insert/delete), create, unlink, rename, mkdir, rmdir, setmode, symlink, setxattr, removexattr, and link. Gated (`crates/merge/tests/increment.rs`, 41 tests, every host): create, link-then-unlink cancels, base-link removal, link-over-existing, and a link/file conflict.

The merge engine on one node landed the same day (`engine.rs`), composing the pure pieces into the submit pipeline (§4.16 "The verdict", "Splice", "Commit"). A `Green` holds the head content per file, the committed deltas per version (for position mapping), the last version each path changed at (the fast-path index), a `seen` set for idempotent retries, and the head. `submit(increment)` is idempotent by identity; for each changed file it takes the fast path when the path is unchanged since the increment's base (counted, with a non-vacuity test), else maps each edited range forward through the intervening deltas — a range disjoint from every intervening change is accepted and its edit re-applied at the shifted position, a range that meets one is a conflict unless the agent produced exactly the green's current bytes for that file (both made the same edit, which accepts). All files accepting commits a new version; any conflict returns byte-exact windows and changes nothing, and the agent rebases onto the head. Gated (`crates/merge/tests/engine.rs`, 9 tests, every host): a submit accepts and updates the green, disjoint files both accept, disjoint ranges of one file both accept (the range merge), an intervening insert shifts a later edit, overlapping edits conflict, an identical edit accepts, the fast path fires and is counted, a resubmit is idempotent, and a rebase after a conflict accepts. This is the laptop-degenerate engine. It now merges namespace changes too: a `PathChange` is a modify, a create, or a remove; create/create with different bytes conflicts (identical bytes accept), a modify of an intervening-deleted file or a remove of an intervening-modified file is a delete/modify conflict, and a remove of an already-gone file is a no-op — each conflict carries its `MergeConflictClass`. Six more tests: create/create conflict, identical create, delete-versus-modify, a remove accepts, removing an already-removed file, and remove of a modified file. The engine now merges the first namespace dimensions too, as independent per-path dimensions the way the deriver composes them: a directory creation (`Mkdir`) and a mode change (`SetMode`). A file and a directory at one path is a `TypeChanged` conflict (both directions: mkdir over a file, create over a directory, modify of a path now a directory); two agents making the same directory accept; two differing mode changes on one path are a `MetaMeta` conflict while an identical one accepts; a mode change on a deleted path is a delete/modify conflict; and a content edit and a mode change on one path are independent (they do not conflict), each tracked by its own last-changed index. Ten more tests (`crates/merge/tests/engine.rs`): mkdir creates a directory, two mkdirs accept, mkdir over a file conflicts, create over a directory conflicts, setmode sets a file's and a directory's mode, two differing setmodes conflict, identical setmodes accept, setmode on a deleted file conflicts, content and mode are independent, and a modify of a path now a directory conflicts. Symlink merges followed the same pattern: a symbolic link (`Symlink`) merges per path, an identical target accepting, two differing targets a `CreateCreate` conflict, and a file-versus-symlink or directory-versus-symlink at one path a `TypeChanged` conflict in both directions; four more tests (now 29 engine tests) — a symlink is created, identical symlinks accept, differing targets conflict, and a symlink and a file at one path conflict either way. File rename merges followed (`Rename { from }`, keyed at the destination and naming its source): the source's current content is captured at merge time (so an intervening edit to the source follows the move) and the source is removed; a source an intervening change renamed away or removed, or a destination an intervening change occupied, is a `RenameRename` conflict, and a destination that is a directory or symlink a `TypeChanged` conflict. Removes apply before sets in a commit, so a chained rename (a→b and b→c in one increment) rotates correctly without losing the moved content. Six more tests (now 35 engine tests): a rename moves a file, a rename carries an intervening edit, a rename of a moved source conflicts, a rename onto an occupied destination conflicts, a rename over a base file replaces it, and a chained rename is correct. Directory removal (`Rmdir`) merges next: the increment's cleared set (its removes, rmdirs and rename sources) is computed once, and a directory is removable when it is empty once that set is applied — so a directory whose children the same increment removes, rmdirs or renames away can be removed, while a live child it does not clear (including one an intervening change added) is a delete/modify conflict; a file or symlink at the path is a `TypeChanged` conflict, an absent directory a no-op. Six more tests (now 41 engine tests): rmdir removes an empty directory, a directory emptied in the same increment is removed, a non-empty directory conflicts, an rmdir of a file conflicts, an rmdir of an absent directory is a no-op, and an intervening child blocks the removal. The engine was then refactored (`6164ef9`) to consume the deriver's canonical ops document plus its sealed post-state, rather than one `PathChange` per path: `Increment { id, base, doc: OpsDoc, post_state }`, resolved into per-dimension groups by path and decided against the intervening history. That closed the remaining kinds. Hard link merges as a namespace edge (`Link`; identical accepts, differing is a `CreateCreate` conflict, file/link is `TypeChanged` — its content sharing is the volume's concern at apply time, §4.16, not the merge's). Xattrs merge per `(path, name)` (`SetXattr`/`RemoveXattr`; identical accepts, differing is `MetaMeta`, names are independent). A directory move needs no special case — it is the deriver's child renames plus mkdir and rmdir, all of which already merge. And several dimensions on one path in one increment (an edit and a chmod and an xattr) now merge together, which one change per path could not express. The green keeps a small per-path content history to reconstruct a base version's bytes for the identity check (the design's chain shares it copy-on-write — the measured optimization, owed). The engine tests were rewritten to build increments as ops documents through a small builder (the shape the deriver emits); 23 tests cover all prior content and namespace behaviors plus hard-link, xattr, several-dimensions-on-one-path, and a directory move as child ops. Per-range identity within a mixed file (the design's two-pass memcmp verdict; the engine's identity check is whole-file today), the fully general intra-increment coordination (a path both renamed away and recreated in one increment), the checkpoint folding of the canonical deltas, and the green chain and holder recomputation in a fleet remain owed.

Owed (the rest of Phase 6): the deriver's interaction edges (a file/directory/symlink/link transition at one path, a metadata or link then a rename of that path, symlink rename, a write through a hard link, and the rare reused-base-path rename combination above), each a typed refusal today; the engine now consumes the whole ops document (`6164ef9`), so every namespace kind merges through it — directory creation, mode, symlink, file rename, directory removal, hard link and xattr — and several dimensions on one path in one increment merge together; the fully general intra-increment coordination (a path both renamed away and recreated in one increment) remains; per-range identity in the engine's verdict; the checkpoint folding above; the fenced ledger-register commit and holder recomputation in a fleet (the single-node commit is an in-memory chain append; the fleet register, mirror and reconfiguration protocols are now simulated, §8h); these build on the
green-volume data model (§4.5's journal is in place; the fleet version chain is not yet).

## 8g. Phase 7 groundwork (2026-09-05)

The archive format — the pure core of D-17 and §2.6 of `research/compression-archive-dedup.md` — landed 2026-09-05 as `slates-archive` (`crates/archive`), ahead of the rest of Phase 7, because the container is a self-contained, in-RAM byte format that needs none of the codec, dedup or CDC work to be correct and directly confirmable, and slates never writes it to disk (R1). An archive is one streamable, content-addressed byte sequence: a fixed header (magic, major/minor, flags, page and chunk-size parameters, the manifest's BLAKE3, chunk count, raw and stored byte totals, volume and snapshot ids, the name-policy id and Unicode version), the chunk records in manifest order (each a BLAKE3 identity, raw and stored lengths, an encoding and level, a dictionary identity, and the payload), the manifest bytes, a seek table (a zstd skippable frame mapping each chunk's identity to its offset), and a trailer (the section index, the whole-archive BLAKE3, and the tail magic). `wire.rs` is a bounds-checked sequential reader/writer, so no byte offset is a literal and a short read is a typed refusal; `encode` is deterministic (the same snapshot yields the same bytes), and `decode` verifies the magic, the major, the whole-archive hash (catching truncation or any alteration), each chunk's identity, and the manifest's identity; `chunk_by_identity` locates one chunk through the seek table without scanning. Every malformed stream is a typed `ArchiveError` (bad magic, unsupported major, unknown required flag, truncated, bad trailer, archive-hash mismatch, chunk-identity mismatch, manifest-hash mismatch, bad seek table), never a panic.

Gated (`crates/archive/tests/archive.rs`, 11 tests, every host): an archive round-trips (T-7.1), encoding is deterministic, the seek table finds a chunk by identity (and reports an unknown one absent), a chunk that fails its identity is refused and named (AC-7.3), a bad magic and an unknown major are refused, a flipped body byte is caught by the whole-archive hash, a short stream is refused; and three hostile proptests (T-7.4): every truncation is refused, any single byte flip is caught (never the original snapshot decoded), and arbitrary bytes never panic. The LZ4 codec landed the same day (D-17's probe and hot-path codec, `lz4_flex`, pure Rust and safe-only so the unsafe budget stays 0): `compressed_chunk` compresses a chunk and keeps the LZ4 form only when it is smaller than the raw bytes (the format-derived floor), else stores raw; `content` and the reader decode an LZ4 chunk to its declared raw length and verify the decoded bytes against the chunk's identity, refusing an undecodable payload with a typed `BadPayload`. Two more tests (T-7.3): a repetitive blob is stored LZ4 and shrinks and round-trips, and a tiny unique blob stays raw. The zstd codec landed 2026-09-05 (D-17's ratio-bearing codec, `zstd`/`zstd-sys`, the C library built here through `cc` with the system clang): `zstd_chunk` compresses and keeps the zstd form only when it is smaller than the raw bytes (the format floor, level 0 = the library default; the calibrated per-chunk level and the LZ4-vs-zstd-vs-raw cost model are owed, they need the boot profile so R3 forbids fixing them here), `content`/`decode_payload` decode a zstd chunk to its declared raw length and verify the decoded bytes against the chunk's identity (`BadPayload` on an undecodable payload), and it uses only the safe `zstd::bulk` API so the archive unsafe budget stays 0. Because `zstd-sys` needs a C toolchain that the Windows/Linux cross-lint lane does not have, zstd is a **default feature**: native builds and tests use it, the `--no-default-features` cross gate compiles without it (a zstd chunk is then a typed refusal, the owed `ruzstd` decode-only fallback). Three more tests, feature-gated (T-7.3): a repetitive blob is stored zstd and shrinks and round-trips through the whole archive, a tiny blob stays raw, and zstd beats LZ4 on structured data (a non-vacuity ratio check). Owed: zstd static contexts carved from arenas and the calibrated cost model (the Btrfs sampler, the LZ4-to-zstd regression, per-volume observation), the `ruzstd` decode-only fallback for no-C-toolchain targets, dictionaries (FastCover training, identity, embedding, GC), the background identity pass and the per-shard hash-prefix partitioning and server wiring of the content index, FastCDC for the measured large-file class, and the export stream through the SDKs/MCP. LZ4 and BLAKE3 need no C toolchain; zstd needs one only where the feature is on.

The content-addressed store with deduplication landed the same day (`store.rs`), the pure core of Phase 7 task 1's content index. Chunks are keyed by their BLAKE3 identity, so identical chunks fold to one stored copy with a reference count; `unique_bytes` (the distinct chunks' stored bytes) shrinks as duplicates fold in while `referenced_bytes` (the undeduplicated total) is unchanged — the accounting the design's worked example names; `release` evicts a chunk on its last reference. Gated (`crates/archive/tests/store.rs`, 4 tests, every host): identical chunks deduplicate with exact accounting, the two-clones-share-an-output worked example, eviction on the last release, and a property test over random chunks with duplicates (byte-exact reads, exact accounting, a non-vacuity check that deduplication shrank the count). The per-shard hash-prefix partitioning, the background identity pass on idle time, and the server wiring are the runtime integration (owed).

The manifest tree landed the same day (`manifest.rs`), the archive's canonical, sorted, Merkle-hashed directory tree (§2.6 item 4). A directory node holds its entries (a name and a child) sorted by name; a file node holds its extent list (offset, length, chunk identity, chunk offset), a hole a zero-chunk extent. Each node's BLAKE3 identity is a Merkle hash over an encoding that names its children by identity, so the root identity fingerprints the whole tree and any change to any node changes it. Two canonical, deterministic encodings: the Merkle encoding (children by identity) defines the identity; the tree encoding (children inlined) is stored and parsed back through the bounds-checked reader, a truncated or malformed tree a typed `ManifestError`. Gated (`crates/archive/tests/manifest.rs`, 8 tests, every host): a tree round-trips, the identity and encoding are independent of entry order, changing a leaf changes the root, a file and a directory differ, a truncated tree and an unknown kind are refused, arbitrary bytes never panic, and generated trees round-trip with stable identities. The archive container now embeds the tree: its manifest section stores the tree's canonical encoding and the header's manifest hash is the tree's Merkle root, which `decode` recomputes and verifies (the archive's 13 tests use a `Node` manifest). Per-node metadata landed the same way: each directory `Entry` carries a `NodeMeta` (inode number, mode, modification and change times in nanoseconds, size, hard-link count, and an xattr-present flag — the field list of §2.6 item 4), written into both encodings and hashed into the Merkle identity, so a change to any entry's mode or times changes the root identity as a content change does; the archive carries, hashes and round-trips it, while applying it to a host path is the landing engine's job under a grant (§4.15). This bumped the format minor to 1 (a v1.0 and a v1.1 tree of the same shape have different roots). Two more tests (`crates/archive/tests/manifest.rs`, now 10): the metadata round-trips through the canonical encoding, and a mode change changes the root identity while identical metadata gives one identity. A golden vector pins the sample tree's Merkle root (`1a1634ff…`), so any change to the node encoding (including the metadata) is caught across versions, not only within a run. The root directory has no naming entry, so its own metadata is not carried; every named node's is (owed only for the root, a minor gap).

Restore landed the same day (`restore.rs`): reconstruct a volume's files from an archive by walking the manifest tree and resolving each file's extents against the chunks — a normal extent reads its bytes from the named chunk (decoded and identity-verified), a zero-chunk extent is a hole of zeros, a multi-extent file concatenates a sub-range of each chunk. An extent naming a chunk the archive does not hold is a typed `MissingChunk` refusal. Restore also surfaces each named node's metadata by path (`Restored.metadata`, the `NodeMeta` from each entry), so a granted landing can apply the mode and times; restore itself reconstructs only the in-memory tree. Gated (`crates/archive/tests/restore.rs`, 8 tests, every host): a file restores to its bytes, a hole restores as zeros, a multi-extent file concatenates its chunks, a tree restores files by path and records directories, a missing chunk is refused, restore works after an encode/decode round trip, an LZ4-compressed chunk restores correctly, and per-node metadata is surfaced by path and survives the stream. The eager whole-tree restore is here; lazy restore (attach after a metadata-only pass, decompress on first read, AC-7.4) is the runtime's job on top of it (owed).

Resumable transfer by the missing set landed the same day (`transfer.rs`; §2.6): the receiver reports the chunk identities it holds, `missing_set` returns the archive's chunks not in that set (sorted, so a resumed transfer computes the same set), and `chunks_for` packages exactly those — the basis of replication and clone-from-archive. Gated (`crates/archive/tests/transfer.rs`, 3 tests, every host): the missing set is everything with nothing held and empty with all held; a receiver holding a previous version's chunk receives only the changed one and combining held with shipped restores the whole archive; duplicate chunks ship once.

## 8h. Phase 8 groundwork (2026-09-05)

`crates/db/src/ledger.rs`, `mirror.rs` and `reconfig.rs` contain pure, direct-call simulations
of ledger adoption, prefix shipping and holder-set changes. The earlier record lists 10 ledger,
6 mirror and 7 reconfiguration tests; those counts were not rerun for A-9. The tests are useful
component evidence but do not establish a correct fleet or a refinement of the TLA models.

The audit at `a1059ed` found a committed-prefix counterexample in `Holder::reconcile`: an
identical record retained its older accepted epoch, allowing later adoption of a conflicting
intermediate-epoch proposal (BUG-12). Separate commit `d9cb6e5` fixes that refresh and reports
its before/after regression plus 40 passing DB tests. It also removes the forced candidate-zero
reachability restriction (BUG-13) and pins a shrunk seed. The direct check immediately after
takeover still compares adopted length rather than values; later committed-prefix comparison
exists. Message-level histories and complete adoption checks remain owed. No tests were rerun
in this documentation pass. `Owner::replicate` also needs its extension precondition enforced. Highest epoch
cannot be learned from unavailable holders' hidden state in a real protocol.

`Mirror::lag` is a count of records, not a duration; identity-only shipping does not close
`await placed(mirror)` for filesystem contents. Reconfiguration simulations cover their local
transition model; message delay, consumer/read leases, bytes, host-local capacity, state-transfer
publication and configuration consensus wiring remain open. These modules provide starting
points for the same N=1/fleet semantics, not proof that all integration work is transport only.

## 8i. A-9 contract correction and open implementation gaps (2026-09-05)

**Mounted conformance follow-up (2026-09-19, §4.3/§4.6/§4.9):** the 500 × four-process
fsstress history now passes all 2,000 logged operations with zero daemon heartbeat kills.
Async NFS connections yield between replies; recovery byte vectors copy in bulk instead of
calling the scalar codec once per byte. The instrumented debug run's maximum publication
was 109 ms and maximum heartbeat gap 294 ms. Reduced regressions fail before each change.
The uninstrumented full command passes fsx (10,000 operations), all nine workloads and
hermeticity (six manifest paths matched, zero outside/unresolved writes); source-download
failures initially prevented fsstress and pjdfstest; the retry passes fsstress again and
reproduces CI's 3,595 pjdfstest failures exactly. The full Linux io_uring workspace passes
1,506 tests with 14 ignored; strict Clippy and `xtask check` pass on Linux and macOS.
This does not close pjdfstest, native FUSE conformance, or cooperative/incremental recovery
publication. The separate real FUSE coherence regression passes; it is not POSIX conformance.
Record: `docs/bugs/2026-09-19-nfs-ready-connection-starves-heartbeat.md`.

**CI follow-up (2026-09-19, §4.3):** a real io_uring rebind regression reproduces
`EADDRINUSE` after runtime shutdown; descriptor closure did not wait for pending kernel
polls to release their listeners. Retirement now cancels and drains requests, with the
required cancellation capability checked at boot. Linux CI asserts its intended driver.
The macOS timer-allocation failure counted other threads' work; controlled foreign
allocations reproduce the same defect in both `rt/tests/timer_allocations.rs` and
`mem/tests/no_alloc.rs`. Their counters are now thread-local and their original allocation
assertions are unchanged. Exact validation and limits are recorded in
`docs/bugs/2026-09-19-io-uring-retains-listener-after-shutdown.md` and
`docs/bugs/2026-09-19-timer-allocation-counter-includes-libtest.md`.

The full local workspace run also exposed interference between runtime test fixtures. A
registry stress test deliberately sent wakes to adjacent global slots owned by other tests,
which could fill their rings before their consumers started. It now owns a separate test
process with the same concurrent history. The driver test permits valid kicks during a timed
wait and releases its registration and worker on unwind. Record:
`docs/bugs/2026-09-19-driver-test-assumes-no-foreign-kicks.md`.

**Local CI regressions (2026-09-19, §4.3/§4.7 and AUD-02):** the Linux listener now uses
one-shot readiness on the control shard, eliminating the level-readable watcher loop.
A delayed-accept test fails on the old watcher and passes on the replacement; it also
serves later connections and shuts down. A timer-allocation regression reports 25,271
construction allocations before and four after for CI's 1,617,130 possible timers;
renewal, cancellation and expiry remain allocation-free. Unix descriptor kicks and
simulation kicks carry registry generations; foreign borrows are pinned through their
syscalls and retirement waits before closing/freeing. The forced-overlap regression
reproduces the former close-during-borrow. Windows IOCP ownership remains a separately
ledgered source finding. Record: `docs/bugs/2026-09-19-startup-wakes-and-kick-retirement.md`.

The FUSE coherence fixture omitted production's root-ownership stamp. It now uses the
mounting uid/gid. The old binary reproduces CI's `Permission denied` as uid 65534 locally;
the fixed mounted scenario passes in 0.13 s on the same kernel. Its cfg-free ownership
regression runs even in root-owned containers. The earlier root-run proof did not establish
ordinary-user access. Record: `docs/bugs/2026-09-19-fuse-coherence-fixture-root-owner.md`.


**Follow-up source audit (2026-09-14, `291907b`):**
[2026-09-14_AUDIT.md](../bugs/2026-09-14_AUDIT.md) records 18 open findings (14 P1,
4 P2) and one unconfirmed KIND rejoin explanation. Mount authorization/coherence and
cross-shard service: AUD-01–04 (GAP-A9-3/-4/-9/-11); publication and transaction recovery:
AUD-05–06 (GAP-A9-6); Raft restart, read authority and authenticated record acceptance:
AUD-07–10 (GAP-A9-7/-9); merge commit, holder fencing, quorum progress, ledger takeover and
retention: AUD-11–14/-16 (GAP-A9-14/-1/-7); SWIM indirect probes, call cancellation and
handshake bounds: AUD-15/-17/-18 (GAP-A9-7/-4/-11). These are source findings and proposed
regression scenarios at the audit baseline.

**Implementation follow-up (2026-09-19, AUD-01 closed; GAP-A9-9's NFS-authority leg closed):** every
volume is served over the loopback **only through a mount capability** — the attachment id and a
random 16-byte token the access-list-checked `verbs::attach` (and the green's `attach_green`) mints,
stores on the `AttachmentRecord` with the rights the attachment was granted (bounded by its intent),
and returns (`Attached.token`). The mount presents it in the `MNT` path
`/<name>@<attachment_hex>.<token_hex>` (or `/@<capability>` for the host root scoped to it); the daemon
stamps it into the root handle and every derived handle (file handle v2) and validates it on the owner
shard on every request against the record, so a handle self-authorizes on any connection and no state
is kept per connection (`crates/server/src/nfs.rs`). The `AUTH_SYS` uid is never authority: a bare `/`
lists nothing, a name without a capability mounts nothing, and an unbound client, a forged uid and a
wrong token are refused at `MNT`. The attachment is the mount's (`Consumer::Bridge`): it outlives the
attaching process and a daemon restart, and ends with the kernel's `UMNT`, a `detach`, or the volume's
destroy. `slates mount ID PATH [--read-only]` attaches as a host mount and mounts under the capability.
Proven by the NFS-socket regression, the recovery crash sweep (a pre-crash handle resolves after every
crash point) and the live kernel-mount CLI flow.
`docs/bugs/2026-09-19-nfs-bypasses-consumer-and-volume-authorization.md`,
`docs/bugs/2026-09-19-mount-capability-attachment-dies-with-its-client-and-the-daemon.md`.

**Conformance follow-up (2026-09-19, AUD-01 / GAP-A9-15):** the Linux root-NFS adapter now
obtains a durable host-mount attachment and supplies its capability, matching the CLI. The
adapter's actual export succeeds over the real daemon's NFS socket; teardown and an abandoned
mount leave no attachment, and the old capability is refused. The regression passes on macOS
(0.88 s) and Linux arm64 (1.13 s). The separate hermeticity startup failure is also reproduced:
ordinary strace stopped the daemon even on unselected allocator calls. The same write filter
with `--seccomp-bpf` allows startup; the live regression proves no startup restart and retained
observation of real file mutations. Both regressions run in the Linux conformance lane before
the mounted suites. Full mounted conformance remains pending. Records:
`docs/bugs/2026-09-19-linux-conformance-mount-authority.md`,
`docs/bugs/2026-09-19-hermeticity-tracer-stops-unselected-syscalls.md`.

**First complete Linux adapter run (2026-09-19, GAP-A9-15 still open):** job `105974090627`
passes fsx and all nine workloads. Fsstress loses daemon service to repeated heartbeat kills;
pjdfstest reports 3,595 unexpected failures; hermeticity observes four writes outside its
granted target, 20 unresolved descriptor annotations and a zero-write landing. Startup success
did not establish the full lifecycle. The separate reproductions and required assertions are
recorded in `docs/bugs/2026-09-19-linux-conformance-first-complete-run.md`.

**Implementation follow-up (2026-09-19, AUD-02 closed; GAP-A9-3/-4's FUSE-coherence leg):** the
FUSE serve loop delivers kernel invalidations at every wake — a kernel request or a `ChangeSignal`
another mutation source notifies (an `eventfd` the loop `poll`s beside the device) — so a change made
while the kernel answers from its unbounded cache is told without waiting for a request; and the
delivery is a pure discipline (`crates/bridge-fuse/src/coherence.rs`) whose cursor advances only past
what was gathered and written, a refused gather keeping it (counted) for the next wake and the
transport's own request moving it only after a whole round. Proven on every host with a recording
sink and on Linux over a real `fusermount3` mount (the cache proven warm by a `GETATTR` count, another
attachment's truncate seen through `stat` with no request, an injected gather refusal retried).
`docs/bugs/2026-09-19-fuse-invalidations-wait-for-a-kernel-request-and-a-refused-gather-loses-changes.md`.
Found by that mounted test on its first run — the FUSE serve loop's first run against a real kernel:
every attribute reply carried the volume's permission bits with no `S_IFMT` type bits, which a kernel
validates and answers by marking the inode bad (`EIO` on everything after), so every FUSE mount was
dead on its first operation; the wire mode is now composed from the seam's kind and permission bits,
and the kernel's type bits are stripped from `CREATE`/`MKDIR`/`FATTR_MODE` before the volume
(`docs/bugs/2026-09-19-fuse-attribute-replies-carry-no-file-type-bits.md`). And with the mount
working, the same test showed a delivered, accepted invalidation ignored: `FUSE_INIT` asked for
writeback cache, under which the kernel owns a regular file's size and times and neither re-fetches
nor takes the daemon's — a change through another attachment stayed invisible to `stat`; writeback
cache is now refused at negotiation, §4.6's list corrected
(`docs/bugs/2026-09-19-writeback-cache-made-the-kernel-the-size-authority.md`). The T-4.13 Linux
leg's status in §4.6 is corrected in the same change: it skips on the CI runner (`allow_other`).

**Implementation follow-up (2026-09-19, a recovery sibling of AUD-01):** the landing counter
(`LandingState::next_landing`) restarted at 1 on every boot while landing records are durable and
guarded against a duplicate id, so after a restart the next `land` was refused `AlreadyExists` once
per recovered record. The counter now boots past the partition's recovered landings
(`verbs::next_landing_counter`), as the attachment counter does; proven failing-first across a restart
over one anchor segment (`crates/server/tests/recovery.rs`).
`docs/bugs/2026-09-19-landing-counter-restarts-at-one-after-a-restart.md`.

**Implementation follow-up (2026-09-19, AUD-08 closed):** latest-state service is now fenced by a
confirmed **owner lease** (`crates/server/src/lease.rs`). An owner serves an object's live head,
head version, status, change list or mounted tree only while `f` of the object's other candidate
holders acknowledged its SWIM probes within the horizon-derived bound — measured on the
suspend-inclusive host clock from the probe's send time, less twice RFC 5905's clock tolerance,
under the installed configuration version — and no peer announced a newer version. Probes/acks
carry the announced version; the confirmation is fanned to every owner shard each period; the gate
refuses `LeaseUnconfirmed` (`NFS3ERR_JUKEBOX` at the mount), pinned immutable reads exempt. A
holder defers a departed owner's promotion until that owner's lease can have lapsed (quorum
intersection). Owed: forwarding a node's own volumes to successors after a same-id re-admission is
GAP-A9-7; the A-9 `FencedRegister` revalidation stands.
`docs/bugs/2026-09-19-latest-state-served-without-a-confirmed-owner-lease.md`.

**Implementation follow-up (2026-09-18, AUD-14 closed):** a taken-over green is materialized on
the successor from its own accepted merge records and held inputs — the catalog record (the record
value now names the green, its evidence policy and its owner), the origin and chain re-recorded
durably, the engine rebuilt and its head identity verified against the adopted record — so every
version reads, new work submits and retries meet their records through the public client on the
successor. A successor whose accepted prefix is shorter than the adopted head stays pending and
counted until the ledger-prefix transfer (GAP-A9-7):
`docs/bugs/2026-09-18-green-takeover-left-no-servable-chain.md`.

**Implementation follow-up (2026-09-18, AUD-11 closed):** at `f > 0` a submit's acceptance
waits for its version's merge record to commit at the quorum — the verb commits its effects but
records no completion and sends no reply until `resolve_accepted` runs for the placed version; a
retry meanwhile joins the wait; a cross-node forward polls the completion within the liveness
budget. Regression with inputs and acknowledgements withheld separately:
`docs/bugs/2026-09-18-submit-acceptance-before-fleet-commit.md`. The owner-loss retry lands with
AUD-14.

**Implementation follow-up (2026-09-18, AUD-16 closed):** the merge engine's rejected-result
cache is bounded in bytes by the derived green-chain cap (oldest evicted first, counted; an evicted
retry is judged again), its retained content history is accounted, folded oldest-first only as far as the retention
budget needs (never past the oldest version a live reader names) and charged to the shard's
budget as retention — secured before the verdict,
refused typed, settled after — with `charged == history + rejected` asserted by use:
`docs/bugs/2026-09-18-merge-rejected-results-and-retained-copies-unbounded.md`.

**Implementation follow-up (2026-09-18, AUD-06 closed):** a transaction whose record cannot be
made durable is rolled back — the partition re-derived from the segment's durable state, so the
effects and the completion record are gone together and a same-id retry re-executes instead of
reading a success from memory — and refused with the new typed `Refusal::Unpublished`; a maintenance
snapshot failing after a durable append is deferred and counted, the commit stands. Database and
by-use regressions (a restart over the same segment agrees):
`docs/bugs/2026-09-18-unpublished-transaction-served-from-memory.md`.

**Implementation follow-up (2026-09-18, AUD-15 closed):** the live SWIM path now runs the indirect
stage — a timed-out direct probe asks up to `k` (derived) relays nearest the target, the relay's answer
returns as an `IndirectAck` and is credited before the suspicion tick; bounded per-member queues,
probe tasks woken on traffic. By-use regression with a negative control (relays disabled → fails with
`acked=0`): `docs/bugs/2026-09-18-swim-indirect-probes-not-wired.md`.

**Implementation follow-up (2026-09-14):** AUD-03/-04/-05/-09/-10/-12/-13/-17/-18 have corrections
in this change: bounded incremental RPC framing and handshake confirmation, deadline-bounded
NFS routing and complete root gathers, publication coverage required before stability, fresh Raft read
confirmation, authenticated record/prepare owners, authority checks before merge recomputation,
independent ordered holder catch-up, and cancellation-owned cross-shard registrations. Commands and
results are in the audit's follow-up. These corrections address nine findings; their broader GAP-A9
rows remain open. In particular, overlays now refuse stability and recovery without retained base
witnesses, rather than silently losing their base or private changes. At that stage nine findings remained open; AUD-07 is corrected below.

**AUD-07 correction (2026-09-14):** every daemon start now uses a fresh voter id and begins
uninitialized. Explicit bootstrap names the current boot; a replacement joins the common prefix
once and is admitted through joint consensus. Group and authenticated sender checks fence Raft
traffic. The three-voter whole-RAM replacement then second-loss commit passed in 9.81 s;
143 cluster and 77 server tests passed. Ten audit findings now have corrections; eight remain open.
The final admission regressions also reproduce and correct replay over a newer learner fetch:
regional epoch 3 instead of 2, root version 5 instead of 3. Fetched read views now stay separate
from each group's deterministic fold. Bootstrap recalculates durability on every shard; a sole
admitted host cannot count absent replicas as protection (35 DB tests passed in 2.15 s).
Complete Raft retention across warm restarts, compacted-prefix transfer and full-message quotas
(GAP-A9-11), and operator recovery
after quorum loss remain owed. A single-region root currently has one representative voter, so
regional fault tolerance does not imply root-quorum survival. Details and exact commands:
[the voter-loss report](../bugs/2026-09-14-raft-voter-state-loss.md).
Configured DNS peers discover addresses and join automatically on local processes, bare-metal
hosts, VMs and Kubernetes through the same protocol. Discovering and enrolling unlisted nodes
still needs a bounded discovery-provider interface and a trust-enrollment protocol. The manifest
is presently the pinned roster. Chart readiness also lacks a committed-admission barrier, so it
cannot yet make rolling scale safe by waiting between replacements.

The separate rejoin task reproduced the retirement/session race and added an in-process correction;
a new-IP KIND run remains unproved here. AUD-07 now covers fresh-member voting safety; complete
Raft retention across warm restarts remains owed. No mount or pod was run. The rejoin design's three
further hardenings landed 2026-09-16 (`docs/bugs/2026-09-14-retirement-closes-the-same-id-restarts-serve-session.md`,
sibling sweep): a terminal transport fault on the probe session is `ProbeOutcome::Broken` and releases
the session (`fleet.probe.broken`), a dial still in its handshake is dropped at its peer's retirement
(`fleet.dial.stale_dropped`; proven by the retire-mid-dial, return-at-new-addresses test), and the
restart test asserts the returned node does not serve its predecessor's volume while the successor does.


Separate workspace work advanced HEAD through archive commit `540fb5b` and ledger fix
`d9cb6e5` during this pass. BUG-12 is fixed there with recorded before/after regression evidence;
BUG-13's reachability restriction is removed, but direct adoption-value/message-level checks
remain owed. This docs pass inspected those changes and the commit's reported tests without
rerunning them. Source findings retain their explicit `a1059ed` baseline.

All rows below are **open**. Their design is now specified in A-9; acceptance requires the
named behavior tests and relevant original phase gates. Source findings BUG-1–BUG-14 are in
[the audit](../bugs/2026-09-05-system-contract-audit.md). Hecate applicability and deliberate
departures are in [the contract review](research/hecate-contract-review.md). No new measurement
or executable regression was performed by this docs change.

| Id | Gap and source finding | Design contract | Closure gate / owning phase |
|---|---|---|---|
| GAP-A9-1 | Per-host admission is all-cost on one code path (2026-09-13): content charged at its buddy block, snapshot retention charged from unpromised capacity by the retaining operation (refused typed before mutation, balanced through destroy and recovery), metadata laid out against the class and every volume's records reserved from a per-shard ledger; effective capacity clamped to the OS/job/cgroup bound; mapped and usable reported; an admitted claim protected through resize and recovery; proven by the charge oracle (150 histories) and AC-2.11's neighbour test. **Pressure signal DONE 2026-09-21** (a hold on each shard's byte budget, sampled from `memory_available_now` at the liveness cadence and fanned as the host shortfall's per-shard share; shrinks admittable only, never a committed claim, so a raised hold refuses a new create `BudgetExceeded` while an admitted volume's within-entitlement writes land — `crates/server/tests/nfs_mount.rs`, the pure budget unit; admission.md §5.5). Owed: the Windows job-object bound, guest and open-reference bytes (docs/wip/admission.md). (Was: locked flag without locked store, mapped-vs-usable capacity, dynamic allocations competing with bounded claims (BUG-1–3); uncharged metadata/transient/retained bytes could defeat the cap.) | §4.2: atomic all-cost per-host admission; disjoint shard credits; protect outstanding entitlement through resize, pressure and recovery. | AC-0.10/T-0.10; AC-2.11/T-2.13; Phases 0/2. |
| GAP-A9-2 | Live bases and immutable complete snapshots conflated; remote delta-only clone drops untouched state; base capture not implemented. | §4.4/§4.15/§4.10: retained BaseRef and explicit coverage; stable-source requirement for complete atomic capture. | AC-1.16/T-1.20; AC-8.21/T-8.19; Phases 1/8. |
| GAP-A9-3 | **Swept 2026-09-14 (`991c84e`..`b4eeb2d`)**: base lookup loads the listing on demand and every metadata mutation copies up through the base plane; the FUSE flags are checked against the kernel header by 18 independent vectors (which found and fixed `flags2` never negotiated); READDIRPLUS/FSYNC/LINK dispatch, every setattr field and rename flag honoured or refused `EINVAL`, statfs truthful to the shard budget, invalidations delivered from the journal and watcher hints with a bounded lifetime for live base entries. Still open: the mounted conformance run in the Linux lane (the FUSE loop's first compile is CI's) and the owner fields of base entries. (Was: base lookup depends on listing, FUSE flag mismatch, undispatched advertised operations, ignored metadata/rename flags, false statfs (BUG-5–10); coherence delivery and metadata copy-up need sweep.) | §4.5–§4.6: complete shared operations, independent ABI checks, truthful capacity, real invalidations and mounted conformance. | AC-1.17/T-1.21 (by use on one host: closed); AC-3.10/T-3.13 (edge and vectors: closed; mounted: Linux lane); Phases 1/3. |
| GAP-A9-4 | **Closed 2026-09-21** (the 2026-09-14 pieces stand: bounded generation-checked handles, the deadline-bounded helper handshake that reaps or cancels on every exit, generations with in-flight pins and a barrier refusing typed `BarrierIncomplete`). Now: the daemon holds **one attachment registry per shard** every transport rides — a mount's request is admitted under the registry attachment its capability maps to (admitted on first use, revoked and drained when the catalog attachment ends), the NFS export serves through the shared registry (`Export::over`), and a guest device is admitted into it (`admit` takes the owner's registry; the device keeps its id, its service passes and terminal step take the registry through `BridgeAccess`); `snapshot` runs the barrier over the volume before freezing the root and its reply **says what it covers** (`SnapshotCoverage { boundary: Complete | ServerVisible, attachments_closed }` — server-visible over an NFS mount, whose client buffers acknowledged writes until its `COMMIT`); the FUSE writeback flush has no work left (writeback cache refused at `FUSE_INIT`, 2026-09-19); and a host mount **binds its mount point** (`BindMount` → `AttachForm::ChosenPath`, §4.4 `Binding → Bound`; `slates mount` binds after `mount_nfs`; `status` lists bound mounts under a budget). Proven by use over the real NFS socket (`crates/server/tests/nfs_mount.rs`: coverage complete/0 → server-visible/1 → complete/0 with the mount's life; a bound mount listed, another principal and an SDK attachment refused, the detach unlisting it), the device suites over the shared registry, and the live kernel-mount CLI flow (`status ID` names the mount point while mounted, none after `umount`). (Was: attachment record is not a mounted path; dirty client caches lack a seal barrier; handles grow and helper error exits can orphan children (BUG-4/14).) | §4.4/§4.6: authenticated binding, ready path/device, barriers, generations, bounded drain/revoke/reuse. | AC-3.11/T-3.14 (seam: closed; kernel-buffered writes: Linux lane); AC-3.12/T-3.15 (closed on this host); Phase 3. |
| GAP-A9-5 | Device half **built** (2026-09-13): the owned FUSE-over-virtio device with device admission, per-attachment credits, refusal-before-access on malformed chains (T-4.14) and the owning-shard loop, wired into the daemon; DAX not advertised. **Report and container halves built (2026-09-14):** `attach`/`status` carry the six §4.6 A-9 facts for every transport, the guest transports from the device's own report; `AttachmentUnsupported{transport, reason}` and `ChosenPathUnavailable{reason}` are `Refusal`s raised before any effect; the OCI form is a verified bind of the host mount, recorded (`AttachForm::Oci`) and handed back as the runtime's `mounts` entry, proven by T-4.13 on macOS over Docker Desktop with a CI Linux variant over FUSE (`docs/wip/oci-handoff.md`). Still open: a real VMM binding (libkrun in-process, vhost-user inherited descriptor — the seam is built, the bindings are not), the guest form's durable `AttachmentRecord`, a live guest for AC-9.7, and the bind on Linux once the daemon serves the FUSE mount. | §4.6: owned FUSE-over-virtio/custom-runtime seam, host and guest containers, device admission, immutable-only isolated DAX if offered. | AC-4.11–4.12/T-4.13–4.14 (device leg proven with the simulated driver; the OCI leg proven by use on macOS and cross-linted for the Linux lane; live guest open); Phase 4. |
| GAP-A9-6 | Daemon-restart content recovery implemented and proven (BUG-11 closed, 2026-09-14): a scratch volume's bytes, roots and snapshots are captured into anchor-owned RAM at every barrier — control verbs inside their completion transaction, mount-transport mutations before their stability reply — and rebuilt on restart; the catalog is the recovery authority, so an image a crash left ahead of the log is trimmed to what was acknowledged (unrecorded snapshot dropped, unacknowledged resize's quota reverted, in-flight destroy completed, clone pins reconciled). Proven by `crates/server/tests/recovery.rs` (byte-identical survival over the NFS transport; crash at every durable step resumes to a clean reference). The base plane is imaged with its overlay (A-48), and its validated source identity is recovered (`an_overlay_recovers_its_private_state_and_refuses_source_drift`); through the daemon, `an_overlays_copied_up_and_created_files_survive_a_restart_over_their_base` (2026-10-03; red with the publication's host withheld). A refused publish refuses its verb typed and rolls it back (AUD-05). **The in-place refinement landed 2026-10-03 (A-64).** It closes the measured latency gap (every barrier re-imaged the shard's content: 0.29 ms a MiB, 75 ms per barrier at 256 MiB, paid by every FUSE `close`). Content now lives once, in each shard's arena range of the anchor's content object, and the image names blocks: 0.11–0.16 ms per barrier at 256 MiB (BENCHMARKS.md, `publish_bench`). Deferred frees keep the committed image's blocks from reuse; recovery claims and sweeps them; the shard relieves a deferral-short arena by publishing before work. A write, truncate or edit refused partway had dropped the file's body; fixed (`docs/bugs/2026-10-03-a-write-or-truncate-refused-partway-dropped-the-files-body.md`). A recovered clone now shares its origin snapshot's records (`Volume::clone_from_image`, rebuilt in lineage order; `1ca4804`): rebuilt copies had leaked every unchanged record at the clone's destroy (`a_recovered_clone_and_its_origin_leave_nothing_behind_when_destroyed`, 3 records left before the fix; `a_clone_recovers_over_its_recovered_origin_serving_inherited_and_its_own_files` through the daemon, red without the lineage path). Owed: a recovered snapshot's directories are still rebuilt privately (the live snapshot shares them with the head); freed on drop, so RAM only. (Was: restart reconstructed empty scratch contents and lost local snapshots.) | §2.6/§4.8/D-18: recover bytes, roots, witnesses, source identity, rights and capacity with effect/completion publication. | AC-2.12/T-2.14 met; base plane met (A-48); in-place refinement met (A-64); Phase 2. |
| GAP-A9-7 | BUG-12 acceptance-epoch fix and BUG-13 reachability correction landed separately in `d9cb6e5`; complete adoption-value/extension checks, message faults, read authority and real configuration core remain unestablished. | §4.8: accepted/proposed/promise separation, exact historical prefix, distinct quorums, safe leases and bounded epochs; configuration only on cold changes. | AC-8.18/T-8.16; AC-8.20/T-8.18; Phase 8. Model refinement/revalidation also owed, not run. |
| GAP-A9-8 | Identity-only replication, mirror lag in records, no byte-complete holder admission, repair or cross-region service. | §4.10/§4.16: verified placed reference graph, real holder capacity, atomic generations, recomputation and time lag. | AC-8.19/T-8.17; original fleet gates; Phase 8. |
| GAP-A9-9 | Enrollment and the grant issuer are built to the A-8 specification for one host: consumers are trusted enrollments under an account with scoped rights, channels bind by a delivered capability (never by uid or channel class), rights are checked before any lookup or effect, and grant authority is a verified capability. The harness delivery channel is built (2026-09-14): the capability travels on an inherited descriptor — a close-on-exec pipe whose read end one child inherits (cleared in the forked child on Unix; a `CreateProcessW` handle list on Windows), named by `SLATES_CONSUMER_FD`, taken once and refused typed when absent, not inherited, the wrong kind, the wrong length, corrupt or consumed — the client binds at connect and again by itself after a daemon restart, and `slates run` is the harness verb (with `enroll`/`revoke`/`share`). Still open: the cross-host consumer scope on the transport, MCP roots against the access list, and the Linux human surface (the anchor's segment is a descriptor only its children hold). | §4.13: trusted consumer enrollment, scoped rights before effects, protected grant issuer and manifest-bound approval; private content sharing scopes. | AC-2.13/T-2.15; AC-5.10/T-5.12; Phases 2/5. |
| GAP-A9-10 | No MCP/SDKs; CLI ids/manual root setup, partial help/output and no grant issuance; schema parity and actual path readiness incomplete. | §4.12: one operation descriptor; scoped names, consistent JSON/help/errors/cursors; discoverable native/guest flows and exact capability status. | AC-5.9–5.11/T-5.11–5.13; Phase 5. |
| GAP-A9-11 | Separate frame classes do not bound CPU, device, arena or full-object transfer costs; resumable helper is not bounded end-to-end ingest. | §4.2/§4.9: all-resource QoS, bounded quanta/credits, verified named and unknown-length transfer, cancellation and release. | AC-0.10/T-0.10; AC-7.7/T-7.8; Phases 0/7/8. |
| GAP-A9-12 | Closed registry, distinct identities with type-enforced causation, and typed loss/absence markers built and proven by use single-node (`e18619a`); open: cross-node trace propagation and the fleet/archive emitters (§4.8/§4.10 integration). (Was: signal names/absence and request-vs-trace causation not enforced end to end; nine spans were called seven.) | §4.14: closed registry, absence/freshness semantics, distinct identities and telemetry loss markers. | AC-0.11/T-0.11; Phase 0 foundation with surface/fleet integration. |
| GAP-A9-13 | **Built single-node (2026-09-14, `docs/wip/clean-digest.md`):** verified current digest only (`Overlay::digest`, the `Digest` verb on the wire/CLI/MCP); invalidated before any mutation (`copy_up`, `base_forget`, listing refresh); bounded cache discovery (`Store.digests`, `digest_capacity`, typed counted `DigestCacheFull`); watcher hints backed by revalidation, overflow drops all. Owed: cooperative slicing of the hash, sealed-content digests, SDK exposure. (Was: clean-file digest export and bounded cache discovery not implemented; stale source knowledge must not imply clean content.) | §4.15: verified current digest only; invalidate before mutation; watcher hints backed by revalidation. | AC-1.17/T-1.21; Phase 1 core and bridge integration. |
| GAP-A9-14 | Service-level Work/Green roles, CLI/MCP flow, pinned attachments and distributed recomputation **built** (2026-09-13, `docs/wip/merge-service.md`); the mounted work/green (T-6.15's mounted client, the bridge's `EROFS`) and the extent-backed chain remain: a work is not a VFS volume yet. (Was: pure merge core lacked service-level Work/Green roles, CLI/MCP flow, pinned attachments and distributed recomputation.) | §4.16: complete immutable green base, writeback barrier, declared operations only, input retention and placed-before-reference. | AC-6.13/T-6.15; AC-8.19/T-8.17; Phases 6/8. |
| GAP-A9-15 | Evidence surface built (2026-09-14, `docs/wip/conformance.md`): a typed record per transport × suite rendered as a doc-truth matrix, with `cargo xtask conformance` driving the real binary over a real mount and a CI `conformance` lane; on macOS NFS fsx and fsstress pass, the workloads differ by two declared NFS limits (AppleDouble sidecars, SQLite WAL) and pjdfstest ran unprivileged (3,348/3,331/2,007) with its failures categorized, none reviewed as expected; Linux runs as a LIMITED root-NFS adapter, Windows/virtio-fs/OCI and the pressure/failure suites are typed skips. Historical overstatement is closed by construction; the transport guarantees themselves remain open. (Was: native/guest POSIX, strict RAM residency and grant-only disk effects lack complete transport-specific evidence; historical stages overstate coverage.) | §4.6, Part 6 and EQUIVALENCE.md: explicit semantics/boundaries; report skipped lanes and limited adapters honestly. | AC-9.7/T-9.1 plus original conformance/workload gates; Phase 9, prerequisite gates run in their owning phase. |

**Additional decisions/evidence owed.** Phase 4 must establish the supported VMM/device seam,
immutable mapping capability and guest cache residency for each host; no guessed support
matrix. Phase 1/4 must establish which read-only source facilities can produce complete atomic
bases without a write or privilege, otherwise refuse that request. Phase 2/5 must establish
the protected enrollment/confirmation channel with the harness; a uid-only demonstration
cannot close it. Derived headroom/retention bounds and provisioning costs need new measured
records; the formulas in the design are contracts, not measurements.

**Prohibited-path review.** Old research recommendations of parallel pure-TS SDK fallbacks,
FSKit compatibility shims or automatic degraded bridge substitution are not authorization to
implement them. Remove such runtime paths if found; requested semantics must be met or refused.
A limited NFS adapter cannot satisfy the full POSIX contract by expanding an expected-failure
list. No subagent, external runtime, checker installation, privilege or disk write is approved
by this ledger.

## 8j. Phase 4 groundwork — the NFS loopback bridge wire codec (2026-09-05)

The macOS NFSv3 loopback bridge (§4.6, D-2 — the FSKit fallback for macOS 14.4/older and where FSKit is disabled, and the differential oracle against the other bridges) began 2026-09-05 as `slates-bridge-nfs` (`crates/bridge-nfs`), the wire codec first, because it is pure and directly confirmable on every host with no socket and no mount, exactly as the FUSE ABI codec was (§8e). Two modules. `xdr` (External Data Representation, RFC 4506) is a bounds-checked sequential reader and writer — big-endian, four-byte aligned; a variable opaque or string is a length then the bytes then the padding; a declared length is checked against a caller-supplied cap and the bytes that remain before any allocation, so a hostile length is a typed `XdrError`, never an allocation or a panic. `rpc` (ONC RPC, RFC 1057/5531) is record marking over TCP (a four-byte per-fragment header, the last-fragment flag plus the length), the call and reply messages, and the `AUTH_NONE` credential the loopback server sends (it trusts the peer of a socket only it created and the kernel connects); `read_record` distinguishes a partial stream (`Incomplete` — wait for more bytes) from a malformed one and refuses a fragment past the message cap (`RecordTooLarge`) before accumulating, `parse_call` reads the call header (program, version, procedure; the credentials validated for shape and skipped) and positions a reader at the arguments, and `reply_bytes` builds an accepted reply. Every wire number is a `size_of` or a named `Format` constant (the RFC message-type and accept-status values, the record-marking flag and length mask), so the literal check stays clean; the unsafe budget is 0.

Gated (`crates/bridge-nfs/tests/wire.rs`, 11 tests, every host): XDR scalars round-trip; a variable opaque is length-prefixed and padded and the reader skips the padding; a string round-trips and non-UTF-8 is refused; a hostile length and a truncated buffer are refused before allocating; a record frames and deframes with the right consumed count; a partial stream is `Incomplete`; an oversized record is refused; a NFSv3 NULL call header parses to its program, version and procedure; a non-call is refused and garbage does not panic; and a successful reply and a program-mismatch reply build to the exact bytes the kernel expects (golden vectors).

The NFSv3 core data types followed (`nfs.rs`, RFC 1813): the status codes (`Nfsstat3`), file types (`Ftype3`), timestamps (`Nfstime3`), the fixed 84-byte attribute structure (`Fattr3`), optional post-operation attributes (`PostOpAttr`), and the file handle (`Nfsfh3`, opaque capped at the 64-byte `NFS3_FHSIZE`), each with XDR encode and decode and every wire value a `#[repr(u32)]` discriminant or a named `Format` constant. Gated (`crates/bridge-nfs/tests/nfs.rs`, 5 tests, every host): `fattr3` and `post_op_attr` round-trip, a file handle round-trips and one past the cap is refused, an unknown file type is refused, and the status and time wire values are golden.

The file-handle codec followed (`handle.rs`), the design's `(volume, inode no, gen)` identity encoded into the opaque `nfs_fh3`: a version byte then the 16-byte `VolumeId`, the inode number and the generation big-endian, 33 bytes, within the 64-byte `NFS3_FHSIZE`. NFS is stateless, so the handle names its object with no server-side table across daemon restarts; the generation makes a reused inode a distinct handle (a stale handle is refused, not answered from whatever now holds the number), and the version byte refuses a handle from an incompatible build rather than misreading it — the discipline `crates/db/src/catalog.rs` applies to Wire types and the Linux in-kernel NFS server applies to its handles (evidence C). Gated (`crates/bridge-nfs/tests/handle.rs`, 5 tests, every host): a handle round-trips within the size cap, its encoding is golden, a reused inode with a new generation is a different handle, and a wrong length or an unknown version is a typed refusal. 21 bridge-nfs tests total.

The MOUNT protocol (`mount.rs`, RFC 1813 Appendix I) and the minimal portmap responder (`portmap.rs`, RFC 1833) followed — the two helper RPC programs the server answers alongside NFS. MOUNT: the `mountstat3` status codes, the `MNT` request path (capped at `MNTPATHLEN` = 1024), and the `mountres3` reply as a `MountReply` enum (correct by construction — a success carries the root file handle and the accepted auth flavors, a failure only its status). Portmap: the `mapping` argument and the bare-port GETPORT reply; slates passes explicit `port`/`mountport` options so a client need not query it, but the responder exists for those that do. Gated (`crates/bridge-nfs/tests/mount.rs`, 4 tests, every host): a successful MNT reply carries the handle and flavors, a failure is only its status, a mount path round-trips and one past the cap is refused, and a portmap mapping round-trips with the bare-port reply. 25 bridge-nfs tests total; the crate's pure wire surface (XDR, ONC RPC, NFSv3 types, file handle, MOUNT, portmap) is complete and confirmable on every host with no socket.

The NFSv3 procedures began over the shared operation layer (`procedures.rs`, §4.6, Phase 4 task 3), the NFS analogue of the FUSE `dispatch`: an `Export` holds a `&mut dyn Bridge` (the `slates-bridge-core` seam the FUSE mount also uses) and a `VolumeId`, decodes a file handle to an inode (a foreign-volume handle is `NFS3ERR_STALE`, a malformed one `NFS3ERR_BADHANDLE`), calls the same `Bridge`, and mints handles for the objects it returns — stateless, no server-side open table. This slice serves the metadata walk a client does first: MOUNT `MNT` (the export's root handle from `Bridge::root`), `NULL`, `GETATTR` and `LOOKUP`, with the neutral translations `NodeAttr` → `fattr3` and `VfsError` → `nfsstat3` (the same neutral error the FUSE edge maps to an errno). Gated (`crates/bridge-nfs/tests/procedures.rs`, 3 tests, every host, no socket): MNT → LOOKUP → GETATTR walks a scratch volume through a real `VolumeBridge` and GETATTR names the same inode LOOKUP returned; a foreign or malformed handle is a typed status; a missing name is NOENT. 28 bridge-nfs tests. A design fork is owed on READ and WRITE: §4.6 keys them by inode ("requests carry (volume, inode no, gen)") but the current `Bridge` keys them by an open handle, so serving stateless NFS read/write wants either an inode-keyed read/write on the shared seam or an NFS-side inode→handle cache with GC. The post-mount query procedures followed — ACCESS (grants the requested access; slates is not a sandbox, a non-goal), FSSTAT (the volume's space from the seam's `statfs`) and FSINFO (static transfer sizes matched to the arena chunk, the maximum file size, one-nanosecond time granularity, and the link/symlink/homogeneous/cansettime capabilities); 29 bridge-nfs tests. The rest of the namespace and directory procedures (READLINK, CREATE, MKDIR, REMOVE, RMDIR, RENAME, SYMLINK, READDIR/READDIRPLUS, SETATTR, PATHCONF, COMMIT) and the RPC record/program routing over the socket follow.

Owed (the rest of Phase 4): the TCP loopback listener held by the anchor; the NFSv3, MOUNT and portmap procedures over the volume core (LOOKUP, GETATTR, ACCESS, READ, WRITE, READDIR/READDIRPLUS, CREATE, MKDIR, REMOVE, RMDIR, RENAME, SYMLINK, and the file handles that name volume inodes); the root mount at an existing user-owned mount point via `mount_nfs` (non-root, no kernel extension, no privilege — R10); the attribute-cache timeout derived from the loopback round trip; and the differential-oracle harness comparing this server against the FUSE bridge over the same volume. FSKit remains the primary macOS 26+ path (§4.6, D-2); this NFS server is the fallback and the oracle, and its wire codec is reusable whichever native bridge is built next. The real mount runs in the macOS lane.

The socket and the server followed, closing most of that owed list. First the blocking `serve_connection` (`src/server.rs`) — the transport loop behind the codec: it reads ONC RPC records off any `Read + Write` stream, dispatches portmap/MOUNT/NFSv3 onto an `Export`, and writes framed replies; `tests/loopback.rs` mounts `/` and reads a seeded file back byte-for-byte over a real socket in CI, and the `nfs_loopback` example serves a real `mount_nfs` (a live kernel mount of a RAM-only volume — no signing, no kernel extension, no privilege beyond the mount, R10). Then the **production async server** (2026-09-09): `serve_connection_async` serves a connection over slates's own runtime, reads and writes awaiting the shard's driver through the rt's new async `TcpStream` (§4.3 — `Driver::register_writable`, the `EVFILT_WRITE`/`EPOLLOUT` sibling of `register_readable`, and `tcp::{TcpListener, TcpStream}`; `TcpStream::write_all` awaits write-readiness so a stalled client yields the shard rather than blocking it), sharing the RPC engine (`dispatch`) and record codec with the blocking form — one engine, two transport adapters, not a second path. A volume is `!Send` (it holds a `Box<dyn Clock>`), so the serve loop reaches its shard by the daemon's own idiom (a `Send` boot task through `spawn_on`, then `futures::spawn` for the non-`Send` loop). Proven by use in CI with no privilege: `tests/async_loopback.rs` mounts and reads a seeded file back byte-for-byte from the async server on the runtime, driven by the same hand-rolled ONC RPC client as the blocking test; the `nfs_async` example serves a real `mount_nfs`.

Multi-volume routing followed (2026-09-09), the first step toward the design's single-root-mount model (§4.6 line 128, "the single kernel mount point per host under which volumes appear as directories"): the server was one volume at `/`, and `Export::mnt` itself named "the path-to-volume resolution of a multi-volume export is owed". `MultiExport` (`src/multi.rs`) now serves many volumes from one server, routing each request to the volume its file handle names. It needs no table: every served NFSv3 procedure begins with a file handle, and the handle already encodes `(volume, inode, gen)` (`handle.rs`), so the router reads the leading handle's volume id through a fresh reader (a new `XdrReader::rest`) and hands the *untouched* request to that volume's `Export`, which re-decodes and validates the handle exactly as for a single volume; a handle for a volume the server does not hold is `NFS3ERR_STALE`. The seam is `NfsService` (`serve_mount`/`serve_procedure`), implemented for both `Export` and `MultiExport`, and `dispatch`/`serve_connection`/`serve_connection_async` now work over `&mut dyn NfsService` — so one server serves one volume or many with no transport change, and the existing single-volume callers coerce unchanged (all prior tests green). Proven by `tests/multi.rs`: one server, two independent volumes, a client mounts each by name over one connection and reads its file, and the bytes never cross (the routing proof). The **synthetic root directory** followed, completing the design's single-root-mount model at the NFS layer: `MultiExport` now serves a read-only root whose entries are the volumes, so one `MNT /` lets a client `ls` the volumes and `cd` into any. `MNT /` returns the root handle; `GETATTR`/`ACCESS` describe it; `READDIR`/`READDIRPLUS` list the volume names (budgeted against the client's `count`, `NFS3ERR_TOOSMALL` if it cannot hold one entry); `LOOKUP` a volume name returns that volume's own root handle and attributes (via a new `Export::root_object`), so descending into it crosses into the volume (a distinct `fsid`, as at any mount point); `FSINFO`/`FSSTAT` answer for the pseudo-filesystem; and every mutation, `READ` and `READLINK` on the root is a typed refusal in the failing procedure's own reply shape (`NFS3ERR_ROFS`/`ISDIR`/`INVAL`), so the stream never desynchronises. Proven by `tests/multi.rs`'s browse test over a real socket: `mount /` → READDIRPLUS lists `alpha`+`beta` → LOOKUP `alpha` → LOOKUP `hello.txt` inside it → READ, byte-for-byte.

The serving core was then reshaped to the **daemon's storage model** (2026-09-09): the first cut held an `Export` per volume, each owning its own store — but a shard holds many volumes sharing *one* store, so a volume must be served through a *transient* `VolumeBridge` built per request (the design's "marshal each operation into the bridge queue of the owning shard", §4.6 line 1341; the shape `bridge-fskit`'s `MountSession` already takes). A `VolumeSet` trait is that seam — `entries`, `serve(volume, …)`, `root_object(volume, …)` — and `MultiExport<V: VolumeSet>` carries the routing and the synthetic root above it. The daemon will implement `VolumeSet` over its `ShardState` (one store, a volume slab); a test implements `OwnedVolumeSet` (one store, several volumes), so the shared-store path the daemon uses is what the tests now drive — `tests/multi.rs` puts two volumes in one store (distinct inode prefixes, as a shard assigns) and both the routing and the browse test pass unchanged. The daemon-side wiring followed, and the daemon now serves NFS over its own provisioned volumes: `crates/server/src/nfs.rs` binds a loopback listener at boot (port on `Daemon::nfs_port`), serves it on the control shard, and `ShardVolumeSet` implements `VolumeSet` over the shard's `ShardState` — a request resolves its volume through `state::with_state` and is served by a transient `VolumeBridge::attached` (the daemon's serve path: it lends the volume slot's base host and a fresh handle slab, since NFS keeps no open state across requests). Each connection is a detached task, so connections are concurrent. Proven with no privilege by `crates/server/tests/nfs_mount.rs`: a single-shard daemon starts, a client provisions a volume through the real rendezvous, then over the daemon's NFS port a client mounts it, **creates a file, writes bytes, and reads them back** — client → NFS → `ShardVolumeSet` → the shard's real volume and back, byte-for-byte. The **cross-shard bridge queue** followed (§4.3, D-7 "bridge queues pinned to the owner"), so the daemon serves volumes on *any* shard, not just the accepting one: a request naming a volume this shard does not own is routed by the volume's owner partition (`verbs::owner_of` mapped to a shard) to run the same `serve_call` on the owner shard — spawned there exactly as the client path forwards a verb (`Control::Spawn`) — and the owner spawns a task back on the accepting shard that hands the reply to the awaiting connection task through a per-shard, thread-local pending map (no new runtime primitive, no lock; a lost-wake-safe slot on a single-threaded shard). `tests/nfs_mount.rs` proves it: a two-shard daemon mounts a volume that lives on a shard other than the NFS listener's and writes then reads a file back byte-for-byte over the bridge queue. And `cd`-ing into a volume from the single host root spans shards: a root `LOOKUP` routes by the looked-up name (a volume id in hex), so `mount /` then `cd <id>` reaches a volume on any shard (`nfs_mount.rs` `a_client_mounts_the_host_root_and_reaches_a_remote_volume_by_id`). And the host root's *listing* now gathers every shard's volumes: a root `READDIR`/`READDIRPLUS` scatters an entry-gather to each other shard and lists them all, so `mount /` then `ls /` shows every volume on the host (`nfs_mount.rs` `the_host_root_listing_gathers_volumes_from_every_shard`; a remote volume's per-entry attributes are absent in READDIRPLUS, filled by a cross-shard `LOOKUP`). **The whole browse — `mount /`, `ls /`, `cd <id>`, read/write — spans shards.** And each request runs as the mounting user: the daemon reads the uid from the call's `AUTH_SYS` credential (`subject_of` over bridge-nfs's `auth_sys_uid`; `AUTH_NONE` → root, §4.13), and the subject rides to the owner shard on a cross-shard call (`bridge-nfs/tests/auth.rs` covers the parse). Now owed on this path (minor refinements): a friendly chosen-path mount name (the id's hex is used now, §4.6 "Chosen path"); the anchor-held listener for restart survival (§4.6, line 509; the daemon binds it now); the attribute-cache timeout from the measured loopback RTT; and the differential-oracle harness against the FUSE bridge over the same volume (the root-listing gather now fans out to the shards in parallel).

## 9. Blocking order toward first light

1. Correct capacity/residency admission and acknowledged-content recovery (GAP-A9-1/6).
2. Complete base routing, FUSE semantics, barriers and owned attachment teardown (2/3/4).
3. Establish trusted consumer/grant boundaries and a usable local CLI flow (9/10). First light
   means a real tool at an actual attached path with correct bytes and isolation, not an
   attachment row. It does not by itself certify full POSIX or fleet durability.
4. Add the virtio-fs/VMM and OCI attachment forms over that same core (5), then the native
   platform-specific gates. Complete Work/Green and MCP/SDK flows with their phase prerequisites.
5. Retain the fixed takeover regression and complete the protocol oracle before fleet wiring (7), then
   verify byte placement, capacity, mirroring and remote bases (8), with bounded transfer/QoS.
6. Close transport-specific conformance, workload, fault, residency and release evidence (15).

These are dependencies for implementation, not work authorized by the documentation request.
Every phase retains its original acceptance gates plus the A-9 additions.

## 10. Model-checking record (A-6)

| Model | Configuration | Result | States | Depth | Date |
|---|---|---|---|---|---|
| `models/Reconfig.tla` | Old {a1,a2,a3} → New {a2,a3,a4}, three records | no error (ReadSafety, NoLoss, TypeOK) | 20,478 distinct | 19 | 2026-09-04 |
| `models/FencedRegister.tla` | 3 holders, 2 hosts, 2 epochs, 2 records per epoch | no error (TotalOrder, Continuity, StaleNeverCommits, ReadSafety, TypeOK) | 1,432,929 distinct | 27 | 2026-09-04 |
| `models/FencedRegister.tla` | 3 holders, 2 hosts, 3 epochs, 2 records per epoch | not completed: stopped after 3 h 7 min with 83 GB of queued states on disk; needs symmetry reduction (TLC symmetry sets over Acceptors and Hosts) and a bounded record alphabet before it is feasible; the two-epoch result stands (one takeover plus a resumed stale owner) | — | — | 2026-09-04 |

The models are architecture artifacts, not CI jobs. These historical runs apply only to the
listed models and configurations. The Rust simulations are evidence with known gaps, not a
proof or established refinement (BUG-12/13). A-9 corrects §4.8; refinement/revalidation remains
owed before GAP-A9-7 closes. Running a checker requires separate explicit tooling authorization;
none ran for A-9. Any authorized rerun must bound work and keep state outside the project tree: TLC's disk-backed state queue otherwise lands in `docs/wip/models/states`, which
is what happened on 2026-09-04 (83 GB, removed).

Two modelling bugs were found and fixed before the runs passed: the first draft let an owner issue two
different records for one sequence number (a model error, not a design error), and its Fencing
invariant was stronger than Paxos promises (a record partially acknowledged before a promotion may
still complete; the correct property is Continuity: the successor's base is at least as new as any
such record). Both are recorded so the implementer knows exactly what the models guarantee.


### 2026-09-15: retained voters, explicit recovery, enrollment and overlay images

AUD-07's warm-state gap is implemented: both Raft groups and their voter identity survive in
bounded anchor publications, with changed state retained before a reply can escape. Complete
corruption refuses; unfinished replacement keeps the last completed publication. Whole-anchor
loss still requires a fresh voter identity and surviving quorum admission.

Explicit quorum-loss recovery is implemented through a reviewed retained-copy digest, a human
proof and fencing/loss acknowledgements. Recovery preserves the selected committed application
view under a new genesis; separately authorized survivors retain their old copy suspended until
validated replacement import. A node-specific read-only recovery key makes approval usable from
an unrelated CLI process on every platform. It does not grant landing or consumer authority.

Unlisted-node enrollment is implemented with optional operator CA roots, signed region/domain
scope, exact-leaf outbound authentication, bounded roster pages and runtime-derived peer capacity.
Configured DNS seeds continue resolving at each fresh dial. Retained discovery rosters survive
warm restarts without partial republishing. Neither discovery nor an outage bootstraps a group.

GAP-A9-6's overlay image slice is implemented: source-directory identity, witnesses, whiteouts,
redirects, private large-file windows and snapshots survive validated reacquisition. Source changes
refuse; source handles are released after failed reconstruction. Independent overlay-clone host
ownership, client open-handle handoff and complete compacted Raft transfer/message quotas
(GAP-A9-11) remain open. This entry does not close those broader gaps or the A-9 model refinement.

Linux arm64: 85 server unit tests, 143 cluster tests, 69 VFS tests, 8 daemon tests, 3 recovery
histories, 2 live fleet histories, 1 certificate-impersonation test and 1 portable recovery CLI test
passed. `cargo check --offline --workspace --all-targets` passed in 29.60 s. macOS workspace
Clippy with `-D warnings` passed in 7.52 s; structural, literal, unsafe and version gates passed.
The cached Linux image has no Clippy component, so Linux compilation supplements host Clippy.

Commands, red/green results and precise limits:
[consensus recovery](../bugs/2026-09-15-consensus-recovery.md),
[enrollment](../bugs/2026-09-15-unlisted-node-enrollment.md),
[overlay recovery](../bugs/2026-09-15-overlay-recovery.md).
The new-IP whole-pod gate and its result are in [the KIND record](kind-lane.md).

KIND verification on 2026-09-15 passed: whole-pod replacement changed IP, retired the old voter,
rejoined at 10.4 s, restored both probe peers and accepted a new volume creation without bootstrap.
Takeover served at 10.2 s. Fresh five-node and three-node formations passed. The isolated cluster
was deleted afterward. WAN netem and safe rolling-upgrade evidence remain separate gates.

The additional warm-discovery refusal/restart regression passed on macOS in 0.79 s, proving
that a failed one-peer-capacity restore does not erase the second retained peer.

### 2026-09-17: CLI process gate

The macOS process gate's two failures are corrected: global options before `run`/`exec`
now preserve the child argument boundary, and the fresh consumer/fleet fixtures explicitly
bootstrap. The fleet fixture distinguishes seed-link replacement from transport faults and
keeps the root representative alive during owner loss. Consumer flow: 1.89 s; fleet
formation, placement, takeover and mounted read-back: 8.90 s. This closes the two failures
in job 105312670519, not the other jobs still running in that workflow.
Evidence and commands: [CLI gate](../bugs/2026-09-17-cli-process-gate.md).


### 2026-09-28: open — one CI failure of the warm-restart fleet test, unexplained

CI run 36498699200 (`95f79ba`, macOS runner) failed `a_warm_fleet_restart_recovers_its_root_and_regional_quorums`
at its second wait: the two restarted survivors did not commit the stopped root leader's retirement from the
council's voters within 30 s. Not reproduced here: 4 of 4 full local suites, then 18 of 18 under six concurrent
copies and 24 of 24 under twelve (about two daemons per core), each copy finishing in 12–14 s. The sampled
earlier failed CI runs show no failure of this test. The assertion now prints every survivor's consensus state
(leadership, committed voters, the council's log and commit indexes, members held alive, refusal counters), so
the next occurrence names its cause; no cause is claimed until one does. A candidate found since: the election
jitter kept two congruent survivors in lockstep (split votes every round; fixed below), and a warm restart resets
both timers — unconfirmed until a failure's dump shows the lockstep.


### 2026-09-28: a stale delivery name took a process's own pipe — fixed

`take_named` adopted whatever pipe or socket sat at the number `SLATES_CONSUMER_FD` names, then read it,
changed its flags and closed it. Every process a consumer starts inherits the variable but not the
descriptor. The name now carries the channel's identity, and the take confirms it with calls that touch
nothing:

- Unix: `fstat`'s device, inode and nanosecond modification time. The device and inode alone repeat on
  macOS 1,999 times in 2,000.
- Windows: a uniquely named pipe on two inherited handles. `CompareObjectHandles` runs first, so the name
  query, which can wait, never reaches a foreign handle.

`NotANumber` became `Malformed`. The failing test (a consumer's own pipe at the freed number lost its probe,
its flags and its descriptor) now passes on macOS and Linux; the Windows arm is proven by both Windows CI
lanes. [Bug record](../bugs/2026-09-28-a-stale-delivery-name-took-a-process-s-own-pipe.md).


### 2026-09-29: the three-process CLI fleet test's takeover wait ran out — a member that missed its promotion refused every election, fixed; a takeover stalled when a survivor never received the head, fixed

CI run 36576662318 (`38c987e`, Ubuntu) failed
`three_daemon_processes_deploy_a_fleet_from_one_manifest_and_survive_the_owners_death` at `wait_successor`:
neither survivor served the dead owner's volume. The wait now dumps each survivor's status when it runs out,
and a loop in Linux Docker with io_uring caught two defects.

**Fixed — a member that missed its promotion refused every election.** In the dump:
- survivor A campaigned 26 times, every pre-vote refused;
- survivor C refused them all "by role", held a lease of the dead leader, and had no path samples to its
  fellow voters.

C's promotion to voter had been committed without it, and the core refused any vote from a receiver not in
its own configuration, against thesis §4.1. A learner's lease also never lapsed, since the learner branch of
both groups' drive skipped the timer.
- The core now answers votes whatever its own configuration says, and a candidate records only votes from
  its own configuration's voters, so the recovery's count and reports stay exact.
- A learner's lease lapses at the minimum election timeout.
- AUD-07's rule (no vote before initialization) stays where it is enforced, and is now pinned by a wrapper
  test.
- The Raft explorer passes at full scale.
- In Linux Docker the deadlock went from 1 dumped in 81 runs to none in 150.
[Bug record](../bugs/2026-09-29-a-member-that-missed-its-promotion-refused-every-election.md).

**Open — a takeover stalled when a survivor never received the head.** The 150 runs after the fix still
failed twice, with the council led: nothing took the volume over. Reproduced deterministically in-process: a
head committed at `f + 1` while the third candidate never received any record of the object stalls its
takeover for good. Two causes:
- a holder with nothing gives no promise, so the successor never reaches `f + 1`;
- a successor with nothing never learns the object.

The design's phase one is a per-host batched round in which every holder, including one with nothing,
replies; the implementation runs per-object rounds from a successor that must already hold the object. Owed:
that round, with the deterministic test (kept out of the suite until then) as its failing test first.
[Bug record](../bugs/2026-09-29-a-takeover-stalled-when-a-survivor-never-received-the-head.md).

**Update, the same day — the fix is designed and being built.** Letting a holder with nothing promise is
safe only if every survivor agrees on each object's candidate set and that set holds the newest record at
`f + 1`. The implementation guarantees neither:
- a holder remembers the cohort of the last record it accepted, so after a neighbourhood change survivors rank
  different successors;
- a neighbourhood change in flight can leave the newest record short of `f + 1` in the owner's latest cohort;
- a successor that dies mid-takeover splits the lineage.

The bug record works through each, and gives the design's own answer (§4.8 is Vertical Paxos II): settled
neighbourhoods with joint writes, retirement records, one batched phase-one round per departed host over the
agreed cohorts, and confirmed shares.

Built so far: the configuration state (step 1 of 3).
- Each neighbourhood keeps the version its host set last changed at.
- Each member keeps its settled neighbourhood, with its hosts' domains.
- Each retirement records the settled neighbourhood, the confirmed survivors and the unconfirmed lineages.
- The commands are `Settle` and `Confirm`.
- The configuration names each object's recovery cohorts and its successor from itself alone.
- Retirements are bounded by the members (`k ≤ n − k` for `k ≤ f`), oldest dropped first.
- The codec checks every count.

Built next: the owner's side (step 2 of 3).
- While a change is in flight, a record whose cohort moved commits at `f + 1` of both cohorts; content stays
  on its current cohort.
- The record plane re-ships heads to the new candidates.
- The coordinator reports `Settle` over a new report stream once every shard holds all it owns at `f + 1` of
  each current cohort, under the version it reports. The council's leader proposes it only about the reporter,
  only when caught up, and only once.
- The owner lease counts over the settled cohort.
- Regression `an_owner_settles_its_neighbourhood_only_once_its_head_is_placed_on_the_new_cohort` (non-vacuous:
  with readiness forced true it fails).
- Found on the way and fixed: a destroyed green's `merge.placed` and `merge.pending` entries stayed forever.
- Recorded open, under GAP-A9-7: a green whose new cohort lacks `f + 1` holders of its history cannot be
  re-placed, so its owner stays unsettled (safe: joint writes go on); and a node re-admitted under its old id
  never settles its stale volumes.

Built last: the takeover itself (step 3 of 3), replacing the per-object path.
- Each retirement freezes the members its successors are ranked among, so installs taken in any order agree.
- Holders resolve each object by the configuration's lineage, through a successor that retired before
  confirming.
- A retired id is not admitted again while a kept retirement names it.
- Each owing survivor runs one paged phase-one round per retired host (stream 16). A holder that holds
  nothing answers too, and its complete answer is its empty promise.
- An object is adopted once `f + 1` of each recovery cohort promised, then re-committed under the successor's
  placement.
- Survivors confirm their shares, and stale copies are reclaimed.
- CI run 36593853664 had failed on this stall: the survivor holding the head had a takeover pending, the
  other held nothing.
- The deterministic test that failed in 61.8 s passes in 8.67 s, covering both successors in one run.

**Built for it:** status now reports the control shard's held record copies, pending takeovers and installed
configuration version (`fleet_held_records`, `fleet_takeovers_pending`, `fleet_configuration_version`), so a
stalled takeover shows in any node's status.

### 2026-09-29: KIND burst — the council under a burst of reconfiguration; a council seats fewer voters than its `f` for a while after formation; retired peers' seed ids held alive — open

**Built.** `cargo xtask kind burst` (docs/wip/kind-lane.md, Piece 7) runs the consensus goal's owed KIND
measurement of the groups under a burst.
- Each trial installs five replicas at `f = 2` under the `wan` profile and times formation until every
  neighbourhood is settled.
- It then cuts the council's leader and one more voter together, from ephemeral `NET_ADMIN` containers.
- The survivors are timed to a new leader, to both retirements committed, and to the burst resolved: every
  neighbourhood settled, no retirement kept, one version.
- Status now reports what the lane needed: the council's committed member count
  (`fleet_configuration_members`), and each group's seated voters and whether a joint change is in flight
  (`fleet_council_voters`, `fleet_council_joint`).

**Measured** (three fresh fleets; kind-lane.md Piece 7). After the cut, a leader after 5.16–15.35 s, both
retirements after 16.42–40.91 s, and the burst resolved after 28.48–63.28 s, in 11–14 commits with one
election each. Medians: led 8.76 s, retired 24.66 s, resolved 29.90 s.

**Decided — the fast track stays closed in the groups.** Members now originate proposals: they report
`Settle` and `Confirm`, which the leader proposes. The fast track saves a far proposer up to one hop (§3.7's
crossover). But the reports' phase, from retired to resolved, took 5.2 to 22.4 s, and it refuses no client
meanwhile. The track would also bring its most complex path into a group whose bursts are mostly membership
changes, which it refuses. This closes the fast-track entry's owed "vote routing in the fleet": no group has
a latency-critical proposer away from its leader.

**Found — a council seats fewer voters than its `f` for a while after formation.** The first trials cut a
fleet whose five members were all admitted and settled 2.2 s after formation. But the council's committed
voter set was still the bootstrap alone: the promotions (catch-up rounds, then the joint change) had not
committed, and `slates-1`'s plan read `voters: [slates-0]`. The cut took `slates-0`, and the council could
never elect again, the documented lost-quorum case. The lane now waits for the seats before a cut. Owed: the
seating time measured, and whether serving at `f = 2` before the council can tolerate `f` losses should be
visible to clients.

**Found and fixed (A-41) — the daemons on the lane's pods were killed by their anchors every minute or two.**
- **The cause.** The machine facts listed a Linux process's cores as `0..available_parallelism()`, which a
  CPU quota lowers. Under the chart's two-CPU quota every daemon fixed its one shard to CPU 1. The kind
  node containers showed all eight slates shard threads in the VM (five here, three in a second cluster) at
  `Cpus_allowed_list: 1`, the VM 90 % idle, the containers' `cpu.pressure` at 78–81 % and `nr_throttled` at
  0.
- **The phase.** The late beats were phase-locked to the readiness probe's `slates status`, every 5 s, in
  phase on all pods from the parallel start. The probe's client work was what a starved shard could not
  absorb.
- **The fix.** The core list is the affinity mask, the cgroup CPU budget is a fact, and shards are fixed to
  cores only when the budget covers the cpuset (§4.3 "Placement").
- **Measured.** The same idle fleet over 180 s: 0 lapse lines, 0 restarts and 0 late beats (319, 15 and 9
  before); every shard allowed on all 18 CPUs; `cpu.pressure` 0.00 %.
- **Record.** `docs/bugs/2026-09-29-every-daemon-under-a-cpu-quota-pinned-its-shard-to-cpu-1.md`.
- **Owed:**
  - the burst measurement above, again, on a fleet that does not restart;
  - the lane asserting `restarts: 0`;
  - the anchor's budget made the operator's input it is documented to become.
- **Found by the same measurement, and fixed (A-42).** An idle shard spun before every park for good once
  any client had connected: `activate` set the flag at a handoff and nothing cleared it. The fixed fleet's
  idle shards used 28–62 s of CPU in 190 s, 15–32 % of a core each. Now client work opens the idle window
  and the shard spins out only what is left of it. The same fleet's shards used 2.3–3.6 s in 190 s
  (`docs/bugs/2026-09-29-an-idle-shard-spun-for-good-once-a-client-had-connected.md`).
- **Found while measuring, and fixed — the provisioning histogram could not run.** `provision_bench`, the
  R9 gate, aborted at its create-destroy loop with `Refused(BudgetExceeded { available: 1478 })`: 832
  volumes it had destroyed still held partition 0's version slots.
  - A destroy forwarded to a shard with no client of its own was never stepped: slices ran only in that
    shard's serve rounds.
  - Every publish imaged the volumes mid-destroy and logged ESTALE for each.
  - Now the destroy wakes its owner's serve loop, the reaper's cadence steps destroys too, and a publish
    leaves out volumes being destroyed
    (`docs/bugs/2026-09-29-a-destroy-on-a-shard-without-a-client-never-completed.md`).
- **Open — the provisioning path is slower than R9's floor.** The histogram runs again, and fails the floor
  on a loaded host (load 10–14): one spinning client's create at p50 52 µs and p99 177 µs, a status round
  trip at p50 16 µs. The quiet-host record of 2026-09-05 was p50 ~9 µs and p99 ~25 µs. That the create is
  36 µs dearer than a status round trip points at the create path itself: the per-verb shard publish
  re-images every volume and writes the frame (docs/wip/recovery.md owes the incremental publish).
  - **Profiled 2026-09-29** (`sample`, 4 s of the bench, `afeed36`):
    - The serving shard spent 1,030 of its 2,375 samples (43 %) in `harvest_io`'s zero-timeout `kevent`.
      §4.3 sets the I/O harvest cadence to the step quantum, which follows the wake estimate since A-31
      (about 2.7 µs on this Mac), and A-39 harvests on every spin turn. So a busy or spinning shard pays
      a syscall every few microseconds, a cost the cadence's derivation leaves out.
    - The rest of the create path: the forwarded verb 451 samples (a new volume's owner is its name's
      hash, so most creates cross shards), the reply's wake 153, the publish about 90, the log commit 70.
    - Leads, not a proven cause. Owed: the harvest cadence derived with the harvest's own measured cost,
      an A/B under one load, then the forward and publish costs in turn, and the histogram recorded on a
      quiet host.

**Open, explained — retired peers' seed ids held alive.** Explained by the restarts above (a restarted
daemon's fresh fleet node seeds every manifest peer alive and re-learns only the peers that reach it); owed:
the burst re-run on the fixed fleet showing the seeds settle. After the council retired both cut voters, every survivor's
detector listed five alive members, 300 s on and with no pod restarted (the daemons inside them were): itself, the other two survivors, and
the two cut pods' *manifest seed* ids (`member_id(anchor, 0)`). The seeds had been folded dead when each
pod's fresh id was learned at formation, so something revived them after the retirement. Status reports them
as members held alive, and a detector-view check never settled. Reading the probe task's idle path and the
learn-on-contact path did not find the reviver; owed: a logged transition of any member to alive, with its
source, and a reproduction.

### 2026-09-29: the owner lease was voided by other hosts' configuration changes — fixed; the lease counted the settled cohort only — fixed; settled-neighbourhood hosts left direct contact — fixed

The open lease refusals below had one cause:
- in a Linux io_uring loop, every NFS-takeover failure that printed its node's counts (4 of 4) was
  `lease.refused.superseded`;
- one location failure reached the successor and was refused `LeaseUnconfirmed { version: 11 }`.

The lease was keyed to the regional version, which every admission, retirement, `Settle` and `Confirm` in the
region advances. After a takeover several such changes follow within a few periods, and each voided every
owner's lease until it installed it.
- **Keyed to the owner's standing.** An owner's standing is the version its settled neighbourhood (the set a
  takeover recovers through) was fixed at, with its current one's. A holder's acknowledgement carries its view
  of the prober's standing. The owner is superseded only by its own retirement or its id's re-admission.
- **Found by the analysis — a safety gap.** The lease counted the settled cohort only while the writes were
  joint. An owner cut off with holders that had not learned its settlement kept its lease on their answers,
  while a successor recovered through the new cohort. The lease is now joint, as the writes are.
- **A retired candidate is not waited for**: it cannot promise.
- **Found with it.** Direct contact dropped the settled neighbourhood's hosts once a change moved them out,
  though the joint writes and the joint lease need them. It keeps them now.
- **Tests.** Failing test first for the direct contact. The lease tests were rewritten for the new rules, and
  the intersection oracle now covers every set of live candidates. Fleet suite 59/59 (257.2 s, macOS).
- **Measured in Linux under io_uring.** The NFS takeover test and the location test alternated, 40 rounds.
  - Before: the NFS test failed 5 of 17 rounds and the location test 4 of 17.
  - With this fix and the location fixes below: 0 of 40 for each. At the earlier rates, 40 clean rounds
    would happen by chance about once in a million for the NFS test.
[Bug record](../bugs/2026-09-29-the-owner-lease-was-voided-by-other-hosts-configuration-changes.md).

**Fixed — the location round refused the one owner it could find (`HomedElsewhere`).** The asker now counts
what each reply did. Two Linux io_uring failures then showed the cause: the successor refused
`placement_behind`, and its placement was still a version behind its council after the round (7 against 8).
- A node now claims under its council's own membership. Ownership moves only when the owner is retired, so
  the placement's install lag cannot make a claim false.
- A peer that is not the owner no longer takes a claim away: only claims compete, the newest winning.
  Otherwise another survivor's unrelated settlement, installed a moment earlier, discarded the successor's
  claim.
- Both are failing tests first. §4.8 "Owner location" carries the status.
- Sibling (open, for Ada): each coordinator period installs the council's configuration once, before the
  record plane and the takeover rounds, so after a takeover the placement lags its council for most of a
  long period. Every reader that compares the two waits for it: a takeover round (`begin_round`) and the
  settlement report.

### 2026-09-29: a formation cohort lost a record its survivor held — fixed; readiness asked a cohort that could not answer — fixed; lease refusals on CI — open

A Linux io_uring loop of the three-process CLI test failed once in 118 runs. Both survivors held the dead
owner's head, and neither ever served it. The new status lines named the cause:
- no retirement was left;
- the council's leader counted `fleet.takeover.lost: 2`.

`bootstrap` forms a region with one node, and the leader admits the others one by one. The owner was admitted
beside the bootstrap alone and died before it reported its later neighbourhood placed. Its recovery cohort
therefore had two hosts, one of them alive. The round demanded `f + 1 = 2` live promisers, found one, and called
the object lost.
- A phase one needs `cohort − f` promises (`Quorum::recovery`; Flexible Paxos's `q1 + q2 > n`). That is
  `f + 1` at the floor, and one survivor of a two-host cohort at `f = 1`, since both hosts hold every commit
  there.
- The lease's bound moves with it, to `min(f, others)`. An exhaustive oracle showed that the old
  `others − f` would have held a lease on zero confirmations while a single promise promoted.
- Found by reading: readiness to settle was judged on the joint shape. An owner whose old cohort lost a host
  could then never settle, and no write of its would ever place. It is now judged on the current cohort, as
  §4.8 says.
- Tests:
  - `a_takeover_recovers_an_owner_settled_beside_one_host_from_that_host` (red: lost 1);
  - two exhaustive quorum oracles (db and lease; each red under the old rule);
  - `an_owner_whose_old_cohort_lost_a_host_is_ready_once_its_current_cohort_holds_its_head` (red).
- Reported siblings: the modelled ledger's `take_over` and `Promotion::promoted` still count `f + 1`. The
  cluster crate's `promote_ledger_record` is no longer called by the daemon and survives only with its test.

**Fixed — a late empty fetch reply counted as a refused join.** The same loop's other two failures (runs 125
and 147) were formation failures on `consensus.join.undecodable` and `consensus_join_refused`. A voter answers
a caught-up member's fetch with no bytes. The on-time fold skipped such a reply, but the late fold passed it to
`adopt_fetch`, which failed to decode it and counted a refused join.
- `adopt_fetch` is now the one judge of a reply (`FetchOutcome`: adopted, current, refused), counting
  refusals itself.
- The undecodable reason is split in two: a torn reply is told apart from a configuration that fails to
  decode.
- Failing test first: `an_empty_fetch_reply_is_neither_adopted_nor_a_refused_join`.
[Bug record](../bugs/2026-09-29-a-late-empty-fetch-reply-counted-as-a-refused-join.md).

**Built for the diagnosis:**
- status lists each kept retirement (`fleet_retirement`) and the node's settled and current neighbourhood
  versions;
- each refused join is counted by its rule (`consensus.join.*`, `consensus.root_join.*`);
- each latest-state refusal is counted by its reason (`lease.refused.superseded`,
  `lease.refused.unconfirmed`).

**Open — CI run 36603261909's two failures are owner-lease refusals.** One is the NFS takeover test's
`NFS3ERR_JUKEBOX`, the other the CLI mount's `LeaseUnconfirmed { version: 3 }`. The lease and every record key
on the one regional version, which each `Settle` and `Confirm` now advances. That churn is the hypothesis, and
it is unproven until the refusal's reason is counted.

**Open — the location test's `HomedElsewhere` (CI on `84cc9c1`) recurred locally, once in a suite run.**
- At the asking node the round met the successor's session out, asked it once it returned, and found no
  owner.
- At the successor, one `fleet.owner_location.foreign_view` was counted.
- That counter covered five conditions. One of them is this node's placement not yet having installed its
  council's configuration: a window of up to a coordinator period after every commit, `Settle` and `Confirm`
  included.
- The serve side now counts each condition apart (`not_ready`, `root_view_differs`, `outside_home`,
  `placement_behind`). The cause is unproven until the next failure names it.
[Bug record](../bugs/2026-09-29-a-formation-cohort-lost-a-record-its-survivor-held.md).

### 2026-09-29: holders promised a takeover without the lease gate — fixed

Found while reading the takeover path for the open stall above. The owner lease (AUD-08) needs `others − f`
confirmations so that every `f + 1` promotion quorum holds a confirming holder, which protects a read only if
every promising holder refuses while its own answers may still feed the lease. Only the successor applied that
gate, to itself. At `f = 1` an owner cut off from the council and the successor, but not from the third
candidate, kept its lease on that candidate's answers while that candidate promised the successor.
- Every holder now keeps the departed owner of each object an install reassigns.
- Every holder refuses to promise while the gate is closed, counted `fleet.promotion.deferred`.
- A holder drops the record when it accepts the object's record from its new owner.

A genuinely dead owner delays nothing: the council's death-confirmation window already exceeds the horizon.
Failing test first: `a_holder_defers_a_promotion_while_its_answers_may_feed_the_departed_owners_lease`.
[Bug record](../bugs/2026-09-29-holders-promised-without-the-lease-gate.md).

### 2026-09-29: the no-panic sweep — ratcheted per crate, 13 of 29 crates clean; the SDKs' id parser panicked — fixed

CLAUDE.md (banned item 6) forbids panics in shipped code: out-of-bounds indexing or slicing, string slicing
off a character boundary, and overflowing arithmetic among them. The workspace lints deny `unwrap`,
`expect`, `panic!`, `todo!`, `unimplemented!` and `unreachable!`. They did **not** deny `indexing_slicing`,
`string_slice` or `arithmetic_side_effects`, though CLAUDE.md names them as enforced.

**Measured 2026-09-29 on this host** (other platforms' `cfg` code aside), in library and binary code:
- 285 indexing sites, 243 slicing sites and 39 string slices;
- 1,077 arithmetic operations that can overflow;
- 3 run-time `assert!`s, plus one compile-time `const` assert, which cannot panic.

**The ratchet.** A clean crate's root carries
`#![cfg_attr(not(test), deny(clippy::indexing_slicing, clippy::string_slice, clippy::arithmetic_side_effects))]`.
Shipped builds are held to it and test builds are exempt. `cargo xtask structural` checks it: every shipped
crate carries it unless it is on `NO_PANIC_PENDING`, and a pending crate that carries it fails, so the list
only shrinks.

Each crate is linted for all three platforms before it leaves the list:
- macOS natively;
- Linux in Docker;
- Windows in a local image with the pinned toolchain and MinGW-w64, which also type-checks `cfg(windows)`
  code this Mac cannot build (`zstd-sys`).

**Clean (batch 1):** `base`, `bridge-oci`, `bridge-winfsp`, `cli`, `client`, `mcp`, `sdk-node`, `sdk-python`.
- **The SDKs' volume-id parser panicked** on a 32-byte id holding a multi-byte character: string slicing
  through the character. It would abort a user's Node or Python process in release. Failing tests came
  first, in both SDK suites. [Bug record](../bugs/2026-09-29-the-sdks-sliced-a-volume-id-through-a-character.md).
- The host's handle ids are allocated checked. An id space spent refuses as `EMFILE`
  (`ERROR_TOO_MANY_OPEN_FILES` on Windows); it never wraps onto a live handle.
- A WinFsp name scan stops at the longest name Windows can hold. A directory entry whose record would not
  fit its `u16` size field is refused `STATUS_NAME_TOO_LONG`, where it was written with a clamped, wrong
  size.
- The Linux mount-table reader appends what it read and no longer slices at computed offsets.

**Clean (batch 2):** `anchor`, `archive`, `cluster`, `land`, `wire`: 13 of 29 shipped crates.
- The wire's header and frame decoding read their fields with checked slices, and refuse `Truncated` where a
  short input was sliced. On a 32-bit host a frame length near `u32::MAX` now saturates and reads as
  truncated instead of overflowing.
- The CRC tables and the `const` schema hashes walk their inputs without an index.
- The landing's grant ids are allocated checked: an id space spent refuses the grant `NoSpace`, and never
  overwrites a record.
- Every counter elsewhere saturates.
- Linted on macOS, on Linux in Docker, and on Windows in the MinGW image.

**Owed:** the 16 crates on the pending list, `xtask`, and then the workspace lint itself. Found on the way,
open: the landing's grant table never drops a record (revoke and consume only change state), so it grows by
one per human grant for the daemon's life.

### 2026-09-29: a campaign asked no one while a session was out — fixed; exclusive session lending — open

**Fixed.** The campaign counters (next entry) named the KIND lane's unanswered pre-elections. Over ten trials,
each was a campaign whose only live voter's session was held out of its link by a discovery page, so its
round asked no one and it waited out a whole election timeout. No late grant was dropped.
- A campaign now waits for a session that is out (held by its link or lent to a dispatch), within its round's
  base deadline.
- The failing test came first (`a_campaign_waits_for_a_voters_session_that_is_out_for_a_moment`: a transfer
  into sessions held for 30 ms; unfixed, no lead in three timeouts; fixed, a lead in 0.09–0.13 s).
- The fleet suite passes 57 of 57.
- On KIND over ten trials each, the median successor fell from 2.77 to 1.91 s, no voter went unasked, and 9
  of 10 won their first pre-election.
[Bug record](../bugs/2026-09-29-a-campaign-asked-no-one-while-a-session-was-out.md).

**Open — one session per peer is lent exclusively.** A discovery page, a coordinator dispatch, a forward and
a campaign each take the whole session out of its link, though the transport runs several exchanges on one
session at once (`Endpoint::begin`/`drive`). Campaigns and forwards now wait for it. Replication rounds and
record dispatches retry a missed voter next period. Owed: a shared session (one owner driving it, exchanges
submitted to it) so no exchange waits for another's.

### 2026-09-29: a lone owner refused its own objects — fixed; campaign counters added for the open KIND items

**Fixed.** CI run 36567187754 (`2c6c034`, Ubuntu) failed a cross-region forwarding test, and a local full suite
failed its sibling. Both tests' fleets are three regions of one node each at `f = 1`. The owner lease
demanded `f` fresh confirmations from the object's other candidates, and a lone node has none. So its lease
held only for the startup allowance (0.9 s) and never again. Sampled every 0.5 s for 10 s, owner a refused
its own volume `LeaseUnconfirmed { version: 0 }` from 0.51 s on, directly and through b's forward. No
successor could ever adopt: a takeover needs `f + 1` promises from other candidates.
- The lease now needs the intersection bound, `others − f`: `f` at the `2f + 1` floor as before, fewer below
  it, and none once `f + 1` promises cannot be gathered.
- Two failing tests came first: a lease unit test below the floor, and
  `a_lone_owner_in_its_region_serves_its_latest_state_past_the_startup_allowance` (served throughout three
  horizons; unfixed, refused from 0.14 s).
- The forwarding tests now record the last reply.
[Bug record](../bugs/2026-09-29-a-lone-owner-refused-its-own-objects.md).

**Built for the open KIND items (measured: the entry above).** Six succession trials on `2c6c034` gave successors in
2.01–5.76 s, none by the outranked pod. In 3 of 6 trials the central survivor's first pre-elections drew no
reply at all. Every survivor's only link event was one discovery deadline. The counter does not name the peer; the page
to the cut leader is the likely one. There were no re-dials, transport faults or invalidations. The daemon now counts, per campaign
round:
- each voter it could not ask, by why: `fleet.election.no_link`, `fleet.election.session_lent`,
  `fleet.election.session_held`;
- each pre-vote grant dropped for arriving after its round (`fleet.election.late_pre_vote_grant`).

The lane's trial line reports both, beside the record-link counters it gained in `2c6c034`.

### 2026-09-29: a suite's departing daemon wrote into the next suite's trace — fixed

CI run 36565560556 (`38ff5b5`, macOS conformance) failed hermeticity on 2 writes "outside". Both were the
workloads suite's daemon writing its last log line and closing its log. That suite's stop had killed and
reaped only the anchor. The orphaned daemon left about 150 ms later (126–163 ms measured here), inside the
next suite's `eslogger` trace, which keeps every slates process's events.
- A stopped anchor now waits until no process runs its daemon's command line, killing one that overstays
  the start wait.
- `stop` refuses typed when the daemon will not leave, so that suite fails with its reason rather than
  polluting the next.
- The failing test came first (`stopping_an_anchor_leaves_no_daemon_of_its_instance_running`): 3 of 3
  before, 0 of 3 after.
[Bug record](../bugs/2026-09-29-a-suites-departing-daemon-wrote-into-the-next-suites-trace.md).

### 2026-09-29: the lease gate refused volumes the node did not hold — fixed; the synthetic root served attributes ungated — fixed

**Fixed.** CI run 36560510107 (`5836a8c`, Ubuntu) failed the three-process CLI fleet test. A survivor answered
`LeaseUnconfirmed { version: 0 }` for a volume it never held, where the answer is `NotFound`. It had no
configuration installed yet, and the owner-lease gate (AUD-08) asked about any volume a latest-state verb
named, held or not.
- The verb gate now applies only to a volume the partition's catalog holds.
- The mount gate now authorizes first, then gates only a volume in the shard's set. An unauthorized request
  answers `NFS3ERR_ACCES` and a destroyed volume's handle `NFS3ERR_STALE`, whatever the lease's state. A
  lapse had turned both into `NFS3ERR_JUKEBOX`, which a hard mount retries without end.
- The failing test came first (`a_lapsed_lease_refuses_only_what_the_node_holds_and_authorizes`), and the
  fleet suite passes 55 of 55.
[Bug record](../bugs/2026-09-29-the-lease-gate-refused-volumes-the-node-did-not-hold.md).

**Fixed the same day — the synthetic root returned a volume root's attributes without the lease.** Its
`LOOKUP` of a volume's name and its `READDIRPLUS` entries went through `root_object`, which no lease gate
covered. The seam now returns the attributes as optional, as RFC 1813 carries them. The daemon withholds
them while the owner lease is unconfirmed and still returns the stable handle, so the client's next `GETATTR`
meets the gate. The same test, extended, failed first (after the lapse the root's `LOOKUP` still carried
them).

### 2026-09-29: KIND succession — a round with no reply yet gave up at its lookahead — fixed; a symmetric partition never healed — fixed; a survivor's record session drops after a leader's loss — open

**Built.** The KIND lane's succession measurement (`cargo xtask kind succession`; `docs/wip/kind-lane.md`,
Piece 6). The council's leader is lost under unequal paths: pod 0's egress 80 ms, pod 1's 20 ms, pod 2
unshaped. Its egress is cut from an ephemeral `NET_ADMIN` container, and the survivors are timed to a
successor. It runs on real daemons, so it checks the lease fix below where the simulation cannot.
- Every node's status now carries its groups' election state: term, priority, rank, lease, the
  pre-elections and elections it began, the pre-vote replies it drew, and the pre-votes it refused, by
  reason (`GroupReport`; `docs/cli.md`).
- The lane now installs the image tag it built and loaded; the lane values' `slates:lane` had been
  installed whatever was loaded.

**Found and fixed: a round with no reply yet gave up at its lookahead.**
- `DispatchWait::judge` stopped a dispatch at three quarters of its deadline when it had gathered nothing,
  against the budget's documented contract.
- A central candidate whose one live voter answered in the round's last quarter failed every pre-election.
  On KIND: 7 and 14 unanswered pre-elections, and successors after 13.4 and 25.4 s.
- A dispatch with nothing gathered is now given its whole deadline, unextended. Two failing tests came
  first, by use over the fabric.
- With both fixes the central survivor succeeds 6 of 6 (median 3.08 s). Without the lease fix the outranked
  pod succeeds 6 of 6 (6.47 s).
[Bug record](../bugs/2026-09-29-a-round-with-no-reply-yet-gave-up-at-its-lookahead.md).

**Fixed the same day — a symmetric partition never healed.** A council leader cut off for 15 s and healed had
not rejoined 180 s later:
- its peers held only each other alive, and it held only itself;
- it began 175 pre-elections at a stale term;
- each side's probe to the other idled as `BelievedDead`.
Re-admission (A-15) waited for the dead-believed peer's own probe, and after a symmetric partition neither
side probes. An idle probe task now reaches out on a backed-off schedule (200 ms, doubling to 6 s; A-40).
- The failing test came first (`peers_that_each_believe_the_other_dead_find_each_other_again`), and the
  fleet suite passes 54 of 54.
- On KIND a healed leader rejoins 4.95–6.30 s after the heal (19 trials).
[Bug record](../bugs/2026-09-29-a-symmetric-partition-never-healed.md).

**Resolved the same day — a survivor's record session to the other survivor "drops" after a leader's loss.**
Logged on KIND in 4 of the 12 trials whose survivors' rounds were logged: a link to the other survivor with
no session and none lent out, for at least 2 s. A pre-election then asks no one.

Nothing tore the session down. The counters show it held by its own link task, on a discovery page, at the
campaign's start: no re-dial, fault or invalidation. The campaign now waits for it (the entry above).
- In 1 of those 4 trials the outranked pod won instead.
- So did one unlogged trial, whose central candidate's 3 pre-elections drew no reply.

**Open — one CI failure, unexplained.** `a_green_chain_survives_a_daemon_restart` (`crates/client/tests/
client.rs`) failed once on CI's Ubuntu lane (io_uring), run 36554081271 on `b036a0e`. The second daemon's
rendezvous bind was refused `EADDRINUSE` after `first.stop()`, the symptom of
`docs/bugs/2026-09-19-io-uring-retains-listener-after-shutdown.md`. It is not in the previous ten failed
runs, and 270 local Linux runs under io_uring did not reproduce it (aarch64, kernel 6.12; isolated, the
whole binary, and on 4 contended CPUs). Owed: a reproduction on CI's kernel, then the cause.

**Measured the same day — a late pre-vote reply is dropped.** With both fixes, 4 of the KIND successors' 10
pre-elections drew no reply within their deadline (one logged at over 127 ms against 124 ms). Raft counts a
vote whenever it arrives within the candidacy, so a late grant folded while the pre-election is current
would cost a period instead of a timeout.

The daemon now counts each late grant it drops (`fleet.election.late_pre_vote_grant`): 0 in 20 KIND trials.
The unanswered pre-elections were campaigns that asked no one (above). So folding late grants is not built;
the counter stays, to show it if it ever matters. The succession step stays out of `kind all`, since its
outcome is statistical.

### 2026-09-29: MLRaft — built, verified, measured, one log kept; a yielding voter refused the voter it yielded to — fixed

**MLRaft** (`crates/cluster/src/multilog.rs`; research record §3.6, slice 14). `n` Raft logs over one voter set:
keyed commands route by key, global ones go to log 0, and each other log's leader appends a barrier after each
committed global command. The merge applies each key's commands in its log's order and a global command only
once every log has reached a barrier naming it. Leaders are spread by priority. At `n = 1` it is the single
log.
- The explorer at full scale drives every node's real logs over an adversarial network with crash-restarts
  (99,499 and 62,858 keyed commands applied), with no violation; a mutation that skips the barrier wait is
  caught at seed 0, step 1,072. CI runs it in the full-scale step.
- Measured across five Azure regions (20 seeds): a keyed command proposed as a region is lost expects
  1,036 ms with one log and 1,301–1,399 ms with two, three or five. With more than one log, a crash of any
  non-designated log's leader stalls every log's keyed commands (3,713–6,312 ms at five logs).
- **Decided:** both groups keep one log. The council's commands are all global. The root group's region
  promotion is keyed but gains nothing in expectation.
- **Owed only if a group ever runs `n > 1`:** compaction across logs. A log's snapshot must not pass entries
  the merge has not applied, so log 0's snapshot waits until every other log has reached a barrier at or past
  it, and the others' snapshots carry an epoch floor. Today a restored multi-log node replays from each log's
  snapshot, which holds only while nothing is compacted.

**Found by its measurement, and fixed: the pre-vote's lease outlived the minimum election timeout.** A follower
forgot its leader only at its own campaign, so a voter yielding its timeout to a more central one (§3.4)
refused that voter's pre-vote. In a three-region group the outranked region won 195 losses of 200 after two
timeouts (6,766 ms median); now the most central survivor wins every one at its first campaign (3,322 ms).
The timer reports the lapse at the minimum election timeout (thesis §4.2.3), and every drive — the council,
the root group, the simulations — forgets the leader there.
[Bug record](../bugs/2026-09-29-a-yielding-voter-refused-the-voter-it-yielded-to.md).
- Accepted with evidence: among five regions three survivors tie within their spread, and 11 losses in 200
  split their vote (p99 4,828 → 6,472 ms; the bug had serialized them). A span of the base, a strict order
  among tied voters, and deferring a campaign after a granted pre-vote were measured and rejected
  (`docs/wip/BENCHMARKS.md`).
- The daemon's in-process fleet suite passes 53 of 53. The fast-track and pipelining measurements are
  byte-identical before and after (no leader is lost in them).

**Owed:** the KIND lane's measurement of the groups under a burst.

### 2026-09-29: pipelined replication — built and measured; a leader's per-message cost grew with its backlog — fixed

**Built** (`RaftNode::replicate_to`; `docs/wip/research/consensus-enhancements.md` §3.5, slice 11).
- A follower whose place is a guess is probed one batch at a time.
- A confirmed one is sent batches ahead of its acknowledgements, as far as its window holds, whenever a resend
  would not carry the whole backlog.

With no window nothing changes, byte for byte.

**Measured** (five Azure regions, the council's one append a period). A window of one batch keeps a group
committing 2,000 proposals a second at a 172 ms median, where no window is overloaded at 7.3 s. At 1,000 a
second it sends 14 % fewer bytes. At a low rate it changes nothing, and CI gates both. The explorer's
followers now buffer across holes (457 and 806 buffered entries at full scale).

**Found and fixed:** the commit rule tried every index of the backlog, and every configuration lookup scanned
the log: 20 ms a proposal at a 5,000-entry backlog, now 61–102 ns at any backlog up to 50,000.
[Bug record](../bugs/2026-09-29-a-leaders-commit-rule-scanned-its-backlog.md).

**Also fixed:** the recovery read window slots only within its own window's reach, so a leader with a smaller
window than a voter's could leave a chosen index free. It now reads every slot above its log, as the verified
model does, so a group's windows may differ. [Bug record](../bugs/2026-09-29-a-recovery-read-only-its-own-windows-reach.md).

**Update, the same day:** the council and the root group set their window each period from their measured
paths (a batch for each period a lost batch takes to repair: ⌈2 × tail / heartbeat⌉, one on a loopback). In
the simulation it matched the best fixed window in every case (201 / 321 ms at 2,000 a second with 1 % loss,
where one batch gives 260 / 458 ms).

**Update, the same day — the fast track's crossover is measured and decided.**
- On five Azure regions a proposer far from the leader commits up to 37 % sooner on it below 4 % loss.
- A proposer beside the leader commits later (a fast quorum of four is larger than a classic three).
- At 10 % loss it is slower for every proposer.
- The groups' proposals all come from their leader, so both keep it closed.
- Found and fixed on the way: under loss the fast track stalled for good behind an index its votes left short
  of a quorum; the leader now fills it with a no-op on its own fast track (Fast Paxos's coordinator-run round).
  At 4 % loss every proposer commits as many commands as on the classic track.

**Owed:**
- MLRaft (built, measured and decided the same day: one log; the entry above), and the KIND lane's measurement
  of the fleet under a burst.
- The fast track's vote routing in the fleet, when a group has proposers away from its leader.

### 2026-09-29: the fast track and the window are built in the core — the design's sync rule lost chosen values; fixed

**Built** (`crates/cluster/src/raft.rs`, `raft_wire.rs`; `docs/wip/research/consensus-enhancements.md` §4,
slice 10). The window (retained and validated), window reports with votes, the sync point and the fast
track's opening in appends, `FastPropose` and `FastVote`, the ballot recovery, the fast track, buffering and
absorption, and the window's byte budget and span. The Raft safety explorer covers all of it on the real core,
with a ghost of every vote cast. At full scale it saw 1,227 and 1,691 fast choices and 743 and 831 fast
commits, with no violation.

**Found on the way, each fixed with a failing test first:**
- A leader's own votes never left its window.
- A handing-off leader kept deciding.
- An opening of the fast track outlived its term.
- The track could open on an uncommitted configuration.
- **The committed design's rule for dropping slots lost a chosen value.** The design said a synced node
  drops its older slots at the sync, and a new leader cleared its window. The explorer met the fault at full
  scale (seed 266); the model then reproduced it in 18 steps at a scope its earlier searches had not
  combined. Pruning under a commit index that counts fast commits fails too. A slot now goes only under a
  classic commit, and the commit index is classic; the leader alone counts its fast choices, to apply,
  acknowledge and read. [Bug record](../bugs/2026-09-29-window-slots-dropped-before-a-classic-commit-lost-chosen-values.md).

**Verified.** The corrected design holds with Raft's strict log matching at 152,906,020 and 188,172,261
classes. Those two scopes are searched by hand (11.7 GB and 14.0 GB); CI searches the scopes under its 4 GiB
ceiling, and each rejected rule where it fails.

**Owed:**
- Pipelined replication. Without it, §3.5's out-of-order acknowledgement has almost no hole to fill: 4
  buffered entries in 3,200,000 explored steps.
- The groups' wiring: the window budget, votes routed to the leader, and the fast track's policy. Today the
  budget is zero in the groups, so nothing opens.
- The timed measurements that set the defaults.
- MLRaft, and the KIND lane.

### 2026-09-28: Fast Raft's published recovery is unsafe — the ballot rule is verified; the dialect is owed

**Model** (`crates/cluster/tests/slot_model.rs`; `docs/wip/research/consensus-enhancements.md` §3.7, slice
8). An exhaustive search of the log model that parallel replication and the fast track share.

**Fast Raft as published** (arXiv:2004.06215 §IV):
- The two readings in which a leader decides an index by votes (the pseudocode as written, and once per
  term) commit two values at one index, in 15 and 18 steps with four nodes; with five nodes a scripted
  history does so in 27.
- The reading that keeps a leader's leader-approved entries has no fault at that scope, but one leader crash
  between a decision and its commit stalls the log for good (all 225 futures commit nothing).

**The ballot rule** (Fast Paxos's recovery, in terms of Raft's) has no fault at any searched scope, up to
23,552,907 classes (five nodes, four terms), and every path it has was taken. CI's full-scale step runs the
searches in release (17 s here, 2.65 GB peak, under a 4 GiB ceiling derived from the measured cost of a
state).

**The dialect's design is verified too** (`crates/cluster/tests/prefix_model.rs`, slice 9): Raft's log
unchanged, a window of slots above it, syncing, windows-only recovery, slots kept until a sync, and in-order
commitment. It holds up to 14,625,406 classes, and each rejected alternative is kept as a variant that fails
or misbehaves: commits counted from windows (12 steps), slots dropped once covered (17 steps), voters
reporting their logs too (safe, but it resurrects stale entries).

**Owed:** the dialect. Windows, syncing, the recovery, the fast track and out-of-order acknowledgement are not
in `RaftNode` yet, and the randomized explorer must cover them on the real code. Then the measurement of the
fast track's crossover (loss, proposer placement), which decides whether it is on by default. Four nodes with
three indices, and five nodes, exceed the 4 GiB ceiling for the prefix model.

**Update 2026-09-29:** the dialect is built and explored (the entry above), and the design this entry calls
verified was corrected: its sync rule lost chosen values. Still owed from here: the pipelining that
out-of-order acknowledgement needs, and the crossover measurement.

### 2026-09-28: priority elections — built; the explorer's diagnosis tool broke CI's `--ignored` run — fixed

**Priority** (`docs/wip/research/consensus-enhancements.md` §3.4). A voter's priority is its measured quorum
round trip. Followers report theirs in `AppendReply`, and the leader returns the table in `AppendEntries`.
The timer yields one timeout per live voter that outranks it, and a leader hands off to one that does.
Measured on Microsoft's published matrix across five regions:

- East US, the fastest-committing region, led all 20 seeds (14 before);
- the median commit latency fell from 189 to 171 ms;
- after East US returned from an outage it led all 20 seeds again (none before).

At full scale the explorer interleaves 1,843 / 2,149 priority transfers with every fault, with no violation.

**CI run 36511884967 (`3316fc0`).** Both gate lanes failed "T-8.13 … at full scale": its bare `--ignored`
swept in the new diagnosis tool, `replay_to_the_first_violation`, which unwrapped absent environment
variables. The tool now skips loudly without them, and the step names its test exactly.

**A test harness that dropped a reply in flight** (`crates/cluster/tests/raft_live.rs`). The priority field
grew `AppendReply` from 57 to 73 bytes, past the first flight of that test's 16-byte frames (4 packets,
64 bytes). The voter dropped its session straight after its last serve, against the transport's contract
(`Endpoint::settle`), so the candidate waited forever for the reply's tail: the gate hung for 12 minutes
before it was stopped. The voter now settles first, as every sibling live test does.
[Bug record](../bugs/2026-09-28-a-live-test-dropped-its-last-reply-in-flight.md).

Sibling observations (open, for Ada): `crates/transport/tests/admission.rs`'s `echo` serves once and stops
driving the server, which holds only while every echo fits one 16-byte frame (all are 13 bytes or less). And
the fleet's Vivaldi integration is incomplete. Each probe task owns a
coordinate engine fed by its one peer, and the coordinate a node announces comes from an engine no sample
feeds. So priority uses measured paths, not coordinates.


### 2026-09-28: learners with catch-up rounds — built

A member promoted into a council or root seat joined the joint configuration at once, with whatever prefix
it held — the availability gap of the thesis's Figure 4.4. Now:

- **Staging** (thesis §4.2.1, `RaftNode::catch_up`). The leader stages the member and replicates to it in
  rounds, counting it toward nothing. It begins the joint change only once a round completes within an
  election timeout, and aborts a member whose lag does not shrink for a whole window.
- **Measured.** Replaying Figure 4.4(a), the group could not commit for 21 replication rounds with the
  newcomer added directly, and committed in the first round when it was staged.
- **Explorer.** It now explores membership changes (one spare node, adds through staging, removals
  including the leader's). At full scale: no violation; 2,571 / 2,200 changes; 1,359 / 1,151 members
  caught up first; 147 / 128 stagings aborted.
- **The `voters()` split.** The groups' `voters()` returned the replication targets, and the recovery plan
  and the drive's elections read it as the voter set. With staged members in that list, a council of one
  voter reported three, and a drain found no one to hand to (`DrainReport { council: NoTarget }`; no
  survivor led within 40 s in the CLI test, before this commit). `voters()` is now the voter set, and
  `replication_targets()` is what the drive replicates to.


### 2026-09-28: the delivery test read a reused handle value as the decoy — fixed (its sibling: next entry)

CI run 36500813478's Windows nightly job failed the delivery test with exit 13 ("the decoy came along"), one
failure in five nightly runs of unchanged ipc code. The child checked only that *some* handle was open at the
decoy's value, and its own handles fill the same small values. The child now confirms the object's identity
(Windows: the named event compared with `CompareObjectHandles`; Unix: the live decoy pipe's device and
inode). Open sibling, the production take: `take_named` adopts, reads and closes whatever pipe or socket sits
at the number `SLATES_CONSUMER_FD` names. A consumer's subprocess inherits the variable but not the
descriptor, so it would take and close its own descriptor. On macOS a dead pipe's `(st_dev, st_ino)` is
reused by the next pipe 1,999 times in 2,000 (measured), so the fix needs a stronger identity than that.
[Bug record](../bugs/2026-09-28-the-delivery-test-read-a-reused-handle-value-as-the-decoy.md).


### 2026-09-28: the consensus groups' logs are never compacted, and an append carries the whole lag — fixed

Fixed in the next change (`docs/wip/research/consensus-enhancements.md`, slice 5; measurements in
`docs/wip/BENCHMARKS.md`). What was built:

- Both groups compact by the thesis's size rule (§5.1.2, expansion factor one; `crates/cluster/src/fold.rs`),
  the snapshot carrying the folded configuration. A leader waits for its followers while the log is within
  twice the snapshot.
- Appends carry at most a fresh session's first credit (`raft_wire::append_batch_bytes`).
- Refusals carry the §5.3 conflict hint.
- The snapshot and its reply ride the wire (tags 8 and 9), and a follower behind the leader's snapshot
  installs it.
- The groups' identity is checked against a retained origin, since compaction moves the bases the genesis was
  computed from.
- Replay no longer copies the log on every message: 25.5 µs per change at 4,000 changes before, 0.6 µs after.

Latent core defects this made reachable, fixed in the same change:
[bug record](../bugs/2026-09-28-a-late-append-could-land-compacted-entries-on-a-log.md).

Sibling observations (open, for Ada):

- The regional configuration's `epochs` map keeps every host ever admitted — retirement does not remove its
  entry — and every daemon start admits a fresh member id. So the configuration, and with it every snapshot,
  grows with the fleet's restarts.
- The transport endpoint buffers an arriving request or reply whole, with no size bound
  (`Endpoint::drain`, `crates/transport/src/endpoint.rs`). A peer is authenticated, but the buffer has no
  derived cap.

The original entry:

Found while designing learner catch-up (`docs/wip/research/consensus-enhancements.md` §3.3). The council and
root group never call the core's `compact` ("These group wrappers never compact their logs",
`config_group.rs`/`root_group.rs`): every election's no-op, membership change, takeover and voter change stays
in the log for the fleet's life. The retained publication (`SavedRaft`, the whole log) is re-encoded at every
term, vote or append, and `replicate_to` sends a follower every entry from its `next_index` in one
`AppendEntries`, so elections, retention and catch-up slow linearly with the fleet's age. The consensus region
(two log budgets and a snapshot budget, about 360 MB at the KIND geometry) holds millions of tens-of-bytes
entries, so the eventual `ConsensusCapacity` refusal — which stops the control shard — is far off, but it is
unbounded growth (banned item 8) with a guaranteed end. Owed, in order:
- bounded `AppendEntries` batches, the bound derived from the fleet frame;
- compaction in both groups, the snapshot carrying the folded configuration (so install-snapshot rebuilds the
  same state), triggered by a derived committed-entry budget;
- then learner catch-up rounds over it.


### 2026-09-28: correlated election jitter livelocked a split vote — fixed

The pre-vote audit on the timed simulation found survivors of a leader loss splitting the vote for up to 19 s
on the multi-region profile: the timer's `(id + attempt) mod span` draw keeps two congruent nodes congruent
forever. The draw is now an independent splitmix64 mix of id and attempt; a successor is elected in 3.0–3.6 s
(one seed 8.3 s). [Bug record](../bugs/2026-09-28-correlated-election-jitter-livelocked-a-split-vote.md).


### 2026-09-28: consensus enhancements — explorer and leadership transfer built

Ada's consensus goal (pre-vote, priority elections, parallel replication, learners, MLRaft, leader transfer,
Fast Raft; `docs/wip/research/consensus-enhancements.md`). Built: the Raft safety explorer (CI runs it at full
scale in release) and leadership transfer (thesis §3.10) in the core, the wire, both groups and the daemon's
drive loops — council handoff 0.103 s against a 1.316 s leader-loss election (medians), root handoff across
three regions 0.093 s. The graceful drain (its first user): the anchor's stop is a request through the
supervision block, not a SIGKILL; a council leader sent `SIGTERM` is succeeded in 0.106–0.127 s by real
processes. Open, in build order: pre-vote audit under partitions, learners with catch-up rounds,
priority elections, ParallelRaft-CE, Fast Raft (counterexample first), MLRaft, and the graceful drain and
WAN/KIND measurements.


### 2026-09-28: a cleared control flag stranded a shutdown — fixed

A full in-process fleet suite hung for over 90 minutes in `Daemon::stop` → `Runtime::shutdown`: a shard sent
`Shutdown` was parked in `kevent` with no deadline. The control-pending flag's two halves were each unsound
under the memory model — the shard cleared it with a load then a store (erasing a later sender's mark), and
senders published with a plain store (ending the earlier sender's release sequence) — so a control message
could sit undrained while the shard parked for good. Both are read-modify-writes now (`parking::ControlFlag`).
A new loom model drives the real code: the old halves deadlock at interleavings 1 and 207; the fixed protocol
passes 5,204 interleavings. [Bug record](../bugs/2026-09-28-a-cleared-control-flag-stranded-a-shutdown.md).


### 2026-09-28: a gossiped seed death split a fresh fleet — fixed

CI run 36462066595 (`fd4f0ef`) failed the KIND lane's five-replica formation, and the local lane reproduced
it (2 of 5 and 1 of 3 scale runs). A node whose first dials to a peer failed (NXDOMAIN before the pod's DNS
record was published) received that peer's **seed** death by gossip from a node that had reached it, idled
its probe on the seed, and never dialed again; the other side did the same, so the pair never meshed. The
direct-contact rule now keeps contact with an unlearned seed until the real id is learned. KIND scale: 10 of
10 after the fix. [Bug record](../bugs/2026-09-28-a-gossiped-seed-death-stranded-an-unreached-peer.md).


### 2026-09-17: KIND rejoin status reconciled

The original whole-pod session-formation gap was fixed by the retirement/re-dial lifecycle
and fresh-member work and already passed the isolated September 15 lane. Current CI job
[105312670637](https://github.com/hyper-light/slates/actions/runs/35253899553/job/105312670637)
confirms replacement IP `10.244.1.2 → 10.244.1.3`, fresh identity, both probe peers restored,
and rejoin in 2.2 s (takeover in 1.8 s). The older §4.8 wording calling this diagnosis owed
is superseded. This is evidence from `a4fe23a`, before the fairness change above; it is not
validation of later unpushed code. Safe rolling upgrade and mounted read-back in a pod
remain separate items. [KIND record](kind-lane.md).


### 2026-09-17: scheduler quantum exercised under OS pressure

The previously owed live pressure history is built. An opt-in Linux quota test uses two finite,
joined load threads and requires original membership samples, acknowledgements completed during
load, and actual probe deadline/interval dilations. Two final runs passed in 16.15 s and 18.75 s
with 899–964 ms measured scheduling delays. A separate fixed-floor build measured 926 ms delay
and failed the dilation assertion with zero uses, while retaining membership. Active use is proven;
preventing false retirement is still an unproven benefit. Consensus timing is unchanged.
[Commands, all trials and limitations](../bugs/2026-09-16-fleet-detection-windows-use-a-fixed-scheduler-quantum.md).

### 2026-09-17: additional CI failures observed, diagnosis open

The repaired macOS CLI job is not the whole workflow. On the same `a4fe23a` run,
[Ubuntu tests](https://github.com/hyper-light/slates/actions/runs/35253899553/job/105312670699)
finished with 40/43 fleet histories passing in 2355.92 s: holder-mismatch timed out in client
`AwaitPlaced`, inputs-placed in client `Destroy`, and fresh-identity restart failed takeover
convergence. These signatures differ from the earlier merge-identity/observation diagnoses;
no cause is assigned from the CI log alone.
[Ubuntu conformance](https://github.com/hyper-light/slates/actions/runs/35253899553/job/105312670403)
reported 6202 unexpected pjdfstest failures out of 8798 cases, editor workload backup-file
output disagreement, and hermeticity startup timing out after 60 s while the anchor repeatedly
reported a missed first heartbeat. Other eight workload comparisons were identical. These remain
open; neither the CLI repair nor the scheduler proof claims to fix them.


### 2026-09-17: Linux fixture connection lifetime corrected

The Ubuntu holder-mismatch and inputs-placed client timeouts above are reproduced and fixed.
The fixtures discarded Linux's liveness socket while retaining their request rings, so the daemon
reaped them during their idle window. All six partial-constructor callers now retain the complete
connection, and that constructor is removed. Before: both exact histories failed in 10.24 s;
after: 6.24 s / 5.23 s, both formerly stuck requests below 1 ms. The separate takeover and
conformance investigations continue. [Evidence](../bugs/2026-09-17-fixture-client-drops-its-liveness-handle.md).

### 2026-09-20: FIFO/socket namespace and recovery (A-26)

Implemented: shared FIFO/socket metadata, hard links, rename/unlink, snapshots/clones,
recovery format 4, archive export/restoration, VFS canonical deltas, NFS/FUSE creation and
actual type reporting. Linux mounts exercise local pipe/socket communication and clone
isolation. Landing refuses before writing; host special files stay excluded. Devices remain
refused; device metadata is a separate decision. FSKit/WinFsp report explicit unsupported errors.

Merge integration (2026-09-20): origin format 2 and explicit `Mknod` declarations preserve
IPC metadata through identity, conflicts, histories, replay and rebase. Snapshot aliases are
preserved; alias reads and invalidations share the primary inode. Retention admission now
reserves superseded values, including payload-free removals. Regression evidence:
[merge IPC record](../bugs/2026-09-20-merge-ipc-origins-refused.md).

Owed: the inode-aware merge namespace journal (IPC rename, metadata declarations through an
alias, and primary-name unlink with aliases); mounted green/work volumes and dedicated
Python/Node/CLI/MCP IPC creation convenience methods. Existing non-IPC removals retain stale
mode/xattr values, and origin namespace validation needs hostile cross-table tests.
Review residual
pjdfstest failures, including cascades from deliberately unsupported devices and the two NFS
limits. The full unchanged rerun improved from 5,175 passes / 3,595 failures to **6,970 passes /
1,800 failures** in 172,343 ms. No expected-failure list has been expanded. Windows cross-check
is blocked by missing target C headers in the existing zstd dependency.
Record: [special-file investigation](../bugs/2026-09-19-pjdfstest-special-files-and-nfs-limits.md).

### 2026-09-20: telemetry drain oracle

Corrected the daemon integration test's round bound to include the spans generated by each
drain. A new small-quota wire scenario failed at round 67 with 118 spans still queued, while
every batch made progress; it passes after using net progress. No production quota or timeout
changed. [Red/green evidence](../bugs/2026-09-20-telemetry-drain-counts-its-own-spans.md).

### 2026-09-20: queued replies lost at the collection deadline

The Linux hedge failure was a collector bug: eight healthy missing-set replies were
already queued when expiration discarded them. Content and record wire regressions
failed deterministically in 0.01 s each. The shared wait now judges after draining,
before sleeping, so the next receive loop processes replies delivered during sleep.
The same correction covers consensus broadcasts and both takeover promise collectors.
A separate 0.01 s regression proved late offers lost their sessions; recovery now
retains both offer and put channels with a fixed two-channel bound. The hedge fixture
also reads actual owner-shard candidates, including failure domains, instead of pausing
a sometimes-wrong peer. Budgets and the three-second hold are unchanged.
Records: [collector diagnosis](../bugs/2026-09-20-collectors-expire-before-reading-queued-replies.md),
[candidate oracle](../bugs/2026-09-20-hedge-test-reconstructs-the-wrong-candidate.md).

Final serial Linux verification: strict workspace Clippy and `cargo xtask check` passed;
**1,524 workspace tests passed, zero failed, 14 ignored**, including all 49 fleet histories.
The original hedge passed ten additional serial trials (281.630–506.180 ms placement).
This does not close the 1,800 remaining pjdfstest failures or the A-26 merge-service work.

### 2026-09-28: concurrent prioritized exchanges on the session plane

Closed the session plane's one-exchange-at-a-time head-of-line blocking: many exchanges run at once on one
session, scheduled by priority class, with stream concurrency credited by `MaxStreams` (research note,
slice 2b). Four bugs fixed test-first, each with a record:

- [data past the stream limit dropped but acknowledged](../bugs/2026-09-28-past-the-stream-limit-data-was-dropped-but-acknowledged.md);
- [an abandoned request's bytes left behind](../bugs/2026-09-28-an-abandoned-request-left-its-bytes-behind.md);
- [probe copies toward a silent peer grew without bound, and settle waited on credit](../bugs/2026-09-28-silent-peer-probe-copies-grew-without-bound.md).

Open, owed in this workstream:

- **Scheduler bake-off.** Pick the winner and delete the loser selector.
- **Congestion grid re-run.** Re-run on this code, with a per-run virtual deadline added to the harness first.
- **No-panic sweep.** 634 indexing, slicing and string-slice sites across the workspace's library and binary
  targets (clippy `indexing_slicing`/`string_slice`, 2026-09-27) before those lints are denied (CLAUDE.md
  item 6).

Whole-workspace verification (2026-09-28): all 175 test binaries run directly, each under a timeout. Before
the fixes below, 173 passed and 2 failed; both failed only outside `cargo test`, and both are fixed rather
than excused:

- **`crates/wire/tests/compile_fail.rs`.** trybuild found the crate through `CARGO_MANIFEST_DIR` and the
  working directory, so it built against the workspace manifest. The test now names its crate and working
  directory itself, and passes started from `/`, `/tmp`, the repository root, another crate's directory, and
  through cargo.
- **`slates-wire-derive`.** Its empty unit-test harness (a proc-macro, dynamically linked to the compiler's
  `libstd`) could not load outside cargo. The crate has no unit tests (the derive is tested in `crates/wire`),
  so no harness is built (`test = false`).

The fleet suite passes 50/50 through cargo.

### 2026-09-28: packets fit the datagram floor

Found by the scheduler bake-off: 1-RTT packets could grow past the 1,200-byte floor, because the budget
counted only stream data, and a receiver truncated them and read them as losses (17 % goodput on a lossless
100 Mbit/s path). Fixed together with two siblings the exact accounting exposed: a credit-blocked sender
could strand when the acknowledgement carrying its credit was lost, and the acknowledgement state grew one
entry per packet on an acknowledgement-only receiver. Record:
[packets past the floor](../bugs/2026-09-28-packets-grew-past-the-datagram-floor.md).

Evidence: transport 130/130 unit tests (every connection oracle asserts each packet fits its budget),
cluster (every binary), and fleet 50/50.

Still owed, in order: the congestion grid on this code, then the scheduler bake-off with the winning
controller.

### 2026-09-28: session-plane controller and scheduler decided

Both are closed: the session plane runs **Copa**, and schedules by **strict priority**. The losers were
deleted (NewReno, CUBIC, BBRv3, Copa-Meta, round-robin, weighted, both selectors, and the delivery-rate
sampler only BBR used). Record: `docs/wip/BENCHMARKS.md`, and the research note's slice 2d.

Two bugs were fixed on the way, each test-first:

- [Copa froze an overshot window](../bugs/2026-09-28-copa-froze-an-overshot-window.md) (ping p99 at
  100 Mbit/s, 20 ms fell from 103 ms to 20.5 ms);
- [an idle peer's late acknowledgements inflated the RTT](../bugs/2026-09-28-idle-peer-acks-inflated-the-rtt.md)
  (a lost reply had cost 7 s; p99 at 64 kbit/s, 1 % loss now 0.62–0.79 s).

Known cost, recorded rather than hidden: on thin links with little loss Copa holds a small standing queue
by design. At 64 kbit/s with no loss its p99 is 678 ms against NewReno's 289 ms.

Owed next on the transport (research note §5): DPLPMTUD, path validation and migration, NAT keepalive,
batched I/O, compression, ACK frequency, the RPC-over-QUIC listener bake-off, and the netem lane. Then the
no-panic sweep's remaining crates, and the consensus queue.

### 2026-09-28: fleet serve sockets are bound before the daemon starts; test ports are held, never released

Closed: [a released test port was taken before the daemon bound it](../bugs/2026-09-28-a-released-test-port-was-taken-before-the-daemon-bound-it.md).
This was the fleet suite's rare 300 s freeze: a daemon stuck at `periods=0` with `fleet.bind` counted.

- `FleetTransport` now takes bound `ServeSockets`. A plan carries `ServeAddresses`, and `FleetTransport::bind`
  is the one step between them. An address in use refuses the start with `ServeBindError`, which names the
  plane and the address.
- The runtime adopts an already-bound socket (`UdpSocket::adopt`, paired Unix and Windows seam).
- The fleet fixtures hold every allocated port under a `PortLease` for the whole test, and give each daemon
  start (restarts included) a duplicate.

Closed in the follow-up change, the multi-process sibling. It was reproduced at 3 in 75 runs, with three
concurrent copies of the three-process test searching overlapping port ranges.

- A daemon adopts inherited serve sockets named in `SLATES_ANCHOR_FLEET_SERVE`. It refuses one bound
  elsewhere, or a malformed list, by name.
- `slates anchor --fleet` holds the node's two ports across daemon restarts, as it holds the NFS listener,
  which also closes the production restart-gap race.
- The CLI test holds its blocks: 0 failures in 75 runs, against 4.

### 2026-09-28: the client tests use the product's deadlines; unanswered wakes are counted

Closed: [the client tests judged a live daemon by a shorter clock](../bugs/2026-09-28-the-client-tests-judged-a-live-daemon-by-a-shorter-clock.md).
CI run 36404234656 failed with `Stalled` at a hand-picked 200 ms reply deadline, where the product derives
1 s. A local six-copy run also found a park-count assertion that was never a design rule (a spurious wake
parks again).

- `client.rs` and `recovery.rs` now use `Deadlines::derive`. A second sweep, after CI run 36413408328 hit
  the same stall in the CLI harness, converted the four the first sweep missed, and the fifth in
  `daemon.rs`. No Rust client builds its deadlines by hand now.
- `ClientEnd` counts unanswered wakes, and a deterministic ring test proves the count.
- Result: 0 failures in 900 six-copy runs after the fix, against 1 before.

Investigated the same day:

- The stall did not reproduce locally, including under x86 ordering through Rosetta and six-copy load,
  where the largest first-`create` latency was 30 ms.
- It is attributed, unconfirmed, to the CI runner's resource limits (4 vCPUs, five daemon-starting tests
  at once).
- On the way, the request doorbell's missing fences (the 2026-09-13 record's open sibling) were proven by
  loom and fixed; see the next entry.

### 2026-09-28: the request doorbell is fenced (the 2026-09-13 sibling closed)

Closed: [a client request could wait for a timer after a lost doorbell](../bugs/2026-09-28-a-client-request-waited-for-a-timer-after-a-lost-doorbell.md).

- **Evidence:** loom deadlocks the old `Release`/`Acquire` protocol at interleaving 1. The fenced one
  passes 47 interleavings at the CI bound and 151 exhaustively.
- **Fix:** the fences live in `slates-ipc` `doorbell.rs`, the serve loop announces, fences and re-checks
  its rings, and CI's loom lane now runs `slates-ipc`.
- **Swallowed errors fixed in the sweep:** `mark_parked`'s ignored `set_parked` result, now counted as
  `ipc.idle_announce`, and a discarded `Control::Active` send, now counted as `ACTIVATION_LOST`.

### 2026-09-29: the reply's park protocol is fenced and the async bridges are level-triggered (A-44)

Closed: [an async client waited forever for a reply that had landed](../bugs/2026-09-29-an-async-client-waited-forever-for-a-reply-that-had-landed.md).
This is the 2026-09-28 doorbell fix's sibling, which that sweep missed.

- **Symptom:** the macOS SDK packaging job hung at `cead594` (1 h 49 min, then cancelled).
  Reproduced here in 1 of 320 contended runs, with the client's loop in `kevent`, its bridge in
  `__ulock_wait2` and the shard parked.
- **Evidence:** loom deadlocks the old reply protocol at interleaving 1. The fenced, level-triggered one
  passes 42,826 interleavings at the CI bound with a client, a daemon and a bridge. 800 contended runs
  pass after the fix.
- **Fix:**
  - `slates-ipc` `park.rs` holds the fences and the bridge's level.
  - `arm_async` and the sync `wait` fence after raising the flag.
  - The daemon fences before reading it and advances the word once more before a wake.
  - The macOS and Windows bridges nudge while the client is armed and a reply waits.
- **Open:** the SDK job's steps have no timeout, so a hang holds a macOS runner for GitHub's 6 h default.

### 2026-09-29: the delivery test places its own pipe exactly at the stale number

The macOS runner failed `bc81da4`'s `slates-ipc` `delivery` test: the consumer child's check put its own
pipe at "the lowest free number at or above" the stale delivery number (4), but the pipe itself had taken
the two lowest free numbers, 3 and 4, so the read end landed at 5. The check assumed the pipe could not
include the stale number, which holds only when no single free number sits below it. The failure was
reproduced here by filling the low numbers until one was free below the stale 6 (the pipe then took 5 and
6, and the read end landed at 7). Whichever pipe end took the number now moves off it first, so the read end
is placed there exactly. Every stale-take check is unchanged and passes under that reproduction.

### 2026-09-29: fleet test polls hold their clients

The macOS runner's `a_takeover_completes_when_one_survivor_never_received_the_head` failed at `afeed36`
with "too many clients (the bound is 2248)". Its poll opened a client per check, measured here at 479–658
per second, while a departed client holds its seat for at least 1 s. The poll, and two audit waits that did
the same, now hold one client per daemon (`f18f02c`). The daemon's bound and typed refusal behaved as
designed.

### 2026-09-28: a seed id's replacement keeps the record link's pending dial

Closed: [formation dropped a dial to the peer it was reaching](../bugs/2026-09-28-formation-dropped-a-dial-to-the-peer-it-was-reaching.md)
(CI run 36408099369, `fleet.dial.stale_dropped: 3` during formation).

- The manifest seed id is a placeholder, never an incarnation, so its replacement by the first fresh id no
  longer drops the handshake in flight.
- A real restart still drops the dial and counts it.
- Boot nonces are never zero.

### 2026-09-28: path MTU discovery, slice 3a — transport parameters

Built: the session plane's transport parameters (`crates/transport/src/params.rs`).
- A required dialect version and the largest UDP payload an end reads, carried in the authenticated
  handshake.
- Decoded and checked by every endpoint when the handshake completes; a bad peer is refused typed.
- Record: the research note's build ledger, slice 3a.

Slice 3b, the same day:
- Every end declares and reads the largest UDP payload through one lent buffer per shard.
- `UdpSocket` gained `readable()` and `try_recv_from`.
- Every datagram socket sets don't-fragment (rt unsafe budget 59 → 61).
- Measured: macOS caps UDP datagrams at 9,216 bytes (`net.inet.udp.maxdgram`).

Slice 3c, the same day: the search, probes and black holes are built.
- `pmtud.rs`, with an oracle over 2,000 paths.
- `Ping` frame, dialect 2.
- Probes stay out of congestion and out of RTT samples.
- Probe timeouts count as black-hole evidence.
- Oversized retransmissions are split.
- The session test finds a 9,000-byte path and falls back through a shrink to 1,500 with no data lost.
- Two bugs fixed test-first before shipping:
  [a lone probe silenced the blocked report](../bugs/2026-09-28-a-lone-path-probe-silenced-the-blocked-report.md);
  [a shrunken path deadlocked before its black hole was seen](../bugs/2026-09-28-a-shrunken-path-deadlocked-before-its-black-hole-was-seen.md).

Slice 3d, the same day: measured.
- 4.8× goodput on real loopback: 1,625 → 7,840 Mbit/s.
- Neutral on floor paths within the noise band (20-seed grids, 100-seed burst loss, a phase sweep).
- Two changes the measurement drove: the raise rechecks the last failed size alone, and a refused probe
  gives its packet number back.
- Record: `docs/wip/BENCHMARKS.md`.

Path MTU discovery is complete. The one remaining measurement belongs to the netem lane (research note §9):
a real multi-hop path with a smaller MTU than the interface.

### 2026-09-28: the focal cross-check's transport items

Focal studies the same questions on quinn, in parallel. Its findings for slates' session plane, each to be
root-caused and measured across laptop, single-cluster and multi-region deployments:

- **Closed.** [Reassembly scanned every buffered segment](../bugs/2026-09-28-reassembly-scanned-every-buffered-segment.md):
  it is now an ordered lookup, 4,000,000 → about 16,000 segment examinations for 4,000 holed arrivals.
- **Closed: spurious loss under reordering.** The reorder-jitter scenario (8 ms at 10 Mbit/s, 20 ms) carried
  0.25 of the link, with 27,016 spurious retransmissions and no real drops. RFC 9002's fixed thresholds
  (3 packets, 9/8 RTT) misread the reordering as loss. The tolerance now adapts
  (`crates/transport/src/reorder.rs`, RFC 9002 §6.1.1, RFC 8985 §6.2). A spurious loss raises the packet
  threshold to the distance it showed, and widens the time threshold by a quarter of the minimum RTT per
  round trip. Both are bounded by the window and the smoothed RTT, and both reset after 16 quiet recoveries.
  Result: 0.254 → 0.540 of the link over 20 seeds, neutral elsewhere (`docs/wip/BENCHMARKS.md`). The by-use
  test fails without the adaptation (20,273 first-half against 20,907 second-half retransmissions).
- **Open: Copa on long fat paths.** Traced at 100 Mbit/s, 300 ms. Slow start overshoots to 1.6× the BDP.
  Then a queue of about 200 ms stands for about 4.5 s, because Copa drains 1/δ packets per RTT until its
  velocity ramps. The velocity then collapses the window to 4 % of the BDP on feedback a round trip late.
  A prototype steps straight to pipe + 1/δ packets whenever a queue stands. Over the 20-seed grids it cut
  congestion steady p99 by 7.3 %, class metadata p99 by 24 %, and raised 100 M/300 ms capacity from 0.50 to
  0.64. It was held until reordering was fixed, because a jitter-inflated standing RTT misleads it; it is
  re-measured next on top of the adaptive tolerance.
- **Open: the thin-link tail** (Copa's standing queue on thin lossless links). The same prototype halved
  the 64 kbit/s, 20 ms steady p99.


### 2026-09-29: comprehensive product, safety and global-scale audit

The [dated audit](../audit/2026-09-29_audit.md) records 87 findings across five
passes; 01, 02, 03, 04, 05, 06 and 07 have the separately recorded closures below, leaving 80 without a
recorded full closure. 02's durable source field for unnamed landings remains owed. The audit began at ae6f48b02bd87faf89c28100c5d3790f93b0714c plus the concurrently
changing working tree.
It is a review, not an implementation change or acceptance closure. It preserves the
existing gap classifications and historical measurements rather than treating them as
current-tree passes.

| Audit findings | Open contract |
|---|---|
| AUD-29-01–07 | Grants must bind the target identity, volume, consumer and chosen state at use; landing must preserve outsider replacements, serialize the target across shards, report real durability and retain bounded, consistent plan/grant state (§4.13, §4.15, R1/R10). |
| AUD-29-08–12 | Safe runtime references must not outlive reclaimed contexts; shared atomic/plain-byte access must enforce its invariant; allocator frees and every handle representation must reject invalid or exhausted identities; runtime startup/drop must own all workers (§4.2–§4.3). **AUD-29-10 closed 2026-09-30:** unforgeable extents with arena identity and incarnation, every forged/stale/duplicate/cross-arena free refused `ForeignExtent` with totals unchanged (`docs/bugs/2026-09-30-the-buddy-allocator-accepted-a-forged-free.md`). **AUD-29-11 closed 2026-09-30:** no generation wraps or is masked: a slab slot retires at its representation's limit (24 bits for the runtime's wake word) and refuses `GenerationExhausted`; a shard slot whose generations are spent is never reissued (`docs/bugs/2026-09-30-generation-wrap-revived-stale-handles.md`). **AUD-29-09 closed 2026-09-30:** a shared object hands out no reference into its mapping; declared words, racy seqlock bytes and plain spans are typed classes validated at create/open, reached by atomics, `AtomicU8` copies and plain copies, the hot path in constant time; three rendezvous claim races fixed on the way (`docs/bugs/2026-09-30-shared-mappings-aliased-plain-bytes-and-atomic-words.md`). **AUD-29-12 closed 2026-09-30:** the runtime's start is transactional (every context built and acknowledged, any refusal stops and joins the started workers and gives every slot back), `Drop` stops and joins, and a failed worker is `WorkerFailed`, never default counters (`docs/bugs/2026-09-30-runtime-start-and-drop-orphaned-workers.md`). **AUD-29-08 closed 2026-09-30:** no safe runtime API lends state its owner frees — contexts are lent for an owner's borrow or a step's span, kept values are `Kept<T>` handles validated against the running context at every use, reclamation is crate-private (a `Registration` token for outside holders), and the transport's demultiplexer and the fleet's dial inputs are handles; compile-fail doctests and a Miri-run ownership test (`docs/bugs/2026-09-30-runtime-lent-static-references-to-state-it-frees.md`). |
| AUD-29-13–16 | Archive decoding/restoration must be admitted and canonical, honor checked file offsets and refuse ambiguous names; rename must validate lazy base-directory emptiness (§4.5, §4.9, §4.11). **AUD-29-13, 14, 15 closed 2026-09-30:** chunk sizes are admitted against the header's maximum and a 1 MiB format cap before decompression, and only canonical chunks decode. The manifest decodes exactly: valid, strictly ordered names; tiling extents matching the recorded size; a 2,048-component depth bound; counts bounded by the bytes left. Restore plans, refuses `OverBudget` before allocating, decodes each chunk once and writes each extent at its offset. The takeover successor and the merge holder restore within admitted budgets, and merge inputs are page-chunked (`docs/bugs/2026-09-30-archive-decoding-and-restore-trusted-declared-sizes-and-layout.md`). **Closed the same day:** a volume nested past 2,048 levels is refused `TreeTooDeep` at the export, never sealed into an archive no reader accepts. The bound is kept as a resource bound, because restore's path keys cost `O(d²)` (`docs/bugs/2026-09-30-an-export-past-the-manifest-depth-bound-emitted-an-unreadable-archive.md`). **AUD-29-16 closed 2026-09-30:** rename and rmdir share one base-aware emptiness check; the overlay loads a merged target's listing first; an unloaded listing is refused `BaseUnavailable`, never taken for empty; only `NotFound` means "no target"; the bridge refuses hostless verbs on an overlay. Siblings fixed in the same change: the listing racy rule, relist-safe descriptors, the landed scratch volume keeping its host, and the crash resume recognizing its own put-back (`docs/bugs/2026-09-30-a-rename-replaced-a-base-directory-it-had-not-listed.md`). **Closed the same day:** a landed volume records its new base (`Op::VolumeRebased`) and publishes its image, and a restart serves it over its base; a landing of an overlay runs inside the volume's own base host (`docs/bugs/2026-09-30-a-landing-used-its-own-host-for-the-volumes-base-and-recorded-no-rebase.md`). Open pins closed the same day: an open base file replaced on disk keeps serving its opener, and a teardown sweep releases a lost mount's opens (`docs/bugs/2026-09-30-an-open-base-file-replaced-on-disk-served-the-new-file.md`). |
| AUD-29-17–18 | Takeover must preserve full volume policy/access and recovery must refuse a corrupt or shortened acknowledged green (§4.8, §4.10, §4.16). **AUD-29-17 closed 2026-09-30:** the design's catalog register class is built. Its object is the volume id with the register-class bit (`ObjectId::catalog`), placed and taken over as the volume is. Its sequence is `VolumeRecord.catalog_version`, raised by every catalog change, and its value is `CatalogValue` (name, size, names, owner, locked policy, access). It ships before the head, and a successor rebuilds from it, keeping grants and locked RAM or refusing typed. Register values carry class bytes, and volume ids refuse at the class bit (`docs/bugs/2026-09-30-a-takeover-rebuilt-a-weaker-volume-and-lost-its-grants.md`). **AUD-29-18 closed 2026-09-30:** green recovery replays each acknowledged entry as an acceptance at its own version, or fences the green (`GreenFence`: origin or entry corrupt, not accepted, wrong version, retention short). A fenced green has no engine, every verb naming it is refused `ContentUnavailable`, and its durable chain is kept as evidence until its admin's `Destroy` releases it (`docs/bugs/2026-09-30-green-recovery-served-an-empty-or-shortened-history.md`). |
| AUD-29-19–24 | SDK admission and completion must remain async, bounded and terminal under cancellation/channel loss; request ids must never wrap into old completions; MCP input and HTTP caller authority must be bounded and verified (§4.7, §4.9, §4.12–§4.13).  **AUD-29-21 closed 2026-09-30:** a client's sequence ends at `LAST_SEQUENCE` with every fresh request refused `SequencesExhausted` before it is sent, `u32::MAX` the resumed session's exhausted marker, retries of issued ids still met by their records, and new work through a new client (`docs/bugs/2026-09-30-request-sequences-wrapped-into-acknowledged.md`). **AUD-29-23 and AUD-29-24 closed 2026-10-01:** the MCP HTTP edge runs on one slates shard with stated bounds (RFC 9112 lines and fields, the shared message bound, the client's derived deadline, `clients_per_shard` connections) and per-connection failure isolation, and authorizes before dispatch (loopback `Host` and `Origin`, `/mcp`, `application/json`, a 128-bit bearer minted at start); stdio lines share the bound (`docs/bugs/2026-10-01-the-mcp-http-edge-was-unbounded-and-unauthenticated.md`). Proven over real sockets: hostile heads refused, a valid client served while slow and idle connections are held open, foreign-origin, rebound-host and unbound tool calls refused with no effect. Owed: servable roots (§4.13); the Windows HTTP edge. **AUD-29-22 closed 2026-10-01:** the client records caller-owned operations, admits a new one only below its ring's slots (`TooManyOutstanding` before anything is sent), keeps only awaited replies (never evicting), drops and counts unawaited ones (the acknowledgement now goes unawaited), and releases a cancelled call through `abandon` (`docs/bugs/2026-10-01-an-awaited-reply-could-be-evicted-from-the-client-buffer.md`); proven against a real daemon with an early reply held through three bounds of drained traffic. **AUD-29-19 and AUD-29-20 closed 2026-10-01 (A-56):** the client's operations never wait (a single send attempt; the rendezvous a claim made at once and polled on every platform, Linux's unbounded `recvmsg` included; a reconnect one step of that claim), and `slates_client::driver` owns every async call to a typed end — answered, refused, `Stalled` at its deadline (queued calls too), recovered under its own id or `DaemonGone` past the reconnect budget, `CompletionLost` on a reader failure, released when cancelled — with both SDKs driving it from their loop's reader and one timer and connecting from the loop; a client watches the daemon's process, so a killed daemon on macOS and Windows is gone rather than stalled (`docs/bugs/2026-10-01-an-async-call-could-hold-the-loop-or-never-end.md`). Proven by the Rust driver test, the rendezvous and exit-watch tests, and both SDKs' acceptance tests over a real anchor (`SIGSTOP`, `SIGKILL`, restart, reader loss or cancel, death). Cancellation: Python by its task's cancel, Node by `client.cancel(promise)` (`AbortError`). All of AUD-29-19–24 closed. **2026-10-01 regression closed:** the driver reported a ready call on every pump until the binding took its reply, so a loop step whose tick pumped after its pump reported it twice and the second event found nothing (`a list answered None`, red on the Linux io_uring lane from `153ca63`, whose stop path shifted the timing); a call is now reported ready once until it is finished, and a reconnect's resend re-arms it (`crates/client/tests/driver.rs` `a_ready_call_is_reported_once_until_it_is_finished`, red before on every host) (`docs/bugs/2026-10-01-the-async-driver-reported-a-ready-call-twice.md`). The Python SDK's loop-held bound now has the noise floor its Rust and Node siblings had (a macOS runner's loop was 0.186 s late against 0.1 s, 2026-10-01): a sampler in a separate process — a thread would share the GIL and mask a native call that wrongly held it — measures the machine's own scheduling noise in the same window. |
| AUD-29-25–30 | Landing and bulk work must not starve owner/control progress; Raft term exhaustion must refuse; packet numbers must not repeat; configuration fan-out and consensus retention need admitted, measured costs (§4.3, §4.8–§4.10a, §4.15).  **AUD-29-26 closed 2026-09-30:** a term at `u64::MAX` refuses every campaign `TermExhausted` before any change (the audit's two-leaders history is a red-then-green test), and log indices refuse past the range (`indices_exhausted`); configuration versions, host, volume and lease epochs refuse at their last value, deterministically on every replica (`docs/bugs/2026-09-30-a-saturated-term-let-two-leaders-share-it.md`).  **AUD-29-27 closed 2026-09-30:** packet numbers end at `2^62` with nothing sent and nothing lost, the session ending `PacketNumbersExhausted`; credit ceilings and segment ends never overflow (`docs/bugs/2026-09-30-packet-numbers-saturated-into-a-repeat.md`).  **AUD-29-29 closed 2026-10-01:** the control shard remembers what each other shard last received of the configuration fan (placement and root versions, readiness, a lease generation counting the lease's changes), and a period clones and sends a shard only the parts newer than that — nothing at all while nothing changed — recording a delivery only once the shard's bounded channel accepted it, so a refused fan is owed again the next period; the lease ledgers are pruned against the borrowed membership instead of two copies (`docs/bugs/2026-10-01-the-configuration-fan-copied-everything-every-period.md`; counters `fleet.fan.sent`, `fleet.fan.unchanged`, `fleet.fan.refused`). Proven by a fan-memory test (first fan, unchanged, lease-only, placement-only, refused-then-owed, install equal on the receiver) and the fleet suite unchanged.  **AUD-29-28 awaits Ada:** sharing peer sessions between an independent consensus loop and the bulk loop needs a bounded multiplexing owner per peer session — the exchange/credit layer the 2026-10-01 QUIC directive moved to `hyper-raft` — and changes to the record-link task this session was told not to read; splitting the loop over today's exclusive session lending alone would leave voter sessions out on bulk dispatches.  **AUD-29-30 closed 2026-10-01:** a leader's proposal or membership entry is admitted against its log budget (each group's log plus an equal share of the room its published record leaves, A-58) before the log changes — refused `false` and counted (`RaftNode::budget_refused`), where before it was appended and the next publication could overflow its region after the protocol had moved — with the log's size kept incrementally (the generated admission test recounts it after every step); the publication's clone, encoding and hash were measured separately across log sizes (`docs/wip/BENCHMARKS.md`, 2026-10-01: linear at about 0.8 ns a byte, the hash three quarters of it, tens of microseconds at the sizes compaction allows), and incremental delta publication was measured-and-rejected on those numbers; interrupted publications at every byte were already covered (`an_interrupted_publication_never_erases_an_acknowledged_vote`) (`docs/bugs/2026-10-01-a-proposal-could-outgrow-the-consensus-record-after-the-protocol-moved.md`). Open: 28 (awaiting Ada, above). **AUD-29-25 closed in part 2026-10-01:** a granted landing is an owned, resumable run (`LandingRun`) stepped one unit at a time — a directory swept, an entry validated or written, a directory synced — in slices of half the shard's step quantum, the shard serving between them; the finish commits with the completion as one atom; an overlay's base host is lent back between slices; an unnamed landing lands an implicit snapshot of the head; one granted landing per volume at a time. Proven: sliced equals one-call on four simulated filesystems, sliced crash-at-every-instruction resumes, and a 600-file landing that held its one-shard daemon for 831 ms now runs in about 1,100 slices with 1,000+ probes answered during it (longest 35 ms) (`docs/bugs/2026-10-01-a-granted-landing-held-its-shard-for-all-of-its-work.md`). A large file is copied a content window per unit (the same day), so no unit's work or buffer grows with a file's size. The landing lease is renewed between slices once half its term has passed (the same day: 1,500 files under a 1 s term land `done` in 1.57 s; without the renewal 1,040 landed, `partial`). **25's percentile lane recorded 2026-10-01:** the shard keeps every slice in a bounded log-linear histogram and counts any slice that ran past its budget by more than its own last unit (the engine times each unit from the previous one's end, so a step is exactly the sum of its units); the fairness test probes reads, provisioning and write-lease changes beside a 600-file landing and publishes p50/p99/p999/max for each and for the slices, with the shard's longest step — three runs in `docs/wip/BENCHMARKS.md` ("The landing's percentile lane"): 1,206 slices each, 0 past budget, slice p999 4.2–25.2 ms (the disk's unit), every probe kind served throughout. **AUD-29-25 closed.** **Sibling fixed the same day:** a granted landing's reply is waited for while the daemon lives (`defers_reply`; the async driver's `submit_patient`, used by both SDKs), where the client had answered `Stalled` after one second of it. The async path is proven by use the same day: one driver, driven by an event loop as an SDK drives it, lands two 1,500-file volumes at once — the plain call ends `Stalled` at the 1 s deadline (the control), the patient one is waited for and lands `done` at 3.1 s; submitted plainly instead, it stalls and the test fails (`an_async_patient_landing_outlasting_the_reply_deadline_is_waited_for`). **Finding 2026-10-01 (open): a landing's cost grows faster than its file count** — measured by the long-landing tests' calibrations: macOS (this machine) 500 files 0.55–0.78 s and 1,000 files 1.45–1.53 s; Linux in Docker 500 files 0.50–3.25 s and 1,000 files 6.2–16.7 s (15× for 2× files at worst). The tests no longer depend on a calibration alone (they double the files until the landing outlasts its span, bounded by `MOST_FILES`); the cause is not yet located. **Also open:** the runners refuse the calibration landing `NoSpace` at the engine's volume stage (`landing.volume_refused`, counted since `a499062`), and a Docker run refused a third sized volume `BudgetExceeded` with 3,690,560 bytes available after two calibration volumes were destroyed; the refused calibration now prints the derived configuration to name the budget. The `BudgetExceeded` is explained by the code (2026-10-01): a volume's inode allowance is `quota / size_of::<Inode>` capped at the whole version slab less its headroom (`verbs::inode_allowance`), so one Dynamic volume with a 1 GiB maximum reserved every version slot of the shard and the next volume was refused; the tests now size their volumes to what their files need (`SIZED_VOLUME_BYTES`), and the Docker run passes 3/3. **Open for Ada (a design question, §4.2): should a Dynamic volume reserve version slots for its whole maximum up front, when it reserves no bytes up front?** As built, one large-maximum Dynamic volume can make the shard refuse every other volume. **A second consequence, shown on the macOS runner (2026-10-01):** a volume whose allowance took every slot also refuses its *own* landing, since keeping the snapshot's version of a landed file charges a retained version slot (`make_current_inode`) and none is left. Reproduced here with the runner's slab (`max_inodes = 828_504`). The landing-fairness volumes are now sized at one inode's worth of quota per file (their bytes are inline), and the suite passes under that slab (`docs/bugs/2026-10-01-a-sized-landing-volume-reserved-the-whole-version-slab.md`). The admission itself is unchanged, pending the question above. |
| AUD-29-31–32 | Structural enforcement must substantiate its stated syscall-boundary claim; capabilities, ratchets, cadence and platform evidence must describe what is actually enforced (R1–R5, R8/R9, Part 6). **AUD-29-31 closed 2026-10-01 (A-59):** the R1 static check is stated as two source-level checks and the tracer, none a linker proof, and both checks were strengthened: the structural scan expands `use` trees (braces, nesting, `self`, aliases) and refuses globs of write-capable modules and aliases of their roots, and no longer takes a production module after a `#[cfg(test)]` function for a test module (it had skipped `slates-machine`'s segment module, whose two memory-object calls and one `unsafe` map now carry reasoned exemptions and count); `clippy.toml` refuses the `rustix::fs` and `libc` write calls by resolved path (aliases and macro expansion included), allowed only on the landing seam's impl and helpers, shared-memory objects and two named test fixtures (`docs/bugs/2026-10-01-the-r1-source-scan-was-evadable-and-skipped-a-production-module.md`). Proven by scanner fixtures for every evasion spelling and the test-module mistake, and the lints clean on macOS, Linux (Docker) and Windows (cross-lint). **AUD-29-32 in part 2026-10-01:** the README's capability table is generated from a registry whose every row names its proving tests (`xtask/src/capabilities.rs`), and `cargo xtask check` fails on drift, a missing or ignored proof, an unproven claim or an owed row without a gap; stale README and CLI-guide claims corrected; CI's "nightly" lanes named for what they are (every push to main; a schedule would add a daily full run and awaits Ada) (`docs/bugs/2026-10-01-the-published-capabilities-had-drifted-from-the-tree.md`). **AUD-29-32 second part 2026-10-01:** an instruction-count gate (`cargo xtask callgrind`: each benchmark's count against the baseline committed per platform in `xtask/callgrind-baseline.json`; a regression past 1%, a benchmark with no baseline, or a stale baseline entry fails, so a missing baseline never passes; the x86_64 baseline is recorded from the first CI run's printed counts), and an executable i686 lane (the official `linux/386` image on the x86_64 runner; `slates-wire`, `-archive`, `-ipc`, `-db`, `-vfs` unit tests). To build at all on i686 the runtime's io_uring driver is compiled only for 64-bit Linux, where its binding exists; 32-bit Linux serves through epoll. The lane at once found a real 32-bit defect: a waker's 64-bit task word does not fit the 32-bit data pointer and became 0. **Fixed 2026-10-01:** a 32-bit waker carries its shard and slot and wakes the slot's current occupant (the reserved generation `Encoded::ANY_GENERATION`, never issued; same-shard wakes already did this on every width); timers and driver interests take the task's identity from the shard's current task (`waker::polling_task`), never the waker; a 32-bit registry holds 256 shards. Proven by `tests/foreign_wake.rs` (failed on the pre-fix tree under `linux/386` with the wake lost; passes on macOS and i686) and Miri. The lane's `bash -lc` never ran cargo (a login shell resets PATH in the image); it now invokes cargo directly and runs `slates-rt` and `slates-mem` whole (`docs/bugs/2026-10-01-a-32-bit-waker-lost-its-task.md`). Six timing checks fail only under arm64 emulation of i386 here; the native CI lane decides them. The x86_64-linux instruction-count baseline is recorded from CI's first gate run (run 36849806140, `8cd9108`; 15 benches; `docs/wip/BENCHMARKS.md`). **AUD-29-32 closed 2026-10-03 (Ada authorized both on 2026-10-03):** the workflow runs on a daily schedule (03:17 UTC; every job, on main, so a quiet day still meets a moving nightly toolchain, runner image or container tag), and the push-only lanes run on it too, renamed for it. A TSan lane, `cargo xtask tsan` (`xtask/src/tsan.rs`, job `tsan-nightly`), runs on every push to main and nightly. It first requires ThreadSanitizer's report from a deliberate race (`crates/mem/tests/race_canary.rs`, `#[ignore]`d; uninstrumented it passes with exit 0 and no report, so a dropped instrumentation fails the lane instead of passing it vacuously). Then it runs `slates-mem`, `-rt`, `-ipc` and `-client` `--lib --tests` instrumented with std rebuilt (`-Zbuild-std`), and any report fails. Measured clean on 2026-10-03 (aarch64 Linux container, nightly 2026-10-02): 220 tests, no report. Two tests are skipped, each because the instrumentation falsifies its assertion, and each runs in every other lane: TSan ignores `mlock`; and the zero-timeout driver test switched out 3 of 12 instrumented whole-library runs against 0 of 10 plain runs (`docs/wip/concurrency.md` §4; `docs/bugs/2026-10-03-the-concurrency-lanes-had-no-tsan-and-no-schedule.md`). Follow-up done 2026-10-03: the server's library and in-process daemon suites joined the lane (195 tests, no report); the fleet and mount suites stay uninstrumented in their own lanes. |
| AUD-29-33–36 | Every admitted queue geometry must preserve unread work; stream final sizes and aggregate receive/reset credit must be checked before buffering or consumption, without panic or exceeding admitted session memory (§4.2–§4.3, §4.9–§4.10a). **AUD-29-33 closed 2026-09-30:** the MPSC ring refuses fewer than two slots (the smallest geometry whose sequences tell full from free), every admitted geometry refuses when full without losing its oldest word and keeps FIFO across wraps; the MPSC consumer is claimed, the SPSC ring splits once, every half is `Send` not `Sync`, and the runtime holds its halves for the context's life (`docs/bugs/2026-09-30-a-one-slot-ring-overwrote-unread-work.md`). **34–36 disposition 2026-10-01:** carried by the shared transport, not patched here, as 50–54 are. Ada's QUIC-compliance directive (2026-10-01) replaces slates' stream and credit layer with `hyper-quic` (a conformed quinn-proto, owned by the mantle session in `../hyper-raft`). There, 34 is RFC 9000 §4.5's final-size rule (`FINAL_SIZE_ERROR` for bytes past a declared final size), 35 is §4.1's connection-level flow control (receive credit charged against the connection's limit before buffering), and 36 is §4.5 and §19.4's `RESET_STREAM` final-size check. Patching `crates/transport` now would be a second path on code being replaced (banned item 7). Acceptance (hostile final sizes, a forged reset size, aggregate credit at the session budget, no panic) runs on the vendored transport. **AUD-29-33 sibling fixed 2026-10-03:** the runtime's wake ring was sized straight from the derived `ring_entries`, which is one slot on a machine whose wake p99 is within a syscall's median (the macOS CI runner), a geometry the ring now refuses, so no daemon started there; the size is floored at `MIN_CAPACITY` (`docs/bugs/2026-10-03-a-one-slot-derived-wake-ring-stopped-the-daemon-starting.md`). |
| AUD-29-37–38 | Raft report recovery must preserve chosen values with bounded admitted gap work; every accepted live transition must produce restorable state, with semantic and authenticated-sender validation (§4.8–§4.9). **AUD-29-38 closed 2026-10-01 (A-57):** every Raft handler admits a message only when the state it would leave passes the retained-state validator's rules, refusing it typed and counted (`Malformed`) before any term, vote, log or configuration changes; the sender and group bindings (`ForeignSender`, `ForeignGroup`) already held (`docs/bugs/2026-10-01-a-follower-accepted-state-its-recovery-rejects.md`). Proven by the audit's witness (red before) and a generated history test: after every step the saved state restores, refusals change nothing, and the census meets all seven refusal kinds and an admitted change. **AUD-29-37 closed 2026-10-01 (A-58):** a won election's recovery plan is sized arithmetically and admitted against the log budget retention derives from the record's room (each group's log plus an equal share of what the publication leaves); over budget the node declines the term, appending nothing (`recovery_refused`); admitted, it is appended a window's bytes per replication call, with proposals, membership changes and reads refused until done and the leader's own sync no-op at the end of a multi-slice plan; no report is discarded for its distance (`docs/bugs/2026-10-01-a-far-report-expanded-into-unadmitted-recovery-work.md`). Proven by a far-report test (red before: 1,000 entries in one step), refusal and crash-between-slices tests, the retention budget test, and the full-scale safety explorer with 143 multi-slice recoveries. Owed: proposals admitted against the same budget (AUD-29-30). All of AUD-29-37–38 closed. |
| AUD-29-39–40 | Exhausted timers must refuse rather than report elapsed time; refused namespace mutations must return reservations and leave names, links, journal and all resource charges unchanged (§4.2–§4.3, §4.5).  **AUD-29-39 closed 2026-09-30:** a sleep completes no earlier than its deadline or is refused typed (`NotOnShardThread`, off a shard); a full wheel makes it wait for a freed timer (bounded by the arena, counted `timer_waits`), a deadline past the clock saturates, `futures::within` keeps work, deadline and refusal apart, and every caller handles the refusal (`docs/bugs/2026-09-30-a-refused-sleep-completed-at-once.md`). **AUD-29-40 closed 2026-10-01 (A-60):** create, mknod, mkdir, symlink, link and rename admit every slot they can take — copy-ups, the new inode and node, entry-tree blocks (a split at its worst case), retention — against each slab's exact room before their first change, or refuse having changed nothing; the trie, the entry tree, the small-to-tree move and both copy-ups are each all or nothing; rename is prepare, publish (new name, then the old one removed, the first undone on refusal), consequences; the overlay admits before it witnesses. The sweep found a split that dropped entries and a rename that lost a file at the inode bound. Proven by refusal injection at every allocation step of nine verbs over three slab dimensions, with and without a snapshot, on scratch and base entries (44 and 46 refusals; names, attributes, contents, usage, accounting, journal, diverged set and slab usage unchanged) (`docs/bugs/2026-10-01-a-refused-namespace-verb-kept-what-it-had-taken.md`). Sibling sweep the same day: unlink and rmdir failed it (a refused unlink kept its copy-up) and now admit their removal whole; chmod, write, truncate and setxattr pass it (16 verbs on scratch entries, 11 on base entries, four slab dimensions; the chunk dimension is not yet exercised, since writes take arena bytes through open extents). |
| AUD-29-41–42 | Every representation of private content and metadata must have admitted protected residency and dump exclusion; the hermeticity oracle must verify actual objects, consumer/grant intervals and sinks rather than exempt disk streams or unproved shared-memory paths (§4.2, §4.13, R1/R10). **AUD-29-42 closed 2026-10-01:** the judge places every inside-target write in the granted landing — the daemon's pid and the landing's interval on the tracers' clock (strace `-ttt`, eslogger `time`) — or refuses it for its reason (no grant, another process, unstamped, outside the interval); a standard stream is descriptor 1/2 to a pipe, terminal or null device only (a log file is a file), and the harness gives the slates processes a pipe as stderr, drained by the harness; hidden siblings must be the landing's own name forms and none may remain. Proven by trace mutations each failing for its own reason, hidden-name and clock golden tests; the live lanes run on CI (`docs/bugs/2026-10-01-the-hermeticity-judge-exempted-by-name-and-descriptor.md`). The first CI run of the live Linux lanes failed them for two causes fixed the same day: strace names events by thread, so the landing's writers are the daemon's threads (`/proc/<pid>/task`); and a write to a kernel control file (the dump exclusion's core filter) is classed by the mount at its path — a kernel virtual filesystem in the tracer's mount table — never by its spelling (`docs/bugs/2026-10-01-the-hermeticity-judge-refused-the-landings-own-threads.md`; the Linux suite, run as a non-root user, then 0 outside). **AUD-29-41 dump exclusion closed for macOS and Linux 2026-10-01:** the anchor and the daemon set a core size limit of 0 soft and hard, and on Linux an empty core filter, before they hold any private byte, and refuse to start if a setting is refused (`crates/cli/src/dumps.rs`); proven from outside the real processes (`the_anchor_and_its_daemon_exclude_themselves_from_core_dumps`: `/proc/<pid>/limits` `0 0`, `coredump_filter` 0 — red before with `0 unlimited`, `0x23`); making them not dumpable as well was set aside the same day (it broke non-root starts by ordering, and it decides who may reach the segment — the owed Linux issuer surface's decision) (`docs/bugs/2026-10-01-the-anchor-and-daemon-could-be-core-dumped.md`). Open in 41–42: 41's residency (no pageout for metadata, rings, records, codec and transport buffers; protected pools prepared before provisioning) and Windows dump exclusion (WER); reads/pageout/dumps in the tracer. |
| AUD-29-43–44 | Replicated chunks/manifests/transients require all-cost admission and authoritative retirement; manifest ownership must acquire referenced chunks independently of shipped bytes, with idempotent retries and exact release (§4.2, §4.10–§4.11). **AUD-29-44 closed 2026-09-30:** each object's manifests acquire one reference per distinct referenced chunk whatever bytes a put shipped; a repeated put is idempotent; unreferenced shipped chunks are never kept; a forget releases exactly what its manifest acquired. Proven by a serial ownership oracle over generated put, retry, partial-put and forget histories, plus the audit's three witnesses as named tests (`crates/cluster/src/content.rs` `ownership_oracle`; the structure is `c05e890`'s). **AUD-29-43 in progress — retirement on destroy done 2026-09-30:** a destroy closes the volume's registers in two stages shipped to every candidate (the tombstone releases the holders' content; the retirement, once every candidate holds the tombstone, drops their records), a takeover adopts the tombstone and never re-materializes the volume (before: the successor served a destroyed volume again), the takeover's stale-copy reclaim releases content, and single-value registers keep only their newest position (`docs/bugs/2026-09-30-a-destroyed-volume-came-back-on-takeover.md`). **Retention done 2026-09-30:** a holder keeps a manifest only while its newest accepted record names it for this holder (a head listing it, or a green ledger position naming it as inputs) or while it is the object's newest placement ahead of every accepted record — released by events (a record at or past its sequence not naming it, a newer placement, the tombstone, the stale-copy reclaim), never by time: a first cut with a time window released a green's inputs before their in-order record arrived and hung a submit (measured, then replaced); a put its records already supersede is refused at the door; the rule runs at the door, after every accepted record and after every held put (`crates/server/src/content_retention.rs`); a put shipping a chunk its manifest does not reference is refused `Unreferenced` before any chunk is decoded. **Admission done 2026-09-30:** every held byte lives in the holder shard's arena, charged at its block length from unpromised capacity (`ShardBudget::charge_replicated`, reported as `replicated=` in `slates status`), the hold's index charged to the metadata ledger at a derived B-tree entry cost; a put is charged whole before any chunk is verified or stored, verifies its new encoded chunks in one charged arena scratch block (`Archive::verify_into`), and is refused `NoCapacity` with nothing held, charged or allocated; every release frees exactly what its hold took (oracle: charges equal the hold's account after every step, and releasing everything returns budget, ledger and arena to zero). `docs/bugs/2026-09-30-replicated-content-bypassed-admission.md`. **AUD-29-43 closed 2026-10-01 — the acceptance:** `a_holders_replicas_cap_at_its_unpromised_capacity_through_churn_and_retire_to_the_survivors_baseline` (three daemons; one refuses content so a single holder carries the churn; an unrelated bounded volume's promise leaves room for two seals; seals, destroys and retried puts churn): the holder's charge peaked at exactly its measured cap (98,304 = first seal + two at 32,768 each), always equal to its hold's own account; puts past it refused `NoCapacity` and counted; the unrelated volume wrote within its promise while the holder was full; a destroy's freed room was taken by a waiting seal; destroying everything returned charge, index and manifests to zero. Sibling reported: the daemon's liveness loop re-samples memory pressure and overwrites `Daemon::inject_pressure_hold` within a cadence, so a test that depends on an injected hold for longer is timing-dependent. **Fixed 2026-10-01** with a second defect the churn test's CI failures pointed at: the pressure baseline was process-wide (the first daemon in a process fixed it for every later one, so a later daemon withheld the process's growth as pressure); now per daemon, an injected hold is pinned, and the churn test pins the holder's hold and samples it per phase (`docs/bugs/2026-10-01-the-pressure-baseline-was-process-wide.md`). | Sibling reported: the owner's in-flight seal archive is uncharged heap. |
| AUD-29-45–46 | Fleet content existence/fetch/put needs current object/consumer authority; receive-credit consumption and source/retransmission storage must remain bounded by admitted destinations and operation ownership (§4.9, §4.13).  **AUD-29-45 closed in part 2026-09-30:** every content offer/put/fetch names its object and is authorized against the committed configuration and the holder's records before any lookup (acting owner and its placement to place; owner, candidates or recovery cohort to read), answers are scoped to what is held for that object, and refusals draw the unheld reply and are counted; **owed:** the requesting consumer's scope across hosts, with the fleet delegation (`docs/bugs/2026-09-30-content-hashes-authorized-fleet-content-requests.md`). **46 disposition 2026-10-01:** the endpoint's receive into growing message buffers and its whole-source send copies are the exchange/credit layer the QUIC directive moves to `hyper-quic` (mantle's), carried there, not patched here (banned item 7). Slates-side residue, tracked here: the content layer already bounds a put or fetch to one manifest or one chunk per exchange (A-54). The archive buffers above it, and the peak-byte measurement the acceptance asks for, are owed against the vendored endpoint. |
| AUD-29-47–49 | Handshake encryption levels must be protected separately; key usage/failure budgets must cause update or typed termination; unvalidated addresses must have bounded output and pre-authentication work (§4.10a, D-15).  **AUD-29-47 closed 2026-09-30:** Handshake-level bytes cross sealed under that level's keys, in their own stream, never a reused nonce; a recording relay finds no certificate on the wire, a tampered fragment is refused and its retransmit completes, a path tampering every sealed fragment never establishes, and a sealed flight ahead of its ServerHello completes (`docs/bugs/2026-09-30-the-handshake-sent-its-certificates-in-plaintext.md`). **AUD-29-48 closed 2026-09-30:** 1-RTT keys update by generation with the key-phase bit before their confidentiality limit, only once the current generation is acknowledged, keep one previous key for stragglers, and end the session typed at either limit; the control-plane seal refuses past AES-GCM's per-key limits (`docs/bugs/2026-09-30-session-keys-never-updated-or-counted.md`). **AUD-29-49 closed 2026-09-30:** a server sends an unvalidated address at most three times what it received, validated by the first sealed fragment that opens; clients pad Initial-level datagrams; a cut-short flight resumes from a cursor; raw datagrams on an established session draw one answer per probe timeout (`docs/bugs/2026-09-30-a-server-answered-any-hello-with-its-whole-flight.md`). |
| AUD-29-50–54 | Congestion/pacing must charge protected packet bytes and select fitting frames; authenticated control must progress through lost bulk, blocked handlers and exhaustion of shared credit/CPU/memory, with peer-chosen class checked against purpose (§4.3, §4.9–§4.10a). **Disposition 2026-10-01:** carried by the shared transport, not patched here. Ada's QUIC-compliance directive (2026-10-01) moves the wire and the exchange/credit layer to `hyper-quic` (a conformed quinn-proto with slates' Copa/pacing/RACK/PMTU patches, owned by the mantle session in `../hyper-raft`), which slates vendors at a recorded revision. 50 is the directive's defect 3 (in-flight bytes count payload only) and 54 its defect 4 (the class is carried in the stream id; the replacement sets it by message kind and sender role). 51 and 52 are the connection's frame selection and retransmission order, and 53's handler-driving is the endpoint's serve model; all are inside the replaced layer, so patching slates' `crates/transport` now would be a second path on code being replaced (banned item 7). Slates-side residue, tracked here: 53's bulk CPU per request is already bounded to one manifest or one chunk per exchange (A-54); the per-request shard-step measurement and the fleet's serial serving are owed against the vendored endpoint. Acceptance runs on the vendored transport. |
| AUD-29-55–58 | Transfers must resume verified partial work, preserve complete metadata and sparse semantics, and independently progress holder offers/puts without a needless slowest-peer barrier (§4.5, §4.9–§4.11). **AUD-29-56 closed 2026-09-30 (A-53, format minor 3):** every node's four times are signed and complete and its extended attributes are carried as names with value extents over the archive's chunks, all in the manifest identity; export cuts the values with the sliced cutter, restore admits them under its budget, the archive's referenced chunks include them, and a takeover restores attributes, owner and all four times (`docs/bugs/2026-09-30-a-placed-archive-lost-attributes-and-times.md`). Proven by the export oracle, the golden vectors and hostile-input refusals, the in-process rebuild oracle, and the three-daemon takeover comparing times and attribute values over NFSv3/NFSv4.2 (red with the restore disabled). **AUD-29-58 closed and AUD-29-55 closed for placement 2026-10-01 (A-54):** an offer stages the manifest on the holder, chunks travel one per exchange and are verified and kept, the chunk completing the closure draws the acknowledgement, a cut transfer's re-offer names exactly the chunks still owed, an abandoned stage returns every charge, and each holder's offer and transfer progress independently (`docs/bugs/2026-10-01-a-cut-transfer-lost-its-progress-and-puts-waited-on-the-slowest-offer.md`). Proven by the transfer oracle (16-case census; a mutation dropping resumed progress fails it), the daemon cut-at-every-chunk and abandon tests, and the simulated barrier test. The fetch half closed the same day: a takeover's fetch stages the manifest and fetches each chunk it lacks into its own hold, verified as it arrives, so a cut fetch resumes with exactly the chunks still owed (`a_cut_fetch_resumes_over_a_session_with_exactly_the_chunks_still_owed`). **AUD-29-55 closed.** **AUD-29-57 closed 2026-10-01 (A-55):** export walks only the chunk windows holding data and tiles each exactly (data extents naming their slice of the window's chunk, a hole extent per gap); restore yields a file as its length and data pieces, admitted on its data bytes; a takeover writes only the pieces and truncates to the length (`docs/bugs/2026-10-01-a-sparse-file-was-exported-and-restored-dense.md`). Proven by the in-process rebuild comparing bytes, length, SEEK map and physical charge with the origin (export hashed 4 of 32 windows), the terabyte-hole restore, and a gibibyte hole taking over under a 1 MiB bound. Owed elsewhere: hole punching (no volume verb; NFSv4.2 DEALLOCATE unserved). AUD-29-55–58 all closed. |
| QUIC-RFC (A-52) | The fleet transport must be RFC 9000/9001/9002 compliant: capped PTO backoff (RFC 9002 §6.2.1), 1 ms first handshake retransmit (§6.2.2), payload-only bytes in flight (§2/App. B), peer-chosen priority class (audit §13.3), whole-exchange retention (audit §11.8), no migration/path validation (RFC 9000 §8.2, §9), not interoperable. Plan: vendored `quinn-proto` conformed to slates' rules, slates' congestion refinements as patches, slates' application protocol on its streams (`docs/wip/transport-quic.md`). **Stage 0 (plan) done 2026-10-01; stages 1–5 open.** |
| AUD-29-59 | Remote holder ACKs require complete closure publication into admitted protected anchor RAM and recovery before the holder serves/counts it; eventual healing is insufficient (§4.8, §4.10). **AUD-29-59 closed 2026-09-30 (A-51):** a holder acknowledges a content put only once its shard's recovery image carrying the hold is committed into anchor-owned RAM; recovery holds the image's replicas again, re-verified and re-owned per object, before the node serves; a refused publish answers no acknowledgement (`crates/server/src/content_holder.rs`, `ContentHold::to_image`/`from_image`, `ShardImage::held`, image version 8). Proven by `an_acknowledged_replica_survives_a_warm_daemon_restart` (red then green) and the ownership oracle's image round trip over every generated history. The volumes' half of the incremental publish landed with A-64 (2026-10-03: content by reference). The held replicas followed the same day (`1ca4804`): the hold image names its blocks, and recovery claims them, re-verifies every chunk against its identity where it lies and adopts the block (`ContentHold::claim_image`/`from_claimed`; the ownership oracle's recovered hold images identically and charges what the live one did, red with adoption off). |
| AUD-29-60–61 | Address changes need bounded authenticated validation and new-path measurements; local UDP backpressure needs owned pending-send/readiness state and accurate acceptance timing (§4.3, §4.10a). **AUD-29-61 runtime seam closed 2026-10-01:** a full local send buffer is typed `RtError::WouldBlock`, apart from a failed socket; `UdpSocket::try_send_to` answers `None` with nothing sent, `writable` awaits write readiness through every platform's driver, and `send_to_writable` sends once the OS has room, with no spin; the simulated fabric injects send pressure; DNS queries wait instead of failing (`docs/bugs/2026-10-01-local-udp-send-pressure-was-a-socket-failure.md`; `a_send_under_local_pressure_waits_for_writability_and_sends_once`). **Disposition 2026-10-01:** the endpoint half of 61 (a packet recorded sent only when the OS accepts it, with a bounded pending datagram) and all of 60 (validated address change; RFC 9000 §§8.2, 9) are the QUIC layer the directive moves to `hyper-quic`, carried there and not patched here (banned item 7). Slates-side residue, tracked here: the vendored endpoint's I/O loop must send through this seam and hand each datagram's source address to the connection (slates' `Demux::route` discards it today). |
| AUD-29-62–63 | Windows base access must retain contained directory handles; fixtures, trace output and property-failure persistence must prove RAM-backed ownership before writes (§4.5, R1/R8, Part 6). **AUD-29-62 closed 2026-10-01:** the Windows host retains an `OwnedHandle` per directory and opens every entry with `NtCreateFile` relative to it (`RootDirectory`, one entry name, `FILE_OPEN`, read access only, `FILE_OPEN_REPARSE_POINT`); the reparse check is made on the opened object — a name surrogate is refused as the wrong kind and listed as a link, an entry whose reparse point is its own data is reopened through its filter and kept only if it is the same object; a listing enumerates the retained handle and fingerprints each entry through its own contained open; reparse and directory records are parsed with every offset checked (golden and hostile-input tests on every host). The sibling on Unix is fixed in the same change: `openat(dir, "..", O_NOFOLLOW)` opened the base's parent, and now both hosts refuse any lookup that is not one entry (`one_entry`; `a_lookup_that_is_not_one_entry_never_leaves_the_base`, red then green here). The R1 wall gained `NtCreateFile`/`NtWriteFile`/`NtSetInformationFile`/`NtDeleteFile`. Proven on the native Windows lane by swapping the root, an intermediate and a final component for junctions and links before list, open and read (`docs/bugs/2026-10-01-the-windows-base-host-re-resolved-paths.md`). |
| AUD-29-64–67 | Production Linux OCI needs a served FUSE export and volume-specific live source authority; recursive mount topology/rights, identity-preserving runtime handoff and runtime/namespace capability evidence must be verified (§4.6 A-9/A-28, R1/R10). **AUD-29-64 in progress 2026-10-01 — the bridge's owner half:** the FUSE crate mounts and unmounts without blocking (`begin_mount`/`PendingHandshake`, `begin_unmount`/`PendingExit`: polled on readiness and tick, the helper killed and reaped if dropped), reads without blocking (`FuseChannel::nonblocking`, `try_read_request`), and splits a request's service into `dispatch_ready` and `send_reply` so the owner runs the §4.8 barrier between them (`Dispatched::needs_barrier`: namespace and attribute changes, `fsync`, and the `flush` every close sends; a plain `write` is unstable until then, as NFS's `UNSTABLE` is) and answers `EIO` when the barrier is refused. Proven on a real Linux kernel mount in a container as an ordinary user (`tests/owner_turn.rs`: create, three flushes and a `mkdir` barriered, write and read not, the refused barrier's `EIO` at the caller, the unmount ending the loop) and by the handshake's cases on every Unix. **The daemon serves it (2026-10-01):** `attach` with the FUSE form (`AttachRequest::FuseMount { mount_point }`; `Client::attach_fuse`) checks the mount point before any effect (an absolute, existing directory owned by the daemon's user and the requesting uid — `fusermount3` alone mounts over a regular file), defers its reply to a task that mounts without blocking the shard, and records the attachment (bridge consumer, chosen path) in the same transaction as the completion once the device is held; the mount's `fsname` is `slates:<attachment>`. One serve task per mount on the owner shard admits each request under the mount's registry attachment, runs the §4.8 barrier before a mutation's reply, and yields between requests; the kernel's unmount, a `detach`, the volume's destroy and the daemon's stop each unmount and end the attachment. The transport report offers FUSE exactly where `/dev/fuse` and `fusermount3` exist (`FuseUnavailable` otherwise). Proven on a real kernel mount as an ordinary user in a container (`crates/server/tests/fuse_mount.rs`: calls through the kernel, the mount table naming the attachment, a second mount seeing the first's work, a file refused as a mount point with nothing mounted or recorded, all three ends, no barrier refused). **The OCI source authority (the same day):** a FUSE mount's table source is `slates:<attachment>`; the verifier accepts exactly that form (`SourceRule::Attachment`; the old shared `slates` source and malformed ids are another volume's) and the daemon holds the named attachment to its live record — this volume, this principal, exactly this mount point — before binding (`Binding::consumer`; unit-tested against a real partition: a stale or foreign id, another point, another principal refused). Linux's container bind stays refused, now `ContainerWorkloadUnproven`, until a container workload runs through it (the audit's condition; T-4.13 through the daemon). **`slates mount` on Linux (the same day):** the command resolves the path and asks the daemon for the FUSE mount; `slates unmount` is `fusermount3 -u` (no privilege), the daemon ending the attachment on the kernel's disconnect; the NFS mount code no longer builds on Linux (the kernel refuses it without a privilege). Proven through the real binary, anchor and daemon as an ordinary user (`slates_mount_on_linux_serves_a_fuse_mount_and_unmount_ends_it`; the CLI suite 15/15 on Linux). **Owed:** the conformance lane over the FUSE mount (it still reaches the export through a root NFS mount); T-4.13 through the daemon; the anchor holding the device across a restart; per-shard channels. **The stop no longer waits behind a shard (2026-10-01):** the stop's unmount ran as a question queued behind the shard's work, so a stop on a busy shard answered the questions already queued there instead of terminating them (the observe suite red on the Ubuntu lane); it now runs from the serve loop's end as the shutdown cancels it (`daemon::EndMounts`; `docs/bugs/2026-10-01-the-stop-answered-questions-queued-before-it.md`). Both gaps that sweep found are closed (2026-10-01). A FUSE attachment is recorded as `AttachForm::FuseMount`; recovery ends its record, and once the shard runs it unmounts the dead mount if the kernel's table still names that attachment. A fenced shard's stop unmounts through a borrow that writes no record (`a_fuse_mount_whose_daemon_was_killed_is_ended_by_the_restarted_daemon`, `a_fenced_shards_fuse_mount_ends_with_the_daemon`; `docs/bugs/2026-10-01-a-fuse-mount-outlived-a-crash-or-a-fenced-stop.md`). **AUD-29-66 closed 2026-10-01 (the identity half):** a verified source carries the kernel's identity of the mount instance — Linux `mountinfo`'s mount ID and device, macOS `statfs`'s filesystem id (`MountIdentity`) — in its evidence on the wire, the CLI's text and JSON; a harness runs `slates oci-check SOURCE MOUNT_ID DEVICE` immediately before its runtime binds the path, which refuses `SourceMissing` (no mount there any more) or `SourceReplaced` (another mount instance, a remount or one stacked over it) from the kernel's table read then (`verify::source_unchanged`, pure); the CLI's container leg (T-4.13) runs it before each bind and uses `docker --mount`, which refuses a missing source rather than creating a host directory. Proven: `crates/bridge-oci/tests/verify.rs` (`a_mountinfo_line_names_its_mount_instance`; `a_source_checked_again_must_be_the_mount_instance_verified`: unchanged, missing, remounted, shadowed), `crates/cli/tests/cli.rs` `a_verified_container_source_is_checked_again_before_it_is_bound` (macOS live mount: checks clean, then `SourceMissing` after the unmount) and T-4.13 through Docker Desktop with the check before both binds (`docs/bugs/2026-10-01-a-verified-container-source-was-not-pinned-to-its-mount.md`). Residual (recorded, not closable here): the instant between the check and the runtime's own resolution of the path — Docker resolves bind sources by path on its daemon host, so no descriptor handoff spans it. **AUD-29-67 closed 2026-10-01 (the consuming runtime):** the report no longer names a runtime from the daemon's `PATH`; the OCI row's evidence is `VerifiedSourceExport` (the export's own proof). The harness's bounded handshake, `slates oci-runtime RUNTIME`, asks the runtime's engine for its profile: endpoint (`DOCKER_HOST`, else the context's; a non-local one refused `RemoteEngine` before the engine is asked), engine kind and version, rootless/`userns` (`UserNamespaceUntested`). It judges the profile against the evidence: Docker Desktop on macOS over its local socket holds T-4.13; any other profile is refused `ProfileUntested`, any other runtime `RuntimeUnsupported`. Bounded by the observe budget and one page of answer, with hostile answers refused typed. Proven: `crates/bridge-oci/tests/runtime.rs` (every profile judged; two mutations red), the parser's hostile-input tests in `crates/cli/src/oci_runtime.rs`, and T-4.13 running the handshake before each bind (Docker Desktop 29.3.1, evidence T-4.13). Record: `docs/bugs/2026-10-01-a-runtime-name-on-path-was-reported-as-container-evidence.md`. Owed: evidence for any further profile (a Linux engine through the FUSE source, rootless) is a container workload through it. **AUD-29-64 closed 2026-10-02 (its required correction met):** volume-specific live source authority through the attachment seam (`slates:<attachment>`, held to its record), the Linux bridge served by the daemon, and the path through the daemon by use: a Docker Engine container through `slates mount --shared` (`a_linux_container_reaches_the_shared_mount_as_its_own_ids`), and every conformance suite over that mount in the Linux container lane (`oci-linux`; pjdfstest as root against its reviewed list, hermeticity with 0 outside writes). Follow-ups, beyond the acceptance: the anchor holding a FUSE device across a daemon restart, and per-shard channels. **The held device is AC-3.4/T-3.5's acceptance, which had no row; it is now designed (A-61, 2026-10-03) and built in four steps.** Step 1, the codec, is built: the kernel's resend offer is kept from `INIT`, the resend notification is checked against the 7.41 header, and the session is two checked bytes with golden and hostile-input tests. Step 2, the anchor's descriptor channel, is built: a Linux `SOCK_SEQPACKET` socketpair with hold, session and release messages, bounded at `shards × MAX_ATTACHMENTS`, drained every supervision step and before every spawn, and proven across real processes (a daemon's device and session handed to its restarted successor). Step 3a/3b is built: references are attributed to a typed owner, a recorded attachment's carried in the recovery image (version 10) and settled at recovery, which also fixes a leak of every recovered orphan whose holder died (`docs/bugs/2026-10-03-a-recovered-orphan-whose-holder-died-was-never-reclaimed.md`). Step 3 is built: the daemon holds each FUSE device with the anchor, and recovery takes back a mount whose device is held and whose kernel can resend, serving it again after one `FUSE_NOTIFY_RESEND`; a write the dead daemon acknowledged and never published was first reported `EIO` at the file's next `fsync` from a dirty log, never a silent hole (superseded the same day by the write log, A-63, which replays it) (found by the in-flight test before landing: `docs/bugs/2026-10-03-a-fuse-takeover-would-have-hidden-the-writes-its-daemon-lost.md`). Proven on Linux 6.12 as non-root: a write after `SIGKILL` waits 23–134 ms and succeeds, open and unlinked files stay usable, a request in flight is resent and served, and no loss is silent (3/3); red-checked. **AC-3.4 met for kernels 6.9+**; older kernels keep the ending, by test. Step 4 is built: a barrier reply rides its publication (shard image v11), and a resent request whose unique matches is answered from it once, never applied twice (unit-proven and red-checked; the publication-to-reply window is too narrow for a kill to target). **A-61 is complete.** Per-shard channels: measured and rejected for one-volume mounts (A-62; a request on another shard pays a 0.5–6.25 µs cross-shard round trip for no parallelism). **Every acknowledged write survives (A-63, 2026-10-03):** each FUSE write is logged whole in anchor RAM before its reply and replayed by the successor when its stamp matches the recovered image's generation (the in-flight test: every page exact, no reported loss, 2–3 writes replayed per run; fails with the replay removed); only a write the log could not take reports `EIO`. **AC-3.4 complete.** |
| AUD-29-65 (closed 2026-10-01) | The container recipe's topology is exact: a **non-recursive** bind of the verified source mount alone with **private** propagation (`options: [bind, ro|rw, private]`), and a source with any mount beneath it is refused `ChosenPathUnavailable{DescendantMount}` before the recipe is published — nothing the bind would hide and no filesystem without a slates attachment reaches the container, and `ro` covers the whole bound view. The CLI's container leg passes the recipe as Docker's `--mount type=bind,…,bind-recursive=disabled,bind-propagation=private[,readonly]`, never `-v` (recursive, and it creates a missing source on the host — AUD-29-66's harness half). Proven: the verifier's descendant test (a mount beneath refused, a name-prefix sibling and a clean source verified) and T-4.13 through Docker Desktop's runc on macOS (`an_oci_container_consumes_the_host_attachment_through_the_runtime_bind`, read-only leg `EROFS`) (`docs/bugs/2026-10-01-the-container-bind-was-recursive.md`). |
| AUD-29-68–70 | A real VMM binding and durable guest authority/lifetime are owed; refused admission, cancellation and failed reclamation must retain one terminal owner and return registry, view, reference and seam resources (§4.6, §4.13). **AUD-29-69 and AUD-29-70 closed 2026-10-01:** a refused admission withdraws the registry attachment it took (revoked and drained) at every later step — queues, memory/configuration, publication — so the owner's barrier afterwards closes nothing; the terminal step always runs to its end — a refused sweep or a missing context is named in what was reclaimed (`sweep_refused`) while the registry attachment, the credits and the seam are released all the same; a device whose volume is gone is `abandon`ed through the registry alone (`BridgeAccess::with_registry`), which the serve loop and the daemon's refused-loop branch now fall back to (the latter counted `virtiofs.reclaim_incomplete`); a device dropped without its terminal step releases its seam. Proven: `crates/bridge-virtiofs/tests/admission.rs` (`a_refused_admission_leaves_no_attachment_behind`, red with the withdrawal mutated out: `Queues: no attachment left behind`; `the_terminal_step_releases_everything_whatever_the_sweep_did`) (`docs/bugs/2026-10-01-a-refused-or-failed-guest-device-stranded-its-attachment.md`). **AUD-29-68: the inherited-descriptor binding built 2026-10-01 (Linux):** a vhost-user back end over an adopted socketpair end (`crates/bridge-virtiofs/src/vhost_user.rs`, `Daemon::attach_vhost_user_device`). The consumer is the socket's peer uid. Guest memory must be sealed memfds mapped through `SharedObject`, and ring addresses are translated. The doorbell is an epoll descriptor over the socket and the kick eventfds, and interrupts go out on the call eventfds. The handshake is bounded by the harness's boot budget, and every unoffered request or feature is a typed refusal. Proven by a front end speaking the real protocol bytes (`a_vhost_user_front_end_drives_a_guest_whose_file_the_host_reads_back`, `a_vhost_user_front_end_that_offers_unsealed_memory_or_leaves_is_refused`; `docs/bugs/2026-10-01-the-inherited-descriptor-vmm-binding-was-not-built.md`). **A live guest ran (2026-10-01):** QEMU 10.0.13 `vhost-user-fs-pci` with `-chardev socket,fd=N` and a sealed memfd RAM backend. A Debian 6.12.111 arm64 guest under software emulation mounted the tag, read the host's file, wrote its own, made a directory, unmounted and powered off in 2.3 s, and the host read the guest's bytes. The run found and fixed a descriptor-coalescing bug (`docs/bugs/2026-10-01-vhost-user-descriptors-closed-with-an-earlier-message.md`). **The live guest in CI (Ada authorized QEMU in CI, 2026-10-01):** the `live-guest` job builds `ci/guest` (QEMU, the distribution kernel, the busybox initramfs, the roster's tools) on the Linux runner, with KVM when `/dev/kvm` is present, and runs the virtio-fs suite. A skipped live guest fails the job. The guest's machine, console and acceleration follow the host's architecture (`GuestMachine`). Open in 68–70: **§6's workloads ran in the live guest (2026-10-01):** all nine Linux roster workloads came out `Identical` between slates and the guest's RAM, judged by the harness's own rule. The vhost-user form reports `LiveGuestWorkloads` on Linux (`a_live_guest_runs_the_roster_workloads_identically_on_slates_and_on_its_ram`, 364 s; `docs/bugs/2026-10-01-no-live-guest-had-run-the-workloads.md`); **the durable guest record built 2026-10-01:** a `Guest` consumer with a `GuestTag` form, committed at admission and removed at the device's end. `status` counts it, `detach` revokes its device, `advance` moves a snapshot device's view, and recovery ends a dead daemon's (`a_guest_device_is_recorded_its_snapshot_view_advances_and_its_detach_ends_it`, `a_dead_daemons_guest_record_is_ended_by_recovery`; `docs/bugs/2026-10-01-a-guest-device-had-no-durable-record.md`); **the destroy-under-a-device case closed 2026-10-01:** a destroy revokes the volume's guest devices and defers its teardown until they end, so each ends `Revoked` with its references swept through its volume or view (`a_volume_destroyed_under_its_guest_devices_ends_them_cleanly`, head and snapshot; before, the device ended `VolumeGone`; `docs/bugs/2026-10-01-a-destroy-left-its-guest-devices-to-find-the-volume-gone.md`). **AUD-29-68 closed 2026-10-02 (its required correction met):** the inherited-descriptor binding is complete with its durable authority and lifetime record, and a real guest runs it in CI on every push (QEMU `vhost-user-fs-pci`, KVM where present). The roster's nine workloads come out identical, and the job is green on `b867332` after its console parser learned the x86 console's control sequences. Nothing was added that the correction forbids: no disk socket, no standalone runtime, no disk image, no privilege. |
| AUD-29-71–73 | Native guest memory must establish complete queue ownership and publication ordering; consumer revocation must fence admitted devices before its acknowledgement (§4.6, §4.13, AC-4.12/T-4.14). **AUD-29-71 closed 2026-10-01:** a device validates its queues together: `configure` refuses `QueuesOverlap` when any ring of one queue shares a byte with a ring of another, and each queue refuses a buffer aliasing another queue's rings (`BufferOverlapsOtherQueue`) before any access, as it refuses one aliasing its own. Proven: `crates/bridge-virtiofs/tests/device.rs` (`queues_sharing_ring_bytes_are_refused_at_configuration`: identical and partially overlapping layouts; `a_buffer_aliasing_another_queues_ring_is_refused_before_access`: the other queue's descriptor table untouched; both red with the checks mutated out) (`docs/bugs/2026-10-01-virtqueues-could-alias-each-other.md`). **AUD-29-73 closed 2026-10-01:** each shard keeps the guest devices it serves with the consumer each was admitted for (bounded by its device limit); a consumer's revocation, on every shard before it is acknowledged, asks those device loops to revoke — they run on the same thread and check at every pass boundary (AUD-29-87), so none serves a request after the acknowledgement, and each terminal step sweeps under its still-live attachment. Proven: `crates/server/tests/virtiofs.rs` `a_consumers_revocation_stops_its_guest_device` (the guest's read answered before; after `Revoked`, a read and a create unanswered for five heartbeats; the loop ended `Revoked`, references swept; red with the hook removed: answered `true`) (`docs/bugs/2026-10-01-a-consumers-revocation-did-not-reach-its-guest-devices.md`). Owed: an access-list reduction's effect on admitted devices (the rights are captured at admission). **AUD-29-72 closed 2026-10-01 (the contract and its placement):** `GuestMemory::order(Edge)` is a required method — every seam states its own ordering — and the ring protocol asks for it where virtio 1.2 puts it: `Acquire` after a non-zero available index is read and before the descriptors and request it covers (§2.7.13.3, paired), `Release` after the reply and the used element and right before the used index (§2.7.8.2), `Full` between the used index and the driver's notification flags (§2.7.10); the device copies a chain whole and reads each request byte once, so a guest rewriting a descriptor mid-pass changes nothing it acts on (stated on the trait). The simulated memory fences and records each edge among its accesses. Proven: `crates/bridge-virtiofs/tests/device.rs` `the_ring_protocol_asks_for_its_ordering_edges_where_virtio_puts_them` (`docs/bugs/2026-10-01-the-guest-memory-seam-had-no-ordering-contract.md`). Owed with AUD-29-68: the native seam's hardware fences for a mapping shared with a guest's CPUs, and a weak-memory run. |
| AUD-29-74–76 | Container identity/group/security semantics, authenticated Pod publication/teardown, access modes and immutable/subtree exports need supported profiles and typed refusals for unavailable forms (§4.6, §4.13, AC-4.11/T-4.13). **AUD-29-74 closed 2026-10-01 (identity per profile):** the tested profile states its identity rule, measured, and the runtime handshake prints it: `HostUserThroughShare`. Through Docker Desktop on macOS every container identity reaches the export as the host user running Desktop. Ids and supplementary groups are not forwarded; the container sees its own ids; permission bits are kept. So the attachment's capability, not a container uid, is the authority, and the source is never chowned. Measured with 501:20, 0:0, 1000:1000 and 501:20 plus group 12345: each wrote, the host saw 501:20, a 0700 directory kept 0700. Unsupported mappings are refused typed by the handshake: rootless or `userns` (`UserNamespaceUntested`), an engine enforcing SELinux labels (`SelinuxLabelsUntested`: slates never relabels a source), and every untested profile, which includes every Kubernetes and Linux engine (`ProfileUntested`; fsGroup has no profile, AUD-29-75). Proven: `crates/bridge-oci/tests/runtime.rs` and `crates/cli/tests/cli.rs` `a_containers_identity_reaches_the_export_as_its_profile_states` (three non-matching identities in real containers; `docs/bugs/2026-10-01-container-identity-semantics-were-unstated.md`). **AUD-29-76 in part 2026-10-01:** the host-mount form attached a snapshot and its mount presented the live head (the test read `after!` where the snapshot held `before`). It is now refused `SnapshotNotPresentedByHostMount` like the FUSE and OCI forms, and the NFS edge never admits a capability whose record names a snapshot (`a_snapshot_is_never_presented_through_a_host_mount_of_the_head`; `docs/bugs/2026-10-01-a-host-mount-of-a-snapshot-presented-the-head.md`). **Snapshot exports built (2026-10-01):** a read host mount of a snapshot presents the attachment's own read-only view, a copy-on-write clone pinning the snapshot. It is opened before the record commits, closed with the attachment and before a destroy, and rebuilt at restart. A container bind binds only to a mount presenting the version it asks for (`crate::snapshot_view`; `a_snapshot_host_mount_presents_the_snapshot_read_only_and_pins_it`, `…again_after_a_restart`; `docs/bugs/2026-10-01-a-snapshot-could-not-be-presented-through-a-mount.md`). **Subtree exports built (2026-10-01):** `slates mount ID DIR --subtree DIR` (macOS NFS, Linux FUSE) presents one directory, scoped by the export itself. `ScopedBridge` on the shared `Bridge` seam makes the directory the root, names it its own `..`, and answers every handle outside it `NotFound`, forged ones included. The scope is recorded by inode, so a rename does not widen or move it. A file or missing subtree is refused before any effect, and a snapshot of a subtree is refused typed. Proven over the NFS wire with forged handles, through the Linux kernel's FUSE mount, and by the real CLI on macOS and Linux arm64 (`a_scoped_host_mount_reaches_nothing_outside_its_directory`, `a_scoped_fuse_mount_presents_one_directory_and_follows_it`, `slates_mount_subtree_presents_one_directory_of_the_volume`; `docs/bugs/2026-10-01-no-mount-could-present-less-than-the-whole-volume.md`). **Advance, guest views and container scoping built (2026-10-01):** `advance` re-pins a snapshot mount (and its container binds) durably to a later snapshot and names exactly the paths that changed, computed by a diff of the two inode tables that skips shared nodes. A guest device presents a subtree or a snapshot (`GuestView`). A container bound to a scoped mount sees that directory alone, proven through Docker Desktop. See `a_snapshot_mount_advances_to_a_later_snapshot_and_names_what_changed`, `the_diff_names_every_changed_path_and_nothing_neither_snapshot_holds`, `a_guest_device_presents_a_subtree_or_a_snapshot_and_nothing_else` and `a_container_bound_to_a_subtree_mount_sees_only_that_directory`; `docs/bugs/2026-10-01-a-snapshot-view-could-not-move-and-no-container-or-guest-could-be-scoped.md`. Advancing a guest's view: done with the guest's durable record (2026-10-01). **AUD-29-76 closed 2026-10-02 (its required correction met):** the authorized scope and version ride the existing attachment and view mechanism. Snapshot views are pinned per attachment and rebuilt at restart. Subtree scope is enforced by the export on every request, recorded by inode, and held across rename. `advance` re-pins a view durably, guests included. Container binds are matched to the version and scope they ask for, with the precise refusals kept for every form not built. Follow-ups, beyond the acceptance: a scoped form for a Windows attach-driven mount once one exists, and measurements of the per-object scope check and of the snapshot diff on a large span. **Follow-up 2026-10-03:** the first large-span measurement of the diff found it omitting changed files: 9,925 of 10,000 written files named. The reverse name lookup missed every entry that opens a directory-tree leaf; fixed, with the tree oracle now checking it for every entry (`docs/bugs/2026-10-03-a-reverse-name-lookup-missed-every-entry-that-opens-a-leaf.md`). Both measurements were then recorded (`docs/wip/BENCHMARKS.md`, 2026-10-03). The diff's cost follows the changes, not the volume: one change costs 1.2 µs at 10,000 and at 100,000 files, and 10,000 changes cost 6.5–7.8 ms. The scope check costs about 40 ns per level of depth. A scoped listing paid that climb once per entry, 3 ms for a 1,024-entry page at depth 64 and 229 ms at depth 4,096. Its entries and a lookup's result are now checked against the directory the request already admitted (the relation is transitive, so the answer is the same), so a page costs 76 µs at depth 64 and 297 µs at depth 4,096, and a lookup is halved. Both AUD-29-76 measurement follow-ups are done. **AUD-29-75: the deployment constraint, stated 2026-10-01; building it awaits Ada (R10, banned item 4).** Kubernetes publishes a volume into a kubelet-owned target path (CSI `NodePublishVolume`). Two ways exist to make that mount visible to the pod, and each needs a privilege slates may not require:
- A node-plugin pod needs `Bidirectional` mount propagation, which "is allowed only in privileged containers" (Kubernetes docs, `concepts/storage/volumes.md`, "Mount propagation", tier B, fetched 2026-10-01).
- A host process must mount over kubelet's root-owned target directory. From memory, to verify: `fusermount3` refuses a mount point its user cannot write, so an unprivileged daemon cannot.

So no CSI lifecycle exists, and none is claimed. The transport report offers no Kubernetes form. The runtime handshake refuses any non-Docker runtime (`RuntimeUnsupported`), and hostPath use is outside every supported profile. **Decided 2026-10-01 (Ada: the unprivileged alternative).** A sidecar cannot serve its own pod's volume, because kubelet sets volumes up before containers start. So the design (§4.6 "Kubernetes publication without privilege") has kubelet, the node's own broker, mount an `nfs` PersistentVolume from a pod-network NFSv4.2 export of the daemon. The volume's capability is the export's path token, and the transport is RFC 9289 RPC-with-TLS with mutual X.509 authentication or nothing (`xprtsec=mtls`; the node's `tlshd` holds a fleet-issued certificate). Evidence: Kubernetes `volumes.md`/`persistent-volumes.md`, nfs(5) and the kernel's `tls-handshake.rst`, fetched 2026-10-01. Docker Desktop's kernel lacks `CONFIG_TLS` (measured), so the kernel leg runs on the GitHub Linux runner. **Built 2026-10-01: the RFC 9289 server and the network listener.** The codec (`rpc_tls`, golden vectors) and the connection (`nfs_tls`) are in place, and a fleet node with an operator authority serves its NFS edge on TCP at its base port. The anchor holds the listener and hands it across restarts. No new attach form was needed: the edge already authorizes every call by its mount capability alone, so a host-mount attachment's path is the PersistentVolume's path. The `Arc` question was settled by measurement: the per-connection build costs 11.0 µs, 3.8% of the handshake, and the shared config was rejected (BENCHMARKS.md). Proven against a real rustls client, a daemon and an anchored node across `kill -9`, on macOS and Linux. An operator gets the PersistentVolume's path from `slates export ID [--read-only] [--subtree DIR]`, which prints `/<name>@<attachment>.<token>` from the export's own attachment. Proven through the real binary on an anchored node: bootstrap, create, export, `MNT` over RPC-with-TLS admitted, `slates detach`, `MNT` refused (macOS and Linux). Teardown proven the same day: on one open session a file reads while its attachment lives, and is refused `NFS3ERR_ACCES` after `detach` and after the volume's destroy, as on loopback (macOS and Linux). **The kernel leg built (2026-10-01; Ada authorized its CI tooling): `cargo xtask kind export`.** It uses a node image with Debian's `ktls-utils`, a chart with the new optional `authority` and `export` values (gated by a render test), and `slates export`. A PersistentVolume names the export over `xprtsec=mtls`; a writer pod writes and a second pod reads through a fresh mount. CI loads `tls` and the NFS client modules and runs it with `--require-kernel-tls`. Run locally it got past kubelet: `mount.nfs` ran, the node's `tlshd` completed the mutual handshake (the server's chain and IP SAN verified, per the ktls-utils 1.0.0 source), and it stopped at `setsockopt(TLS_ULP)`, which Docker Desktop's kernel lacks. Three bugs were found and fixed on the way. The export refused clients offering no ALPN, and Linux's `tlshd` offers none (`docs/bugs/2026-10-01-the-export-refused-the-linux-clients-handshake.md`). The `tlshd` unit was never started in a kind node. `tlshd` refused the client key at mode 0644. **AUD-29-75 closed 2026-10-02.** The leg's first run on the GitHub runner (run 36965111945, commit `56272c0`, `--require-kernel-tls` after `modprobe tls`) was green: "kubelet mounted the volume over RPC-with-TLS; a second mount read the bytes the first wrote", one enrolled node rolled out in 7.5 s. Until then the report offers no Kubernetes form. Found on the way: `oci::tests::a_bind_source_binds_only_as_the_live_attachment_its_table_entry_names` had been red since `6ba0678`, whose not-shared rule its fixture predated. Its fixture is now a shared mount, and a new test pins the refusal. |
| AUD-29-77–78 | RAM admission/residency must include actual runtime/VMM/cache/mapped/retained-copy boundaries; the 14 skipped OCI/virtio-fs records and capability evidence require reconciliation and real consumer tests (R1, §4.2, §4.6, Part 6). **AUD-29-78 in part, 2026-10-01:** the 14 OCI and virtio-fs records are reconciled without turning a skip into a pass — the OCI skip no longer says no container form exists: it names the verified non-recursive private bind, `slates oci-check`, the Docker Desktop workload (T-4.13) and what is owed (a harness leg that runs the suites inside a container; the Linux bind refused `ContainerWorkloadUnproven`); the Linux FUSE adapter's `not_covered` names the daemon's own FUSE mount and the `allow_other` grant the suites over it need (`crates/conformance/src/capability.rs`; records regenerated by `cargo xtask conformance plan` into a scratch directory and only the OCI/virtio-fs records copied back, so the Linux lanes' run records stay; the matrix regenerated with `matrix --write`). The container leg consumes the recipe's exact semantics (`--mount`, non-recursive, private; AUD-29-65). **78, the in-container runner (2026-10-01):** the harness runs a suite inside a real container through the OCI attachment (`xtask/src/conformance/container.rs`). The sequence is the session's macOS host mount; the runtime handshake, whose profile line the record keeps (engine, version, endpoint with `~` for home, user namespaces, the identity rule); `attach --oci`; `slates oci-check` just before the bind; then one `docker run` of the exact entry as `--mount` (non-recursive, private) as the mounting user, compiling the pinned fsx source inside `rust:1.98.0`. Its record is normalized to `<mount>`, `<uid>:<gid>`, `<scratch>` and `<pid>`. OCI × fsx is now `RAN` (Docker Desktop 29.3.1, 2026-10-01: 10 000 operations, seed 1, passed, 18.5 s). OCI × fsstress is `RAN` too: LTP's pinned sources are staged with the Linux shim and compiled inside the container (500 operations × 4 processes, seed 1, all 2 000 logged with dread/dwrite, passed; the daemon answered afterwards). A host whose engine does not answer is skipped typed, and an untested profile fails the lane. The hand-written lane table in `docs/wip/conformance.md` §4 no longer says the OCI form or the Linux FUSE transport does not exist. OCI × workloads is `RAN` as well (git, cargo and python identical between a Desktop-shared host directory and the bind). Getting there found a cross-transport vfs bug: a directory removed while a transport held it named a freed node, so every image walk met a stale handle and every barrier on the volume was refused. It is fixed on every lane, as is a Docker Desktop hard-link limitation, now stated in the profile (`docs/bugs/2026-10-01-a-removed-directory-a-transport-held-named-a-freed-node.md`). OCI × pjdfstest runs too: LIMITED, unprivileged, 238 files and 8,798 cases (2,314 passed, 2,765 failed, 3,691 needs-root). Its failures are shaped by cause in `docs/wip/conformance.md` §3.4a: the reviewed NFS-FIFO-OPEN client limit, surfaced by Desktop's server opening every node it makes, plus its knock-ons. The harness gained `--transport` to run one transport's cell. **The Linux container profile built (2026-10-01; Ada authorized user_allow_other on CI):** `slates mount --shared` makes an `allow_other` FUSE mount, and the Linux bind binds only it (else `MountNotShared`). The Docker Engine profile is tested with `ContainerIdsAsHostIds`, measured: root bypasses the bits, a foreign id meets them, the mounting user owns what it makes, and hard links are served at once (`a_linux_container_reaches_the_shared_mount_as_its_own_ids`; `docs/bugs/2026-10-01-linux-had-no-container-profile.md`). **The Linux container lane and the hermeticity container leg built (2026-10-01).** The harness's `oci-linux` transport mounts with `slates mount --shared` and runs each suite inside a Docker Engine container bound to it. Linux runs `fsx`, `fsstress` and the workloads (git, cargo and python identical; the other tools are absent from the workload image, as on Desktop) and hermeticity, all `RAN` in the Linux container lane (Debian 13, 2026-10-01). Hermeticity uses a root tracer (`sudo strace -u USER`: an unprivileged tracer strips `fusermount3`'s setuid, measured `fusermount3: mount failed: Operation not permitted`). It drives the workload inside the bind: 0 outside writes in 353 calls, and the landed tree equals the mounted tree. Calls made under an OS mount broker's image (`OS_MOUNT_BROKERS`: `fusermount3`, `mount`) are set aside on Linux and counted and named in the record. The kept trace showed `/bin/mount` (run by `/usr/bin/fusermount3`) making libmount's `/run/mount`. This is the counterpart of macOS's eslogger leg, which keeps only the slates executable's events (`only_calls_under_an_os_mount_brokers_image_are_set_aside`, mutation red). Fixes on the way: the harness reopened only the anchor's first handoff object, so the grant met `ENODEV` (both are reopened now); the mount helper's stderr is carried in `MountError`; and a landing bug (directory modes lost to the umask; `docs/bugs/2026-10-01-landed-directory-mode-umask.md`). pjdfstest runs there as container root, the profile's ids reaching the mount as themselves. It is the first run over slates' own FUSE mount. It matched the root-reviewed Linux list on 1,798 of its 1,800 cases, and its 18 other failures were one bug: a component past `NAME_MAX` answered `EINVAL` or `ENOENT`, not `ENAMETOOLONG`. That is fixed on every bridge (`docs/bugs/2026-10-01-a-long-name-was-einval-or-enoent-not-enametoolong.md`). The rerun is `RAN` against its own list (`expected-failures/oci-linux.pjdfstest.txt`, 1,798 entries, reasons carried over; `conformance.md` §3.4b). `xtask conformance tally` now honours `--transport`; it had ignored the flag. CI's conformance job installs `fuse3` and grants `user_allow_other`. Also owed: a live guest (AUD-29-68). Its siblings are fixed: `Daemon::refusals_on_every_shard` serves the volume-scoped assertions, and `publish.volume_skipped` is a per-shard status refusal naming the volume, replacing the process-wide `PUBLISH_SKIPPED`. **AUD-29-77 in part 2026-10-01:** a virtio-fs device's kept copy buffers are charged to the attachment exactly once. A chain charges its buffers' growth, reserved exactly; the bytes stay charged while kept and are given back when let go or at reclaim, and a growth that does not fit makes the device give back its buffers first. Every transport's residency now names what slates protects (`daemon_ram`) apart from what its bytes reach beyond it (`host_kernel_cache`, `runtime_vm`, `guest_page_cache`), so an export is never reported as a protected workload (`the_copy_buffers_a_device_keeps_stay_charged_to_its_attachment`, the MCP residency assertion; `docs/bugs/2026-10-01-a-guest-devices-kept-copy-buffers-were-uncharged.md`). **AUD-29-77 closed 2026-10-01 (the VMM's guest memory measured):** a guest's residency names `guest_memory` beyond protection: the VMM's memory, holding its page cache and the device's reply buffers. The live QEMU run samples the daemon's own `smaps`: the device's mapping of the 256 MiB guest peaked at 580/516/452 KiB resident over three runs, with 0 KiB locked, asserted (`a_linux_guest_mounts_the_volume_through_qemu_over_vhost_user`; `docs/bugs/2026-10-01-a-guests-memory-was-reported-as-its-page-cache-and-unmeasured.md`). **AUD-29-78 closed 2026-10-02 (its required correction met):** the records are reconciled without a skip turned into a pass, and the tested profiles are pinned. Docker Desktop 29.3.1 and Linux Docker Engine come with their identity rules in the runtime handshake, and QEMU 10.0.13 with the Debian 6.12 guest kernel. The by-use and refusal cases run through the exact OCI mount entry (`--mount`, non-recursive, private), with `slates oci-check` before each bind, on both engines. The live guest that this row owed exists and runs in CI. Follow-ups: the guest's fsx, fsstress and pjdfstest legs (their records say owed), and the pressure and failure suites no transport has yet (Phase 9). **fsx and fsstress legs done 2026-10-03:** the live-guest image builds both from the harness's own pins (each SHA-256 verified at build; `ci/guest/build-exercisers.sh`, held equal to `xtask/src/conformance/fetch.rs` by a doc-truth test), and `crates/server/tests/virtiofs.rs` `a_live_guest_runs_fsx_and_fsstress_over_the_tag` runs them over the tag inside the guest under the harness's bounds (now shared, `slates_conformance::exerciser`): fsx A-OK over 10,000 operations, fsstress exit 0 with all 2,000 operations logged, the tag unmounted cleanly, 85 s in the guest under software emulation; the CI live-guest job requires both lines. **pjdfstest leg done 2026-10-03:** the guest builds the pinned pjdfstest from the harness's probes (now shared, `slates_conformance::pjdfstest`) and runs every file as root over the tag; `a_live_guest_runs_pjdfstest_over_the_tag` judges the gathered results by the harness's rule against `docs/wip/conformance/expected-failures/virtio-fs.pjdfstest.txt`: 238 files, 8,798 cases, the 1,798 failures exactly the oci-linux root list's (reasons carried over), the two NFS-only cases of the native Linux list passing; 23 min in the guest under software emulation. All three guest legs required by the CI live-guest job. |
| AUD-29-79–81 | Guest cache negotiation must have actual invalidation delivery; FUSE creation must apply umask exactly once and preserve profile-specific creating uid/gid separately from Consumer authority (§4.6, §4.13, AC-4.11–4.12). **AUD-29-80 and AUD-29-81 closed 2026-10-01:** every FUSE creation (create, mkdir, mknod) applies its request's umask — under `FUSE_DONT_MASK` the kernel sends the mode unmasked with the umask beside it; without it, masking again changes nothing — and the ABI comment that said the opposite is corrected; the creating process's uid and gid from the request header are stamped as ownership metadata (never authority: the enrolled subject is unchanged), and the shared stamp applies the set-group-ID parent rule for every transport (the parent's group, and the bit on a new directory). Proven by the real dispatch over a real volume (`crates/bridge-fuse/tests/creation.rs`: 0600/0755/0640 owned 1000:100 under a context enrolled as 501; under a set-group-ID parent the parent's group and the bit; red with the fix mutated out: `0666` owned `501:20`) (`docs/bugs/2026-10-01-fuse-creations-dropped-the-umask-and-the-creator.md`). **AUD-29-79 closed 2026-10-01:** the cache a transport promises its kernel now matches the mechanism it has: an attachment carries its transport's `CacheCoherence` (admitted `Revalidated`; the native `/dev/fuse` channel, which writes the seam's invalidations before each request, declares `Invalidated`), every context carries it, and a revalidated context gets zero entry and attribute lifetimes (`VolumeBridge::cache_lifetime`) and an INIT answer without explicit invalidation or expire-only entries but with `AUTO_INVAL_DATA` (cached pages dropped when a revalidation shows a change; an open keeps no page cache), so a virtio-fs guest — no notification queue — revalidates on every use and converges with any other attachment's change, and its capability reports no explicit invalidation. Proven: `crates/bridge-virtiofs/tests/device.rs` `a_guest_without_invalidation_delivery_is_promised_no_cache` (the INIT answer, a created entry's, a GETATTR's and a LOOKUP's lifetimes; red with the default mutated to `Invalidated`: explicit invalidation answered); the native channel keeps its invalidations (Linux kernel FUSE suites, the daemon's mount, the CLI 15/15) (`docs/bugs/2026-10-01-the-guest-was-promised-invalidations-it-could-not-receive.md`). Owed: a notification queue (`VIRTIO_FS_F_NOTIFICATION`) would let a guest cache again; a live-guest stale-read run with the real driver comes with AUD-29-68. |
| AUD-29-82–83 | Guest replies/fsync require checked anchor recovery publication and the same current owner/epoch fence as the live NFS path, including reads and f=0/fleet equivalence (D-18, §4.6, §4.8, R8). **AUD-29-82 closed 2026-10-01:** a guest request that changes what survives a restart (the shared `needs_barrier` rule — namespace and attribute changes, `fsync`, the `flush` every close sends; a write is unstable until one of them) is dispatched and its reply scattered, but its used element is held: the pass stops, the serve loop runs the owner's barrier outside the bridge borrow (`BridgeAccess::barrier`: the daemon publishes the shard's recovery image and asks whether it captured the volume), then completes the chain — published as served when captured, else answered `EIO` with the reply's grants given back (counted `virtiofs.barrier_refused`, the device's `barriers_awaited`/`barriers_refused`); `VolumeBridge::flush`'s claim that its bytes were already durable is corrected. Proven: `crates/server/tests/virtiofs.rs` (`a_guests_acknowledged_close_survives_a_daemon_restart`: create, write, flush and release acknowledged; a stop that publishes nothing; a second daemon over the same segment reads the bytes back over NFS; red with the barrier mutated out: `LOOKUP kept.txt: status 2`), `crates/bridge-virtiofs/tests/device.rs` (`a_mutations_used_element_waits_for_the_owners_barrier`: no used element before the barrier, a refused one answers `EIO` and gives back two grants, a write waits for none) (`docs/bugs/2026-10-01-guest-mutations-were-acknowledged-before-the-barrier.md`). **AUD-29-83 closed 2026-10-01:** the guest loop asks the owner before every pass whether it may serve the volume's latest state (`BridgeAccess::fenced`; the daemon answers with `verbs::live_tree_fenced` — configuration group ready and owner lease holding, the NFS live tree's gate, its refusals counted by reason); while fenced no pass runs: the guest's requests wait in its rings, re-asked each heartbeat (the cadence the confirmations that restore the lease arrive at), a pause and never a stale answer, as a hard NFS mount retries `NFS3ERR_JUKEBOX`; a revoke or the guest's hangup is still seen each interval. The daemon's FUSE turn applies the same fence (no request read while fenced) — a sibling found here. Proven: `crates/server/tests/fleet.rs` `an_isolated_owner_holds_its_guests_requests_rather_than_serve_them` (f = 1, three nodes: the guest's read answered while A's lease held; A isolated until its lease lapsed; a fresh read and a CREATE unanswered for five heartbeats; the loop's `fenced_waits` moved and the lease refusals were counted; the hangup ended the loop; red with the fence mutated out: answered `true`), `crates/bridge-virtiofs/tests/serve.rs` `a_fenced_owner_holds_the_guests_requests_until_its_lease_holds` (held, then answered when the fence lifts) (`docs/bugs/2026-10-01-the-guest-path-ignored-the-owner-lease-and-a-busy-guest-postponed-revocation.md`). Owed: the FUSE turn's fence by test (a fleet with a Linux kernel mount); the f = 0 leg is the same code (`needed = 0`, the lease holds) and runs in `crates/server/tests/virtiofs.rs`. The crash at every publication step and full recovery storage are covered by the shared publish path's own oracle (`crates/server/tests/recovery.rs`), not re-run per transport. |
| AUD-29-84–85 | Revocation must fence mounted Consumer/borrowed OCI capabilities before acknowledgement; malformed reply geometry must refuse before effects and retain owned release of every unreported handle/reference (§4.6, §4.13, A-28). **AUD-29-85 closed 2026-10-01:** a request whose fixed-size success reply (entry, create, attributes, open, write) cannot fit the room posted is answered `EIO` before dispatch — by the virtio-fs device (counted `replies_truncated`) and by the dispatch itself; reads and directory pages are clamped to the room, and READDIRPLUS checks an entry fits before referencing it; a success reply that never reaches its caller (a refused barrier's `EIO`, the kernel's `ENOENT` for an interrupted request, a failed scatter into guest memory) has its lookup references forgotten and its handle released by the server (`reclaim_unreported`; counted `fuse.reply_reclaimed` and the device's `reclaimed`). Proven: `crates/bridge-fuse/tests/dispatch.rs` (`a_reply_with_no_room_refuses_before_any_effect`, red before the fix: the CREATE reached the seam; `reclaiming_an_unreported_reply_gives_back_exactly_what_it_granted`), `crates/bridge-virtiofs/tests/device.rs` (a 16-byte CREATE has no effect and its retry succeeds; a CREATE whose reply memory fails the write gives back one reference and one handle, and the unlinked inode is reclaimed), `crates/bridge-fuse/tests/owner_turn.rs` on a real Linux mount (the refused `mkdir`'s reference given back: `reclaimed: 1`) (`docs/bugs/2026-10-01-an-undersized-or-lost-reply-orphaned-its-grants.md`). **AUD-29-84 closed 2026-10-01:** the revocation's per-shard step, which already marked the consumer's channels before `Revoked` was acknowledged, now also ends every attachment the consumer holds on that shard's partition (`attachments_held_by_consumer`, each ended through `end_attachment` as a recorded operation): its host mount's token then authorizes nothing at the NFS edge (checked against the record on every request), a FUSE mount is unmounted, and a container binding's held record is gone; a shard that cannot end one refuses the revocation (the durable revocation stands and a retry ends what is left), never acknowledging one not in force; the account's own attachments are untouched. Proven: `crates/server/tests/daemon.rs` `a_revoked_consumers_mount_capability_reaches_nothing` (macOS: the consumer's read served before the revocation; after it the old handle answers `NFS3ERR_ACCES`, the old capability mounts nothing, the account's mount still reads; red before: the old handle read `0`) (`docs/bugs/2026-10-01-a-revoked-consumers-mount-capability-outlived-the-revocation.md`). Guest devices admitted for the consumer are revoked as well (AUD-29-73, closed 2026-10-01). Owed: Linux's consumer FUSE mount ends through the same `end_attachment` the detach test proves. |
| AUD-29-86–87 | Directory enumeration/encoding needs page-bounded admitted semantic work; busy guest queues must observe stop/authority state at bounded service boundaries and complete owned reclamation (§4.2–§4.3, §4.6, T-4.14). **AUD-29-87 closed 2026-10-01:** the guest loop checks the revoke request at every pass boundary (and every fenced wait), so a guest that keeps its rings full yields between passes and is stopped at the next one; the terminal step owns what was in flight (a chain held for the barrier included: the sweep releases its grants, the ledger restores its charge). Proven: `crates/bridge-virtiofs/tests/serve.rs` `a_revoke_requested_mid_service_ends_the_loop_at_the_next_pass` (16 requests in the ring, the revoke asked from inside the first pass: one pass, 8 answered, `Revoked`, reclaimed; red with the check mutated out: two passes) (`docs/bugs/2026-10-01-the-guest-path-ignored-the-owner-lease-and-a-busy-guest-postponed-revocation.md`). **AUD-29-86 closed 2026-10-01:** directory cookies are the top 31 bits of the name hash (`.` 1, `..` 2, children ≥ 3), stable across other names' changes; a page is one descent to the first hash at or above the cookie and its own entries, bounded by what the reply can hold (plus one), and never ends inside a group of names sharing a cookie (a group larger than the reply refuses `EOVERFLOW` / `NFS3ERR_TOOSMALL`). Proven: `crates/vfs/src/dirtree.rs` `a_listing_resumed_from_a_hash_equals_the_maps_range_from_it` (oracle over three levels of splits and after merges), `crates/bridge-core/tests/volume_bridge.rs` (`a_directory_paged_by_cookie_lists_every_entry_exactly_once`, `unlinks_between_pages_skip_and_repeat_no_survivor`, `a_page_never_ends_between_names_sharing_a_cookie`), `crates/bridge-fuse/tests/volume_bridge.rs` (READDIR pages resumed from the kernel's offsets; `EOVERFLOW` for a group larger than the page); the macOS kernel NFS mount (CLI 13/13) and the Linux kernel FUSE mount and CLI (15/15) list through it (`docs/bugs/2026-10-01-a-directory-page-did-whole-directory-work.md`). Owed: an overlay page repeats the base merge check; the unkeyed FNV-1a order lets a writer craft a cookie group that makes its directory unlistable through a small page; no per-page work counter (the bound is structural: one descent plus the page). |

The fifth pass uses baseline 733ea3e with concurrent IPC/fleet edits; f18f02c and
bc81da4 arrived during inspection, followed by further landing edits. Linux v6.12
primary sources, checked 2026-09-29,
provide independent umask/cache expectations and kernel permission posture. The
compiled-library diagnostic (§7.6, 20-second build/compile and three-second execution
bounds) uses public Bridge/Device APIs and the existing simulated queue fixture over
anonymous RAM. It reproduced DONT_MASK with discarded umask (0666 vs 0600 and 0777 vs
0700), creating credentials 1000:100 discarded for enrollment/parent 501:0, explicit
invalidation negotiated with u64::MAX metadata validity, CREATE/WRITE EIO after visible
effects, two open handles for one successful CREATE reply, and 66 bridge rows for a
32-byte/one-entry directory page. Maximal requested directory size with a 16-byte
posted reply also returned EIO after processing. These are bounded diagnostics, not a
native guest, protected-residency or large-directory performance pass.

Fresh bounded offline tests passed 10 bridge-core, 58 FUSE and 10 virtio-fs cases
(78 total; no failures/ignored in selected binaries). The guest differential uses the
same dispatcher on both legs and cannot expose their shared semantic mistakes.
Source inspection found missing guest recovery-publication/owner-lease gates, mounted
Consumer revocation fencing and stop checks inside continuously busy queue drains.
No real mount, daemon crash, container, Pod, VMM or network experiment ran. No new
implementation repair or acceptance closure follows from these passes.

The audit's §14 adds 16 P-case families to the 30 E/24 O families (70 total) and
records page/cursor, metadata projection, incremental recovery, coherence and
loss-aware continuation experiments. Its 100,000-child/128-entry-page calculation
(782 pages, about 78.2 million row visits; 234.6 seconds of hypothetical sequential
300 ms RTT) is a scale inference, not a measured WAN result. No controller change,
new constant, lock service, per-write consensus, disk object or privilege is authorized.
The new tripwires extend GAP-A9-1's transient/reference admission, GAP-A9-5's mounted
semantics, GAP-A9-4's guest barrier scope, GAP-A9-6's recovery publication,
GAP-A9-9's mount/device authority, GAP-A9-11's semantic-work cost and GAP-A9-15's
independent transport evidence. All nine fifth-pass findings
remain open. Separate landing closures below are preserved as recorded evidence and
were not independently rerun by this continuation.

The fourth pass uses baseline b455527a4887ef309335e3f5e42a947b179a50da and primary
OCI runtime-spec v1.3.0, VIRTIO 1.2, Docker, Kubernetes, CSI and VMM documentation
checked on 2026-09-29. The audit's §13 distinguishes mount interoperability, Docker
engine behavior, Pod storage lifecycle and native filesystem devices; none is
certified merely by a runtime name. Production Linux OCI is blocked by both daemon
FUSE wiring and the volume-identity check. No Kubernetes volume publication or real
VMM binding is implemented. Optional DAX/packed-ring support is not a baseline
compliance requirement, and lack of CSI is an integration gap rather than invalid
OCI mount syntax. No disk socket/image/directory, privilege or fallback is authorized.

Eight OCI verification and 28 simulated guest tests passed under 20-second command
bounds. The public-API driver (§7.5, 20-second compile / three-second run bounds)
reproduced five boundaries: absent Linux volume source identity, a live registry
attachment after refused guest setup, absent seam release/live registry state after
dropping an admitted device, identical queue layouts accepted, and a writable buffer
pointing at another queue's ring accepted. Its binary was removed. Source inspection
also found that acknowledged consumer revocation marks client channels without
fencing guest registry/device authority. All 14 OCI/virtio-fs conformance records are
skipped, with stale OCI reasons. The 24 new O-case families extend the 30 E-cases;
native runtime/Pod/VMM and strict RAM evidence remain owed. No service, mount,
container, cluster or guest was launched and no implementation repair is accepted.

Concurrent commit 7836621 records the grant-binding repair/closure of AUD-29-01
during this review; the audit preserves its recorded evidence and does not claim
to have rerun the landing tests. All 15 fourth-pass findings remain open.

This adds explicit open tripwires to GAP-A9-5 (guest and OCI integration),
GAP-A9-9 (consumer authority reaches devices), GAP-A9-11 (actual retained/mapped
costs) and GAP-A9-15 (transport evidence). Earlier by-use histories remain evidence
for their exact profiles; they do not close these boundaries. No acceptance row or
design decision changes in this audit.

**Closed 2026-09-29 — AUD-29-01 (P0): a grant binds its consumer, volume, snapshot and target identity at
use.**
- **The binding.** A grant carries what it was presented for: the consumer by its exact principal key, the
  volume, the snapshot, and the target by its key and the opened directory's device and inode
  (`slates_land::grant::GrantBinding`).
- **The check.** The engine builds the landing's own binding before any write-capable step, and
  `Grants::check` refuses `Unbound { field }` when they differ. A session grant may differ only in
  snapshot and manifest. The server answers `GrantMismatch` and counts `grant_unbound.<field>`.
- **The durable record.** It now names the consumer it was made for and that consumer's session, not the
  issuer.
- **Failing tests first.** `crates/land/tests/grant_binding.rs`: a same-manifest plan landed into another
  directory, into a directory that replaced the target at its path, and for another consumer. All three
  now refuse with the disk unchanged; the approved landings still land. The daemon's `grant_scenario`
  refuses a retarget end to end.
- **Record.** `docs/bugs/2026-09-29-a-grant-did-not-bind-its-target-volume-or-consumer.md`.
- **Still open in this row.** 02's durable source field for unnamed landings; 02 (exact snapshot landing),
  03, 04, 05, 06 and 07 are closed below.

**Closed 2026-09-29 — AUD-29-06: a session grant was recorded consumed, and a restart lost every grant.**
- **Transitions.** `finish` records only the transition the engine made: a single-use grant consumed by a
  finished landing, never a session grant.
- **Restart.** A restarted shard rebuilds its runtime grants from the durable records, in their states,
  with each binding intact: the record now carries the target identity. New grant ids pass every recorded
  one, where the first grant after a restart used to be refused `AlreadyExists`.
- **Evidence.** `a_session_grant_outlives_a_restart_and_a_single_use_grant_stays_spent`, red with either
  part disabled
  (`docs/bugs/2026-09-29-grants-were-lost-at-a-restart-and-session-grants-recorded-consumed.md`).

**Closed 2026-09-29 — AUD-29-04 (A-43): a landing's removal could remove an outsider's replacement.**
- **The rule.** Every removal is a move to the landing's own aside name, a check there, then the removal.
  The move is an exchange or a rename that replaces nothing; this covers deletes of files, symlinks and
  other entries, directory removals, clears, directory renames, both exchange fallbacks and the undo
  paths. An entry that is not the witnessed one goes back without replacing anything, or is kept and
  reported (`Degradation::Kept`) when its name was taken. The seam no longer has a replacing rename.
- **The resume.** It settles aside names before its verdicts. A fallback crash inside its window puts the
  old entry back, so each path is old or new again.
- **Evidence.** The removal oracle, its rules stated once in `crates/land/tests/common/removal.rs`: an
  outsider save armed at every seam call and at every pair of calls, over nine removal kinds.
  - Over the simulated host (139 single and 547 pair histories): red on the old engine for seven kinds
    with one save and all nine with two.
  - Over a real Linux tmpfs directory: red on the old engine for six kinds with one save.
  - T-1.15 without the exchange: red when the sweep follows validation, or when a witnessed aside is
    removed while its name is free.
  - All green now. The exchange path's engine cost is unchanged within noise.
  - Record: `docs/bugs/2026-09-29-a-landing-removal-could-remove-an-outsiders-replacement.md`.
- **Siblings, open.**
  - The sweep swallows its errors (AUD-29-05, next).
  - A recursive removal's witness is the directory's inode only, so an outsider's file written beneath a
    removed directory goes with it (`Rmdir` as defined; a subtree witness would be a §4.15 amendment).
  - A landing never resumed leaves its hidden siblings, now including a fallback crash's aside entry,
    until a landing with its id runs.

**Closed 2026-09-29 — AUD-29-05 (A-45): a landing advanced entries it had not made durable, and hid its
cleanup failures.**
- **The boundary.** An entry leaves the overlay only when its directory synced (a rename's two) and the
  media barrier held when the grant asked for it. The rest are `held` and the resume advances them, so a
  failed sync can no longer strand landed work outside the overlay. `Done` means every entry advanced; a
  skip that left an entry private is `Partial`.
- **Reported, not hidden.** A directory that cannot be opened or synced (`Unsynced`), a failed requested
  media barrier (`MediaUnsynced`, as against `BarriersOnly`), a sibling the sweep cannot settle or a
  temporary a failed write cannot remove (`Leftover`), and a directory the sweep cannot list (`Unswept`)
  are typed cells. The reply now carries them, with the durability, `held` and the ramp depth, to the CLI,
  MCP and the Node SDK.
- **Found on the way.** A `mkdir` that met a file counted as already there; it is now a type conflict.
- **Evidence.** `crates/land/tests/durability.rs`: five histories under simulated faults, four red on the
  old engine and all green now, each resuming to the reference. The daemon's landed reply is asserted.
  Record: `docs/bugs/2026-09-29-a-landing-advanced-entries-it-had-not-made-durable.md`. Part of the change
  landed inside `ca53844`, whose commit swept the working tree's edits.
- **Open.** A grant asking for media durability on a target that cannot perform the barrier should be
  refused from a capability, not held on every attempt. The land verb never asks for media durability.

**Closed 2026-09-29 — AUD-29-07 (A-46): landing presentations were never consumed, expired or bounded.**
- **The lifecycle.**
  - A granted landing runs under the id of the presentation its grant was issued from and consumes it when
    it finishes; an aborted one keeps it, so a resume keeps the id its siblings carry.
  - A client's re-presentation of the same volume and target replaces its own.
  - The reaper abandons a retired client's presentations on every owner shard.
  - A shard holds at most one per client seat of the daemon, refusing `LandingsAwaitingFull` past that
    before anything is allocated or recorded.
  - The status report counts them against the bound.
- **Evidence.** The consumption and abandonment tests failed on the old lifecycle and pass now; the bound's
  refusal changes nothing. Record: `docs/bugs/2026-09-29-landing-presentations-were-never-consumed-or-bounded.md`.
- **Open.** Presentations do not survive a restart (a grant for an earlier one is `NotFound`), and a single
  client can use the whole bound until it lands or goes.

**Closed 2026-09-29 — AUD-29-03 (A-47): a target's landing lease was per shard and per path.**
- **One lease per target.** The lease is the control partition's durable record under the target's
  canonical identity, the opened directory's device and inode, so every owner shard and every spelling of
  the directory meet one lease. A take is refused `LandingLeaseHeld`, naming the holder, while it is
  unexpired, whoever asks; the holder is one landing attempt, and the generation is the take's log
  sequence, so it only grows across releases and restarts.
- **The landing under it.** A granted landing runs as an owned task on its volume's owner shard: it takes
  the lease by a bounded message, runs the engine under it, commits the landing's records with the
  request's completion, releases the lease and then replies; a retry joins it. The engine writes only under
  a live lease on its own target (`LandingLeaseLost` otherwise) and starts no entry after the term
  (`Skipped(LeaseEnded)`, `Partial`).
- **Bounded and never stranded.** A take past its caller's deadline takes nothing, a lost answer is
  compensated by a release keyed to the attempt, and a take releases every lease whose term has ended. The
  status report counts `landings_in_flight` and `target_leases`.
- **Evidence.** Two red histories on `c5b47cb`'s engine (a lease in another shard's table, and under another
  spelling, each let a granted landing through to `Done`); the engine's live-lease and paused-holder
  histories; four control-shard tests; a daemon test with two shards' volumes, one through macOS's
  firmlinked spelling, refused naming the one holder and then both landed with nothing left; and a restart
  test where the first daemon's lease refuses a landing under the second until its term. Record:
  `docs/bugs/2026-09-29-a-target-landing-lease-was-per-shard-and-per-path.md`.
- **Open.** The term is still the failover bound (a measured landing-duration term is owed); since
  2026-10-01 a running landing renews it between slices once half has passed (AUD-29-25), so a landing longer
  than one term completes. Two daemons on one machine do not exclude each other's landings; a directory and a directory
  inside it are two leases; the record is not yet written to the host's candidate holders. The cross-shard
  call's reply handoff drops a refusal uncounted (`xshard::call_on`, the forward path), bounded only by the
  caller's deadline.

**Closed 2026-09-30 — A-48 (found preparing AUD-29-02): a clone of an older snapshot was judged by the head's
witnesses.** The base plane's witness, home, whiteout and redirect tables are versioned by epoch; a snapshot
and a clone of it read what the snapshot froze; a destroy drops what only it read (at most live snapshots + 1
versions per key); image layout 7. The clone landing that replaced an outsider's file on `c5b47cb` now
conflicts (`docs/bugs/2026-09-30-a-clone-of-an-older-snapshot-was-judged-by-the-heads-witnesses.md`). Open:
the tables are heap outside the metadata ledger (§4.2).

**Open 2026-09-30 — A-50 (audit §9; Ada's requirement): slates is pure in memory.** Zero disk access is the
baseline requirement on every platform, Windows included: only a base read (host directory or remote file
server) and a granted landing touch disk. No `/tmp`, no temporary directory, no RAM directory (tmpfs,
`/dev/shm`, RAM disks are filesystems), in tests as in the product. The contract is in the design (§0.2) and
its site table is enforced by `cargo xtask check`. **Done 2026-09-30:** every test's real host directory
(landing targets, bases, kernel mount points, conformance scratch and records) is in the build output on every
host; `mktemp`, `$RUNNER_TEMP`, `/dev/shm` and the `SLATES_TEST_RAMDIR` gate are gone, so the OS landing,
removal, base-watcher and host-differential suites run on macOS too; the tracer judges `/dev/shm` writes as
outside. That found the dead kqueue base watcher (fixed,
`docs/bugs/2026-09-30-the-kqueue-base-watcher-drained-into-a-zero-length-list.md`) and a removal oracle that
held only on tmpfs (fixed, `docs/bugs/2026-09-30-the-removal-oracle-assumed-tmpfs-identity-and-timing.md`).
**AUD-29-63 closed 2026-09-30:** property suites replay their reviewed seed files compiled in and report a new
failing seed on the error stream, never a file (`crates/test-seeds`; four by-use tests, one of which proves a
compiled seed is replayed first; `cargo xtask check` refuses a suite that could write); the fleet trace goes to
the error stream (`SLATES_FLEET_TRACE=1`); the landing target fixture is in the build output. Defects still to
remove: the CLI's reads of fleet manifest, certificate, key and recovery-key files (and the test fixtures that
write them); proof of what backs each kernel pseudo-file the machine profile reads. The residency, disclosure and ownership claims stay open with their findings (AUD-29-41, -42, -43,
-44, -45, -59, -62).

**Closed 2026-09-30 — AUD-29-02 (A-48, A-49): a landing lands exactly the snapshot it names.** The engine's
source is explicit through plan, verdict and write; the snapshot's own witnesses judge it (A-48); the advance is
relative to it, so the head's later edits stay private and land next as replacements. The daemon test was
refused `Unsupported` on `57a1f2f` and lands now (`crates/server/tests/snapshot_landing.rs`,
`crates/land/tests/source.rs`). Owed: an unnamed landing's durable records name the head snapshot while the live
head lands (a catalog format version). History of the mitigation follows.

**Mitigated 2026-09-29 (superseded 2026-09-30, above) — AUD-29-02: a landing of a named snapshot landed the live head.**
- **Now.** A named snapshot lands only while the head is still exactly its state (`Volume::unchanged_since`:
  nothing but snapshots journaled since, no record dropped). A head changed since is refused `Unsupported`
  before any host access, and a snapshot the volume never had is `NotFound`
  (`docs/bugs/2026-09-29-a-landing-of-a-named-snapshot-landed-the-live-head.md`).
- **Owed.**
  - Exact landing of an older snapshot. Its first need, the base plane's witnesses frozen per snapshot,
    is done (A-48, 2026-09-30).
  - The engine's source made explicit through plan, validate, write and advance, with advancement
    guarded by the head still equalling what landed.
  - A source field in the durable landing and grant records, which needs a catalog format version.

The third pass uses baseline 8ab25deb7c31bfca77a33cc681ce120bac76e7d1 plus concurrent
fleet/heartbeat diagnostics; those diagnostics were subsequently committed in ca60bd9
and continued changing during the review. They are not accepted here as repairs.
Fresh bounded offline cluster/transport and conformance builds passed. Six holder tests
and four Copa tests passed. Four compiled-library drivers reproduced retry/orphan
retention and premature shared-chunk release, raw-flight certificate visibility,
payload-only congestion accounting and a fitting control frame blocked by the
prospective budget, and trace exemptions without object/grant/residency evidence.
The documented driver sources were rechecked under 20-second compile / three-second
run bounds and their binaries removed. No live spill, reflected traffic, Windows
leak or full-product/network acceptance is claimed.

The audit's §§9–12 specify the strict access/residency boundary, sophisticated transfer
experiments, congested/unstable/extremely-low-bandwidth measurements and 30 end-to-end
case families. The literal no-disk-access request is stronger than the current
design-sanctioned base/bootstrap reads; no exception is inferred. The selected Copa
and scheduling decisions remain in force, with packet-accounting/ownership/security
repairs required before new performance comparisons. The current chosen-controller
grid has 56 scenarios; the historical 57-case selection included coexistence and is a
different evidence claim. Existing admissions, guest/cache and conformance gaps stay
open; neither the new programme nor passing focused tests closes an acceptance row.

The second pass uses baseline 16c847b4a9191e6601bfe93ad026d1a2f2165560 plus
concurrent CLI/IPC/MCP/server/KIND edits. Compiled-library probes reproduced all
eight new findings. Bounded tests passed 230 cluster, 18 VFS and 93 merge cases;
one cluster and one VFS case were ignored. Transport passed 140 cases and failed
two loopback fixtures at bind (OS error 1). Database model fixtures were blocked
by shm_open (OS error 1); the selected publication executable did not run. These
environment refusals are not claimed as either implementation regressions or
recovery passes. The first pass was incorrectly called complete; no acceptance
closure follows from either pass.

Bounded source-inclusion probes on Darwin arm64, rustc 1.98.0 (2026-09-29), reproduced
invalid buddy free/accounting, stale slab-handle revival, oversized allocation panic,
two Raft leaders in the same maximal term, manifest trailing/component acceptance,
ignored restore offsets and request-sequence exhaustion. The audit records the commands
and fixture limits. Format passed; an archive Cargo test was stopped at another build's
directory lock before tests ran. No full-suite, Miri, live-mount or WAN pass is claimed.

The dedicated Raft assessment distinguishes implemented PreVote, CheckQuorum, joint
membership, learners, ReadIndex, compaction, priority transfer and replication windows
from the remaining live scheduling/session/retention evidence. It retains the existing
measured decisions on one leader-origin log and fast-track enablement. No item above is
closed until a failing behavioral test, design-consistent correction, sibling sweep and
the applicable acceptance evidence land.

### 2026-10-03: the Linux startup timeouts — the arena lock's page population, and observations counted in shard time

One CI run of the Linux server tests ended four daemon startups `Deadline`. In a Linux container beside 108 CPU burners
the library suite reproduced it, 7–13 per run. Two causes, both fixed (A-65; §4.2 and §4.14 statuses):
- Shards that were runnable but starved were read as wedged by the observation's wall budget. The budget is now the
  observed shard's CPU time (`docs/bugs/2026-10-03-an-observation-read-a-starved-shard-as-wedged.md`); 7–13 → 1–2.
- The remaining shards had consumed no CPU for the whole budget. They waited in state D in `mmap`/`munmap` while one
  shard's strict-volume arena lock faulted the range in under the process's memory-map lock. The lock is now on fault
  on Linux (`docs/bugs/2026-10-03-locking-an-arena-stalled-every-shard-on-the-memory-map-lock.md`); six loaded runs
  end no observation `Deadline`.

Owed:
- **The quick machine profile under heavy oversubscription.** The tests' and `--quick` profile (5 ms per probe, wake
  extension to 62 ms) refused `MeasurementTimeout { probe: "wake" }` in alternate runs beside 108 burners on 18 cores.
  A full profile has the default budget. No CI lane runs under that load.
- **GitHub runner memlock limit not read.** Whether the CI lane's runner allows a multi-GiB `mlock` (and so reached
  the populate stall) is not yet read from a CI log. The observation fix covers the starvation half either way.
- Still owed from GAP-A9-6: a recovered snapshot's directories are rebuilt privately (RAM only).

### 2026-10-03: AWS-LC is the one cryptographic library (A-66)

Ada's 2026-09-28 decision, built: rustls and rcgen run on aws-lc-rs, the control-plane seal is AWS-LC's AES-256-GCM, the
key schedule AWS-LC's HKDF-SHA-256. `ring`, `aes-gcm`, `hkdf` and `sha2` are out of the build. The golden vectors pass
unchanged, so the wire is byte-identical. Jitter entropy is off at build time (first random bytes 17 ms → 12–17 µs).

Owed:
- **Vendored with mantle's patches (closed 2026-10-03, Ada's decision).** `vendor/` holds ../mantle's copies (mantle
  `3b16867`): the RNDR retry and operating-system fallback (a transient ARM RNDR failure no longer aborts the process),
  #1241, #1165, #617, and the system-library guard. Their suites (850 tests) gate CI on Linux, macOS and Windows.
- **NASM (closed 2026-10-03, authorized by Ada).** The Windows CI jobs and the release build install NASM and refuse
  the prebuilt objects (`AWS_LC_SYS_PREBUILT_NASM=0`).
- **Release targets not yet built with AWS-LC:** `i686-pc-windows-msvc`, `aarch64-pc-windows-msvc` and the musl
  CLI targets build only on a release tag. No CI lane has compiled AWS-LC for them.

### 2026-10-03: integrating ../hyper-raft (A-67)

Owed, in order (`docs/wip/transport-quic.md` §6):
- **H-2** membership on hyper-swim with probes on hyper-datagram's plane. It replaces slates' detector and SWIM driver,
  and the transport's unused control-datagram seal.
- **H-3** the Raft core, the election law by suspicion, node-pair liveness, and hyper-durable over anchor RAM. It
  waits for hyper-raft's R-3.
- **H-4** hyper-quic, hyper-tls and hyper-transport (A-52 stage 3). It waits for the `quic-tls` merge, and closes the
  audit's transport findings AUD-29-34, 35, 36, 46, 50–54 and 60, which are recorded as carried by the shared
  transport.

Done: **H-1**, the snapshot of hyper-timing, hyper-swim and hyper-datagram at hyper-raft `687244f`
(`vendor/hyper-raft/`).

**H-2 built 2026-10-04.**
- What: the membership task drives hyper-swim's one detector over hyper-datagram's sealed plane on the probe port.
  Epochs are keyed from each pair's canonical record session. Every record session announces its dialer's identity.
- Removed: the per-peer probe stack (1,764 lines of `server/src/fleet.rs`) and slates' old SWIM modules
  (`detector`, `swim`, `gossip`, `coordinates`, `fixed`).
- Ported: the takeover acceptance test (`crates/cluster/tests/member_plane.rs`), and the two gossip-admission
  security tests (`member_task.rs`).
- Fleet tests changed, each with its reason in its doc:
  - two-node detection now asserts suspicion without condemnation;
  - the forged identity is asserted at enrollment;
  - the indirect stage is asserted on the plane's counts;
  - mutual-death rejoin is asserted by refutation;
  - the restart fixture's anchor is fixed (it had built a different machine, which the old probe sessions hid).

### 2026-10-04: re-vendor hyper-timing and hyper-swim for the zero-granularity fix (A-67 H-2)

hyper-raft branch `zero-granularity` (`97b9366`, on top of `core-r3`) closes the defect slates' integration found: a
detector whose wakes read exactly on time never configured and never judged. `G` is now bounded below by the owner's
clock resolution. `Detector::new(local, history, members, resolution)` takes that resolution, and
`Detector::unmeasured()` counts the round trips not taken while a member is still measuring. Owed when it reaches
hyper-raft main, green on six targets:
- re-vendor both crates;
- pass slates' monotonic clock resolution (1 ns for its Instant reads);
- report `unmeasured` in `slates status`.

The same revision states hyper-swim's rule that a two-member view never condemns, which slates' fleet suite now asserts.

Also owed in the same re-vendor: hyper-raft `c875028` (branch `swim-first-probe`), the fix for the second defect slates
found. A detector whose first measurement probe, or its answer, was lost waited with no wake for a message only another
detector's probe would send, so a fleet whose first probes were all lost never probed again. slates hit it at a re-key
that dropped datagrams (7 of 10 three-node formations hung); slates now keeps a peer's last address until the fresh
one resolves, so it drops none, but real UDP loss would still wedge the vendored `687244f`. hyper-raft also added
`Detector::join_measured(peer, round_trip)` (`b6e5353`): until a member measures its own round trip to that peer, its
probes wait on the handshake's round trip instead of RFC 6298's fixed 1 s. Owed with the re-vendor: `join_addressable`
joins with the keying record session's measured handshake round trip.

### 2026-10-04: an intermittent async-SDK overflow assertion (open, not yet reproduced)

CI run 37183276944 (`ec4a56c`, ubuntu SDK packaging): `test_every_async_call_ends_across_restart_silence_cancellation_and_death`
failed with `0 not greater than 0 : the overflow is refused at once`. With the daemon stopped (`SIGSTOP`), three times the
client's outstanding limit of `list()` calls produced no `TooManyOutstanding`. It passed on the next runs, and `ec4a56c`
changed only the transport's exported secret.

Suspects, unverified:
- `_daemon_pids` named no live daemon, so nothing was stopped and completions freed the slots;
- the outstanding count admits a call before the bound is checked.

Next: reproduce under parallel load with `SIGSTOP` timing logged, failing test first.

### 2026-10-04: a strict volume's first arena lock on macOS is one unsliced step (owed)

Measured with a scratch probe on an Apple M5 Max: `mlock` wires 4 GiB in 143–144 ms, about 36 ms a GiB, in one call
on the shard that admits the first strict volume. Other threads' mapping calls are unaffected (worst 24–30 µs), so
this is not Linux's cross-shard stall (fixed by lock-on-fault, §4.2). It is one step past the shard's budget
(CLAUDE.md §3, bounded work).

Owed:
- lock in slices across steps, a resumable lock the way `LandingRun` slices a landing;
- or lock a strict volume's own chunks as they are allocated (GAP-A9-1's refinement).

Windows `VirtualLock` commits its range too, and is unmeasured.

### 2026-10-04: the VFS's caller tails on a busy machine (open)

`crates/cli/examples/vfs_tails.rs` times file calls through a real kernel mount under 0–2 spinner threads per
core. The connection now moves to its volume's owner (§4.6 status note), and the daemon's own service time is
about 50 µs p99 under load. The caller still sees tails of 80–400 ms at one or two spinners per core (rename p99
163 ms in the full sweep at load average 40–97) (docs/wip/BENCHMARKS.md "The VFS on a busy machine"). Owed, in
order:
- Attribute the time outside the serve. Candidates: the shard's wake from its driver (not inside the service
  timer); the caller's six sleeps per `rename(2)`; the kernel client. Each needs a measurement before any change.
- Reduce RPCs per call. Two of a rename's six serve macOS's AppleDouble sidecar, which named attributes
  (NFSv4) would remove.
- Find why four callers on one mount are still about 6.7× slower per create than one. A create's `close` sends
  a COMMIT, which is a durability barrier; per-procedure service times are next.
- Gate the lane: a ratcheted p99 and p999 per op at load 1, once the noise floor is measured in-process (as
  `destroy_rows` does).

Sibling found: `crates/cli/examples/slates_mount.rs` mounts `localhost:/<name>` with no mount capability. That
export path stopped working at AUD-01 (`/<name>@<attachment>.<token>`), and on macOS `slates mount` now uses
`mount(2)` (A-34), so the example fails with `No such file or directory`.

### 2026-10-04: loopback NFS confidentiality and hostile connections

`crates/server/tests/nfs_hostile.rs` runs honest clients beside breakers under a spinner per core. The breakers
send partial records, the largest record marker, garbage bodies, writes closed before their reply, and resets
right after the call that moves a connection to its owner shard.

Results:
- 0 honest mismatches over about 3,300 breaks.
- No leaked connection task.
- Every one of a handle's 456 single-bit flips outside the inode counter is refused. The test found the
  inode-prefix alias, now fixed (`docs/bugs/2026-10-04-a-handle-with-flipped-inode-prefix-bits-read-its-file.md`).

Open:
- **Plaintext capability on loopback (macOS v3).** The token rides in every handle. BPF is root-only by default,
  but a host with a group granted BPF access (Wireshark's ChmodBPF) exposes it to that group. The macOS kernel
  client speaks neither RPC-over-TLS nor RPCSEC_GSS, so the fix cannot be encryption on that leg. Candidates:
  - refuse NFS-program calls on a connection a user process owns (the kernel's mount socket has none; the CLI's
    MNT exchange is the only user-process caller);
  - rotate the token per mount.

  The decision and its evidence are owed.
- **Network export.** RPC-with-TLS (RFC 9289) over rustls/aws-lc with the capability (AUD-29-75). The same hostile
  run over the TLS export (tampered records inside the TLS session, truncated TLS records, resets mid-handshake)
  is owed.
- **Fleet planes.** Mutual TLS (QUIC) for records, and hyper-datagram's sealed AES-256-GCM plane for membership.
  Their hostile tests exist at the codec level. A daemon-level break-under-load run like this one is owed.

### 2026-10-04: post-quantum key exchange by default; a GEO-latency handshake defect filed with hyper-raft

slates' rustls dependency had `default-features = false` without `prefer-post-quantum`, so every fleet and export
handshake negotiated classical X25519. Proven by `a_fleet_handshake_negotiates_the_hybrid_post_quantum_group`
(`crates/transport/src/handshake.rs`), which failed with `Some(X25519)`. The feature is on now, and both sides
negotiate X25519MLKEM768.

The amplification tests' fixture was resized for the two-datagram hybrid ClientHello:
- 300 certificate names;
- a spoofed source is silenced after the server first answers, since it can send a whole first flight blind.

The bound held throughout: 7,187 bytes sent against a 7,200-byte allowance.

Owed, fixed at the source (hyper-raft, reported to the mantle agent 2026-10-04): at 500 ms one way, the interim
transport's server ends `NotReady` within about one RTT. The client's handshake retransmits back off from the timer
granularity, not RFC 9002 §6.2's PTO from 333 ms. Each two-datagram retransmit counts twice against the server's
32-retransmit cap. The first GEO-class leader moved from 9.15 s to 19.5 s.
`at_the_geo_class_profile_the_fixed_timing_campaigns_against_a_live_leader_and_the_derived_timing_does_not` is
ignored, with that reason, until the H-4 re-vendor of hyper-quic, which must carry the tests the report asks for.

### 2026-10-04: lease reads wait for their confirmation (closed)

A latest-state verb meeting an unconfirmed owner lease is parked until it confirms or the lease bound passes,
instead of refused at once. This closes the CLI deployment's spurious first-mount `LeaseUnconfirmed`
(`docs/bugs/2026-10-04-a-first-mount-after-create-was-refused-lease-unconfirmed.md`).

Still open from the same CLI runs at load average about 60, each seen once:
- `a_fleet_node_under_its_anchor_keeps_its_serve_ports_across_a_daemon_restart` ("a manifest port was free while
  the daemon was down");
- `an_anchored_node_serves_its_export_over_rpc_with_tls_across_a_daemon_restart` ("a fleet daemon answered").

Both passed 3/3 alone and the suite then passed 5/5 at load average 12–42. Owed: a reproduction under load,
then a diagnosis from logs.

### 2026-10-04: a landing's writes cannot be redirected outside its target by link swaps (proven)

`crates/land/tests/os_escape.rs` strikes a real granted landing before each of its seam calls in turn (59 on macOS
APFS, 58 on Linux in the CI image) with one of three links an attacker on the host would swap in:
- the written directory replaced by a symlink outside;
- an overwritten file replaced by a symlink to a file outside;
- that file replaced by a hard link to it.

After every history the outside directory is identical: entries, inodes, sizes, mtimes and bytes. The engine
writes only through handles (`openat`-relative creates, renames that replace an entry, never a write in place
through a name).

Still owed for condition 4: the same battery against the base overlay's reads, and against the container and NFS
paths (names with `/` or NUL, `..` at an export root, a container's own symlinks followed by a host-side tool).

### 2026-10-04: barriers publish deltas (A-68 built); what remains

The quadratic barrier is fixed: create and untar rounds are flat as the volume grows, and the daemon's own service
is about 9% of a macOS bsdtar round's wall time (docs/wip/BENCHMARKS.md, A-68). Owed:
- **Snapshot-aware and base-plane deltas.** A volume with snapshots, a clone origin or a base plane still publishes in
  full at every barrier, so a snapshotted volume's barriers stay linear in its size. `trie::changed` (the
  copy-on-write diff of two tables) is the likely tool.
- **macOS round trips.** About 100 µs of kernel client per RPC, times about 37 RPCs per extracted file. NFSv4 named
  attributes would end the AppleDouble sidecars, but the macOS client speaks NFSv4.0 and slates serves 4.1 and 4.2.
- **Daemon memory grows** about 120 MB per round of 4,000 small files (160 → 283 MB in two rounds). Attributed and
  cut (A-69, below): each 4 KiB file and its AppleDouble sidecar took a 16 KiB block on macOS arm64.

### 2026-10-04: hyper-raft re-snapshot at 3a6c288 (done)

hyper-swim, hyper-timing and hyper-datagram are vendored at hyper-raft `3a6c288` (S-4 on `17c9964`), green on all six
targets plus Miri and the model checks (run 37233401622). The snapshot carries:
- the lost-first-probe wedge fix (`c875028`; slates found it);
- `Detector::join_measured` (`b6e5353`): slates now joins each peer with the keying record session's smoothed round
  trip, so first probes wait on a measured round trip, not a 1 s default;
- `G` bounded below by the owner's clock resolution (`97b9366`), which slates now measures
  (`slates_machine::clock::resolution_ns`: `clock_getres` of the clock it reads; Windows' 100 ns tick);
- hyper-timing's log-linear histogram and path-sample freshness.

This closes the 2026-10-04 re-vendor entries above. Still owed from hyper-raft: the hybrid-handshake high-RTT tests
(a)–(d), queued there, and H-3/H-4.

### 2026-10-04: content allocated in 4 KiB granules (A-69, done)

The arena's block unit is `min(page, 4096)`; chunks stay sixteen host pages, and the buddy's per-granule arrays are
zero-allocated. Daemon RSS with 4,000 files of 4 KiB fell from 294 MB to 115–124 MB and the empty daemon from 41 MB
to 34 MB, with 256 MiB write throughput unchanged (BENCHMARKS, "Content granule"). Attributed the same day (BENCHMARKS): the rest was
the AppleDouble working copy, one 4 KiB block per file, now dropped when canonical (A-70; RSS with 8,000 small files
84 MB). Measure with `footprint --forkCorpse` and a `MallocStackLogging=1` memgraph, never `vmmap`, which suspends
the daemon long enough for the anchor to kill it. Still owed: each provenance attribute takes a whole attribute
inode (about 600 B of slab per file); a small value held in the owner's table was rejected by §4.5 for copy-on-write
cost and is not revisited without a measurement.
Found on the way and fixed: `observe.rs`'s slow question spun by the wall clock, which under load is not late by the
shard's own clock that A-65 budgets in (docs/bugs/2026-10-04-the-slow-observation-test-was-not-slow-under-load.md).

### 2026-10-04: containers over slates on macOS; the partition log's pages (A-71)

- **Done (A-71):** the partition log ring rewinds when a trim empties it; a long run keeps one snapshot interval's
  pages, not the whole 2.86 GB ring's (BENCHMARKS).
- **Measured:** a Docker volume of type `nfs` (NFSv4.2 from Docker Desktop's own Linux kernel, `slates export`)
  runs a full file workload, including `rm -rf`, with the volume empty afterwards. A host `slates mount` bound into a
  container cannot: Docker Desktop keeps every touched file open on the host, so deletes become `.nfs.*` entries
  (2,473 of 2,524). `slates export` now prints the port beside the path (`port: N`, and `"port"` in `--json`; A-73,
  proven against the kernel mount's own port in `slates_mount_establishes_a_real_kernel_mount_and_unmount_removes_it`).
  Owed: the OCI report should name the NFS-volume form for Docker Desktop.
- **Owed:** the read pass over the NFSv4.2 volume is 2–9× the host bind's (1.7–4.3 s against 0.46–0.82 s for 2,524
  files). Every open is a round trip. NFSv4 read delegations (RFC 8881 §10.4) are the standard remedy, to measure.
- **Done (A-72):** trie removal frees the nodes it empties, and the op log's ring is capped at its budget. Rounds 3 to
  24 now grow the daemon +0.66 MB, consistent with the 512 MiB volume's op log still filling toward its charged
  5.4 MB.

### 2026-10-04: NFSv4 under a parallel build (A-74, A-75)

- **Done:** session slots from one client's in-flight bound (A-75); buffered calls served and answered together
  (A-74); NFS service-time signals and `nfs4.*` forwarding counters.
- **Fixed on the way:** `delivery::take` compared a named descriptor's times before its kind, and `/dev/null`'s
  times move with any process's write (sampled: every 0.2 s), so the test read a device as `NotInherited` at
  random on a busy machine. The object (device, inode) is now compared first, then the kind, and the times only
  for a pipe or socket.
- **Next, Ada's "do all" (2026-10-04), in order:**
  - (A) run a compound where its volume lives — **done (A-76)**: the session moves with its connection, so the
    other-shard case matches the listener's shard. Native v4 operations (one pass per OPEN instead of about 2.5 v3
    calls, all local now) remain a smaller, CPU-only gain;
  - (B) read and write delegations, with the session back channel and recalls from every other mutation path
    (RFC 8881 §10.2–10.4, §20). **B-1 done (A-77): the back channel**, granted, carried, probed, and answered
    by the real Linux client. Next: B-2, read delegations granted on OPEN (durable like A-37 opens),
    `DELEGRETURN`, `CLAIM_DELEGATE_CUR`; then B-3, recalls from every mutation path (NFSv3, FUSE, SDK, merge,
    landing), with the conflicting call delayed and revocation after one lease;
  - (C) directory delegations (§10.9).
  Docker Desktop's transit dominates per call on macOS, so B (fewer round trips) is the larger lever there; A
  removes two thread wakes per v3 call whenever the volume is not on the listener's shard.
- **Owed:** the dynamic slot target (RFC 8881 §2.10.6.1) for long-round-trip clients.

### 2026-10-04: NFSv4 locks and shares across two mounts (fixed); delegation records (B-2 started)

- **Fixed:** opens, share reservations and byte-range locks are keyed by the file's identity, not the handle bytes
  that carry each mount's capability (`docs/bugs/2026-10-04-two-mounts-of-a-file-never-met-in-the-lock-table.md`).
- **B-2 started:** durable delegation records in the partition (`NfsDelegationRecord`, `Op::NfsDelegationSet` and
  `NfsDelegationCleared`, appended; in the snapshot; purged with the client; the db model test's histories carry
  them). Owed: the file-state table's delegations, the grant on OPEN, `DELEGRETURN`, `CLAIM_DELEGATE_CUR`, and B-3's
  recalls. Grants stay off until recalls cover every mutation path.
- **B-2 done (A-78):** read delegations granted on OPEN at the file's owner (back channel up, read-only, deny none,
  settled for one lease, not recently recalled, under the table's bound); `DELEGRETURN`, `CLAIM_DELEGATE_CUR`,
  revocation after a lease, `NFS4ERR_DELEG_REVOKED` and `SEQ4_STATUS_RECALLABLE_STATE_REVOKED`.
- **B-3 done (A-79):** the recall gate in `make_current_inode` covers every change path. NFSv3 calls are held for the
  return, which took the macOS host write from 4,033 ms to 23–46 ms. A callback answered `NFS4ERR_DELAY` is retried
  on the same slot sequence; before that, the back channel was marked down in two of four Linux sessions
  (`docs/bugs/2026-10-04-a-callback-answered-delay-marked-the-back-channel-down.md`).
- **Owed:**
  - FUSE and SDK callers parked rather than refused;
  - `SEQ4_STATUS_CB_PATH_DOWN`;
  - a forwarded NFSv3 call held, not answered `JUKEBOX`;
  - write delegations: **done (A-80)**, with a zero-byte space limit; a space reservation that would let a holder
    cache writes past close is owed, as is `CB_GETATTR` in place of the GETATTR recall;
  - directory delegations (C, §10.9): **no client to serve yet.** Checked 2026-10-05: Linux mainline's client
    requests them (`fs/nfs/nfs4proc.c` `should_request_dir_deleg`, gated on `NFS_CAP_DIR_DELEG`), but Docker
    Desktop's 6.12 kernel has only nfsd's handler (`nfsd4_get_dir_delegation` in `/proc/kallsyms`) and no client
    code. Built with the microVM lane (condition 2), which boots a mainline kernel and so gives C a client to be
    measured against;
  - the dynamic slot target;
  - the one 84 ms `DELEGRETURN` stall, seen once, and the 74–87 ms client-side DELEGRETURN queue under load
    (BENCHMARKS, A-80; the daemon answered no `NFS4ERR_DELAY`).
- **Fixed (2026-10-05):** every fresh daemon announced one NFSv4 server owner and minted the same first client id; a
  Linux client merged a new daemon with a dead one and hung its mount
  (`docs/bugs/2026-10-04-every-daemon-announced-one-nfs-server-owner.md`).


### 2026-10-05: MCP speaks 2026-07-28, dual era (A-81; condition 13, first piece)

- **Done:**
  - **Protocol.** A request naming its version in `_meta` is served in that modern revision (`server/discover`;
    `resultType`; `ttlMs`/`cacheScope` on list and discovery results; the server's identity in result `_meta`). An
    unknown version is refused `-32022` with `{supported, requested}`. Legacy `initialize` clients are still
    served, each in its own legacy revision, or the newest when it names none.
  - **Errors.** An unknown tool is `-32602`, a protocol error, as the spec requires (it was `-32601`). A bad
    argument, a daemon refusal and an unreachable daemon are tool-execution errors (`isError`, SEP-1303), with the
    typed code in `structuredContent.error` (they were JSON-RPC errors the model never saw).
  - **HTTP.** The edge checks `MCP-Protocol-Version`, `Mcp-Method` and `Mcp-Name` (Base64 sentinels decoded)
    against the body: `400` with `-32020` on a missing or mismatched header, `400` on an unsupported version,
    `404` on an unknown method, and `400` on an unparseable body (all were `200`).
- **Proven:** `crates/mcp/tests/mcp.rs`, over a live daemon on stdio and on the HTTP edge (`assert_modern_protocol`,
  `assert_modern_http`), and the spec's own sentinel examples in `http::tests`.
- **Skills over MCP: done (A-82).** Three skills under `skills/`, compiled into the binary and served as
  `skill://slates/<name>/SKILL.md` resources (`text/markdown`, with a URI template), as prompts of the same names,
  and through `slates.help {skill}`:
  - `working-in-slates-volumes`;
  - `merging-work-in-slates`;
  - `landing-slates-work-to-disk`.
  Each is checked as a client reads it against the Agent Skills specification's constraints
  (`assert_skills_over_mcp`).
- **Owed for condition 13, in order:**
  1. `slates skills install` (writing `.agents/skills/` and `.claude/skills/`): **Ada's ruling needed.** It writes a
     user's project directory outside a granted landing, which R1/R10 forbid as written; D-19 lists it. Until then
     the skills reach clients over MCP and as the raw tree in the repository;
  2. codemode: one tool that runs a bounded program over the volume verbs (vorpal's query-language pattern; output
     reduction is the measured win, `research/mcp-skills-sdks.md` §2.2.5);
  3. `slates.fs` listing: **done (A-85, `slates.fs.list`)**; plain-volume write, move and delete over MCP are owed
     (works already edit and declare);
  4. `subscriptions/listen`;
  5. the official conformance suite, run in a container (no host install).

### 2026-10-05: edits and reads past one bulk chunk (fixed); work volumes are uncharged (owed)

- **Fixed:** the typed channel refused any edit or read larger than its 4 KiB bulk chunk
  (`docs/bugs/2026-10-05-edits-and-reads-past-one-bulk-chunk-were-refused.md`). Reads are paged (`ReadRange`, with a
  stamp that refuses `ChangedWhileRead`); large edits are staged on the work's owner and applied as one splice
  (`StageBegin`, `StagePut`, `EditStaged`), the buffers charged, owned and expiring.
- **Owed:**
  - the SDKs' async read and edit page loops (one message today: a large one is refused, typed);
  - ~~work volumes are uncharged~~ **fixed (A-84):** a work is charged for its content and journal against the
    shard budget, before each verb changes it (`docs/bugs/2026-10-05-work-volumes-grew-uncharged.md`). Owed: a work
    still copies its green's whole content at creation (now charged); sharing the green's bytes copy-on-write
    comes with VFS-backed works.
