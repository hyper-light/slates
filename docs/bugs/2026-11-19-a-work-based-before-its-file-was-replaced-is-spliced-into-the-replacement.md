# A work based before its file was replaced is spliced into the replacement (2026-11-19, found by A-596's oracle; open; g267)

## Description

A work edits a file it saw at its base. After that base the green replaces the file: another work unlinks it and makes
it again, or renames another file onto it. The work is then submitted. The verdict accepts it, and its edit lands at the
end of the new file, which the work never saw. The edit is spliced into the new file at an offset shifted past the new
bytes. Reproduced by `a_work_based_before_its_file_was_replaced_conflicts` (`crates/merge/src/engine/reanchor/tests.rs`,
parked with `#[ignore]`). On the Mac Studio (kiln r504 and r529, 2026-11-19) the file `<1>\n<2>\n` was unlinked and made
again as `<9>\n`. A work based before both inserted `x` at offset 1 and was answered `Accepted { version: 3 }`, and the
file became `<9>\nx`.

## Root cause

`Green::merge_content` judges a work's content ops on a path by whether the path holds content at the head, and maps
the ops through the path's deltas since the work's base (`Green::intervening`). It never asks whether the file at the
head is the file the work edited.

A replacement's delta does not say that the old bytes left. `merge_create` and `merge_rename` both record the new file
as one insert of its whole length at offset 0 (`insert_whole`). By the edge rule, an insert at offset 0 shifts every
range at or after 0 (`crate::map::map_through_one`). So each of the work's ops moves past the new bytes and is spliced
at or past the new file's end (`apply` copies what precedes an op's offset, clamped, and then adds its bytes).

`fits_base` judged the ops against the file's length at the base, so nothing judges them against the head's file.

## Impact

An edit to a file that another work replaced is accepted rather than answered as a conflict. Bytes the work wrote for
one file stand in another, and no client is told. Every holder recomputes the same verdict from the same record, so
nothing detects it. Any work based before a re-creation or a rename onto its path is exposed: an agent's work through
the SDK or a mount, and a server-side merge of a review request.

A review comment is not misled: its step refuses a delta that does not account for the file's change of length
(`slates_merge::review::move_through`, `FileReplaced`).

## Fix owed (unclaimed)

- The verdict answers a conflict on the path (`DeleteModify`, as a work editing a path the green removed already is)
  when the path's content history records a removal after the work's base, or a value not made by an edit of the one
  before: a create after a removal, or a rename's destination.
- The replacement's delta states the old bytes' removal (a delete of the old length, then the insert), so the position
  mapping of works and of review comments meets it.
- Then un-ignore the test, and let A-596's oracle base works before their file's last replacement (`History::made` in
  `crates/merge/src/engine/reanchor/tests.rs`).
