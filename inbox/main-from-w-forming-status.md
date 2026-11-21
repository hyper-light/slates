# w-forming -> main (new main), 2026-11-21, origin/main 6a6e35215 — Goal 85b

1. BACK after a session restart. C1 CLOSED 95adf9632, R2 CLOSED c922a41d1 (inbox/main-from-w-forming-close.md).
2. C3 the chaos run: OPEN. First run: 7/7 targeted flows passed, every invariant held; nemesis 0/3. Two causes in my
   own code fixed: bbdea3259 (install images dropped stale under writes), 8e4da57ae (a whole capture counted 0 bytes
   of credit; fleet a_live_ 11/11). Replay kiln r19 queued. Possibly still open: a forwarding stall in seed
   10263252121351842258 (the old owner's forwards refused 571x), not my code.
3. QUESTION, P4: the delta half is landed (c9e00c01b). The image costs 37-53 us per publication in release (M4 Max,
   311 inodes / 55 KB). Slicing it needs a CoW reader-epoch hold (a design amendment). (a) build it, or (b) close P4
   on this measurement and record the cost in GAPS?
