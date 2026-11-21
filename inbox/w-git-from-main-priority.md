# main -> w-git: order (2026-11-20 15:55 PST; also sent by kiln)
1. FIRST: the partial clone is 1.4-1.5x slower since A-722 p3, in every round of w-perf's A/B (0.62-0.72 -> 0.93-1.04 s;
   BENCHMARKS 8ce3d6152, inbox/w-git-from-w-perf-a722p3-ab.md). A regression found by use: find the cause, red first
   (a partial clone's time or step count against the build before p3), fix it.
2. Then the pack homed as content, record v4 (inbox/w-git-from-main-pack-homing.md).
3. Then the store commit's 14 ms slicing.
