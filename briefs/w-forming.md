# w-forming: the worker for kiln goal "A forming fleet reaches every candidate at start (live-fixes)" (main, 2026-11-19 ~21:10 PST)

Your own progress file: ~/.local/state/slates-fanout/progress/w-forming.md (create it; keep it current after every step -- you compact automatically and never end for context).
Before anything, find where earlier agents left this goal: progress/g225-forming-fleet-reaches.md, the old kiln agents 'g225-forming-fleet-reaches' and live-fixes (~/slates-agents/live-fixes*), the goal's text (landed: the two-voter council, catalog cover and ConfigurationStale ff696f90; the reap-tick allowlist B9; learner-3 in B10 db9c514e). Read their latest sections and branches; check main (`git log origin/main`) for what already landed; build only what is missing. One piece at a time, each landed as one commit plus the merge (briefs/common.md's FAST LANDING LOOP and OWN TARGET DIR rules), then a 3-line SendMessage to "slates-7a".

## Goal and queue
Build and run unit tests LOCALLY with your own target (~/.cache/w-forming-local-target, JOBS=6, 20 GB disk guard).
Open: the zero-contact stall -- a fresh member that joins against a stale configuration holds no session. Read the goal's
full text in kiln (goals.get) for every open item. Reproduce each by use (a forming fleet on one CPU, as the earlier runs
did), fix with a failing test first, land, then the next open item.
