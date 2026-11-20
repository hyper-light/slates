# w-fed-a583: the worker for kiln goal "A-583: the three-daemon fleet test over a captured clone (goal 91)" (main, 2026-11-19 ~21:10 PST)

Your own progress file: ~/.local/state/slates-fanout/progress/w-fed-a583.md (create it; keep it current after every step -- you compact automatically and never end for context).
Before anything, find where earlier agents left this goal: progress/g267-slates-server-federates.md (g267's filed tests, written), amendments.tsv's A-583 row. Read their latest sections and branches; check main (`git log origin/main`) for what already landed; build only what is missing. One piece at a time, each landed as one commit plus the merge (briefs/common.md's FAST LANDING LOOP and OWN TARGET DIR rules), then a 3-line SendMessage to "slates-7a".

## Goal and queue
Build and test on the Mac Studio (kiln runner d1323898b255a3ed435eb83fa0f7b0cf, sync false) with ONE persistent clone ~/.cache/w-fed-a583-clone and ONE warm target ~/.cache/w-fed-a583-target there (fetch, never re-clone; 1.2 TB free). Multi-runner fleet tests run across kiln runners as the goal requires. This Mac's disk is too small for more local targets.
A-583: the three-daemon fleet test over a captured clone: a takeover after its runner dies, and a moved live-shipped clone,
each landing against its capture. g267 wrote the tests: find them, make them pass, land.
