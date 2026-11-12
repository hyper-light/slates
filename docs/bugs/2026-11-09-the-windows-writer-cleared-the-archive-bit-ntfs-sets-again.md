# The Windows writer cleared the archive bit NTFS sets again (2026-11-09, A-262, A-185, §4.15 step 8)

## Description

After merging main `1c49c33f` (A-262: a Windows fingerprint carries the kept attribute bits, `ARCHIVE` among them) into
the Windows writer's branch, `a_landing_of_a_snapshot_writes_its_bytes_and_the_head_lands_after_it`
(`crates/server/tests/snapshot_landing.rs`) failed on both CI Windows runners (diagnostics run 42688573318) and on
jupiter: the head's landing over the file the snapshot's landing had placed was refused `LandingConflict { entries:
["/f"] }`.

## Measurement

A print in `verdict::replaced`, removed after (jupiter, 2026-11-09): the witness and the disk's fingerprint differed in
`windows` alone, 0 against 32 (`FILE_ATTRIBUTE_ARCHIVE`); the device, file index, size, times, mode and change time were
equal. The writer's seam reproduces it alone (`a_placed_file_keeps_after_its_close_what_was_read_before_it`): a file
written, given a mode, placed and read through its own handle shows `windows: 0`, and read by name after its handle
closes shows `windows: 32`, with the change time unmoved.

## Root cause

`OsLand::set_mode` wrote `FILE_ATTRIBUTE_NORMAL` (or `READONLY`), clearing the `ARCHIVE` NTFS gave the new file. NTFS
set it again when the written handle closed, after the engine had read the placed file's fingerprint through that
handle (`placed`, the next landing's witness). Before A-262 the fingerprint carried no attribute bits, so nothing saw it.

## Impact

Every Windows landing over a file an earlier landing had placed would have been refused as a conflict. Not released: the
writer had not landed.

## Edits

- `crates/land/src/os_windows.rs`: `set_mode` keeps `ARCHIVE` (with `READONLY` when the mode has no write bit), as A-262
  states a new file's bits.
- `crates/land/tests/os_seam_windows.rs`: the test above, red before (`windows` 0 against 32) and green after.

## Sweep

- `create_temp` and `mkdir` create with `FILE_ATTRIBUTE_NORMAL`; NTFS adds `ARCHIVE` to a new file at creation, and the
  landing records no witness of a directory it makes. Not changed.
