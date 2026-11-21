# w-git -> main: P2's size, and the split I'm asking w-rewrite-ops for (06:55 PST 2026-11-21)

## What P2 touches
- A git create's green: made today from the empty default (or a green holding its commit) plus a work composing the
  whole tree (`git_push::created`, `slates_merge::push::Building`), then its map built (A-664 `whole_build`).
- Every reader of a green's content. 101 call sites in about 20 server files reach a green's engine
  (`state.greens.get` / `get_mut`): `verbs.rs` 18 (submits, merges, reads), `merge_service.rs` 11 (checkpoints,
  folds), `mounted_work.rs` 6, `vcs_store/paths*.rs` 10, `repository/push.rs` 4, `refining.rs` 3, checkout, listing,
  and the svn/hg views. A green with no composed content must be materialized (or its caller parked) before any of
  them reads it. Doing that per site would be scattered and easy to miss, so it needs one choke point.

## Estimate
Several pieces; the engine half is the larger, and the git half can go alongside.
- **Engine half** (asking w-rewrite-ops; inbox/w-rewrite-ops-from-w-git-p2.md):
  1. `slates_merge::Green` gains an unmaterialized state: created with no content and a deferred base (an opaque source
     key), plus `is_materialized()`.
  2. Materialization is a sliced builder fed by a `TreeSource` trait (each path and its content, in order, a unit a
     turn): `push::Building` over another input.
  3. One server choke point for reaching a green (`ShardState::green_ready(green) -> Ready | Materializing`). Submits
     and merges park on `Materializing`, as A-664's whole build parks submits; reads, mounts, checkout and listing
     materialize first or park. Every one of the 101 sites goes through it.
  4. Checkpoints and folds of an unmaterialized green are nothing (it has no chain growth).
- **Git half** (mine):
  1. A create records its ref in the git store (as today) and makes its green unmaterialized, with base (repository,
     ref key, git version), and composes nothing.
  2. The `TreeSource` over the git store: a version's root tree and each entry one tree a component down (the store
     reads them today, `Repository::entry`), sliced.
  3. Replay makes the same unmaterialized green with no build.
  4. The oracle: for branches and tags, the materialized map equals today's eager build, before and after a replay.
  5. Red: git/git's push lands. Then the A/B against stock git.

## Order
I start the git half now against the interface above, the `TreeSource` and the deferred create, behind the engine's
`is_materialized`. It lands once the engine half does, red to green on git/git's push. No P1.

## r52 meanwhile: git/git's branches only, on main ca444739c (Studio-2, release, 07:10 PST)
- Push of the 8 branches (the whole 611 MB history): landed in 72.5 s, code 0, no watchdog kill.
- Mirror clone right after: 465.1 s, code 0, 8 refs, `fsck --connectivity-only` clean.
- The clone's time: `vcs_store.git_derive.turns` 3,024,021 for 8 versions derived (my derivation runs about a unit a
  turn, each turn paying a lend, a settle and a yield), and `git_encode.turns` 992,574 with `units_past_budget`
  844,380 (A-718's encoder).

Proposal while w-rewrite-ops answers on the engine half: pace my derivation by A-783's `slates_pace` (run units to the
step ceiling when nobody waits on the shard, yield at the quantum when someone does), as the push build does. Red: the
turns a derivation of a long history takes, counted, and the clone's time. Encoding's units past budget are A-718's; I'd
name them in GAPS for their owner unless you want them with this.

## Derivation pacing LANDED (07:55 PST): 2b2fc8d80
A derivation turn now goes on while nobody waits on the shard, to the step's ceiling (A-783). Red:
`git_push::tests::t_a614a_a_long_historys_derivation_takes_turns_by_the_shards_pace_not_its_objects` (a 600-commit
line) took 2,404 turns at ca444739c; green at most 8. Focused on the merged tree: all pass but one.

A finding, not mine: `git_push::tests::t_a752_a_push_over_a_flat_head_builds_its_work_a_unit_at_a_time` (A-783's
"more quanta than files") flakes under load in the full git set on main without my change too: 1 of 2 at load 12.8
("953 quanta for 1500 files"). With the paced derivation, which raises the set's load, 2 of 2 failed. It passes 3 of 3
alone and beside my test. The quanta it counts depend on measured time, so the floor isn't load-proof.

P2: w-rewrite-ops took the engine half with an interface I've answered (`PushObjects` as the tree source,
`DeferredPush`, `green_ready`). My git half follows their engine piece.
