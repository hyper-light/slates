# A WinFsp drive died with its daemon

Observed 2026-11-02, A-198 and Step 9.3 of the VFS and DAX plan.

## Description and evidence

The public mount oracle keeps an ordinary Windows file handle and mapped view open, kills the serving process,
and checks that the drive still names the original kernel volume. Diagnostic branch `diag-s9-mount-baseline-1`,
commit `6514b4cc81fedf074ac7a5f4d5c0d664301acbcb`, run
[41404129748](https://github.com/hyper-light/slates/actions/runs/41404129748), loses that target at the first kill
on both `windows-latest` and `windows-11-arm`. Each log prints `drive held false` before the assertion, and each test
reports zero passed and one failed. The diagnostic job succeeds only when that particular runtime assertion fails.
Command: `cargo test -p slates-cli --test winfsp a_winfsp_drive_handle_and_view_survive_twenty_killed_daemons -- --exact --nocapture --test-threads=1`.

## Root cause and impact

The daemon alone owns WinFsp's volume control handle and the temporary DOS drive-link handle. Process termination
closes both. A-198 records the WinFsp v2.1 source evidence: cleanup of the last control handle deletes the volume,
and the last link handle's close removes the drive letter. The generic anchor hold from Step 9.2 is not wired to
these objects yet. Recovery also explicitly removes the WinFsp attachment record.

Each per-open context is currently a leaked `Box<OpenFile>` pointer. Keeping the volume alone would leave those
kernel-held pointers naming a dead process's heap. They must become generation-checked records in anchor-held RAM,
resolved by the owning shard rather than dereferenced on foreign dispatcher threads.

## Implementation and verification

The failing test was written and run before changing the mount implementation. The hold, per-open ledger, successor
adoption and takeover coherence work are in progress. This record makes no claim that the 20-kill criterion passes.
Requests in flight and refusal of a foreign successor remain Step 9.4's separate proof obligations.
