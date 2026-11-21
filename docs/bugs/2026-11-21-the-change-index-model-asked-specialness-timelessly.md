# The change index's model asked whether a link's target was ever special, timelessly (A-810, 2026-11-21)

## Description
`crates/merge/tests/checkpoint.rs`, `a_green_restored_from_its_checkpoint_answers_as_the_green_it_was`, failed about
one run in five to ten on main with `changed_between(0, 1): ["e/f"], the model ["a", "e/f"]`. It was found during
A-809's runs (w-rewrite-ops). The same seed fails on pure origin/main.

## Root cause
The failing case:
- Origin: `a` is a hard link to `e/f`, which holds nothing, and `d/y` is a FIFO.
- Version 1 links `e/f` to `d/y`.
- Version 2 removes `d/y`, so `e/f` becomes the FIFO.

The change index records a link as changed with its target when the target is or was a special node as the version
commits (`Green::record_changes` asks `was_special` then). Its model, `changed_between_by_histories` (the `test-model`
feature, A-264's oracle), asked `was_special(target)` of the green as it is now. That counts the target's specialness at
any version, later ones included. So the model named `a` in `(0, 1]`, because `e/f` became special at version 2. Yet `a`
reads the same at versions 0 and 1: a link resolves one level, and `e/f` was no special node at version 1 (`base_at(1)`
lists `d/y` alone).

**The index was right. It was not the restore:** the original green disagreed with the model too, and the restored green
inherited the difference.

## Impact
Tests only. The product reads the index, which answered as the reads do. The oracle's false red hid nothing, but it made
the checkpoint suite intermittent on main.

## Edits
- `crates/merge/src/engine.rs`: the model counts a link as changed with its target when the target changed in the span
  and was a special node by its last change there (`last_change_in`, `special_by`, both `test-model`).
- `crates/merge/tests/checkpoint.proptest-regressions`: new, holding the seed. The suite is now `seeded`.
- `crates/merge/tests/checkpoint.rs`: `a_link_changes_with_its_target_only_once_the_target_is_a_special_node` pins the
  rule against `base_at`'s reads.

## Evidence
- Before: the seed fails on origin/main, and the suite failed 1 run in 3 locally (main's own 10 runs passed without the
  seed).
- After: the seed passes. Runs since:

| target | runs | failures |
|---|---|---|
| checkpoint | 25 | 0 |
| engine | 10 | 0 |
| slates-merge lib | 5 | 0 |

`changed_since_cost`, which reads the model too, holds only an ignored cost run, so it was not counted.

## Siblings, reported
`Green::index_restored_changes` notes a restored link at its target's last change when `was_special(target)` holds at
restore time. That is the same timeless question, asked at the checkpoint. It agrees with the model in every run above,
since a restored green's histories end at the checkpoint, so no later specialness exists to leak. It is noted here, not
changed.
