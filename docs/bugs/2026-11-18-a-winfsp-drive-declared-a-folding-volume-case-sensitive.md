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

Part 2 (A-551, 2026-11-19, g258; the Windows half compiled and run by main):
- `crates/bridge-winfsp/src/host.rs`: `Mounted::new` declares `CaseSensitiveSearch` only for an exact volume, and a held
  drive keeps the posture its record names; `system_upcase` (ntdll's `RtlUpcaseUnicodeChar`, the table the driver
  compares by); `record_open_reply` names each open by `crate::normalized::node_name` over its `stored_path` and records
  that name in the ledger and `OpenNames`; `Reply::Opened` carries it, and `finish_open` copies it into
  `FSP_FSCTL_OPEN_FILE_INFO`'s `NormalizedName` when it fits (`put_normalized_name`).
- `crates/bridge-winfsp/src/normalized.rs` (new, pure): the rule. Each prefix as Windows already holds it, else the
  stored spelling where the driver takes it for the asked one, else as asked; past 1,024 units the driver's own
  upper-cased name; a path that lost an unpaired surrogate unanswered.
- `crates/bridge-winfsp/src/notify.rs`: `OpenNames::searching` keeps the open names by the units the driver compares
  them by, so an open finds the spelling Windows holds a prefix of its path by (`spelled_at`).
- `crates/bridge-winfsp/src/held.rs`: the record's flag byte keeps the drive's case posture (bit 1).
- `crates/bridge-winfsp/src/inflight.rs`: `finish_response` clears what a successful Create's size counts besides its
  name and descriptor (WinFsp v2.1's DLL counts the name twice when it places the descriptor, `fsop.c` 468 then 413).
- `crates/bridge-winfsp/src/ffi.rs`: `OpenFileInfo`, `FSP_FSCTL_OPEN_FILE_INFO`'s layout, checked when it compiles.
- `crates/server/src/winfsp.rs`: `name_search` gives the mount its volume's policy; the shard's `OpenNames` searches as
  the drive declared (`Ready::search`).
- `crates/cli/tests/winfsp_semantics.rs`: the drive tests.

## Tests

`every_spelling_of_a_file_on_a_folding_volume_names_the_path_it_was_made_with` (`crates/bridge-core/src/lib.rs`, every
host): red with the open named as asked, green with the walk, on the Mac Studio (d1323898, debug). Command:
`cargo test -p slates-bridge-core --lib stored_path_tests`.

Part 2, every host: `normalized::tests` (each spelling of a file names one node by its made spelling; a case-sensitive
drive answers nothing; a stored spelling answered only where the driver takes it; the spelling Windows already holds a
path or a prefix by; past 1,024 units the driver's upper-cased name; a lost surrogate unanswered; final sigma takes the
open node's name), `notify::tests::the_spelling_windows_holds_a_path_by_follows_opens_closes_and_renames`,
`inflight::tests::a_create_answered_with_a_name_and_a_descriptor_sends_no_byte_of_an_earlier_response` and
`held::tests::the_mount_record_keeps_how_its_drive_compares_names`. Command:
`cargo test -p slates-bridge-winfsp --lib -- normalized:: notify:: inflight:: held::` on the Mac Studio (d1323898,
debug, 2026-11-19): 9 of 43 red with today's rules restored in a copy, 43 of 43 green, the whole lib suite 96 of 96.
Windows (main's): the drive
tests `every_spelling_of_a_file_on_a_folding_drive_is_one_file_to_windows` and
`an_exact_drive_keeps_two_spellings_two_files` in `crates/cli/tests/winfsp_semantics.rs`.
