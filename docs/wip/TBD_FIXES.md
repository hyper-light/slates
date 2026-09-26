# Remaining fixes and verification

Updated: **2026-09-25**. Checkpoint against `3b38d15`, including the instruction-benchmark
repair below. This is the remaining-work
list for the audit/CI repair session; [GAPS.md](GAPS.md) remains the authoritative contract
ledger. Historical audit findings below need closure evidence against current source, not
blind reimplementation of their original baseline. No new test, install or deployment is
authorized merely by appearing here.

## Current CI repair checkpoint (2026-09-22)

- [x] **Run 35791239154: client seats followed the wake tail.** `slots_per_ring` came from one boot
  wake p99, so pods of one image seated 1 to 1285 clients (`too many clients (the bound is 1)` on a
  KIND pod, `the bound is 2` on CI's macOS restart test). Size the ring by §4.7's Little's law; a
  regression fails before and passes after; workspace, fleet and CLI-flow suites pass; every pod of a
  two-CPU fresh-cluster run seats 330. See
  `docs/bugs/2026-09-22-client-ring-sized-by-the-wake-tail-not-littles-law.md`.
- [x] **The wake probe.** One event (a confirmed sleeper on the production placement), its mean
  converged, rounds compared by their medians; spin window, step quantum and timer tick read the mean,
  the inbound ring the p99. Median 8.9–10.8 µs over ten quiet container runs where the old probe's
  split 416 ns / 10,041 ns. See
  `docs/bugs/2026-09-22-wake-probe-mixes-two-events-and-reports-an-unconverged-tail.md`.
- [x] **The runtime's own wakes refine the estimate; long polls attributed (A-31).** Shards and
  clients refine an online mean from confirmed sleeper wakes (Linux shards by voluntary switches,
  clients by the daemon's wake finding a sleeper); quantum, spins, idle windows and slices read it
  live; a long poll is the task's, the host's or unattributed. See
  `docs/bugs/2026-09-25-wake-estimate-frozen-at-boot-and-preemptions-counted-as-long-steps.md`.
- [ ] **`ipc_bench`'s parked row times a race, not a wake.** 2–18 of 2,000 parked trips slept (Apple
  silicon), 1–3 on Linux; the row is ratcheted at 1,233 ns as "the cost the spin window is compared
  against". Hold the bench daemon's reply until the client sleeps and gate the confirmed-sleeper
  mean instead (same record, found 1).
- [x] **A step costs O(ready set).** `take_ready` swapped out every ready slot and `step` re-pushed each
  unpolled one. A drain now takes at most its batch from a FIFO. `observe.rs` ran 95–97 s before and
  12.6 s after. See `docs/bugs/2026-09-26-a-shard-step-cost-its-whole-ready-set.md`.
- [ ] **The archive walk's unit exceeds the quantum.** At least one file piece per slice, larger than
  the mean-wake quantum; size the unit by the slice (BLAKE3 hashes incrementally) (same record,
  found 3).
- [x] **Run 36191789379: a forward refused while the owner's session was out.** The forward now waits
  for the session inside its deadline; a deterministic test fails on the old forward with CI's refusal.
  See `docs/bugs/2026-09-25-a-forward-refused-while-the-owners-session-was-out.md`.
- [x] **Formation left a member outside its council — runs 36191789379 and 36199796152.** Not the
  product: the memory-bound fleet test ran unserialized and its dialers took the copyset test's
  just-released serve ports (the left-out node recorded `fleet.bind` and saw foreign hosts). Serialized;
  the two tests together 8/8 after (1/6 failed before), the full suite 50/50 on Linux. See
  `docs/bugs/2026-09-25-an-unserialized-fleet-test-took-another-tests-ports.md`.
- [x] **Run 36201084174: `slates-ipc --lib` aborted (an owned descriptor closed twice).** A delivery
  test took a descriptor number it had already closed; a parallel test's pipe could hold it by then. The
  test now proves the close by `EPIPE` on the kept write end. See
  `docs/bugs/2026-09-25-a-delivery-test-took-a-descriptor-number-it-had-closed.md`.
- [x] **Run 36201648573: the wake probe reported a zero mean.** Every round's setup outlasted a 6 ms
  share on a loaded macOS runner. The probe now extends its rounds under the sample floor and refuses
  `MeasurementTimeout` short of it. See
  `docs/bugs/2026-09-25-the-wake-probe-reported-a-zero-mean-when-it-measured-nothing.md`.
- [x] **Run 36202635768: a server unit test's fixture was refused `MeasurementTimeout`.** Fixtures measured
  the machine beside each other: 33 refusals in 18 six-copy runs before, 0 in 30 after measuring once per
  process. Fixed shared-object names in the same class now carry the process id. See
  `docs/bugs/2026-09-25-test-fixtures-measured-the-machine-beside-each-other.md`.
- [x] **Run 36202635768: Windows claims and rings used the word wake, `Unsupported` there.** A `Bell`
  per doorbell (a named Event on Windows) and one READY Event per claim slot; the doorbell thread no
  longer spins. Proof owed to the next Windows run. See
  `docs/bugs/2026-09-26-windows-rings-and-claims-used-the-unsupported-word-wake.md`.
- [x] **Run 36260818113: Windows shards were never kicked.** The runtime registered `Kick::None` on
  Windows, so a parked shard ran a spawn or a shutdown only when a timer or an I/O completion woke it.
  The UDP tests' new pulse report showed it (steps and waits frozen). The registry now owns the
  completion port with a generational `KickPort`. Proof owed to the next Windows run. See
  `docs/bugs/2026-09-26-windows-shards-were-never-kicked.md`.
- [x] **Run 36261369758: the differential compared one of several refusals; folding renames lost the
  host's spelling.** The suite now allows any refusal POSIX names when more than one applies (computed
  from the pre-state). A folding volume respells a case-only rename and keeps a replaced entry's
  spelling, as APFS does. APFS differential 10/10 × 1,000 after. See
  `docs/bugs/2026-09-26-folding-renames-lost-the-hosts-spelling.md`.
