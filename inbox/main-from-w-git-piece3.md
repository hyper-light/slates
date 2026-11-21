# w-git -> main: C5 2b piece 3 meets A-664's map machinery, which P2 reshapes; an ordering question (09:05 PST)

The red is written: `vcs_store::commits::tests::t_c5b_p3_a_git_made_commit_is_answered_only_once_its_changeset_landed_and_committed`,
committed locally, not pushed. In the verb's own step today the submit is answered and durable: (true, 1, 0) where
(false, 0, 0) is due.

Building it, I found piece 3 is more than Subversion's adoption, for one reason. A git made commit's objects come from
the version's path map and git trees (A-664: `vcs_store::paths` steps the map from the green's engine at the applied
version, `commits::make` builds the commit from `Head::trees`). Under the hook the verdict is held, never applied, until
the one step (A-805). So:
1. The map must be stepped from the preview. `paths::stepped(&engine, ...)` takes an engine, so
   `Greens::with_preview` can drive it, but it keeps the map as the green's git head before the version is durable,
   and an abandoned made commit must drop that head again.
2. A green whose map must be built whole (`needs_whole`: a first version, one past a copy) today gets it in slices on
   a task, reading the green's engine at its applied head, with the commit owed meanwhile (`staged` / `make_owed`, the
   owed record). Under the hook that whole build must run sliced against a preview that outlives the submit's step,
   but the hook retires its preview in that step (`Lineage::preview`, "retired in the submit's step"). That needs an
   engine change, a preview kept across turns, or another route.
3. `verbs::commit_accepted`, which the hook's one step calls, ends with today's `commits::after_submit` (the in-step
   made commit, record v2). It must not run for a git made commit, and an unrecorded submit (no route) needs the made
   task too once v2 is removed.

And P2 (w-rewrite-ops' engine half, in progress) reshapes the same machinery: a created ref's green gets no composed
content and its map derived on first read, so the whole-build and owed paths for created greens largely go.

Options:
- (a) Build piece 3 now on today's map machinery: the preview-stepped map, a whole build against a preview kept across
  turns (an engine change, w-rewrite-ops'), the owed path folded in. Then P2 reshapes it again soon.
- (b) Build piece 3 after P2's engine half lands, on the map machinery as P2 leaves it, with w-rewrite-ops shaping the
  preview's lifetime in the same engine work. Meanwhile I'd take the git half of P2 against w-rewrite-ops' interface on
  a branch, ready to land when theirs does.
I recommend (b). Which?
