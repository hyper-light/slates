# main <- w-fed-move: your identical-op drop ruling, measured; the real mechanism fixed (A-817, Slates 351c9e2a; commit b7fa8328), 2026-11-21 ~05:00 PST

## The drop: built to your conditions, measured, NOT landed
- Built: `increment::drop_identical`, applied after the engine's rebase, with its oracle on generated histories.
  Outcomes and heads are equal with and without the drop, except git's case, and a deterministic case passes.
- Measured in the 1,000-client stand-in: it fired 0.00, 0.00, 0.01 and 0.03 times per client at 300, 100, 30 and 10
  hot files. The rebases' file asks did not move (14.2 per client at 10).
- Why: the engine's restatement already leaves out an op whose effect became the head's, so there was nothing to drop.
  The deterministic case passed only vacuously, on an empty journal.
- By CLAUDE.md §5 (a dead-even A/B does not land), I did not land it, though you ruled for it. The patch is kept at
  ~/slates-work/w-fed-move-a817-rejected-drop.patch, and the design log records it as measured and rejected. Tell me if
  you want it landed anyway for git-like semantics.

## The real mechanism: fixed
I counted why each file was asked (`merge.work.base_missing.<unentried|unheld|replaced_whole>`). Every ask was for a
path replaced whole: 76.3 paths per client at 10 hot files.
- Cause: `refine::whole_replacement` takes a lone overwrite at offset 0 as a whole-file replacement, whatever the
  file's length. The harness's first slot sits at offset 0.
- Effect: every such work asked for the whole file at each rebase and submit, though the refinement only refines a
  replacement whose base length is the file's.
- Fix: ask only when the work's kept entry names that length, or names none. Nothing the refinement does changes.

## Tests
- Red first: `an_overwrite_at_the_start_of_a_longer_file_asks_no_base_bytes`.
- Passing: `works_off_owner` (18), slates-merge's lib and tests, `green_post_state`, `mounted_work`,
  `mounted_work_cow`, `merge_load`, `work_cow`, `work_edit_cow`, `verdict_pages`, `session_log`.

## A/B
Asks per client at 300, 100, 30 and 10 hot files:
- before: 14.1, 17.6, 27.5, 49.2;
- after: 11.1, 12.9, 19.1, 30.0.
At 10 hot files that is 69.4 before A-808. File asks are now zero at every level, and the oracle is clean in every run.

## What remains
One rebase ask per rebase (18.4 per client at 10 hot files), plus verdicts and first-edit lengths. These grow only
with the rebases themselves.

## Also noted
`verbs::tests::an_unbuilt_works_shard_asks_its_greens_owner_again_by_message_and_installs_it` failed once inside the
full `--lib` run and passed alone. It looks flaky under parallel load; I left it untouched.
