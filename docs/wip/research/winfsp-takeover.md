# WinFsp takeover evidence for A-198

Reviewed 2026-11-02 against WinFsp v2.1 and the slates tree based on `71cf09ac`.
Kiln plan: `01a19979ace9754fa2e96d7f2deabb39`; research:
`01a19979aceb7373acf761400ff8511f`. All 23 source quotations were found by kiln's
checker. The grades below are kiln's evidence scores, not measured takeover success.
The design remains unproven until the Windows kill tests pass.

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

The proposed guard records only requests it has observed. The DLL has a gap
between returning from transact and entering that guard; the driver's timeout
still bounds requests lost there. Cleanup and close replay need an idempotent
terminal step. A later implementation may not treat an old hint, an old slab
slot, or a partially published record as current merely because its numeric value
matches.


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
