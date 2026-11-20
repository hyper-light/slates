# w-hg: the worker for UI goal 93's part "The Mercurial server built" (main, 2026-11-19 ~20:15 PST)

Your own progress file: ~/.local/state/slates-fanout/progress/w-hg.md (create it; keep it current: piece, branch, runs, next step).
Work ONE piece at a time, in this order; each piece's handoff details are in the brief and progress file named beside it -- read them when you reach that piece. Each piece lands as one commit plus the merge, failing test first, then a 3-line SendMessage to "slates-7a", then the next piece. You are the only worker on this goal; the other five goals have their own workers (w-shared, w-git, w-hg, w-svn, w-interop, w-perf) -- coordinate with them by SendMessage only where a piece touches theirs.

## Queue
1. LAND A-680 part 2b (hg push over HTTP). It is NOT on main: g303's landing push was rejected (~16:18 PST) though
   its notes say "landed as 95c3edd18 (+ merges 4013e0f03, 6d8a99656)"; none is an ancestor of origin/main. Take the
   piece from g303's branch (progress/g303-hg-http.md, briefs/g303.md), merge origin/main, re-run its focused tests on
   the Studio, push. Then un-ignore g083's stock-hg HTTP round trip if it passes.
2. LAND A-687 (durable import + the one Mercurial record kind), green on agents/g300-a687-import-durable 6c492db95,
   conflicted with main's import.rs/push.rs: briefs/g300.md; progress/g300-hg-import.md. Then record 2b's pushes with
   its kind (2c) if 2b landed without it.
3. A-696 hg push over SSH: g304's branch already has 2b merged as a scratch base; land it as its own commit after 2b
   is on main: briefs/g304.md; progress/g304-hg-push-land.md.
4. hg `lookup` (found by use: stock `hg identify` aborts): briefs/g258.md.
5. Stream clones parts 1-3: briefs/g083.md; progress/g083-w1-under-1ms.md (the hg inventory is there too).
6. Then the inventory's missing list by user impact: server-made changesets + rebase on push (pushrebase), clone
   bundles, narrow/ellipses/remotefilelog, largefiles + lfs, obsmarkers on push + topics, push follow-ons,
   `slates hg serve --stdio` for OpenSSH forced commands, hgweb json/raw, compression and perf.
