# Windows mount holders admitted foreign duplication

Observed 2026-11-02, A-198 Step 9.4.

## Description and evidence

The independent CLI probe is a sibling of the anchor, with no inherited process or mount handle. After mounting a
real volume it opens each holder by pid, asking for `PROCESS_DUP_HANDLE`, `WRITE_DAC` and `WRITE_OWNER`. Before the
fix its first request is granted. Diagnostics branch `diag-s9-foreign-baseline-1`, commit
`fcc9aa4d81afbd4b6f5af108c2a82fcf1f115785`, run
[41426238795](https://github.com/hyper-light/slates/actions/runs/41426238795): both Windows architectures print
`WINFSP FOREIGN: process … granted forbidden rights 0x40`. The parent test fails, zero passed, one failed (0.94 s
x86_64, 1.21 s arm64). The workflow accepts only this specific runtime failure.

Command: `cargo test -p slates-cli --test winfsp a_process_outside_the_anchor_cannot_take_either_holders_mount_handles -- --exact --nocapture --test-threads=1`.

The real-supervisor fixture also checks its child immediately after `start`, before that child role changes any
process permissions. On jupiter, `cargo +1.98.1 test -p slates-anchor --test handles_windows the_supervisor_acknowledges_the_hold_before_commit_and_hands_it_to_its_successor -- --exact --nocapture --test-threads=1`
failed, `a newly created child granted foreign duplication authority`, zero passed, one failed. The child waits for
its hold acknowledgement, so it cannot exit before this check. The parent owns and reaps it even on failure.

## Root cause and impact

The anchor and daemon retain the token's default process DACL. `CreateProcessW` also receives no process security
attributes. Another process of the same user can therefore obtain new duplication authority, then duplicate held
handles out of a mount holder. Inheritance restrictions alone do not constrain a later `OpenProcess` call.
Windows checks that call against the process DACL; duplication requires `PROCESS_DUP_HANDLE`.
[Microsoft process rights](https://learn.microsoft.com/en-us/windows/win32/procthread/process-security-and-access-rights).

The daemon already inherits held objects when it is created. Protecting itself only after it begins running leaves
the interval before its first instruction uncovered. Its parent receives an all-access handle from process creation,
so a restrictive creation DACL can reject new opens while that already-issued handle continues to serve the hold.
The current-process pseudo handle retains all access on the supported Windows versions (the API-specific contract
limits the older DACL-dependent behavior to XP and Server 2003).
[GetCurrentProcess](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getcurrentprocess).

## Exact change and verification

The fix uses one checked security-descriptor owner in IPC. Its protected `OWNER RIGHTS` DACL allows query, wait,
termination and reading the descriptor; it grants no duplication, DACL rewrite or owner change. The explicit owner
entry also removes the owner's implicit DACL-write authority, as the peer-section descriptor already does (A-175).
The descriptor is passed to `CreateProcessW` for supervised children. The dedicated anchor and daemon CLI roles set
their own process DACL before acquiring held resources. Ordinary consumer spawns still choose their own process
security through the same spawn operation; there is no second spawning implementation.

Verification remains in progress. Both failing tests precede the implementation. The existing supervisor fixture
must still transfer the live held object using its spawn handle, and the twenty-kill mount oracle must still work.
This DACL controls newly opened process handles; it does not revoke authority already issued by Windows.
