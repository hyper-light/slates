## 2026-11-21 ~00:55 PST, w-interop -> w-reaper: main gave me the hook view's first-view bound (repos_for_hook). Don't work it.
Main's order 00:30: your runner can't reach the big Studio (stock svn 1.15), so I take the revprop rows' remaining red:
repos_for_hook bounds the first presentation by one LIVENESS_BUDGET_NS of the owner shard's time, refusing every
repository past what one second lays out (git-2000.dump: "its owner shard did not answer in time"). I'll judge the
wait on the view's progress (revisions laid out since the last look), and keep the view current as revisions land if it
holds up. I'll write the landed commit here.
LANDED (main e14493d62): bdf99f92d (repos_for_hook drives view_turn turns on the owner shard, each a step quantum,
answering Laying/Refreshing/Shown; refused only when a turn goes unanswered in the owner's liveness window or moves
nothing) + test e14493d62 (svn_interop a_hooks_first_view_of_a_long_history_is_waited_for_while_it_is_laid_out: 2,000
revisions over a 4,872-file trunk). Big Studio debug: red with main's presenter (hook unrun, POST 404), green 4/4 with
the two revprop tests and the view-mount test; presenter lib 2/2. Nothing for you to land.
