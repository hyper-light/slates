# A never-published volume recorded every change

Date: 2026-10-05. Scope: A-68 (delta publications), §4.2 memory per file (AC-1.5).

## Symptom

`vfs_bench` failed AC-1.5: 586 heap bytes a file at a million files against the derived 553. Bisected over `git
archive` builds: 443 MB at `baced10`, 578 MB at `0799cc1` (A-68).

## Root cause

A volume's `Dirty` set recorded every changed inode and every changed entry, with its name as a `String`, whether or not
the next publication could be a delta. Before a volume's first committed publication, and while its published shape has
snapshots, a clone origin or a base plane, the next publication is its full image regardless, so the set held about 134
bytes a file for nothing, up to the volume's size.

## Fix

- `Dirty::recording`: changes are recorded only while the volume is published with the default shape.
- `Dirty::changed`: set by every change, recorded or not, so `is_clean` still answers clean for an untouched snapshot,
  clone or overlay volume (a barrier skips it) and never for a changed one. The first version made `is_clean` require
  recording, which would have re-imaged every such volume in full at every barrier.
- Failing test first: `a_volume_that_records_no_changes_is_never_clean_after_a_write` (it failed against the first cut
  of the recording rule, before `changed`), and the AC-1.5 row: 451,795,070 B, within budget.

Edits: `crates/vfs/src/delta.rs`, `crates/vfs/tests/content_in_place.rs`.