- [x] **The first macOS root pjdfstest review.** 1,905 cases listed by cause; 0 unlisted against CI run
  36202635768. See `docs/bugs/2026-09-26-macos-pjdfstest-root-review.md`.
- [ ] **macOS workloads: cargo's `._target` AppleDouble sidecar** (NFSv3 has no xattrs; the client
  stores cargo's backup-exclusion xattr as a visible file). Open, as GAPS records.
- [ ] **macOS hermeticity: 92 of 93 write-capable calls unresolved** (fs_usage cannot attribute
  descriptors; the DTrace attribution is authorized and owed). Open, as GAPS records.
- [x] **The anchor segment on Windows: `CreateFileMappingW` 1450.** 167–188 GB derived, which a
  pagefile section is charged in full at creation. The segment and content object are now a
  `SparseObject`: `SEC_RESERVE` on Windows, committed as ranges are reached, reached only by ranges.
  Proof owed to the next Windows run. See
  `docs/bugs/2026-09-26-windows-committed-the-whole-anchor-segment.md`.
- [ ] **For Ada: the op-log ring is bounded by recovery time, not by memory.** 16.4 GB per partition on
  a 128 GB Mac, 4 GB in a 1 GiB KIND pod; `trim` frees nothing, so a wrapping ring touches all of it
  (same record).
- [ ] **KIND scale-down formation chain** (run 36262457777): after 5 → 3, `slates-0` and `slates-2`
  each saw only `slates-1` for 180 s, with NXDOMAIN for fresh pod names. This is the audit's open
  "formation remains a chain" item; it passed the two runs before.
- [ ] **Serve ports are still bound-then-released.** A test's own dialers could take its next node's port;
  none traced yet. Allocate below the kernel's ephemeral range (same record, found 1).
