# A truncate based before an append removes the appended bytes (2026-11-19, found by A-596's oracle; open; g267)

## Description

A work removes a file's tail to its end, which the deriver composes as a truncate (`compose_content`: "a removal that
runs to the base end is a Truncate"). After the work's base, another work appends at that end. The truncating work is
then submitted, is accepted, and the appended bytes are gone: a lost update.

Reproduced by `a_truncate_based_before_an_append_keeps_the_appended_bytes` (`crates/merge/src/engine/reanchor/tests.rs`,
parked with `#[ignore]`). On the Mac Studio (kiln r529, 2026-11-19) the file `<1>\n<2>\n` had `<3>\n` appended. A work
based before the append truncated it at 4 bytes and was answered `Accepted { version: 2 }`. The file became `<1>\n`.

A-596's oracle found it in kiln r504, shrunk to five works and a comment. A comment on an untouched first line was
refused at the truncate's version (`FileReplaced`), while a diff of the two texts kept the line.

## Root cause

A truncate's span is the bytes it saw, `[at, base_len)`. An append at `base_len` is a zero-length insert at that span's
end. By the edge rule the two do not overlap (`Range::overlaps`), and `map_through_one` neither shifts nor refuses the
truncate. So the verdict accepts it at its base offset.

The splice then applies a truncate as "to the end of the file" (`apply`: `OpKind::Truncate => pos = base_len`), with
`base_len` the head's length. That removes the appended bytes too. The version's delta still records the truncate's
own length, so it states 4 bytes leaving where 8 left, and a position mapping through it misplaces whatever follows.

## Impact

An append is lost to a work that never saw it, and nothing is refused. Any client whose removal of a file's tail is
based behind another's append is exposed (an editor's truncate-and-rewrite against an agent's append, say). Holders
recompute the same verdict, so nothing detects it.

A review comment is not misled into a wrong position: its step refuses the delta (`FileReplaced`), which is
conservative. Its answer there is outdated, where a diff would have kept the line.

## Fix owed (unclaimed)

- At the head, a truncate removes only the bytes it saw, at their mapped place (a delete of its length), when the file
  grew past its span after its base. Or the verdict answers a change at its end a conflict. The design's rule for "two
  works change the file's end" decides which.
- Its recorded delta states exactly what left.
- Then un-ignore the test, and let A-596's oracle base truncating works behind the head (the `truncates` guard in
  `History::edit`, `crates/merge/src/engine/reanchor/tests.rs`).
