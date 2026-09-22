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
| Tracer startup/lifetime | fs_usage is spawned without an observed-readiness barrier, stopped after the daemon, and has no drop guard. | Prior CI: zero events, `ktrace_start: No such process`; next CI: early mount failure followed by orphaned tracer error. Native privileged reproduction awaits local authentication. |
| Suite subprocess errors | pjdfstest runner discards stderr, read errors, and exit status. | Audit of `xtask/src/conformance/suites.rs::run_test_file`. Structured failure capture and negative controls owed. |
| Workload equivalence | The comparator filtered every `._*` name, including ordinary user files. | Added-name and changed-content histories fail before removing the filter and pass afterward. All 48 conformance cases pass on macOS and Linux. Native AppleDouble behavior remains unresolved; no new exclusions. |
| macOS NFS capabilities | `_PC_PATH_MAX` is unsupported by Apple's client; FIFO access and several privilege cases differ from Linux. | Primary Apple NFS source and 117 failure IDs beyond the Linux list. Per-history review incomplete; no expected-failure list copied or expanded. |
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
failure recorded before cancellation; it is not a completed pass. A full Linux
workspace and mounted-conformance rerun is in progress.

The first Terminal trace attempt stopped during compilation and produced no
tracing evidence. The rebuilt reproduction uses the isolated source and a fresh
task-owned target directory; the existing shared build outputs include root-owned
files and were left untouched. The script refuses to run Cargo as root and
elevates only the existing tracer. The privileged reproduction is still pending.

The native workload now creates its working directories successfully and runs
eight installed tools. Four compare identical; git, Python, rsync and editor
fail because provenance attributes create visible AppleDouble sidecars. The
watcher tool is absent. This is the previously recorded NFS limitation in
`2026-09-14-nfs-appledouble-sidecars.md`, not a passing workload verdict.
