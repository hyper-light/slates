# WinFsp takeover evidence for A-198

Reviewed 2026-11-02 against WinFsp v2.1 and the slates tree based on `71cf09ac`.
Kiln plan: `01a19979ace9754fa2e96d7f2deabb39`; research:
`01a19979aceb7373acf761400ff8511f`. The initial eight findings' 23 source quotations were found by kiln's
checker. Subsequent findings and corrections are recorded below. The grades below are kiln's evidence scores, not measured takeover success.
Source grades are separate from the live Windows measurements; the successful kill histories are recorded below.

| Finding | Evidence and conclusion | Kiln grade |
|---|---|---|
| f1 | WinFsp deletes a volume during cleanup of its control file object. Windows sends cleanup after the last handle closes. An anchor-held duplicate should keep that file object open across daemon death; the live test must establish the resulting volume lifetime. [WinFsp volume source](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/src/sys/volume.c), [Microsoft cleanup contract](https://learn.microsoft.com/en-us/windows-hardware/drivers/kernel/irp-mj-cleanup), [DuplicateHandle](https://learn.microsoft.com/en-us/windows/win32/api/handleapi/nf-handleapi-duplicatehandle). | B, 78/100 |
| f2 | `FspMountSet_Drive` opens the drive link with `DELETE` and makes it temporary. Hold its handle as well as the volume's, and retain explicit detach cleanup. This is the kernel drive link of the existing mount; it creates no on-disk link. [WinFsp mount source](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/src/dll/mount.c), [temporary-object lifetime](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nf-wdm-zwmaketemporaryobject). | B, 76/100 |
| f3 | `FspFileSystemCreate` initializes DLL TLS, and dispatchers transact on the structure's volume handle. Creating the DLL state and substituting the held handle is a candidate inferred from source, not a documented reopen API. Verify parameters, ABI, shutdown and failure behavior on both runner architectures. [WinFsp dispatcher source](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/src/dll/fs.c). | B, 76/100 |
| f4 | `UserContext2` is retained per file descriptor. Replace process-local pointers with bounded records carrying generation-checked file identity, bridge handle and purge-open classification. A kill during publication must leave either a complete usable record or a typed refusal. [WinFsp create source](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/src/sys/create.c), [A-198](../SLATES_DESIGN.md). | B, 76/100 |
| f5 | Pending requests differ from requests already sent to user mode. The latter are matched to responses by `Hint`. A missing current hint is ignored; that does not prove a saved hint is safe after its address is reused. Account for pending requests, observed processing requests and the transact-to-ledger gap separately. [WinFsp queues](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/src/sys/ioq.c), [response matching](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/src/sys/volume.c). | B, 76/100 |
| f6 | The bridge already releases a predecessor's open through `release_restored_open`, and exposes `takeover_inodes` for stale cached state. Reuse those contracts. An old open must not release a new open at a reused slot; test open-unlinked files and hard-link names. [Bridge release](../../../crates/bridge-core/src/volume_bridge.rs), [takeover contract](../../../crates/bridge-core/src/lib.rs). | B, 72/100 |
| f7 | Duplicating out of a process requires `PROCESS_DUP_HANDLE`; a spawn's explicit handle list narrows inheritance. The anchor should duplicate out of its live child and acknowledge before the daemon commits the attachment record. Test unauthorized duplication independently of the rendezvous ownership check. [Process rights](https://learn.microsoft.com/en-us/windows/win32/procthread/process-security-and-access-rights), [handle-list attribute](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute). | B, 76/100 |
| f8 | Completion requires zero drive losses across twenty kills on each Windows architecture, old-handle reads, mapped-view updates, a no-hold red-check, a typed in-flight result and a refused second daemon. Source inspection and the event-handle oracle do not establish those live-mount results. [A-198](../SLATES_DESIGN.md), [platform gap 2](../GAPS.md). | B, 70/100 |

## Verification sequence

1. Exercise handle transfer across real processes with a signalled event. Require
   the sender, successor and no-hold checker to execute their test bodies. Require
   an unrelated inheritable object to be absent from the successor.
2. Wire acknowledgement before attachment-record commit. Kill before duplication,
   after acknowledgement and before commit; reconcile held-but-uncommitted entries
   without adopting another attachment's handles. Keep channel and entry bounds
   derived from the attachment capacity, with typed refusals at those bounds.
3. Exercise the real WinFsp mount through twenty kills on `windows-latest` and
   `windows-11-arm`. Keep one file handle and one mapped view throughout. After
   every takeover change the volume through another path and require both readers
   to follow. The diagnostic tree without the hold must lose the drive.
4. Kill with a request in flight, then during cleanup/close. Require an answered
   request or typed cancellation, no double delete, no released-new-open alias,
   and no application of a queued purge's zero bytes. A ledger entry is not proof
   that its response reached the kernel.
5. Attempt takeover from a second daemon and duplicate the held handles from an
   unrelated process. Require refusal without adding privilege or changing the
   user's system configuration. Run the required gates and main CI before closing
   the gap.

The proposed guard records only requests it has observed. The DLL has a gap between returning from transact and
entering that guard. The source audit below corrects the initial timeout claim: a request lost there has no timeout
under WinFsp v2.1's actual queue policy. Cleanup and close replay need an idempotent terminal step. A later
implementation may not treat an old hint, an old slab slot, or a partially published record as current merely because
its numeric value matches.


## Step 9.4: process authority at creation (2026-11-02)

The independent live-mount probe fails on both architectures before confinement: diagnostics
[41426238795](https://github.com/hyper-light/slates/actions/runs/41426238795), on `fcc9aa4d`, grants a sibling
`PROCESS_DUP_HANDLE` on a mount holder. The real-supervisor fixture on jupiter also grants a new duplication handle
immediately after child creation. The child role has not changed its own DACL. These are measured failures, separate
from the successful twenty-kill mount history.

The process DACL belongs in `CreateProcessW`'s process attributes for supervised children, because the child already
has its inherited handles before it executes. Microsoft documents that CreateProcess returns an all-access process
handle to its caller; that already-issued handle remains the supervisor's duplication authority. The dedicated CLI
anchor role also protects itself before holding objects. This is a process-object policy, not a privilege or a change
to the user's account or system configuration. Evidence B:
[process rights](https://learn.microsoft.com/en-us/windows/win32/procthread/process-security-and-access-rights),
[process creation attributes](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessw),
[SetKernelObjectSecurity](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-setkernelobjectsecurity).

The general process-rights page describes the pseudo handle as DACL-dependent, while the API-specific
[GetCurrentProcess](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getcurrentprocess)
contract grants all access on supported Windows and limits the older behavior to XP and Server 2003. Use the specific
contract and retain the real-process transfer oracle: after protection the holder must still duplicate its own
objects, and its supervisor must still transfer them through its spawn handle. OWNER RIGHTS follows A-175's measured
peer-section policy, which also suppresses the owner's implicit WRITE_DAC. The new probe separately requests
WRITE_DAC and WRITE_OWNER so refusing duplication alone cannot hide an ACL rewrite.

The initial probe did not control the caller's debugger privilege. Jupiter and both runner architectures actually
have it enabled, which bypasses the process DACL. A restricted copy of the probe's own token now disables privileges
on that thread only; PrivilegeCheck independently observes `during false`, and the helper checks restoration.
With identical normalization, [the red run](https://github.com/hyper-light/slates/actions/runs/41432477849) still
grants duplication on both architectures, while [the protected run](https://github.com/hyper-light/slates/actions/runs/41432451084)
refuses duplication, DACL rewrite and owner change for both holders. The latter retains zero drive losses across
twenty kills per architecture, all six live tests, all 32 active anchor tests, native lint and docs. This establishes
new unprivileged process-open policy, not confinement of already-issued authority or enabled SeDebugPrivilege.
Evidence B: [OpenProcess](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-openprocess),
[restricted token](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-createrestrictedtoken),
[same-identity impersonation](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-impersonateloggedonuser).

## Step 9.4: processing requests do not time out (2026-11-02 source correction)

WinFsp v2.1's [driver.h](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/src/sys/driver.h) defines
`FSP_IOQ_PROCESS_NO_CANCEL`. Under that definition [ioq.c](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/src/sys/ioq.c)
sets a processing request's timestamp to infinity, installs no cancellation routine, and excludes processing and
retried requests from RemoveExpired. The previous claim that the transact-to-guard gap times out is wrong. A
guard-only journal cannot establish bounded settlement of every taken request. This is a source finding (B), not a
passing live-mount result.

There is a second boundary: [the DLL dispatcher](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/src/dll/fs.c)
returns a response in its next transact. A guard leaving an operation does not prove the driver accepted that
response. A saved hint can already have completed; another dispatcher can have taken a request at its reused IRP
address. The driver matches only that address in its processing queue. Missing-hint behavior is insufficient.

The new live oracle retains a real Request::Read after the production host delivers it, kills the serving process,
and starts a successor on the same held controls and ledger. The reader must return bytes or
ERROR_OPERATION_ABORTED within ten liveness budgets. Its first run must fail before request recovery is added.
The fixture uses a fixed read-only file in each process to isolate transport settlement; the CLI's twenty-kill
history remains the persistence and view-coherence oracle.


## Step 9.4: receive boundary and coherence corrections (2026-11-02)

These are kiln's current grades after source checking on 2026-11-02. An inference is named as such; its grade does
not substitute for the live regression. The original guard-only design and timeout claim above are rejected.

| Finding | Evidence and conclusion | Kiln grade |
|---|---|---|
| f9 | Processing requests never expire under `FSP_IOQ_PROCESS_NO_CANCEL`; a callback guard leaves an uncovered receive gap. [Driver configuration](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/src/sys/driver.h), [queue implementation](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/src/sys/ioq.c). | A, 90/100 |
| f10 | Inference: retain the direct-output batch in held pages and settle it before any new receive, including after recovery, to prevent a recycled hint from matching a fresh processing request. [Transaction format](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/inc/winfsp/fsctl.h), [direct I/O buffers](https://learn.microsoft.com/en-us/windows-hardware/drivers/kernel/buffer-descriptions-for-i-o-control-codes), [driver transaction](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/src/sys/volume.c). | B, 79/100 |
| f11 | Process termination requests cancellation of that process's pending I/O; the successor must await process exit before reading its receive journal. [TerminateProcess](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-terminateprocess). | A, 85/100 |
| f12 | Inference: the shortest admitted TransactTimeout, 1000 ms, bounds an idle worker's join without stopping the held volume. This is distinct from processing-request lifetime. [Public limits](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/inc/winfsp/fsctl.h). | B, 72/100 |
| f13 | Inference: announce readiness with Transact0 before exposing the drive, matching current WinFsp's dispatcher. The older v2.1 tag omits the newer hardcoded early-request gate, so the difference is recorded rather than silently treating the tag as the installed driver. [Current dispatcher](https://raw.githubusercontent.com/winfsp/winfsp/master/src/dll/fs.c), [current volume gate](https://raw.githubusercontent.com/winfsp/winfsp/master/src/sys/volume.c). | B, 72/100 |
| f14 | Inference confirmed by the failure traces: retain the original kernel name and target the private purge by file identity. A successful purge at the renamed namespace home does not reach the old cached node. [Exact-name kernel lookup](https://raw.githubusercontent.com/winfsp/winfsp/v2.1/src/sys/file.c), [diagnostic 41465398912](https://github.com/hyper-light/slates/actions/runs/41465398912). | B, 76/100 |
| f15 | Inference: TokenId cannot survive a copied security context; a mount-specific restricting SID in a less privileged token survives that copy. Authenticate PID, marker and the bounded pending purge together. [Security-context copy](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/nf-ntifs-secreateclientsecurityfromsubjectcontext), [restricted tokens](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-createrestrictedtoken). | C, 68/100 |
| f16 | Inference: an owned auto-reset event wakes the private Begin/End receiver even when an open yields no callback. The control call remains owned until completion. [SetEvent](https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-setevent), [WaitForMultipleObjects source](https://raw.githubusercontent.com/MicrosoftDocs/sdk-api/docs/sdk-api-src/content/synchapi/nf-synchapi-waitformultipleobjects.md). | C, 68/100 |
| f17 | Inference confirmed by the stage log: an empty WinFsp round advanced its generation without registering for the next volume change. Rearm after delivery and before parking. [Waiter source](https://github.com/hyper-light/slates/blob/01923954cfa6137dba286f9972a85f1cc3e2d7ae/crates/vfs/src/volume.rs), [stage trace 41482946787](https://github.com/hyper-light/slates/actions/runs/41482946787), [bug record](../../bugs/2026-11-02-a-winfsp-empty-round-left-the-next-change-unheard.md). | B, 72/100 |

The retained-read oracle first hangs for 10.14 s per architecture in
[41437374883](https://github.com/hyper-light/slates/actions/runs/41437374883), even after a fresh successor read
succeeds. With the single receiver, [41457226598](https://github.com/hyper-light/slates/actions/runs/41457226598)
settles one counted request and returns both files' bytes in 0.15 s on x86_64 and 0.16 s on arm64. That run retains
zero lost drives of twenty per architecture. No-hold and no-confinement red-checks are recorded in A-198 and the
process-authority section above.

The callback-free event oracle is independent: omitting only its signal fails with WAIT_TIMEOUT after 10.00 s on
each architecture ([41480713447](https://github.com/hyper-light/slates/actions/runs/41480713447)). The real channel
passes all 54 native bridge tests. The subsequent untraced suite still found the empty-round loss, establishing why
passing callback traces could not close the whole change.

## Final untraced verification (2026-11-02)

Diagnostics [41484542209](https://github.com/hyper-light/slates/actions/runs/41484542209), commit
`a7e5bce5c4ec70c9773a434887cc66c9b865a90e`, pass without operation traces on both Windows architectures. Each
runner passes all nine live CLI tests and loses zero drives in twenty kills; every iteration requires the original
handle's counted read and the original mapped view following the successor, with four restored reads in the final
daemon. The live renamed view records one completed purge/echo; the renamed takeover records two. Neither revives
the removed public name, and no purge is refused or kept. All six separate foreign process-rights requests are
denied, and the independent second daemon is refused while its original owner still serves.

The same run passes all 54 bridge tests, one separately enabled real-driver captured-read oracle (one counted
settlement, 0.15 s on x86_64 and 0.19 s on arm64), all 32 active anchor tests, native clippy and private Windows
documentation. The CLI histories take 41.51 s on x86_64 and 21.76 s on arm64. Mac formatting, workspace clippy,
structural/literal/unsafe checks and private workspace documentation pass in 92.15 s on the same code.

## A-215: what one transact does, for a response that rides a receive (2026-11-05)

Read in WinFsp v2.1 (tag `v2.1`, commit `ddca7bd`, cloned from `github.com/winfsp/winfsp`), the version CI pins. Each
line below is the source's own, graded B (deployed code read at its source).

| Question | Answer and citation |
|---|---|
| In one transact, are responses finished before requests are delivered? | Yes. `FspVolumeFastTransact` runs the response loop (`src/sys/volume.c:1079–1114`), then retried IRPs (1116–1139), then the wait (1167–1186), then delivery (1188–1271). |
| How is the receive buffer transferred? | `FSP_FSCTL_TRANSACT_BATCH` is `METHOD_OUT_DIRECT` (`inc/winfsp/fsctl.h:104`). The driver writes through `MmGetMdlVirtualAddress`, the caller's own locked address (`volume.c:1147–1149`). The input is the I/O manager's buffered copy (`volume.c:1024`). |
| Is a request written before it is processing? | Yes: `RtlCopyMemory` (1224), then `FspIoqStartProcessingIrp` (1228). |
| What does the single-request code transfer? | `FSP_FSCTL_TRANSACT` is `METHOD_BUFFERED` (`fsctl.h:100`), which the DLL's dispatcher uses (`src/dll/fs.c:288`). Its output reaches user memory only when the call completes, so a process killed after delivery would lose requests the driver had put in processing. No crash-safe receive can use it. |
| What does a dying caller's wait do? | The wait is `FspQeventCancellableWait`, an alertable kernel wait. It returns `STATUS_THREAD_IS_TERMINATING` when the thread is terminating (`src/sys/driver.h:941–966`). That maps to `FspIoqCancelled` (`src/sys/ioq.c:617`), and the call returns having delivered nothing (`volume.c:1181–1185`). |
| How is a response matched to its IRP? | By address alone, in the processing queue's table (`FspIoqEndProcessingIrp`, `ioq.c:673–680`; the peek, 349–383). The hint check at `volume.c:1099` is a debug-build assertion. Completion is chosen by the IRP's major function, not the response's kind (`src/sys/iop.c:414–427`). An address not in processing is dropped as BOGUS (`volume.c:1093`). |
| What happens to processing IRPs when the transacting process dies? | Nothing. `FSP_IOQ_PROCESS_NO_CANCEL` (`driver.h:970`) gives them no cancel routine and keeps them out of expiry (`ioq.c:574`). Only `FspIoqStop` (`ioq.c:535–552`) ends them, at the cleanup of the volume's control file, which the anchor's duplicate handle keeps open (A-198). |
| Can a response's completion put the same IRP back to be delivered? | Yes. A Create completion can post an Overwrite request on the same IRP (`src/sys/create.c:1211`, retried through `FspIopRetryPrepareIrp`, 825). It arrives as a new request carrying its own hint and is answered as one. |

The candidate (the supervisor's, 2026-11-05) holds with one change. The batch being answered and the batch being received
cannot share a buffer: recovery needs the answered batch whole when the call delivered nothing. So the journal has two
receive buffers (A-215). The model oracle in `crates/bridge-winfsp/src/inflight/capture.rs` kills the owner at every
step, twice over, of a ten-IRP history whose freed addresses go to the next IRP. On 2026-11-05 on macOS
(`cargo +1.98.1 test -p slates-bridge-winfsp --lib every_irp -- --nocapture`), 7,483 scenarios gave 0 lost, 0 wrong,
0 completed twice and 0 wrong contexts. 1,227 kills fell in a delivered hand-off and 2,367 in an undelivered one, and
6,392 deliveries reused an address freed in the same call. Red-checks, each run once:
- answering a delivered hand-off's old batch again (A-198's hazard): 300 wrong and 1,266 lost;
- leaving an undelivered hand-off's batch unsettled: 1,128 lost;
- keeping the saved response when the delivered batch is taken: recovery refuses (`Refusal::Hint`).

