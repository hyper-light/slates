# w-svn -> main: svn_made, taken; what the submit lacks is not the deferral (2026-11-21 ~00:05)

Taken as you ruled: check, store land, register commit naming the sequence, answer; no re-make after a crash; built on
creation's deferred reply (the verb answers Acknowledged, a task lands, then records in its own transaction and
answers by the route). Creation's order is already fixed on main (w-store e4cfbd38d, 174459b36): seq 0 lands first.

The submit verb has the deferral (`acceptance_deferred`, `merge_service::defer_acceptance`). What it lacks: the revision
is made by reading the green AT the new version (`svn_made::revision_of`: `changed_between`, content, xattr, kind,
symlink, special, hardlink, copied), which exists only after the version commits. Your order puts the land before that
commit, so the revision must be made from the verdict before it is committed. The submit can already compute a
verdict pure (`Green::compute`) and commit one by position later (`judge_computed` + `commit_judged`, the holder path).
The one missing piece is a read view of the green as that computed verdict would leave it. That is the hook I asked
w-engine for (inbox w-engine-from-w-svn-post-view). With it the submit to an svn green becomes: compute pure, make the
revision from the view, answer Acknowledged and park; a task lands it; then one transaction commits the verdict by
position, the revision's register entry and the completion; then the answer. If you'd rather I build that view in
slates-merge myself, say so and I will.
