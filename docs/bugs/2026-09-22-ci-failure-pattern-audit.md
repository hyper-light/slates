# Audit of recurring CI failure patterns

Date: 2026-09-22. Baseline: `3b38d15` and linked runs 35604717581 / 35615970514.
Scope: reproduce the linked jobs locally, repair their causes, and inspect sibling
paths. Windows work is paused at Ada's request. An open row is work still owed;
this report does not claim all lanes pass.

## Findings and current evidence

| Boundary | Finding | Evidence / state |
|---|---|---|
| Allocator measurement | Both memory and runtime fixtures allocate their own channel-wait state inside the measured interval. | Independent Linux reds, bounded stack synchronization correction; memory 100 repetitions pass. Dated fixture report. |
| Status pagination | Small pages also shrank retained-report credit below the report size. | Exact 1024-byte refusal reproduced; preserve credit and force small pages independently. 14 Linux daemon tests pass. |
| Mounted ownership | The shared registry retains MOUNT's Unix uid and stamps later users' objects with it. | Both linked conformance lanes; native mount red; wire uid 1001 creates uid 0. Corrected with per-request ownership separate from enrolled authority; 9 daemon NFS tests pass. |
| Creation siblings | CREATE, MKDIR, SYMLINK and MKNOD share the owner stamp. | Add all-kind ownership regression; shared bridge suite and 42 NFS procedure tests pass. |
| Reserved directory names | RMDIR `..` answers NOENT; Apple interprets that as success. | pjdfstest rmdir/12.t:4 and wire red. Explicit dot-component refusals pass; mounted confirmation owed. |
| Restart completion retention | The restart fixture retries an already acknowledged create when measured rings are small. | Four-slot red: create sequence 1, acknowledgment 4; eight slots pass. Corrected test requires acknowledged refusal plus replay of a genuinely unconsumed reply; 5 client lifecycle tests pass. |
| Asynchronous acknowledgments | The watermark appears to follow sent requests rather than replies consumed by the caller. | Code audit of `Client::ack_watermark`; regression and correction pending. Also audit pending-reply eviction and sequence wrap. |
| Golden tool input | Workspace and KIND use different Helm renderers for a byte-for-byte golden. | CI render differs in five separator blank lines; KIND's pinned 4.3.0 passes. Pin workspace to the same tool; no golden or assertion changes. |
| Tracer startup/lifetime | fs_usage is spawned without an observed-readiness barrier and has no drop guard. | Native privileged red: ENOSPC, zero trace bytes, then `ktrace_start: No such process`. Readiness and ownership corrected, preserving tracing through daemon teardown; six real-process regressions pass. Mounted rerun remains owed. |
| Trace parser bounds | A malformed fs_usage wait suffix creates a backwards slice. | Regression panics in 0.00 s. Checked slice access preserves the following valid violation; all 49 conformance cases and strict crate Clippy pass. |
| Suite subprocess errors | pjdfstest runner discards stderr, read errors, and exit status. | Audit of `xtask/src/conformance/suites.rs::run_test_file`. Structured failure capture and negative controls owed. |
| Workload equivalence | The comparator filtered every `._*` name, including ordinary user files. | Added-name and changed-content histories fail before removing the filter and pass afterward. All 48 conformance cases pass on macOS and Linux. Native AppleDouble behavior remains unresolved; no new exclusions. |
| macOS NFS capabilities | `_PC_PATH_MAX` is unsupported by Apple's client; FIFO access and several privilege cases differ from Linux. | The 117 additional failure IDs are classified in `2026-09-22-macos-nfs-conformance-boundaries.md`, with standard/client evidence and the mounted checks still owed. No expected-failure list copied or expanded. |
| KIND membership | Formation remains a chain A↔B↔C after 180 s; endpoints each probe one peer. | Job 106386649727 diagnostics. Local isolated-cluster reproduction and session-formation diagnosis owed. |

## Validation completed and its limits

