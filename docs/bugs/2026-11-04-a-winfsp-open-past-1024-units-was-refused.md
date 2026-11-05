# A WinFsp open past 1,024 UTF-16 units was refused

**Amendment:** A-206 (corrects A-198).

**Found by:** the audit of 2dc9f3eb..98000d56 (2026-11-03), step 6 of plan 01a1a34bf7e973069e94f773afdea44a.

## Description

Since A-198, every open records its kernel name in the held ledger before Windows gets its context. The ledger's record
ended at 1,024 UTF-16 units, so every open past that was refused with `VfsError::RecoveryIncomplete`'s status, on paths
that worked before A-198.

## Root cause

`opens/names.rs` sized its record by `notify::MAX_PATH_UNITS`, documented as "the longest path WinFsp hands a file
system", from `FSP_FSCTL_TRANSACT_PATH_SIZEMAX`. That is not what the constant limits. In WinFsp v2.1 it bounds a
response's normalized name (`src/dll/fsop.c`) and a notification's name relative to the record before it
(`src/sys/volume.c`). A request's name is bounded only by the whole request (`src/sys/iop.c`, `FspIopCreateRequestFunnel`:
`FSP_FSCTL_TRANSACT_REQ_SIZEMAX`, 16,320 bytes). Three places took the wrong limit: the ledger's record, the notification
round (which left the name out, so another path's change never reached its node) and the rename parser in
`inflight.rs`, whose refusal ended the receive worker on a rename to a long name.

## The fix

- `MAX_PATH_UNITS` is derived from the driver's own bound: a request's maximum less its fixed part and the name's
  terminator, 8,115 units. The live test measures the limit and holds this equal to it.
- The driver checks no length when a directory's rename lengthens the names beneath it (`FspFileNodeRename`), so a rename
  that would take a held name past the cap is refused `STATUS_NAME_TOO_LONG` before it has any effect
  (`OpenNames::rename_fits`). Such a name could not be opened again through the driver anyway. This is unit-tested only:
  the driver also limits renames over open descendants, so a live case is not dependable.
- A record keeps its first 1,024 units in place and the rest in a second region, with the checksum over both. Windows
  commits by the 64 KiB granule, so a short path commits what it did before, and a read commits nothing past the name's
  own length. The reserve is 2,133,147,696 bytes of a `SEC_RESERVE` section per mount at `MAX_OPEN_HANDLES` (65,536 slots,
  two versions), made in about 2.2 ms for 4,166,312 bytes of paged pool (jupiter, 2026-11-04, 3 of 3). The ledger's format
  is `WFO4`.
- A notification record is limited only by its 16-bit `Size`, 32,761 units, far past any name held.

## The criterion, restated

The goal said 0 refused between 1,025 and 32,767 units. The WinFsp driver itself refuses part of that range before any
request reaches a file system: `FspIopCreateRequestFunnel` answers `STATUS_INVALID_PARAMETER` when the request's fixed part
and its name pass `FSP_FSCTL_TRANSACT_REQ_SIZEMAX`. The supervisor restated it on 2026-11-04 as **slates refuses no path the
WinFsp driver passes.**

Measured live by halving on `windows-latest` and `windows-11-arm`, run 41978665514, WinFsp 2.1.25156: **the driver refuses
from 8,116 UTF-16 units**, as the source predicts. 8,115 units round-trip through slates. 8,116 with its parents in place
answers `os error 87`, where slates could only answer not found. 32,761 units, the longest a drive letter can name, answers
206 (`ERROR_FILENAME_EXCED_RANGE`) above the driver.

## Evidence

`names_up_to_the_drivers_limit_are_kept_whole`, `a_torn_rename_of_a_long_name_keeps_a_complete_name`,
`a_corrupt_tail_is_refused`, `a_short_name_never_reaches_the_tail_region` (`opens/names.rs`),
`a_name_past_a_thousand_units_is_told` (`notify.rs`) and `a_rename_of_a_long_path_is_read_whole` (`inflight.rs`) run on any
host. Live on CI's Windows lanes: `opens_past_a_thousand_units_are_refused_only_by_the_winfsp_driver` and
`a_file_at_a_two_thousand_unit_path_is_served_across_a_killed_daemon` (`crates/cli/tests/winfsp.rs`).
