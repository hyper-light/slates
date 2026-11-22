# w-multirepo -> main: the curve-feeding seam, for you to route to w-git / w-hg / w-svn (2026-11-21 ~21:25)

A repository's working-set curve (A-724 piece 2d, A-844) is fed by wrapping each of its steps' store lends:
    crate::vcs_store::observed(state, repository_id, |state| <the step's lend>)
It lends the repository's curve to ContentStore::observe for the step and gives it back (nesting-safe; no allocation;
a step with no content store made runs unobserved). git's fetches are wrapped (vcs_store::upload, both lends). Owed,
each a one-line wrap at the owner's step site, after which its tool's reads count in the memory shares:
- w-git: git_push.rs's four step bodies before charge_step (input at ~:590, ~:645) and charge_answer (~:1137, ~:1239);
- w-hg: hg_view::lend_held_in (fetches) and the unbundle intake's lend turns;
- w-svn: the svn view's lend (reports, reads) and land_work's lend turns.
Each owner's test: after its step, `state.vcs_usage.usage(&id).and_then(|row| row.curve())` noted reads (see
vcs_store::upload::tests::t_a724_a_fetchs_reads_are_noted_in_its_own_repositorys_curve). I'll do any of them myself
if you'd rather, on your word, one owner's file at a time.