An isolated archive of `3b38d15` plus the allocation/paging corrections passes
**1,561 Linux workspace tests**, with zero failures and 14 existing exclusions,
under the required io_uring driver. `cargo xtask check` passes. Command script:
`/private/tmp/slates-ci-35604717581-linux.sh`; log:
`/private/tmp/slates-ci-35604717581-linux-validation.log`.
This run predates the new NFS/replay corrections and does not establish mounted
conformance, Helm availability, or KIND success. Reruns must include those inputs
explicitly; a skipped gate is not proof.

The subsequent isolated source includes the NFS, restart, Helm and comparator
corrections. Strict workspace Clippy passes on macOS. The targeted Linux io_uring
run passes **125 cases**: 21 shared bridge, 42 NFS procedure, 9 NFS daemon, 5 client,
and 48 conformance cases, followed by `cargo xtask check`. Command script:
`/private/tmp/slates-ci-35615970514-linux-targeted.sh`; log:
`/private/tmp/slates-ci-35615970514-linux-targeted.log`.
The full macOS workspace run was interrupted at Ada's hold request, with no
failure recorded before cancellation; it is not a completed pass.

The full Linux rerun subsequently passes **1,564 test functions, zero failed,
14 ignored**, including all **49 fleet histories in 269.51 s**, followed by
`cargo xtask check`. Helm was absent in this container, so its four functions
returned through their environment gate. A subsequent checksum-verified Linux
Helm 4.3.0 run passes all four chart gates in 0.05 s (one deliberate golden writer
ignored); script/log: `/private/tmp/slates-ci-35615970514-linux-helm.{sh,log}`. Other opt-in native tests
also require their dedicated commands; the workspace total does not close them.

The same container's mounted NFS conformance sequence passes:

- fsx: 10,000 operations, seed 1.
- fsstress: 500 operations in each of four processes, 2,000 logged, seed 1.
- pjdfstest: 238 files, 8,798 cases; 6,970 passed, 1,800 reviewed expected failures,
  zero unexpected failures, zero listed-now-passing, 28 TODO cases. No list edits.
- Workloads: all nine compare Identical, including the stricter dot-underscore policy.
- Hermeticity: 200 write-capable calls; 22 inside the target, all six landed paths
  matched, 90 RAM-object calls, 88 standard-stream calls, zero unresolved or outside.

Command script: `/private/tmp/slates-ci-35615970514-linux.sh`; log:
`/private/tmp/slates-ci-35615970514-linux-validation.log`; kept artifact:
`/private/tmp/slates-ci-35615970514-conformance.tar.gz`.
This is the same **LIMITED Linux NFS adapter** as the linked conformance lane;
it is not a native FUSE mount verdict. The mounted step completed at 21:52:34 UTC
on 2026-09-22. This run predates the separate malformed-trace-row correction.

The first Terminal trace attempt stopped during compilation and produced no
tracing evidence. The rebuilt reproduction uses the isolated source and a fresh
task-owned target directory; the existing shared build outputs include root-owned
files and were left untouched. The script refuses to run Cargo as root and
elevates only the existing tracer.
A second attempt reached the serial lock while the Linux run was active and
timed out without starting a tracer. The third attempt ran the mounted workload:
`d/f2` returned ENOSPC, the trace remained empty, and fs_usage tried to attach after
the daemon exited. The new barrier/ownership regressions first failed (two cases,
0.05 s), then six real-process cases pass in 0.24 s. Fifty conformance parser/record
cases pass; strict targeted Clippy passes. The mounted rerun must still establish
the complete tracing verdict. It now proves readiness and cleanup: 188 rows /
47,000 bytes, empty stderr, no surviving task process. The space refusal is the
fixture's exhausted eight-inode allowance (three extra client metadata files), with
only four content bytes charged and no snapshot. A page-based fixture completes
the native workload and snapshot; the complete-tree landing oracle includes all
client metadata. The full Linux NFS/strace rerun passes with 220 write calls and
zero unresolved/outside writes. macOS descriptor attribution remains open; see
`2026-09-22-hermeticity-fixture-omits-client-metadata.md`.

The native workload now creates its working directories successfully and runs
eight installed tools. Four compare identical; git, Python, rsync and editor
fail because provenance attributes create visible AppleDouble sidecars. The
watcher tool is absent. This is the previously recorded NFS limitation in
`2026-09-14-nfs-appledouble-sidecars.md`, not a passing workload verdict.
