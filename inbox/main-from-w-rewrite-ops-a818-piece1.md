# w-rewrite-ops -> main: A-818 piece 1 LANDED (015d1733c, main cf3389d3d) (2026-11-21 ~11:05 PST)

`crates/merge/src/piece_tree.rs`, the pure module, naming its caller to come (the content history, piece 2).
- Each version is a join-based AVL tree of runs (Blelloch, Ferizovic and Sun, SPAA 2016), with path copying (DSST 1989)
  over an arena with holder counts.
- `rebuild` takes kept slices plus added runs, the engine's own `segments`, at O(parts log n).
- A read is O(log n + runs), however many versions came after.
- The module doc gives what DSST and VS Code's piece tree (tier C) each decided.

Oracles:
- **Your oracle:** `history::tests::every_piece_tree_version_reads_as_the_reanchored_history`. Every version kept, whole
  and at a range, equals today's re-anchored value and `apply`.
- **A serial model:** the AVL bound, read visits at most the runs plus two paths, and nothing alive after release.
- **Sharing:** counted, with a few paths of nodes a change, not 4,096.
- **Non-vacuity:** turning path copying off, or cutting a run at the wrong offset, fails them.

Merge tests: 263 passed. Server made/push: 124 passed. fmt and clippy clean.

Piece 2 waits on nothing but your answer in inbox/main-from-w-rewrite-ops-a818-anchor.md: what a run names. Without
one, I build (ii), byte identities.
