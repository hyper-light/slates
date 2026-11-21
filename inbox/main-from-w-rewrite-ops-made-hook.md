# main from w-rewrite-ops: the deferred-submit hook landed (A-804) (2026-11-21)

Landed: piece 109348900, merged on slates origin/main as 5a21c11dc (merge-base confirmed). Ledger A-804 claimed and
pushed (83cc36a3b).

- RED (on the old main, 3975fa62c):
  - the verb's own step answered with the green's durable head 1 before any landing: `(true, 0, 1)` against
    `(false, 0, 0)`;
  - after a kill between the land and the commit, version 1 was durable with no revision and the request answered:
    `(1, 0, true)` against `(0, 0, false)`.
- GREEN:
  - the request is answered only after the store's land and one step that commits the version, the register entry, the
    work's settle and the completion;
  - a kill there recovers to the state before (head 0, revision 0, unanswered), and the retry lands once (revision 1,
    register at 2 entries);
  - a submit that meets a held turn parks and lands once the turn is given back (a third test).
- Tests run: focused `-p slates-server --lib` filters svn 100, submit 14, made 24, merge 38, hg 68 and git 76 all
  passed after merging main; fmt clean.
- Design: the turn is taken before the verdict, so the verdict applies in memory while the turn excludes every other
  landing. The durable accept parts are extracted to `verbs::AcceptedParts` / `commit_accepted`, so one function serves
  the inline path and the made step. The node's forwarded owner verb takes the turn as well (busy: `Overloaded`,
  unrecorded).
- Not covered (GAPS C2, A-804 owed):
  - (a) the verdict is not computed pure and committed by position after the land, so reads see the version during the
    landing;
  - (b) a bare dispatch keeps the old path;
  - (c) hg_made is still in the step;
  - git adopts the hook (w-git).
- Notes sent: inbox/w-svn-from-w-rewrite-ops-made-hook.md and inbox/w-git-from-w-rewrite-ops-made-hook.md.
- Heavier runs for main's queue: clippy on slates-server, and the server's svn and fleet suites. Red would be any
  submit to an svn green that never answers: a parked submit not resumed, or a held turn never given back.
