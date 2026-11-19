# A WinFsp drive declared a folding volume case-sensitive

Date: 2026-11-18. Area: `crates/bridge-winfsp/src/host.rs` (the mount's volume parameters, the open replies),
`crates/server/src/winfsp.rs`; for the fix's first part `crates/bridge-core` and `crates/vfs`. Design: §4.4 (name
policy), §4.6 "Windows"; A-300; amendment A-543. Goal 85 (B2). Found 2026-11-17 by reading WinFsp v2.1's source for
A-300 (GAPS).

## Description

A volume made with `--fold` (`NamePolicy::Fold`) finds a file under any spelling that folds alike, as NTFS does. Its
WinFsp drive told Windows the opposite: the mount sets `CaseSensitiveSearch` for every volume. WinFsp's driver keys its
open files by name and compares case-sensitively under that bit (`src/sys/device.c` 676 and 750, `src/sys/create.c`
316-318). So a file opened as `A.TXT` and as `a.txt` is two of the driver's file nodes: an exclusive open under one
spelling does not exclude the other, where NTFS answers `ERROR_SHARING_VIOLATION`, and Windows keeps two caches over one
file. T-4.6 (one entry, `EEXIST` on a folded duplicate) had no test through the drive.

## Root cause

The drive's volume parameters ignore the volume's name policy. Clearing the bit for a folding volume is not enough by
itself: WinFsp then names the driver's file node by the upper-cased path unless the file system answers the open with
its normalized name (`create.c` 1036-1043), and nothing in slates could name a path as the volume stores it.

## Impact

Windows programs on a folding volume's drive: share modes did not hold across spellings, and two cached views of one
file could disagree. The Windows workloads cell runs on a folding volume (A-300), but its roster opens no file under two
spellings, so it never met this.

## Exact edits

Part 1 (A-543, this change):
- `crates/vfs/src/volume.rs`: `Volume::stored_name_no`, the name an entry has as its directory stores it.
- `crates/bridge-core`: `Bridge::stored_entry` (the look-up with the stored name; the default answers the name as
  asked), implemented by `VolumeBridge` (after its look-up, so an overlay's base entry is in the directory) and
  `ScopedBridge` (admitted as its look-up is); `stored_path`, a path named component by component as stored.

Part 2 (owed, Windows-only, main's to write and compile): `CaseSensitiveSearch` for an exact volume only; each open and
create of a folding volume answered with its `stored_path` as WinFsp's normalized name (`FSP_FSCTL_OPEN_FILE_INFO`'s
`NormalizedName` when it fits `NormalizedNameSize`), the ledger and the open names recording that path; and the drive
test: an exclusive open of `A.TXT`, then `a.txt` refused `ERROR_SHARING_VIOLATION`, and the final path the stored one.

## Tests

`every_spelling_of_a_file_on_a_folding_volume_names_the_path_it_was_made_with` (`crates/bridge-core/src/lib.rs`, every
host): red with the open named as asked, green with the walk, on the Mac Studio (d1323898, debug). Command:
`cargo test -p slates-bridge-core --lib stored_path_tests`.
