# The Windows writer walked its own drive before A-227 looked (2026-11-09, §4.6, §4.15 step 4, A-227, A-185)

## Description

A-227 refuses a landing target on a mount the daemon serves before any open, since a daemon thread opening its own
volume waits on an answer only the daemon can give. On Unix the writer checks each component as it walks. Porting the
Windows writer (A-185) onto a main that has A-227, its walk (`slates_base::walk_dir`) opened every component of the
target from the drive's root first, and A-227's Windows check (`check_root_volume`, by drive) ran only afterwards, in
`OsHost::open_root`.

## Measurement

Both CI Windows runners, with WinFsp, `a_landing_target_on_the_daemons_own_drive_is_refused_before_any_open`
(`crates/cli/tests/winfsp.rs`), on the port onto `ecb86b4f`:
- the writer as fixed (`38b71fab`): passed, x86_64 and arm64 (run 42375546626).
- the fix without the early check (`f7b889fa`, a diagnostics branch): on both runners the test step never wrote its
  results; the job ended first (run 42378512740). That matches the hang A-227 describes, a daemon waiting on its own
  drive, but no log of the hang was kept, so it is not proof.
- the fix with `OwnMount` reported as `TargetUnavailable` (`344d2bcd`): passed on both runners. The test then checked only
  that the error named `OwnMount`, and the unavailable reason names it too. It now checks for `LandingTarget` and no
  `TargetUnavailable`; that version has not yet run under the break.

## Root cause

The Windows walk and A-227's Windows check were written apart (A-185 on 896233d1, A-227 after it), and the walk came
first.

## Impact

None released: the Windows writer had not landed.

## Edits

- `crates/base/src/windows.rs`: `walk_dir` runs `check_root_volume` before it opens the drive's root.
- `crates/land/src/os_windows.rs`: `TargetRefusal::OwnMount`, and a host's `OwnMount` mapped to it.
- `crates/cli/tests/winfsp.rs`: the test above.
- `.github/workflows/ci.yml`: both Windows lanes run `slates-mcp`'s test.

## Sweep

- Unix's twin of the test checks the word alone (GAPS).
- `crates/cli/src/dumps.rs`'s unit test does not build on Windows (GAPS).
