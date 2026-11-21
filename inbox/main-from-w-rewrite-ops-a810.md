# main from w-rewrite-ops: the checkpoint failure is fixed (A-810, main d9bc5f00c); A-803's leftovers are not measured gaps (2026-11-21 ~04:30 PST)

## A-810: the change index was right; its model was not
Landed on main as d9bc5f00c, with the GAPS correction below as fd031603d.

It was not the restore. The original green disagreed with the model too, and the restored one inherited it.

The case:
- Origin: `a` is a hard link to `e/f` (empty), and `d/y` is a FIFO.
- Version 1 links `e/f` to `d/y`.
- Version 2 removes `d/y`, and `e/f` becomes the FIFO.

What happened:
- The model (`changed_between_by_histories`, A-264's oracle) asked `was_special(target)` timelessly. It named `a`
  changed in `(0, 1]` because `e/f` is special at version 2.
- `a` reads the same at versions 0 and 1: `base_at(1)` lists `d/y` alone as special, and a link resolves one level.
- The index asks `was_special` as the version commits (`record_changes`), so it was right.
- The model now asks it by the target's last change in the span.

Red, then green:
- Red: the seed, now in `crates/merge/tests/checkpoint.proptest-regressions` (the suite is now `seeded`).
- Named: `a_link_changes_with_its_target_only_once_the_target_is_a_special_node`, against `base_at`'s reads.
- After: checkpoint 0 failures in 25 runs (it failed about one in five), engine 0 in 10, lib 0 in 5.
- Bug record: `docs/bugs/2026-11-21-the-change-index-model-asked-specialness-timelessly.md`.
- Sibling, reported, not changed: `index_restored_changes` asks `was_special` at restore time. That's safe, since a
  restored green's histories end at the checkpoint.

## A-803's leftovers (a) and (b): not measured, so I've stopped per your condition
My GAPS line called them "measured, not worked". That was wrong: both come from A-809's design, and neither was measured.
fd031603d corrects the line and names what each measurement needs:
- (a) An old version's read resolves layers: O(log + layers over each piece). It would need a rebase or old-read time
  against m, the overwrites of a hot range since.
- (b) A path alternating overwrites and inserts flattens at each insert. It would need `--preload` with inserts, which
  the harness doesn't write today.

Say if you want either measured.
