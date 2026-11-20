# w-perf: the worker for UI goal 93's part "An order of magnitude faster than competing servers, measured and recorded" (main, 2026-11-19 ~20:15 PST)

Your own progress file: ~/.local/state/slates-fanout/progress/w-perf.md (create it; keep it current: piece, branch, runs, next step).
Work ONE piece at a time, in this order; each piece's handoff details are in the brief and progress file named beside it -- read them when you reach that piece. Each piece lands as one commit plus the merge, failing test first, then a 3-line SendMessage to "slates-7a", then the next piece. You are the only worker on this goal; the other five goals have their own workers (w-shared, w-git, w-hg, w-svn, w-interop, w-perf) -- coordinate with them by SendMessage only where a piece touches theirs.

## Queue
1. A-699: land the harness and STOCK baselines (release builds; best of N with all N shown; hardware, dataset
   commits, date, load average; docs/wip/BENCHMARKS.md's format): git daemon or http-backend on git/git (push all,
   clone, incremental fetch, shallow, --filter=blob:none); then hg serve and svnserve on comparable large
   repositories. Harness state: progress/g309-interop.md (g309 started it; its numbers: stock git daemon push 30.3 s,
   clone 13.5 s for git/git f7de8145).
2. Slates' numbers for everything that works today against those baselines (git clone/fetch over the daemon; hg reads;
   svn as it lands). The large push waits on w-git's fix.
3. Then the gaps to 10x: profile the slowest paths on the Studio and send main a ranked plan (each with its measured
   cost) before changing code; then implement them one piece at a time, each A/B measured (interleaved, fresh state).
