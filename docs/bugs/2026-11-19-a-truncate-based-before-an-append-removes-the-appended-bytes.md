# A truncate based before an append removed the appended bytes (2026-11-19, found by A-596's oracle; fixed by A-612; g267)

## Description

A work removes a file's tail to its end, which the deriver composes as a truncate (`compose_content`: "a removal that
runs to the base end is a Truncate"). After the work's base, another work appends at that end. The truncating work was
then submitted, accepted, and the appended bytes were gone: a lost update.

The same happened in a rebase of such a work, whose rebased file and journal dropped the append, and in a work that
renamed the file and truncated it after the rename (A-547's edits after a rename).

Reproduced on the Mac Studio (kiln r529 and r613, 2026-11-19): the file `<1>\n<2>\n` had `<3>\n` appended. A work based
before the append truncated it at 4 bytes and was answered `Accepted { version: 2 }`, and the file became `<1>\n`.

A-596's oracle found it in kiln r504. A comment on an untouched first line was refused at the truncate's version
(`FileReplaced`), while a diff of the two texts kept the line. The version's delta stated 4 bytes leaving where 8 left.

## Root cause

A truncate in a net op set is a removal of the base bytes it covers, `[at, at + len)`, which reaches the base's end.
The interval algebra composes a removal as a hunk over base bytes (`slates_vfs::algebra`), and §4.16 judges it by its
span with the edge rule. An append at the base's end is a zero-length insert at that span's end, so it is disjoint, and
the two commute.

The verdict mapped the truncate unchanged to the head. The splice then applied a truncate as "cut to the end of the
file" (`apply`: `OpKind::Truncate => pos = base_len`), with `base_len` the head's length, which grew by the append. So
the cut removed the append too, and the recorded delta (the truncate's own length) misstated what left. `rebase`
restates the same mapped op as a truncate to a new length (`content_volume_ops`), and `edits_after_rename` mapped a
rename's edits the same way.

## Impact

An append was lost to a work that never saw it, against D-27's verdict: last-writer-wins is a lost design, and a
disjoint change must survive. Nothing was refused, and holders recomputed the same result. A position mapping through
the version's delta misplaced whatever followed the cut; review comments refused to follow it (`FileReplaced`).

## Edits (A-612, `crates/merge/src/engine.rs`)

- `at_head(op, head_len)`, new: a truncate mapped to a head that grew past its span (`at + len < head_len`) is the
  delete of its span, which is the head's canonical form for a removal that does not reach the end. Every other op, and
  a truncate that still reaches the end, is unchanged.
- `merge_content` applies it to each mapped op with the head's length, so `apply`, the version's delta, the chain
  entry's `Spliced` ops, the history's spliced value and a rebase's restated journal all remove exactly the span.
- `edits_after_rename` applies it with the source's current length, so the bytes a rename moves keep the source's
  append.
- No other path needed it. Against the green's own truncate, a work's append at the base's end already maps to the cut
  and is kept. An overwrite ending at the cut is disjoint and kept. An insert at the cut shifts the truncate past it. An
  overwrite inside the cut span, or a shorter or the same truncate, overlaps and is decided as before (the sweep test
  below).

## Tests

`crates/merge/src/engine/unseen_bytes_tests.rs`:
- `a_truncate_based_before_an_append_keeps_the_appended_bytes`: the file `<1>\n<3>\n`, and comments on both lines
  followed through the version's delta, exact;
- `a_rebased_truncate_keeps_the_appended_bytes`: the journal is a delete of the line the work saw;
- `a_truncate_after_a_rename_keeps_what_the_green_appended_to_the_source`;
- `a_truncate_and_a_change_at_its_cut_are_judged_by_their_spans`: the sweep, both directions.

A-596's oracle bases truncating works behind the head again. Red on r613 with the fix reversed, green with it:
`cargo test -p slates-merge --lib`, 43 of 43.
