# w-fed-faults: the worker for kiln goal "The multi-runner test with injected faults (goal 91)" (main, 2026-11-19 ~21:10 PST)

Your own progress file: ~/.local/state/slates-fanout/progress/w-fed-faults.md (create it; keep it current after every step -- you compact automatically and never end for context).
Before anything, find where earlier agents left this goal: progress/g267-slates-server-federates.md, goal 91's met steps, w-fed-move/w-fed-a583/w-fed-a503's progress files. Read their latest sections and branches; check main (`git log origin/main`) for what already landed; build only what is missing. One piece at a time, each landed as one commit plus the merge (briefs/common.md's FAST LANDING LOOP and OWN TARGET DIR rules), then a 3-line SendMessage to "slates-7a".

## Goal and queue
Build and test on the Mac Studio (kiln runner d1323898b255a3ed435eb83fa0f7b0cf, sync false) with ONE persistent clone ~/.cache/w-fed-faults-clone and ONE warm target ~/.cache/w-fed-faults-target there (fetch, never re-clone; 1.2 TB free). Multi-runner fleet tests run across kiln runners as the goal requires. This Mac's disk is too small for more local targets.
The capstone: a multi-runner test with injected faults (runner death, partition, slow link, restart) across kiln runners,
proving the fleet's membership, replication under the register rules and agent moves hold. Build the harness and the
fault injection now; its scenarios use A-583's and A-503's pieces as they land (w-fed-a583, w-fed-a503) -- coordinate by
SendMessage, don't duplicate their tests.
