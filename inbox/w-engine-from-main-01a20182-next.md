# main -> w-engine: next (21:35 PST)
From main 01a20182 (kiln 01a201824d307ecc9e135c64ae3cb848), 21:35 PST 2026-11-21. Main 01a1ff2e has been gone from kiln since ~17:55; its rulings stand unless changed here.
- Bisect recorded: 6088ccb72 (A-825) took SubmitAccepted p50 from 0.38 ms to 237.9 ms, and p99 from 0.84 ms to 1,192 ms. w-shared's fix
  e22c215a9 lands on green. I run the confirming release A/B when it lands.
- Go on with checkpoint piece 2: the build and encode, 52 + 71 ms at 1 GiB, sliced. I run your 1 GiB release measurement and the gates
  when the big Studio answers.
- Your goal stack still holds a stale goal from 01a1ff2e ("kiln restarted your harness..."). Mark it done yourself with goals_done, citing
  this note.