- [ ] **A first placement that never converged** kept `poll_until` waiting past twelve minutes while the
  test polled `AwaitPlaced` about a thousand times a second; probably the same port collision, not
  confirmed (the forward record's found 3).
- [x] **The location round skips a peer whose session is out** (same record, found 1). The round now
  asks such a peer once its session returns, inside one deadline. A deterministic test failed before
  (`HomedElsewhere`); fleet 50/50 after.
- [ ] **The pressure hold.** Replace the boot-baseline shortfall with the design's hold above
  `committed` against the memory this daemon can actually be given (evidence in
  `docs/bugs/2026-09-22-client-ring-sized-by-the-wake-tail-not-littles-law.md`, sibling 4).
- [x] **Run 35615113353: KIND takeover never assigned.** The council seated its voters by
  lowest id, so the owner's replacement took the live leader's seat beside its unretired
  predecessor, then led without ever retiring it; no takeover was assigned. Seats now follow
  incumbency and liveness, the id only a tiebreak. Pinned local cluster: 1 failure in 8 before,
  6 of 6 after; two council regressions; cluster and fleet suites pass. Owed: the replacement's
  blind spot after two overlapping faults; root representatives by id order. See
  `docs/bugs/2026-09-22-council-seats-follow-id-order-not-liveness.md`.
- [x] **Run 35615970514: mounted ownership.** A MOUNT under uid 0 caused later uid 1001
  creations to retain owner 0. Preserve the catalog principal as attachment authority and
  stamp each request's Unix ownership separately. Native mounted red reproduced; 9 NFS
  daemon, 21 bridge-core and 42 NFS procedure tests pass, including dot-entry RMDIR refusals.
- [x] **Run 35615970514: restart fixture and Helm.** The four-slot restart history
  acknowledged create sequence 1 through watermark 4, then incorrectly required replay.
  The revised oracle requires both acknowledged refusal and replay of a discarded, unacknowledged reply;
  all 5 client lifecycle tests pass. Pin workspace Helm to KIND's 4.3.0, retaining exact
  golden bytes; all 4 chart tests pass. Recheck both host workspaces after final edits.
- [x] **Current targeted Linux validation.** The isolated NFS/restart/comparator corrections
  pass 125 cases under io_uring plus `xtask check`; strict workspace Clippy passes on macOS.
  Script/log: `/private/tmp/slates-ci-35615970514-linux-targeted.{sh,log}`. The full macOS
  run was cancelled at the requested hold, with no recorded failure; a complete rerun is owed.
- [x] **Full Linux rerun of the repair batch.** `9c82f19`'s code passes 1,564 workspace
  functions (zero failures, 14 ignored), all 49 fleet histories in 269.51 s, `xtask check`,
  and the full mounted NFS conformance sequence. All nine workloads are Identical;
  pjdfstest has zero unexpected failures; hermeticity has zero unresolved or outside writes.
  Script/log: `/private/tmp/slates-ci-35615970514-linux{.sh,-validation.log}`.
  This adapter does not cover native FUSE. The workspace's opt-in steps still need their
  dedicated runs; in particular Helm was absent and returned through its environment gate.
- [x] **Linux Helm.** Checksum-verified Helm 4.3.0 passes all four chart tests in 0.05 s
  inside the disposable Linux container. The deliberate golden writer remains ignored.
  Script/log: `/private/tmp/slates-ci-35615970514-linux-helm.{sh,log}`.
- [x] **Dedicated Linux FUSE and CLI rerun.** On `628962b` plus the tracer changes, both
  ordinary-user FUSE mount regressions pass (0.39 s and 0.01 s). The real CLI command
  returns 10 passed in 8.17 s; macOS-only branches explicitly skip, while the provisioned
  recovery-key and three-process fleet histories execute. Six tracer lifecycle cases,
  50 conformance cases and `xtask check` also pass under the io_uring-required container.
  Script/log: `/private/tmp/slates-ci-35615970514-native-and-tracer-linux.{sh,log}`.
- [ ] **Native tracer verification.** The authorized Terminal reproduction reached ENOSPC
  at `d/f2`, followed by `ktrace_start: No such process` and a zero-byte trace. Startup now
  requires a parsed event; owned cleanup covers errors and successful draining follows daemon teardown.
  The corrected native run captures 188 rows / 47,000 bytes, with empty stderr and no
  surviving task process. Remaining: sound attribution for shared-memory descriptor
  writes and the socket opened before tracing. Task-scoped DTrace is explicitly authorized;
  the prepared probe awaits Terminal sudo authentication. Descriptor attribution must also
  handle replacement: Apple's filesys/network filter suppresses dup/dup2 rows, and the
  parser currently ignores close_nocancel. A late descriptor snapshot cannot alone establish
  which object an earlier write reached. See the dated fs_usage ownership report.
- [x] **Hermeticity workload admission and complete landing oracle.** Seven entries plus
  the root exhaust the former eight-inode fixture; three entries are kernel-created
  AppleDouble files. Four content bytes and no snapshots rule out a retained-byte leak.
  Eight queried host pages admit the unchanged native workload and snapshot (131,072
  bytes admitted, 98,312 referenced, 12 surviving entries). Every entry is now compared
  by name/kind/mode/content with its landed counterpart; the core workload checks remain.
  Linux passes 24 harness cases, structural checks and the full NFS/strace lifecycle:
  220 writes, six paths matched, zero unresolved/outside. Strict Clippy passes on macOS;
  the current Linux image lacks Clippy. See the dated client-metadata fixture report.
- [ ] **Broader CI audit.** Follow `docs/bugs/2026-09-22-ci-failure-pattern-audit.md`:
  async acknowledgment/pending-reply bounds, fs_usage readiness and ownership, pjdfstest
  child errors, macOS NFS capability cases, and initial KIND formation A↔B↔C without A↔C.
  The comparator's blanket `._*` filter has been removed; native provenance sidecars still
  break real git/Python/rsync/editor histories. Do not add exclusions to conceal them.
  The 117 additional macOS pjdfstest failures are grouped with evidence and the next
  checks in `docs/bugs/2026-09-22-macos-nfs-conformance-boundaries.md`.
- [x] **Trace parser hostile row.** A four-token fs_usage row with a wait suffix panicked
  before later violations could be judged. The regression fails in 0.00 s; checked slice
  access rejects the incomplete row and preserves the following outside-write violation.
  All 49 conformance cases and strict crate Clippy pass after the correction.
- [x] **Run 35604717581: allocation and paging fixtures.** Linux reproduces local channel
  allocation in the memory test (trial 31; diagnostic trial 95) and its runtime sibling
  (trial 61). Replace the measured channel waits with bounded stack-owned synchronization,
  keeping exactly zero local allocations and requiring the foreign allocation. The memory
  binary passes 100 Linux repetitions. The status fixture now preserves the report credit
  while forcing small pages; CI's eight-slot allowance is explicit and passes natively in
  0.75 s. Both allocation binaries and all 14 Linux daemon tests pass under io_uring. Full
  workspace rerun passes 1,561 cases and `xtask check`; final strict checks and the later
  NFS/replay changes still need validation in that container. See the dated fixture diagnosis.
- [ ] **Run 35604717581: macOS conformance.** The downloaded artifacts contain 1,906
  pjdfstest failures and a separate empty hermeticity trace (`ktrace_start: No such process`).
  Review every failed history against the pinned suite and the NFS client; no assertions or
  expected-failure entries have been changed. Task-scoped macOS tracing is authorized, but
  this host requires interactive sudo authentication. Prepared local command:
  `bash /private/tmp/slates-ci-35604717581-macos-trace.sh`. Windows work is paused.
- [x] **Explicit shard counts.** The Linux Python 3.14 SDK
  test fails because `--shards 1` keeps capacities already divided among three shards.
  Serial/concurrent and fresh/after-destroy probes all admit five of the unchanged eight
  8 MiB requests; destroy returns both committed counters to zero. Select the count before
  deriving every capacity, replacing the late override. The two real-daemon regressions
  fail before the fix and pass in 1.42 s afterward; the second also catches overcommit when
  increasing the count. The unchanged Linux SDK suite now passes: five installed-wheel
  Python cases (0.384 s), five direct Node cases and three installed npm cases (0.192 s),
  zero skips. Wheel and sdist pass strict Twine checks. The Linux sweep passes **1,557
  workspace cases**, zero failures, 14 existing exclusions, including all **49 fleet
  histories in 271.81 s**. Strict Clippy, xtask, both non-root FUSE regressions and the
  real CLI step also pass (10 functions, 8.36 s; platform-specific skips remain, while
  the portable recovery-key history now executes). Command/log:
  `/private/tmp/slates-run-linux-shard-selection.sh`, `/private/tmp/slates-linux-shard-selection.log`.
  macOS passes **1,548 workspace cases**, zero failures, 14 existing exclusions, including
  **48 fleet histories in 321.69 s**; strict lint/structural checks, the real CLI binding,
  restart and strict-detach flow (10 functions, 28.19 s), and all 13 installed SDK cases pass.
  The portable operator-key CLI history skips on macOS without its RAM directory; its Linux
  execution is recorded above. Commands/log: `/private/tmp/slates-run-macos-shard-selection.sh`,
  `/private/tmp/slates-macos-shard-selection.log`.
  No test quota, concurrency, container memory or timeout changed.
  Commands/log: `/private/tmp/slates-run-linux-sdk.sh`, `/private/tmp/slates-linux-sdk-ci-green.log`.
  See the dated shard-count diagnosis.
- [x] Linux ARM64 instruction-count command completes **14/14** at `46ca485`, using the
  approved task-container profile that allows exactly `personality(PER_LINUX |
  ADDR_NO_RANDOMIZE)` in addition to the earlier io_uring calls. Runner **0.16.1**;
  command `cargo bench --offline --workspace --bench callgrind`; log:
  `/private/tmp/slates-linux-callgrind-approved.log`. This first run had no saved baseline;
  it establishes execution, not a regression comparison or x86-64 instruction equivalence.
- [x] **Instruction-count harness quality.** Memory/runtime/wire benchmarks now require
  successful operations and observable completion; setup refuses instead of substituting
  fixtures or spinning. All four negative controls fail and all 14 valid benches pass in
  the Linux ARM64 container. Explicit, non-inlined collection requests fix two measured
  boundary defects: teardown being counted and a small encode reporting zero instructions.
  Eightfold CRC verification leaves both profile totals at 2,653; doubling the encode
  increases 34 instructions to 58. Strict instrumented Clippy and xtask checks pass on
  isolated `2179cc2` plus the repair. The libclang install and new dependency cache remain
  container-only; ordinary tests and Miri do not enable the benchmark feature. Evidence:
  [the diagnosis](../bugs/2026-09-20-callgrind-reports-refused-work-as-success.md).
- [ ] **Instruction regression gate.** The workflow needs a reproducible comparison baseline and
  enforced regression policy; printing instruction counts alone does not enforce D-20.
  The ARM64 validation above does not establish GitHub x86-64 instruction equivalence or
  integration with the separate snapshot-coverage task's concurrent edits.
- [x] Replace the IPC ring fixture's scheduler assumptions with queued-reply and armed-wait
  histories. Linux single-CPU stress: 192/192 test executions passed. Performance gates remain.
- [x] Page the daemon report under the client's existing reply credit; retain one charged,
  immutable capture per channel and validate every continuation. The original one-core fleet
  regression passes in 1.58 s; real-ring fragmentation, mutation isolation, cancellation,
  cross-client refusal, hostile pages and retention-release regressions pass.
- [x] The earlier status-paging/ring-scheduling change passed Linux workspace **1,547/1,547**
  and macOS **1,538/1,538**,
  strict Clippy and xtask checks. Fleet: Linux 49/49, macOS 48/48. Those workspace
  runs predate the later landing and lifecycle changes; the current reruns and additional
  workflow steps below are separate obligations.
- [x] Linux rerun after the landing/lifecycle changes: **1,554 passed, zero failed, 14
  ignored**, all **49 fleet tests** in 255.76 s; strict Clippy and xtask checks passed.
  Real non-root FUSE coherence/retry and local FIFO/socket isolation both passed afterward
  (0.13 s and 0.01 s). Command wrapper and log:
  `/private/tmp/slates-run-linux-lifecycle.sh`, `/private/tmp/slates-linux-lifecycle.log`.
  The subsequent Linux delta run passes all five client cases, including identity exhaustion
  (1.51 s), and all five real landing cases (0.05 s): `/private/tmp/slates-linux-delta.log`.
- [x] Current macOS rerun: **1,546 passed, zero failed, 14 ignored**, all **48 fleet
  tests** in 298.31 s; the five client histories include identity exhaustion. The real
  CLI suite passes **10/10 in 28.85 s**, including Docker, daemon restart and exact detach
  results. Commands: `/private/tmp/slates-run-macos-current.sh`; log:
  `/private/tmp/slates-macos-current.log`. This does not make its subsequent ratchet green.
- [x] macOS SDK packaging at `46ca485`: wheel and sdist pass `twine check --strict`;
  installed-wheel Python tests **5/5 (2.297 s)**, direct Node addon **5/5 (1.116 s)**,
  installed npm packages **3/3 (0.974 s)**, zero skips. Ada requested the existing Python
  **3.14.3** instead of CI's 3.11; Twine **7.0.0** was installed only in the task venv.
  Node **24.14.1**, npm **11.11.0**, napi **3.7.2**, maturin **1.14.1**. Commands/log:
  `/private/tmp/slates-macos-sdk-ci.sh`, `/private/tmp/slates-macos-sdk-ci.log`.
  This records 3.14 validation, not a claim to have run the 3.11 interpreter.
- [x] Linux concurrency commands pass at `46ca485`: all five loom models (6 / 26 / 157 /
  3,865 / 27 explored interleavings) and both shuttle histories (200 schedules each; merge
  0.26 s, clones 0.09 s). Commands: `/private/tmp/slates-run-linux-delta.sh`; log:
  `/private/tmp/slates-linux-delta.log`. Instruction counts remain a separate job.
- [x] Miri at `46ca485`, interpreting the Linux x86-64 target from macOS: **82 passed,
  11 existing ignored cases**. Memory (33) and wire (30) retain leak checks; runtime units
  (17) and differential histories (2) use CI's existing intentional-leak exclusion.
  Commands: `cargo +nightly miri test --offline --target x86_64-unknown-linux-gnu
  -p slates-mem -p slates-wire --lib`, then `MIRIFLAGS=-Zmiri-ignore-leaks cargo +nightly
  miri test --offline --target x86_64-unknown-linux-gnu -p slates-rt --lib --test differential`.
  Logs: `/private/tmp/slates-miri-linux-target.log`, `/private/tmp/slates-miri-rt-linux-target.log`.
  Miri's printed durations are simulated time. This is not a native Linux kernel check.
- [x] Supply the Linux CLI step's missing RAM directory so its portable recovery-key
  history executes. The exact real-CLI history passes in 0.42 s with absent/wrong/oversized
  key refusals and successful/idempotent approval. The earlier Linux CLI total includes
  this skip; its passing function count was not evidence for the history. See the dated
  CLI CI coverage report and `/private/tmp/slates-linux-cli-recovery-key.log`.
- [ ] Finish every CI-equivalent local job, including million-operation/differential tests,
  CLI and Swift steps, performance gates, conformance, SDK packaging, concurrency and KIND.
  Reproduce Windows natively in the VM Ada authorized creating on 2026-09-20.
- [x] The unchanged root pjdfstest rerun passes its first source-reviewed expected list:
  6,970 passed, 1,800 expected failures, 28 TODO; zero unexpected, now-passing or absent
  listed cases (174.353 s). fsx, fsstress, all nine workloads and hermeticity also pass.
  No file wildcard or upstream assertion changed. The adapter remains LIMITED, native FUSE owed.
- [x] Repair the host differential oracle's absolute-path escape from RAM scratch; its
  deterministic root/nested alias regression and 2,000 generated histories pass in 0.29 s.
  The million-operation model passed. The next real-host gate exposed an absolute digest
  reuse-count assumption; measure the final read's counter delta and verify its bytes.
- [ ] Sibling limits: page the other growing replies (`List`, grants, audit/content reads),
  and complete a status scatter whose return task is lost. Expiring retained status bytes
  alone does not complete that gather. See the status paging bug record.
- [x] Fix landing exchange verification across its own ctime update and classify Linux
  symlink containment refusals. Eighteen oracle cases and all five real Linux landing
  tests pass; the crash-instruction oracle and same-timestamp outsider preservation pass.
- [x] Complete the Linux CLI flow after repairing its GNU-incompatible mktemp template:
  9/9 in 7.65 s. The million-file landing bench completes; the ratchet skips explicitly
  because this machine has no recorded baseline. The latest complete workspace evidence
  predates these additional landing changes; repeat affected checks before committing.
- [x] The macOS OCI/CLI and Swift/FSKit steps pass after replacing the obsolete bare-export
  matcher with the actual capability source format: eight pure checks and ten CLI tests,
  including malformed sources, exact volume matching and bearer-token redaction. The later
  crash/retirement changes require their current suite reruns; performance remains open below.
- [x] **OCI ownership (2026-09-20).** Preserve the real CLI caller and exact detach outcome.
  Bindings borrow a checked source-mount attachment, with atomic dependent removal.
  The new regression covers client exit, daemon crash, fresh post-crash CLI calls, explicit
  detach, parent unmount and read-only source refusal (7.90 s). Database recovery/refusal
  and cross-platform regression runs continue below; no `NotFound` cleanup exception remains.
- [ ] **Cross-shard attachment retirement (2026-09-20), implemented, validating.** The real OCI probe sees one
  exited CLI's attachment reaped, while another survives on its volume's owner shard.
  `reap_client` visits only its own partition, and `Consumer::Sdk` lacks origin-host scope.
  The reaper now reserves dead seats, cancels unsent forwards, and gathers then removes
  exact attachment ids across owners; refusals retain the seat for retry. The two-owner
  SIGKILL regression passes in 3.81 s under its original lease limits and in the Linux
  workspace rerun. Owed: queued-forward and refusal/cancellation histories. Cross-node attach is currently refused;
  enabling it needs origin-scoped lifetime ownership, not a claimed proof on a nonexistent path.
- [ ] **Restart admission identity (2026-09-20), implemented, validating.** The CLI crash
  history exposed fresh client 13/sequence 1 receiving an old `Created` for `Status`.
  A control-partition reservation and recovered allocation floor prevent reuse. The portable
  restart case passed on macOS and Linux. The last-id/refused-fresh/restarted-session history
  passed on macOS in 0.98 s and in the five-case Linux client suite (1.51 s).
  Failed-publication coverage remains owed.
- [ ] **macOS performance ratchet (2026-09-20).** Snapshot cost grew 24 ns against a
  17 ns measured allowance in `/private/tmp/slates-macos-gates-lifecycle.log`. A controlled
  same-tree experiment found journal turnover, not tree size: the million-file tree measured
  53 ns during named-record eviction and 28 ns afterward. Normalize only the journal state
  before sampling, retain the original allowance and reject failed operations. The corrected
  snapshot comparison passed all three VFS runs in the current full ratchet.
  Separate destroy overruns remain (296,417 ns / 146,250 ns, 368 units, no allocator free
  time). Include the eager `Volume::destroy` preparation walk in the bounded-work proof;
  the old benchmark starts its slice timer after that walk. Do not widen the gate.
- [ ] **Nine remaining macOS ratchet failures (2026-09-20).** On the recorded Apple M5 Max
  machine, the latest three-run ratchet still rejects small-file copy-up, database mutation,
  simulated landing, buddy allocation, both namespace rows, burst create and both write rows.
  Small-file copy-up medians are 96,625 / 97,042 / 104,958 ns against a 30,667 ns ceiling.
  Its unpinned source directory has grown from the baseline's dozen entries to 69; determine
  that contribution independently of production cost before changing the benchmark. The other
  eight failures also need controlled comparisons. No ceilings were changed. Log:
  `/private/tmp/slates-macos-current.log`.
- [ ] **Copy-up benchmark diagnosis (2026-09-20).** An isolated `9344472` build, pointed
  at exactly the current real input, also measures 94,667 / 98,584 / 96,583 ns; current
  medians 99,709 / 100,500 / 98,000 ns have overlapping intervals. A separate probe confirms
  sixteen successful samples retain 32 extra inodes and 16 directories; no refusal occurred
  in that run. Fix the input definition, sample reclamation and ignored operation errors
  before relying on this comparison. Keep `OsHost` and the original ceiling. The controlled
  allocator comparison reaches 31–35 ns on both old/current code, so the earlier allocator
  red is not by itself evidence of a source regression. See the dated copy-up benchmark report.
- [ ] **Lifecycle siblings (2026-09-20).** Plain `attach` without an OCI or host-mount
  lifetime still creates a ring-owned root record from an exiting CLI. Client-id release
  can be lost at control-channel saturation (`RELEASE_LOST`); safe cleanup must acknowledge
  release without allowing a late retry to free a resumed client's identity. Green OCI/guest
  requests formerly bypassed form establishment and now refuse instead of reporting a record
  as a bound path. Source-mounted green support requires its own verified version semantics.


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

- [ ] **Full Linux conformance run (2026-09-19).** Job `105974090627` reaches every suite:
  fsx and workloads pass; fsstress loses the daemon to repeated heartbeat kills; pjdfstest
  has 3,595 failures requiring a capability/correctness review; hermeticity finds four writes
  outside the target and a zero-write landing its harness did not reject. Reproduce each
  through the real Linux NFS adapter with io_uring available. Also cover strace's actual
  deleted-descriptor annotation. The mounted hermeticity leg now passes with six entries
  verified and traced, zero outside/unresolved/unmatched calls; fsstress reproduces locally
  at 500 × four processes while its 50-operation prefix passes. The completed fix yields
  between RPCs and copies recovery byte vectors in bulk; the full history now passes twice
  (all 2,000 logged operations). Fsx and all nine workloads pass locally. Pjdfstest exactly
  reproduces CI's 3,595 failures; its special-file gaps and two inherent NFS limits are in
  `docs/bugs/2026-09-19-pjdfstest-special-files-and-nfs-limits.md`. No list was expanded.
  Record:
  `docs/bugs/2026-09-19-linux-conformance-first-complete-run.md`.
- [ ] **Landing advancement and snapshot authority (2026-09-19).** A saturated inode
  reservation can refuse retention during `land_advance` after disk writes and syncs, returning
  `NoSpace` instead of a truthful completed/partial report. Reserve advancement capacity before
  writing and prove unchanged state on refusal. Also test that a requested snapshot, rather than
  a subsequently changed head, supplies the manifest and bytes; preserve the target host's
  descriptors after advancing a scratch volume to an overlay. These are separate from the
  contained-writes and missing-directory regressions fixed in the current change.
- [x] **Runtime test isolation (2026-09-19).** The registry contention fixture sent wakes to
  unrelated unit-test rings, and a driver test assumed its timed wait could receive no other
  kicks. The stress history now owns its integration-test process; timed waits retain one
  absolute deadline across valid wakes, with registry and worker cleanup on assertion failure.
  Record: `docs/bugs/2026-09-19-driver-test-assumes-no-foreign-kicks.md`.
- [x] **io_uring retirement verification (2026-09-19).** A local io_uring regression
  reproduces the warm-restart bind failure. Cancellation plus a drain barrier is implemented;
  twenty rounds of both the five-test reclamation suite and the original warm-voter
  history pass with real io_uring. The epoll leg passes five tests too. The full Linux
  workspace passes 1,506 tests; strict Clippy and `xtask check` pass. CI requires its
  intended backend. Record: `docs/bugs/2026-09-19-io-uring-retains-listener-after-shutdown.md`.
- [x] **Allocation counter attribution (2026-09-19).** `rt` and `mem` test counters included
  libtest's other threads. A controlled foreign allocation fails both counters before the
  change; thread-local counters pass without relaxing either allocation gate. Record:
  `docs/bugs/2026-09-19-timer-allocation-counter-includes-libtest.md`.
- [ ] **Readiness cancellation during a live runtime (source review, 2026-09-19).**
  `Ready` has no per-registration cancellation and accepts a second poll as readiness even
  after a spurious wake. Driver retirement now ends all pending requests; separately test
  abandoned waits while the driver stays alive, including descriptor reuse and multiple
  waits from one task. This broader lifetime issue is not established by shutdown tests.

- [x] **Rendezvous wake amplification (2026-09-19).** Linux now drains and rearms the
  listener through the control shard's one-shot driver readiness. There is no Linux watcher
  thread. Holding both real shards and queuing a connection reproduces the old unrelated
  eventfd kick locally; the new path passes and accepts subsequent clients (0.22 s).
- [x] **Lease timer reservation at boot (2026-09-19).** The bounded timer slab reserves its
  exact capacity in one backing allocation. The counting-allocator regression reduces total
  construction calls from 25,271 to four at CI's 1,617,130 slots, with no allocations while
  filling, renewing, cancelling stale handles, refusing overflow or expiring timers.
- [x] **Unix kick descriptor close/borrow race (2026-09-19).** A generational kick resolves an
  immutable owned descriptor under the registry's reader pin. Retirement removes lookup,
  waits for borrowers, frees the entry and only then frees the slot. The forced-overlap
  regression fails before and passes after. Unsafe Send/Sync and the mutable descriptor cell
  are gone. Simulated kicks also carry generations. Evidence for these three repairs:
  `docs/bugs/2026-09-19-startup-wakes-and-kick-retirement.md`.
- [x] **FUSE mounted fixture ownership (2026-09-19).** The volume root now belongs to the
  mounting uid/gid, matching real provisioning. A saved pre-fix binary fails locally as uid
  65534 with CI's `Permission denied`; the fixed real mount passes in 0.13 s. A cfg-free
  bridge-attribute regression catches the mistake even under root. Record:
  `docs/bugs/2026-09-19-fuse-coherence-fixture-root-owner.md`.
- [ ] **Windows completion-port kick ownership (sibling review, 2026-09-19).** Unix and
  simulation kicks now retire safely; `Kick::Iocp` still carries a raw port address whose
  driver closes it. Also verify Windows registration: `register_kick(None)` publishes
  `Kick::None` rather than the driver's port. Add a real Windows wake/shutdown/reuse regression
  before changing port ownership. This finding has not been reproduced on Windows.

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
- [x] **Mounted snapshot barriers (GAP-A9-4)** — **DONE 2026-09-21.** One shard-owned attachment
  registry every transport rides (NFS mounts through `Export::over`, guest devices through `admit`
  over the owner's registry); `snapshot` runs the barrier over it before freezing the root and reports
  its coverage on the wire (`Snapshotted.coverage`: `Complete` / `ServerVisible`, attachments closed;
  `BarrierIncomplete` typed); the FUSE writeback flush has no work left with writeback cache refused;
  a host mount binds its mount point (`BindMount`, `status` lists bound mounts). Evidence:
  `crates/server/tests/nfs_mount.rs` (coverage complete/0 → server-visible/1 → complete/0 across the
  mount's life; the bound mount listed, another principal and an SDK attachment refused, the detach
  unlisting it), `crates/bridge-virtiofs/tests` over the shared registry, the live CLI mount flow.
  Design status under §4.6 "Writeback and snapshot barrier"; GAPS GAP-A9-4.
- [ ] **Mounted Work/Green (GAP-A9-14):** VFS-backed work journals and green volumes, read-only
  enforcement, version-pinned attachments and the extent-backed retained chain; prove the mounted
  merge workflow, not just the service-level protocol.
- [ ] **Overlay/clone recovery (GAP-A9-2/-6):** independent overlay-clone host ownership, client
  open-handle handoff, complete immutable capture/remote clone coverage and changed-source refusal.
  Retained overlay witnesses/images are already implemented; do not list them as missing again.
- [ ] **Placement and repair (GAP-A9-8):** close verified reference-graph placement, real holder
  capacity, repair/healing and byte-complete cross-region/mirror service with time-based lag evidence.
- [ ] **Memory/QoS (GAP-A9-1/-11):** ~~pressure-driven admission stop~~ **DONE 2026-09-21** (the hold
  on each shard's byte budget, sampled from `memory_available_now` at the liveness cadence and fanned
  as the host shortfall's per-shard share; a raised hold refuses a new create `BudgetExceeded` while an
  admitted volume's within-entitlement writes land — `crates/server/tests/nfs_mount.rs`, the pure
  budget unit; admission.md §5.5). Still owed: Windows job-object bounds, guest/open-reference charges
  and bounded end-to-end large/unknown-length transfers, with cancellation and resource release across
  shard/device/transport limits.
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
- [x] Rerun full mounted hermeticity after the 2026-09-19 tracer repair. The shared boot clock
  alone did not fix startup: ordinary strace stopped even unselected allocator syscalls.
  `--seccomp-bpf` retains the filesystem trace selection and permits first-heartbeat startup;
  explicit `--kill-on-exit` ties tracee lifetime to the harness. The live Linux startup and
  write-observation regression passes (1.37 s). The real Linux NFS-mounted lifecycle now
  lands and verifies six entries, with all six matched in the trace and zero outside, unresolved
  or unmatched writes (`2026-09-19-linux-conformance-first-complete-run.md`).
  Anchor format 3 intentionally rejects incompatible format-2 retained state.
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
limits. The first unchanged rerun improved from 5,175 passes / 3,595 failures to **6,970 passes /
1,800 failures** in 172,343 ms. The subsequent source review and exact expected-failure rerun
are recorded at the top of this ledger; no upstream assertion changed. Windows cross-compilation
stopped at missing target C headers in zstd. The authorized Windows VM has been created;
its official ARM64 installation image is fully downloaded (7,994,415,104 bytes), with SHA-256
`638aa2c88e94385b00f4f178d071e3df0b7d9e335577a83bd533b7f2eb65adf0` verified against Microsoft.
Ada approved the Windows 11 Pro license; installation completed and the guest reached its
first-start setup. It is waiting for the network driver. Official UTM media is downloaded
and attached (`/private/tmp/slates-utm-guest-tools.iso`, SHA-256
`65b6a69b392ee01dd314c10f3dad9ebbf9c4160be43f5f0dd6bb715944d9095b`); its Windows ARM64
NetKVM driver is version `100.100.104.27100`. Automatic approval review blocked the
"Install driver" click under AGENTS §2.14; specific task-VM driver authorization is pending.
No driver was installed. Native Windows tests have not run. An ARM64 guest can exercise x64
user-mode binaries through Windows emulation, but
does not establish equivalence to GitHub's x64 kernel.
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
