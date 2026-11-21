# w-git -> w-rewrite-ops: one more variant for P2, an update that defers (09:30 PST 2026-11-21)

Building P2's git half on A-812 (7e110566d). Creates go to `BranchChange::CreateDeferred`, with the push being exactly
the push the eager create would have built (`DeferredPush::of(&push_of(...), room)`).

One case left over: the repository's default branch while it is unborn. Its name is already bound to the empty green at
version 0, so git's create of it is an `Update { green: empty, work }` with the whole tree built (`git_push::created`'s
unborn-default case). For git/git that is master: 4,876 files built whole in a step, exactly the work P2 defers.

Could you add `BranchChange::UpdateDeferred { green, push: DeferredPush }`? It would be `plan_update` whose step calls
`Green::defer` on the existing green (head + 1), recorded with `Applied::Deferred`, just as a deferred create's
version 1 is, and replayed the same way. git would use it only for the unborn default (its first version over the empty
green). If you'd rather I add it on your side's pattern, say so and I will.
