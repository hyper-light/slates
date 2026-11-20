# w-unresolved: the worker for kiln goal "An unresolved object is recovered, not just kept (A-272 (ii), live-durability)" (main, 2026-11-19 ~21:10 PST)

Your own progress file: ~/.local/state/slates-fanout/progress/w-unresolved.md (create it; keep it current after every step -- you compact automatically and never end for context).
Before anything, find where earlier agents left this goal: progress/g250-unresolved-object-recovered.md, the old kiln agent 'live-durability' (~/slates-agents/live-durability), the goal's met parts (A-484, A-508, A-529, A-538, A-546, A-556, A-577 landed). Read their latest sections and branches; check main (`git log origin/main`) for what already landed; build only what is missing. One piece at a time, each landed as one commit plus the merge (briefs/common.md's FAST LANDING LOOP and OWN TARGET DIR rules), then a 3-line SendMessage to "slates-7a".

## Goal and queue
Build and run unit tests LOCALLY with your own target (~/.cache/w-unresolved-local-target, JOBS=6, 20 GB disk guard).
The goal: adoptions are kept as records, so Settled rests on one; the current members run a round for an Unresolved object,
so f+1 counts the members that exist now (here the replacement holding the live log). Open parts:
1. Whatever of A-272 (ii) is not yet on main (adoption records; the round for an Unresolved object among current members).
2. "Lineage.tla text for A-341 and A-484": the Lineage model as TEXT only (CLAUDE.md: TLA+ models under docs/wip/models/
   are architecture artifacts; write the text, do not add TLC to CI).
3. "Goal 84 checked clause by clause, with evidence": a written check of goal 84's clauses against main, each with its
   test or commit.
