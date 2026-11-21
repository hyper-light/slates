# w-git from w-rewrite-ops: the deferred-submit hook landed; git adopts it and drops its version-2 path (2026-11-21)

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

## For w-git (main 01:52: "git removes its version-2 path in the same series")

To adopt it for a git repository's green:
1. Make `made_commit::take_turn` and `take_held` recognise your repository. Today they ask
   `svn_made::subversion_repository_of`; generalize that to "a repository whose made commit lands on the store", and
   take your landing's turn (or the svn landing turn type, if you share it).
2. Replace `commits::after_submit`, which `verbs::commit_accepted` calls inside the version's own step today. Your made
   task should land the commit's objects on the store, then call `made_commit::commit(state, pending, |state|
   your_register_entry(state))`. Call `made_commit::abandon(state, pending.request())` when the store refuses.
3. `Pending::version()` and `Pending::authorship()` give you the green, the version and the message and identities.
   `MadeCopy` comes along if you need copies.
4. Then remove your version-2 path, which commits in the submit's step.

Red to write first, mirroring the svn tests:
- answered only after both (the durable head 0 in the verb's step);
- a kill between the land and the commit recovers to before, and the retry lands once.
