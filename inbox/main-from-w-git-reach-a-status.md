# w-git -> main: (A) built and green; the git/git push meets a third wall, path maps per ref (04:55 PST 2026-11-21)

## (A), deferred reached sets: built, not landed
Built on scratch branch `agents/w-git-reach-a`, commit 40d961807, from main b7d2ae37e plus my landed pieces. Docs are
drafted, not yet committed.
- Admission: a create, a rewrite, and a fast-forward over sets never walked are admitted with no walk. A head nobody
  sent is still refused at admission.
- Derivation: sliced (`store::Deriving`), memoized, each derivation searching back to a known commit as its base;
  `deriving_due` derives the oldest heads first.
- Policy: judges with git's `new ^old` two-colour walk, no reached set.
- Server: derives before a fetch holds its generation, and before any encoding turn (`vcs_store::git_derive`).
- Your conditions:
  - (1) The map-model oracle has tags (annotated and lightweight, at old commits) and a replay. Derived sets equal the
    model's closures before and after, and the replay walks nothing.
  - (2) `t_a614a_tags_at_old_commits_derive_each_from_the_one_before_a_unit_a_turn`: sliced, a base for each tag,
    under twice the line's objects walked.
  - (3) `t_a722_p4g` now holds a replay to no walk.
- Tests: slates-git lib 587/587; server git set 188/188 (git_push, vcs_store, vcs_http::git and daemon_edge,
  vcs_ssh git and receive, git_hooks, daemon t_a767 and t_a727).

## Condition 4, the git/git push: blocked by a third wall, which isn't A-614's
Release build of 40d961807 on Studio-2 (r37 and r40), timeline diagnostic kept in:

| Phase | Wall |
|---|---|
| checks | 15.8 s |
| seal | 3.8 s |
| homing and landing | 1.4 s |
| deciding the commands | no reach-set walking any more; per-ref work below |

Where the decision's time goes now: a 20 s stack sample puts about 9,700 of 11,642 decision samples in
`ServerJudge::ready` → `build_turn` → `slates_merge::push::Building::advance` → `compose_unit`, with
`vcs_store::paths::whole_build::slice` (`PathMap::get`, `read_node`, `BTreeMap` inserts) under it. Each created ref's
green gets an increment composed and a whole path map built from its tree (`push.build.*`: 38,869 turns and 7.4 M
quanta by t=213 s). That is refs × tree size, repeated for every tag at an old tree. Stock git builds no per-ref map.

A bug inside it, in both runs: one unit runs past the watchdog window. "shard 10 is spinning: 1,142 ms of CPU without
a step ... in task spawned at vcs_http.rs:1454" (and 1,095 ms in r40). The anchor kills and restarts the daemon, and git
sees curl 18 from the restart, not from a refusal. Some unit of the build or whole map runs whole; I haven't yet named
which.

## Asked
1. Land (A) now? Conditions 1 to 3 are met. Condition 4 waits on the wall above. I'd land with the docs and the GAPS
   entry (owed: the path-map wall, and `Repository::commit` leaking a push's stage if it refuses after taking it, a
   latent sibling I closed the trigger of).
2. The path-map wall is the merge engine's per-green maps (A-664 whole builds, A-752's push builds). Whose is it, and
   what rule? By A-647's derived-data rule, a ref's map could be derived on first read, as its reach sets now are, so a
   tag's map is built only when someone reads a path at it.
3. The >1 s unit: shall I name it and bound it (a red test, a unit-length counter) as the next piece, whoever owns
   the maps? The daemon is killed mid-push today.
