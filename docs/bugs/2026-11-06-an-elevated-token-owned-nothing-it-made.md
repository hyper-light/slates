# An elevated token owned nothing it made (2026-11-06, §4.15 step 4, A-185)

## Description

Porting the Windows landing writer onto main, the two server lib tests that had answered `Unsupported` now answered
`TargetUnavailable { reason: "the target is owned by another user" }` on both CI Windows runners, and so did
`crates/client/tests/presentation.rs`. Every `slates-land` suite passed in the same run.

## Measurement

Both runners, a pwsh probe (run 42343224241's branch, 2026-11-06): user `runneradmin`, elevated, token default owner
`BUILTIN\Administrators`; the workspace, `crates\server` and a directory the step made all owned by
`BUILTIN\Administrators`.

jupiter, same user, same machine, two shells:
- PowerShell: token default owner `BUILTIN\Administrators`; a new directory is owned by it.
- Git Bash: token default owner `corph`; a new directory is owned by `corph`.

Before the fix, from PowerShell on jupiter: `os` 1 of 6 passed, `os_seam_windows` 1 of 10, each failure `TargetNotOwned`.
The same suites passed from Git Bash, which is where the first runs and the first diagnostics ran them. That Git Bash sets
the default owner is inferred from those measurements; the mechanism was not read in its source.

## Root cause

`owned_by_this_user` compared a target's owner with the token's user alone. An elevated process's own creations get the
token's default owner, which is not its user. So the writer refused every directory such a process made. Unix's check,
`st_uid == geteuid`, asks whether the target bears the owner this process's creations get, and the Windows check did not.

## Impact

Landing was refused on any elevated Windows session and on both CI runners, for directories the user had made. It failed
closed: nothing was written.

## Edits

- `crates/land/src/os_windows.rs`: a target's owner may be the token's user or its default owner.
- `crates/land/tests/os_seam_windows.rs`: `a_directory_this_process_makes_is_a_target_it_owns` (red before, from
  PowerShell) and `a_target_another_user_owns_is_refused` (System32, TrustedInstaller's; with the check forced true it
  fails).
- `crates/land/src/engine.rs`: the move check sets the birth time aside on Windows alone (`BIRTH_SURVIVES_A_MOVE`). The
  WIP set it aside everywhere, which took from Unix the field that tells a reused ext4 inode number (A-151).
- `crates/server/tests/lease.rs`, `snapshot_landing.rs`, `common/target.rs`: paths are joined, since a verbatim path takes
  no forward slash (`Os` code 123). `alias_of` splits on either separator, so Windows runs the two-spellings half it had
  skipped.
- The diagnostics' Windows legs run from pwsh; `CLAUDE.md` records why.

## A widening to weigh

An elevated process now accepts a target any administrator of the machine owns. Such a process can already write there,
so this adds no reach, and the human's grant still names the target. An unelevated token's default owner is its user, so
it still refuses them.

## Correction

The commit `8258c486` says a rename set to replace, an entry's open set to follow junctions, and a create set to overwrite
all passed `os`, `os_escape` and `os_removal`. That run left out `os_removal` for those three. Run with it (2026-11-06,
jupiter): the replacing rename fails `os_removal`; the other two pass all three suites, so they are what
`os_seam_windows.rs` adds.

## Sweep

- Every other `format!("{}/…")` over a target path in the server's landing tests was changed with these.
- Reported, not changed: `crates/land/examples/land_bench.rs` and CI's other Windows steps were not reviewed for the shell
  they run under.
