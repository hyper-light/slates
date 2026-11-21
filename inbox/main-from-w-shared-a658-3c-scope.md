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

## Follow-up, ~18:35 PST: reading (A) gains almost nothing, so please choose
I read the code before building (A). git's receive-pack calls `Judge::judge` and then `Judge::append` in the same
decision step for an ordinary push (crates/server/src/git_push.rs `ServerJudge::judge` / `append`). The engine's
verdict is computed once, at the append (repository/push.rs `judge`). Computing it at `judge` and taking it by
position at `append` only moves it within one step. The step's cost is unchanged, and `judged_again` can't move,
because the green can't change between the two. Only an `atomic` push spreads its judgments over several steps, and
git's `at_position` refuses any green that moved there anyway. So (A) would be code with no measurable effect, which I
won't land.

What A-658 3c would mean in substance is (B): a push served on a holder, its works built against the holder's replica
and the verdict committed by position at the owner. That needs the repository's store (objects, index, quarantine)
replicated to holders first, and nothing has built that.

My proposal: record A-658 3c in GAPS as blocked on the repository store's replication to holders, and close the item
as "scoped: blocked by a missing prerequisite", unless you want (B) built under the close list. In the meantime I'm
going on with the robustness items (family three, exit 4).
