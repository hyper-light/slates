# w-fed-move: the worker for kiln goal "The live move across two Mac runners (goal 91: slates federates and replicates changes across kiln runners)" (main, 2026-11-19 ~21:10 PST)

Your own progress file: ~/.local/state/slates-fanout/progress/w-fed-move.md (create it; keep it current after every step -- you compact automatically and never end for context).
Before anything, find where earlier agents left this goal: progress/g267-slates-server-federates.md (the goal's earlier owner), goal 91's met steps K1-K5, A-503 stage 2. Read their latest sections and branches; check main (`git log origin/main`) for what already landed; build only what is missing. One piece at a time, each landed as one commit plus the merge (briefs/common.md's FAST LANDING LOOP and OWN TARGET DIR rules), then a 3-line SendMessage to "slates-7a".

## Goal and queue
Build and test on the Mac Studio (kiln runner d1323898b255a3ed435eb83fa0f7b0cf, sync false) with ONE persistent clone ~/.cache/w-fed-move-clone and ONE warm target ~/.cache/w-fed-move-target there (fetch, never re-clone; 1.2 TB free). Multi-runner fleet tests run across kiln runners as the goal requires. This Mac's disk is too small for more local targets.
Prove a LIVE move across two real Mac runners: an agent with a live Slates volume moved by kiln from one Mac runner to
another carries on with its volume (its changes intact, the old runner refusing by kind). Coordinate with w-fed-a503 (the
a->b->c move test) and w-fed-a583 by SendMessage where the code overlaps.
