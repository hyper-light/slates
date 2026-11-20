# w-shared: the worker for UI goal 93's part "The shared pieces of section 4.17.1 built" (main, 2026-11-19 ~20:15 PST)

Your own progress file: ~/.local/state/slates-fanout/progress/w-shared.md (create it; keep it current: piece, branch, runs, next step).
Work ONE piece at a time, in this order; each piece's handoff details are in the brief and progress file named beside it -- read them when you reach that piece. Each piece lands as one commit plus the merge, failing test first, then a 3-line SendMessage to "slates-7a", then the next piece. You are the only worker on this goal; the other five goals have their own workers (w-shared, w-git, w-hg, w-svn, w-interop, w-perf) -- coordinate with them by SendMessage only where a piece touches theirs.

## Queue
(main 2026-11-19 ~22:00: LANDED so far -- A-664 p3 commit c6f7976c3, mode fix 73cbdfc42, branch-map follow-on 9f51d7525.
NEW ORDER: (1) A-636 2b-2 then 2c (item 2 below; it unblocks holders' maps); (2) A-664 holders' maps; (3) A-664 sliced
whole builds; (4) A-664 hg and svn server-made objects; (5) A-697 (item 3 below). "Copy through a work" waits for a
client path that makes a copy (svn copy over svn:// once w-svn's commits land). Read progress/w-shared.md first.)
1. A-664 part 3's commit: server-made git commits from each submit (+ the chmod+x-then-mv mode check by use). Details:
   briefs/g308.md; progress/g308-server-commits.md. Then its follow-ons (holders' maps, a copy through a work, sliced
   whole builds, hg and svn objects), each its own piece.
2. A-636 part 2b-2 (successor routed by id, never per-green config entries) then 2c. Details and main's rulings:
   briefs/g261.md; progress/g261-git-mercurial-subversion.md.
3. A-697 (a)-(f) (works on holder nodes; test-only hook removed in the last piece) then A-658 3c. Details and rulings:
   briefs/g305.md; progress/g305-merge-anywhere.md.
