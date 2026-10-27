# Windows base file times saturated, so no outsider's edit was ever seen (2026-10-26)

## Description

On Windows, an overlay volume never noticed a file changed in place on the disk beneath it. A Shards guest's
section of an untouched base file was never refilled with the edit
(`crates/client/tests/shards.rs`, `a_section_of_an_untouched_base_file_is_refilled_with_an_outsiders_edit`,
G-shards-windows step 5, A-180). Slates' own read and the guest's `READ` returned the new bytes, but the guest's
`GETATTR` showed the same times before and after the edit. No `shards.dax_refilled` was counted.

## Root cause

Found by instrumenting `follow_live_disk` (`crates/vfs/src/base.rs`) in a scratch tree on jupiter (Windows
10.0.26200). Before and after the edit, the host's `fstat` reported `mtime_ns = ctime_ns = 9223372036854775807`
(`i64::MAX`), and the inode held the same. So `adopt_observed` found nothing changed and marked no backing stale.
Windows' own metadata showed the modified time move by 92 ms.

`crates/base/src/windows.rs` kept every host time as nanoseconds since 1601, the FILETIME epoch. `filetime_ns`
multiplied the file time's 100 ns intervals by 100. In 2026 that count is about 1.34 × 10^17, so the product is about
1.34 × 10^19, past `i64::MAX` (an `i64` of nanoseconds spans about 292 years, so that domain overflows for any date
after 1893). It saturated, so every modification, change and birth time of every Windows base file was `i64::MAX`.
`FILE_BASIC_INFO.ChangeTime` saturated the same way, and `now_ns` added the 1601 offset in seconds before scaling and
saturated too.

## Impact

Windows only, since the base host was written (AUD-29-62):
- an in-place change to an untouched base file was never adopted: its size could follow (`follow_live_disk` reads it),
  but a same-size edit left its attributes as they were, and no Shards backing of it was refilled;
- every base file's times, as any transport shows them, were `i64::MAX` nanoseconds (measured: the guest's `GETATTR`
  above). From reading the code, not run: the WinFsp bridge converts attributes as Unix nanoseconds
  (`filetime_from_unix_ns`), so a Windows program would see a date in 2262;
- from reading the code, not measured: the racy-window rule (`now − mtime ≤ granularity`) compared two saturated
  values, so every file would look racy and a kept digest would never be trusted;
- birth times, which tell a replaced file from a renamed one (A-151), were all equal. The file index still tells them
  apart, so identity held.

## Exact edits

- `crates/base/src/windows.rs`: `FILETIME_UNIX_EPOCH_TICKS` (116,444,736,000,000,000 intervals) replaces
  `FILETIME_EPOCH_OFFSET_S` and `NS_PER_S`. `unix_ns_of_ticks` subtracts it before scaling, used by `filetime_ns` and
  for `ChangeTime`. `now_ns` returns the wall clock's nanoseconds since the Unix epoch. Every host time is now in the
  domain the Unix host and the rest of slates use.
- `crates/base/tests/host.rs`: `a_files_times_are_unix_nanoseconds_and_an_in_place_edit_moves_them`, on every host
  (`tick_for` and `TICK_FLOOR` no longer Unix-only). It failed on jupiter before the fix (`mtime 9223372036854775807 is
  not near the wall clock 1793061607718573700`) and passes after; it passes on macOS before and after.
- The Shards refill test above passes after the fix, 3 of 3.

## Siblings checked

Every FILETIME conversion in the tree: only `crates/base/src/windows.rs` (three times and `now_ns`, all fixed) and the
WinFsp bridge's `filetime_from_unix_ns`, which converts the other way, from Unix nanoseconds, and was right.
