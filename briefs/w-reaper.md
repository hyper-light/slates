# w-reaper: the worker for kiln goal "The event-driven reaper: dead clients by hang-up (A-475), every other reap job by its own event, the reap tick deleted" (main, 2026-11-19 ~21:10 PST)

Your own progress file: ~/.local/state/slates-fanout/progress/w-reaper.md (create it; keep it current after every step -- you compact automatically and never end for context).
Before anything, find where earlier agents left this goal: progress/g225-forming-fleet-reaches.md and progress/g255-cross-shard-remote.md (they name A-475 and the reap tick), the old kiln agent 'liveness' (~/slates-agents/liveness*), amendments.tsv's A-475 row. Read their latest sections and branches; check main (`git log origin/main`) for what already landed; build only what is missing. One piece at a time, each landed as one commit plus the merge (briefs/common.md's FAST LANDING LOOP and OWN TARGET DIR rules), then a 3-line SendMessage to "slates-7a".

## Goal and queue
Build and run unit tests LOCALLY with your own target (~/.cache/w-reaper-local-target, JOBS=6, 20 GB disk guard).
1. Dead clients reaped by hang-up (A-475): the client's connection end (socket hang-up / peer death) triggers its reap, no polling.
2. Every other reap job driven by its own event (find each job the reap tick serves today and give it its trigger).
3. Delete the reap tick, with a test proving no reap waits on it.
Failing tests first for each.
