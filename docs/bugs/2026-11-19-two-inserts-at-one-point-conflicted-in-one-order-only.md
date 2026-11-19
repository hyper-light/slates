# Two inserts at one point conflicted in one order only (2026-11-19, found by A-659's restatement; fixed by A-668; g306)

## Description

Two works insert at one point of a file, both based before the other. One of them also overwrites the bytes starting
at that point, or truncates the file from it. The verdict answered them by their order:

- the overwriting (or truncating) work landed first, then the plain insert: the insert was accepted, placed ahead of
  both of the other work's changes;
- the plain insert landed first, then the other: a conflict, two inserts at one point.

D-27 makes two inserts at one point a conflict ("same-position differing inserts", §4.16 "The verdict"): accepting one
ahead of the other chooses an order, which is an inferred merge. So the first order was wrong.

Found while building A-659 (works over works), whose restatement moves a green's changes over a work's operations by
the verdict's own walk and so had to state, for every pair of operations, which one the verdict puts first.

## Root cause

`crate::map::map_through_one` walks a delta's operations, which are sorted by offset and at one offset by kind in the
canonical order (`OpsDoc::canonicalize`: overwrite, extend, truncate, insert, delete). For each operation it asks:
does it meet the range, does it end at or before the range's start (then it shifts the range), or does it start at or
after the range's end (then the walk stops, since every later operation lies further on).

An insert's range is a point, empty, so its end is its start. An overwrite or a truncate starting at the point does not
meet it (the edge rule), does not end before it, and does start at its end, so the walk stopped there. An insert at the
same point, which canonical order puts after the overwrite or the truncate, was never reached. In the other order the
point's own insert is the delta, and the two meet as they should. Inserts in front of a delete at one offset were met,
since canonical order puts the insert first.

## Impact

A lost decision, not lost bytes: both inserts landed, in an order the verdict chose. Every reader of the walk took it:
the verdict for a work's content, the bytes a rename moves (`edits_after_rename`), a rebase, pass two's point, and
A-659's moved changes. A comment's whole lines are never a point, so review records were not touched.

## Edits (A-668)

- `crates/merge/src/map.rs`: `lies_past(at, range)`, new: an operation lies past a range when it starts after the
  range's end, or at its end for a range that is not a point. `map_through_one` stops only there, so a point walks on
  past an overwrite or a truncate starting at it (no shift: they lie after the point) and meets an insert behind them.
- `crates/merge/src/engine/stack/moved.rs`: the restatement's walk takes the same rule. An operation starting at a
  point lies after it (`Meets::Beside`), is not counted among those before it, and the walk goes on.

## Tests

- `crates/merge/src/map/tests.rs`:
  - `a_point_meets_an_insert_at_it_behind_an_operation_starting_there`: the point meets an insert behind an overwrite
    and behind a truncate at it, and one in front of an overwrite; an overwrite alone, or one with an insert past it,
    leaves the point where it was.
  - `two_inserts_at_one_point_conflict_in_both_orders`: an insert against an overwrite-then-insert and against a
    truncate-then-insert at one point, each landed first in turn; the second is a conflict in all four.
- A-659's stack oracle generates the shape (an overwrite or a truncate, then an insert, at one offset), which it had
  avoided.

Windows x86_64 on jupiter (kiln, debug, 2026-11-19; the macOS run owed): red (r1445) with the two files restored to
their state before the fix in a scratch clone, 3 of 4 failing (the map test: the point `Shifted` to 4 where it must
meet the insert; the verdict's both orders; and the stack oracle, which met the pair by itself); green (r1448)
`cargo test -p slates-merge --lib` 166 of 166, `cargo fmt -p slates-merge -- --check` clean.

The merged tree, A-668 at 92038d5aa over main e3234e618 (r81, 17:12 to 17:32 UTC, same box): `cargo test --locked -p
slates-merge --lib` ran 180 tests (main's 178 and the 2 above) and passed 179, both of the above among them;
`cargo fmt -p slates-merge -- --check` clean. The failure was A-626's copy oracle, whose census met no nested copy
whose inner copy replaced a tree (0 of 30 nested pairs) while every history agreed with its model. That oracle bases
every work at the head, so its verdicts map through no delta and never reach the walk this change touched; the same
test alone passed again (r131, meeting 3 of 37). The census zero is the oracle's own chance (reported to main).
