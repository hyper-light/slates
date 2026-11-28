# An import's directory removals were judged again over a half-made count

- **Date:** found 2026-11-27 by main's git-2000 release rounds at 604aa12a0; diagnosed and fixed by w-svn the same
  day.
- **Status:** fixed.
- **Subsystem:** the merge judge's directory removals (`crates/merge/src/engine/deciding.rs`,
  `Green::decide_rmdirs_some`), stepped since A-920's ee0c95e8f.
- **Severity:** correctness. A Subversion import was refused `ContentUnavailable` at a revision that removes
  directories with everything under them: 4 of 5 git-2000 imports, at r593. Any work judged in steps (an import's
  batch, a made commit) could be refused, or could loop until the step clock happened to fall elsewhere.

## Symptom

```
slates-server: partition 0: an imported revision's version was not staged:
  NotAccepted(Conflict { windows: [ConflictWindow { path: "trunk/contrib/emacs", range: Range { start: 0, len: 0 },
  class: DeleteModify }] })
slates: refused: ContentUnavailable
```

git-2000's r593 removes 62 paths. Among them are each file under several directories, then the directories. Stock
svnadmin 1.15 loads it. The refusal was intermittent: one import of the same dump on the same binary succeeded a
minute later.

## Root cause

A directory's removal counts, over steps, the paths the work removes itself under it (`RmdirWalk`, A-920). The phase
kept its place (`Deciding::after`) only when a step was spent right after a directory was decided.

Take a step that decides directory D1, is not spent, and is then spent partway through counting D2's children. It left
`after` before D1. The next step decided D1 again on top of D2's half-made `RmdirWalk`. So it skipped D1's own check,
counted from D2's last child (none under D1), and held D1's keys against D2's partial counts. The result was a second
unit for D1 and a `DeleteModify` window on a directory the work removes whole. Where the step clock fell decided whether
it happened, which is the intermittency. In a test whose clock is spent every fourth check, the judge never ended.

The log that found it (`ImportBatch::refusal_logged`) showed the batch's base at the member's head, every path's last
change at 0, the member holding exactly what the work removes, the maps' counts matching the walk, and one window on
one directory pushed 140 and 2,345 times.

## Exact edits

- `Green::decide_rmdirs_some`: each decided directory's place is kept whether or not the step is spent there.
- `ImportBatch::refusal_logged` (import_batch.rs): a revision the batch's scratch member refuses is logged with the
  batch's base, head and staged count, the member's head, the base the work was judged at, and for each window what the
  member holds there (walked), its maps' counts, each path's last change, and the work's operations there.
- `Green::counted_under` and `Green::last_changed`: what that log reads.

## Verification

- **Red, then green:**
  `engine::applying_tests::a_revision_removing_a_directorys_files_then_the_directory_is_accepted_however_it_is_judged`
  (`slates-merge --lib`). It uses r593's shape (two directories of three files, removed with 54 other files) and judges
  it whole, then with its steps spent every 1st to 12th check. Before: "a judge spent every 4 checks took more than
  100000 steps". After: every judgment is accepted.
- `slates-merge --lib` whole; git-2000's import end to end in debug through `an_svn_writing_session_measured`, which
  was refused at r593 in 3 runs of 3 before the fix (logs 65, 67, 68 in ~/.local/state/w-svn/logs).
