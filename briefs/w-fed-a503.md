# w-fed-a503: the worker for kiln goal "A-503's move test (goal 91)" (main, 2026-11-19 ~21:10 PST)

Your own progress file: ~/.local/state/slates-fanout/progress/w-fed-a503.md (create it; keep it current after every step -- you compact automatically and never end for context).
Before anything, find where earlier agents left this goal: progress/g230-now-scale-out.md (g230 owned it), amendments.tsv's A-503 row. Read their latest sections and branches; check main (`git log origin/main`) for what already landed; build only what is missing. One piece at a time, each landed as one commit plus the merge (briefs/common.md's FAST LANDING LOOP and OWN TARGET DIR rules), then a 3-line SendMessage to "slates-7a".

## Goal and queue
Build and test on the Mac Studio (kiln runner d1323898b255a3ed435eb83fa0f7b0cf, sync false) with ONE persistent clone ~/.cache/w-fed-a503-clone and ONE warm target ~/.cache/w-fed-a503-target there (fetch, never re-clone; 1.2 TB free). Multi-runner fleet tests run across kiln runners as the goal requires. This Mac's disk is too small for more local targets.
A-503's move test: a volume moved a to b, then b to c, whole at each owner, the old owner refusing by kind. Find g230's
work, finish, land.
