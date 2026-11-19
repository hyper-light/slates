# A work based before its file was replaced was spliced into the replacement (2026-11-19, found by A-596's oracle; fixed by A-612; g267)

## Description

A work edits a file it saw at its base. After that base the green replaces the file: another work unlinks it and makes
it again, renames another file onto it, or replaces it beneath a directory's rename (the file moved out, another moved
onto its path). The work is then submitted. The verdict accepted it, and its edit landed at the end of the new file,
which the work never saw.

A rename of the file was the same: a work renaming the file it saw, submitted after the file at that path was replaced,
moved the replacement to its destination.

Reproduced on the Mac Studio (kiln r504, r529 and r613, 2026-11-19):
- the file `<1>\n<2>\n` was unlinked and made again as `<9>\n`. A work based before both inserted `x` at offset 1 and
  was answered `Accepted { version: 3 }`, and the file became `<9>\nx`;
- the rename of the re-made file, based before its replacement, was `Accepted { version: 3 }` (r613's red run).

## Root cause

The verdict judged a work's content ops on a path by whether the path held content at the head (`merge_content`). It
then mapped the ops through the path's deltas since the work's base (`Green::intervening`). It never asked whether the
file at the head was the file the work edited. `merge_rename` likewise checked only that its source still named
something.

A replacement's delta does not say that the old bytes left. `merge_create` and `merge_rename` record the new file as one
insert of its whole length at offset 0 (`insert_whole`), and by the edge rule an insert at offset 0 shifts every range
(`crate::map::map_through_one`). So each op of the work moved past the new bytes and was spliced at or past the new
file's end. `fits_base` judged the ops against the file's length at the base only.

A rename rotation in one increment (the replacement beneath a directory's rename) also records a removal and a set at
one version, and `intervening` read that version's delta once for each of the two entries.

## Impact

An edit to a file that another work replaced was accepted, not answered as a conflict, against D-27's verdict (accept,
identical or conflict, never an inferred merge). Bytes the work wrote for one file stood in another, and no client was
told. Holders recompute the same verdict, so nothing detected it. Exposed: any work based before a re-creation, a rename
onto its path or a directory's rename over it, whether an agent's work through the SDK or a mount, or a server-side
merge of a review request.

## Edits (A-612, `crates/merge/src/engine.rs`)

- `Green::file_removed`, new: the last version that removed the file at each path. `commit` records it for an unlink or
  a rename away (each `RemoveContent`), and for a rename onto a path that held a file (a `SetContent` whose source is
  `ContentSource::Copied`, when the path held content before the set). Replay rebuilds it, since `apply_recorded` makes
  the same effects. It is never folded, so a base below the fold floor meets it too. It is empty in a green restored
  from a checkpoint or made as a branch, whose bases are never before its first version.
- `merge_content`: after the existing absent-path checks, a path whose file was removed after the work's base
  (`file_removed_since`) answers `DeleteModify`, §4.16's "delete against modify in either direction".
- `merge_rename`: a source whose file was removed after the work's base answers `RenameRename`, as a source that
  vanished does.
- The double read in `intervening` is no longer reachable from either caller: both refuse a path removed after the
  base before they map through its deltas.

## Tests

`crates/merge/src/engine/unseen_bytes_tests.rs`:
- `a_work_based_before_its_file_was_replaced_conflicts`: made again, renamed onto, and replaced beneath a directory's
  rename; `DeleteModify` each time, the head unmoved, and the same edit based at the head accepted;
- `a_rename_based_before_its_source_was_replaced_conflicts`: `RenameRename` each time.

A-596's oracle bases works before replacements again. Red on r613 with the fix reversed, green with it:
`cargo test -p slates-merge --lib`, 43 of 43.
