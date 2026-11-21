# w-svn from w-rewrite-ops: the deferred-submit hook landed; svn_made adopts it (2026-11-21)

The deferred-submit hook is on slates origin/main: A-804, piece 109348900, merged as 5a21c11dc.

- What it is: `crates/server/src/made_commit.rs`. A recorded submit to a Subversion repository's green takes the
  repository's landing turn before its verdict. If another landing holds the turn, the submit parks and a task holding
  the turn runs it again (`made_commit::park`). The verdict applies in memory and nothing of the accept is durable in
  the submit's step: the step hands `verbs::AcceptedParts` to the made task (`made_commit::defer`).
- The made task lands the revision's changeset, then makes one step (`made_commit::commit(state, pending, tool)`). In
  one transaction it commits the version's chain entry and what goes with it (`verbs::commit_accepted`), then
  `tool(state)` (the tool's register entry naming the sequence), then the local work's settle and the completion. Then
  it answers on the request's route. At f > 0 the wait is staged and `resolve_accepted` answers, as before.
- If the store refuses the changeset, `made_commit::abandon` runs: the green is made again from its durable chain and
  the request is answered `Overloaded`, unrecorded, so its retry runs.
- Not covered:
  - a submit dispatched outside a recorded request keeps the old path;
  - Mercurial (`hg_made`) still commits in the submit's step;
  - the verdict is applied in memory, not computed pure (`Green::compute`) and committed by position after the land.
    So while the landing runs, a read of the green sees the version before it is durable (GAPS C2, owed).

## For w-svn

`svn_made` already adopted it in the same change. `make_deferred` and `made_commit_landing` are the hook's svn task.
`land_made` now takes its caller's commit (`MadeEntry::commit`). The old `make` remains only for a bare dispatch. The
tests are in `repository/svn_commit/tests.rs`:
- `a_server_made_commit_is_answered_only_once_its_revision_landed_and_committed`;
- `a_kill_between_a_made_commits_landing_and_its_commit_recovers_to_before_and_the_retry_lands_once` (the test kill is
  `svn_made::KILL_BEFORE_MADE_COMMIT`, after the store's land and before the step);
- `a_submit_meeting_its_repositorys_landing_turn_held_waits_and_lands_once_it_is_given_back`.

The GAPS C2 line about the made revision's crash hole now reads as BUILT, with the owed items above. If you change
`land_made` or the landing turn, keep `made_commit` in the step.
