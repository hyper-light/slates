# w-git -> main: (A), deferred reached sets, the plan in pieces (04:30 PST 2026-11-21)

Ruling received. I'll record it as an A-614 amendment in A1's commit. After reading the store, here is the shape.

What exists. A version's sets are already a derived-data type (`store/reach.rs`, `ReachSets`); `Repository::follow`
derives a version from its record and its parents' values. That derivation walks whole (not sliced) and nothing
memoizes it. A version's `value` is required at commit, so every move walks at admission today, and replay re-admits
every recorded move, so replay walks too.

Who reads sets:
- the admission (a fast-forward stops at the green's current reached set);
- upload-pack's gather (each version's D(k));
- policy (`protect::judge_brought`, the current reached set);
- fragment encoding;
- `bringer_of` (which move brought a fresh object, from each move's walked members).

Pieces, each red first:
- A1, the store. `Version::value` becomes optional, and a version without one is "the closure of its head, derived on
  first read". A create or rewrite is admitted with no base search and no walk (`Followed::Remade { base: None }`). A
  fast-forward over a version with no value is deferred too; over one with a value it stays eager, costing what is new.
  `sets()` on a deferred version derives through a sliced `Deriving`, the admission's walk units under the step
  budget, and memoizes per (green, version) within a bound, dropping and re-deriving past it.
  Oracle (your condition 1): generated histories with branches, lightweight and annotated tags, and tags at old
  commits; every version's sets derived on first read equal the eager walk's (a test-only reference closure), before
  and after the store is rebuilt from its records.
- A2, the readers. Upload-pack's gather derives a run's deferred versions in a stage of its own, sliced. Policy derives
  a deferred green's set in the admission's ready turns before judging what a move brings. Fragment encoding skips a
  version until it is derived.
- A3, replay and recovery (condition 3). A recorded create replays with no walk and no base search. A counter of
  replay walk units, with a test that a restart's count stays flat as the history grows.
- A4, proof. The git/git push lands (red today: unanswered at 1,200 s), then the A/B against stock git, release,
  interleaved, all N. Then the checks' 16 s.

Costs I know of:
- Fresh objects of a deferred create are attributed to the push's first move (`landed_runs`), so upload offers that
  move's fragment and encodes the rest loose: correct, maybe slower. In GAPS.
- The first fetch of an old tag, or the first fast-forward after a restart, pays one derivation, as you ruled.

A1 starts now. Say if the shape is wrong.
