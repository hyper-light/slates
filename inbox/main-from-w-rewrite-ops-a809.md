# main from w-rewrite-ops: piece 3 landed, A-803's fix (A-809, main 5eeb3c481); and a checkpoint seed failing on main (2026-11-21 ~04:00 PST)

## A-809: a length-preserving change walks one value
Landed as piece 2f35ad7d8, merged on main as 5eeb3c481 (merge-base confirmed).

### What it does
Option 1, built as a layered anchor:
- An overwrite inside a file copies the bytes it overwrote as the anchor's next numbered layer, indexed by offset.
- The anchor names the new block, the old one is let go, and the superseded latest value reads the anchor at that
  layer's epoch.
- No other value is walked: O(log + the bytes overwritten).
- An insert, delete, truncate or extend first resolves the layers, then re-anchors in full as before. So does a change
  that keeps nothing or changes nothing.
- A layer below the least epoch any piece reads is dropped, so folds free them.

### One deviation from option 1 as written
A per-value interval index alone would still touch about every earlier value on a random overwrite, because every
earlier value has an anchor piece over the overwritten range. So the change stays O(k). The layers move that cost into
the anchor, where it is O(log).

### Tests
- Red: `a_length_preserving_change_walks_one_value_however_long_its_history`. The 300th overwrite walked 300 values; it
  now walks 1.
- Oracle: `the_layered_anchor_reads_as_the_full_reanchor_through_folds_and_checkpoints`. Generated overwrites,
  inserts, deletes, folds and checkpoints run against a twin taking only today's re-anchor (`FULL_REANCHOR_ONLY`).
  Every version reads equal to it and to the model, and the totals stay balanced.
- Also: `slates-merge --lib` 253 of 253; the engine, checkpoint and chain_replay targets; the server filters svn, hg,
  merge, submit, green and history.

### A/B
Release `load_bench --preload`, 1,000 clients over kernel/, M3 Ultra, three interleaved rounds, all shown (BENCHMARKS,
"A length-preserving change walks one value"):

| preload | before judge µs | after judge µs | before values a verdict | after values a verdict |
|---|---|---|---|---|
| 0 | 22.9, 22.3, 22.2 | 21.5, 20.1, 20.2 | 7.8 | 1.6 |
| 10^4 | 28.3, 27.5, 27.5 | 22.1, 21.7, 20.7 | 29.9–30.2 | 1.6 |
| 10^5 | 66.4, 64.7, 65.2 | 22.3, 23.1, 22.1 | 227.4–227.9 | 1.6–1.7 |

- Verdicts/s at 10^5: 4,851–5,178 before, 5,684–5,961 after.
- Oracle failures 0 in all 18 runs.

### Left (GAPS A-803)
- (a) A read of an earlier value resolves its anchor's layers.
- (b) A path alternating overwrites and inserts flattens at each insert.

## Found, not mine, not worked: `crates/merge/tests/checkpoint.rs` fails on origin/main under one seed
`a_green_restored_from_its_checkpoint_answers_as_the_green_it_was` fails about one run in five or ten, with
`changed_between(0, 1): ["e/f"], the model ["a", "e/f"]`.

Evidence:
- Without the seed, origin/main passed 10 of 10 runs.
- With the seed below added to a scratch seed file, pure origin/main (exported with `git archive`, no A-809) fails the
  same way.

Seed line:
```
cc 59538896987fc02990742a07d68e2acef5218ae1f88be939df06c058ec966005 # shrinks to origin_rows = [(53, 0, 48, 0, 0), (90, 46, 0, 0, 0)], chain = [(114, 76, 18, 0, 0), (235, 165, 0, 0, 0)], extra = (0, 0
```
It's the change index after a restore, not the history values.

Run requests for your queue: clippy on slates-merge, and the merge crate's full test suite.
