# A replay's choice of named runs was cubic in a push's files (2026-11-24, A-887, A-760)

Found by main 01a20182's r854 on the big Studio at 8f9e38262: `hg_push_restart` over Mercurial's own history (57,054
changesets, dev opt-level 2) was ended by the watchdog a minute in: "shard 0 is spinning, 1138 ms of CPU without a step
against its 1000 ms window, in task spawned at crates/server/src/daemon.rs:1190, at its poll 13". That task is the
shard's start (`start_shard`), so the step was the restart's recovery.

## Description

A-887 (7e356fa8b) gave A-760's replay a rule: a replay reads no named run its apply names whole
(`Increment::replay_reads`), and recovery plans the named bases a Mercurial green's replay reads
(`hg_named::named_bases`, through `Increment::named_whole_files`). Both rest on `named_wholes`, which found each change's
run by scanning every run, and `replay_reads` then asked, for each run, for each change, whether that change and run
were a pair among all the pairs found: three nested scans. A push of a history's tree is one chain entry naming thousands
of files, and recovery makes the choice several times for it (the order's plan, the replay's read, `recorded_unlent`,
`apply_recorded`), each in one step.

## Root cause, measured

The choice for a push entry of n files, each whole of a run of its own (debug, the MacBook): 10.1 s at n = 1,000, 480 s
at n = 4,000, about n cubed. The red, `engine::named_tests::a_replays_choice_of_runs_is_linear_in_a_pushs_files`, took
94,685,203,416 ns at n = 2,000 against the shard's liveness window of 1 s.

## Impact

Since 7e356fa8b, a restart over a Mercurial push of more than a few hundred files spent seconds to hours of CPU in one
step of its recovery, and the watchdog ended the daemon. No data was lost: the restart never completed. Live pushes
were unaffected by the choice (a push's apply names its runs through `sole_runs`, which was linear); they met the same
shape in two dedupes of A-887's, quadratic: the push builder's `built` keeping the named objects its runs name, and
`hg_push::built_works` keeping each named revision once.

## Edits

- `named_wholes` finds a whole operation's run by halving the runs' ascending spans; `replay_reads` marks each run any
  operation of another change or a document payload takes a byte of, in one pass over the entry's operations
  (`mark_runs_read`): n log n.
- `slates_merge::push`'s `built` keeps the named objects by a set of the runs' identities; `built_works` sorts and dedups
  the named revisions once; the recovery planner keeps a set of the bases planned.
- `hg_push_restart` prints the push's line before the restart, so a run the restart ends still says where the push got.

## Sweep

Every loop A-887 added over a push's runs or files: `whole_named_runs` (ops, a map by path), `name_runs` (sorted),
`sole_runs` and `named_files` (linear), `bring_back_named` (one pass), `named_objects::bring_back_reads`'s grouping (per
parked call, its reads few). No other nested scan over a push's files remains.
