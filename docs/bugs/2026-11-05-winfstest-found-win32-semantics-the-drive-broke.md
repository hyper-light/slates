# winfstest found Win32 calls the WinFsp drive answered wrongly

**Amendment:** A-214.

**Found by:** WinFsp's own winfstest, run over `slates mount ID E:` for the first time (A-214; diagnostics run
41982130174, 2026-11-05, on GitHub's `windows-latest` x86_64 and `windows-11-arm` runners). Its base scripts failed
cases a Windows program acts on; the cases were the same on both architectures.

## Description

Four calls answered success, or kept what they should not, where Windows answers otherwise:

1. **A move not asked to replace replaced a taken name** (`t/base/04.t` cases 7 and 23). `MoveFileExW(bar, baz, 0)`
   with `baz` existing answered success and `baz` held `bar`'s bytes; Windows answers `ERROR_ALREADY_EXISTS` and
   leaves both. Asked to replace (`MOVEFILE_REPLACE_EXISTING`), a move onto a directory replaced it (case 26), which
   Windows never does (`ERROR_ACCESS_DENIED`).
2. **A directory holding a file answered its removal with success** (`t/base/03.t` case 3). `RemoveDirectoryW(dir)`
   with `dir\foo` in it answered success, and `dir` stayed; Windows answers `ERROR_DIR_NOT_EMPTY`.
3. **`SetFileTime` answered success and kept nothing** (`t/base/06.t` cases 5 and 6). The access and write times set
   through a handle were not applied, though the volume keeps all of them (A-53).
4. **An open with `TRUNCATE_EXISTING` kept the file's bytes** (`t/base/05.t` case 12): a 42-byte file stayed 42 bytes.

## Root cause

All four are in `crates/bridge-winfsp/src/host.rs`, WinFsp's callbacks:

1. `rename` received WinFsp's `ReplaceIfExists` as `_replace` and passed `RenameFlags::default()` to the volume, so
   every rename replaced, the POSIX rule, never refusing a taken name; and POSIX `rename` replaces an empty directory,
   which Windows refuses.
2. `can_delete` answered `STATUS_SUCCESS` unread. Its comment relied on Cleanup's `rmdir` refusing a non-empty
   directory, but Cleanup answers the kernel nothing: its refusal was counted and printed, and the caller had already
   been told success.
3. `set_basic_info` ignored its times (`_last_access_time`, `_last_write_time`, `_change_time`) and answered with the
   object's current information: a request swallowed with a success.
4. An open with `TRUNCATE_EXISTING` does not reach `overwrite`: the bridge's own log shows it arriving as
   `SetFileSize(new_size 0, set_allocation_size 1)` (diagnostics run 41993895943). WinFsp's contract for `SetFileSize`
   truncates a file whose allocation is set below its size; `set_file_size` answered every allocation-only call with the
   file's current information.

## Impact

A Windows program that moves a file without asking to replace (Explorer's paste, `MoveFile`, `rename` in most C
runtimes) silently destroyed the target. A program that removes a directory it believes empty (a build's clean step,
an installer) was told it was gone while it stayed. A copy or archive tool that restores times (`robocopy /DCOPY:T`,
`tar`, `git` checkouts) restored none. A program that truncates by reopening could read its old bytes past what it
wrote.

## Edits

- `crates/cli/tests/winfsp_semantics.rs` (new): four tests through a real drive, each making the call a program makes
  and checking the answer and what the drive then holds, through Windows and through slates. Written first; on main's
  bridge each fails (the red twin, below).
- `host.rs`: `Op::Rename` carries `replace`, passed to the volume as `RenameFlags { no_replace: !replace }`, and a
  rename asked to replace a directory other than its own source is refused `NotPermitted` (`STATUS_ACCESS_DENIED`).
  `Op::CanDelete` reads a directory's first three entries (its `.` and `..` and one child) and refuses `NotEmpty`
  (`STATUS_DIRECTORY_NOT_EMPTY`). `Op::SetTimes` applies the access, write and change times WinFsp passes (0 and all
  ones leave a time alone) through the volume's `setattr`. `Op::SetAllocation` truncates a file whose allocation is set
  below its size, and leaves any other allocation as it is.
- `lib.rs`: `unix_ns_from_filetime`, the inverse of `filetime_from_unix_ns`, with its unit test.
- `unsafe-budget.toml`: `slates-bridge-winfsp` 131 → 132, the `CanDelete` callback's handler lookup.

## Evidence

- Found: diagnostics run 41982130174 (x86_64 and arm64; 628 cases, 244 passed, 384 failed, case for case the same).
- Red: main's bridge with the tests, run 41989370753: all four fail on x86_64 and arm64 (`Ok(())` for the move and the
  removal, the old write time, 42 bytes).
- Green: run 41989365843 with the first three fixes (three pass, the truncation fails on both), and run 42046112993
  with all four: the seven tests pass on both (the four, the directory replace, and the two extension replays below).
- winfstest with the fixes: 628 cases, 254 passed, 374 failed and every one listed; before them 384 failed.

## Open: fsx over the drive wedges at its operation 164

The WinFsp fsx port at seed 1 completes 163 operations over the drive and then waits until the step's bound ends it
(`signal 2`, `testcalls = 163`), on both architectures (runs 41984660605, 41989365843, 41993895943). Operation 164 is a
`TRUNCATE UP` from 158,132 to 246,956 bytes, made by the C runtime's `_chsize`, which writes zeros from the end; 163 was
a `TRUNCATE DOWN` from 234,244 bytes, and 162 a cached write at 205,924 whose bytes had not yet reached the bridge. The
bridge's log shows no request at all after the shrink's `SetFileSize`, so the wait is inside Windows, below the single
WinFsp receiver (A-198), not in a slates callback. The same four operations replayed alone through `SetEndOfFile` and
through zero writes complete (`a_file_shrunk_then_extended_by_its_end_reaches_the_size`,
`a_file_shrunk_then_extended_by_zero_writes_reaches_the_size`), so the wedge needs fsx's earlier history, its mapped
views among it. Owed to the WinFsp dispatcher's owners; the fsx cell stays failed until then.

## Siblings

- The creation time cannot be set: the bridge's attributes carry no birth time (it reports the change time) and the
  volume has no setter. Stated as a difference (A-214, CREATION-TIME), not fixed here.
- The attribute bits `SetBasicInfo` and `Create` receive (READONLY, HIDDEN, SYSTEM, ARCHIVE) are not kept: a stated
  difference (A-214, ATTRIBUTES). A read-only file deletes and is replaced (`t/base/01.t` 13, `t/base/04.t` 10).
