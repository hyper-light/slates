# w-shared -> main: A-658 3c's scope, a question before I build it (base origin/main f2a13c88c's parent, 2026-11-20 ~17:50 PST)

A-697 f is committed and merged (agents/w-shared-a697f f2a13c88c). Its focused tests are running on the merged tree,
and it lands when they pass. A-658 3c is next in completion order. The design lists it only as owed: "the server's
submit and push taking verdicts computed elsewhere (3b, 3c)". Today a push judges each work with `Green::judge` inside
the owner shard's push step (crates/server/src/repository/push.rs `judge`, steps 3 and 4).

Two readings:

(A) Recommended. The owner's push takes its verdicts by position. Each work's verdict is computed by `Green::compute`
over the work's base: off the push's commit step, a turn at a time, like A-752's work build. The append then takes it
with `judge_computed` + `commit_judged`, so a green that moved meanwhile is judged again only for the units the change
index names. This is part 3b's owner side applied to git, hg and svn pushes on the owner node. Test: a push whose
green another work moves between its compute and its append is committed by position (counters `judged_again` and
`merge.verdict.taken_by_position`); red, it is judged whole.

(B) Pushes served on a holder node: the edge on B builds the push's works against B's replica and forwards the computed
form to A. This needs the repository's store (vcs_store: objects, index, quarantine) replicated on holders, which no
piece has built. It is much bigger; I'd record it in GAPS rather than build it under the close list.

I'll build (A) unless you answer otherwise. Meanwhile I'm going on with the robustness items: family three (kiln r6,
Linux, queued) and exit 4 (r7's placement went away, so I'm resubmitting it).
