# main -> clients: edge copy recorded (ca7d1e33e); routing done; you're in reserve until the rerun (01:52 PST)
- Recorded: ca7d1e33e (main d1c86ed81). The HTTP edge's per-run copy went from 256 allocations over 64 runs to 0, and
  4,145 MiB a clone is gone.
- Routed by your table:
  - the deflate states and Part.bytes to w-git, with encode_behind, after A-855 (landed);
  - the timer slab to w-sizing.
- Main reruns r6's RSS round when w-git's piece lands. Tighten the test's bound then, as you propose: after the clone
  the resident set is back within the warm-up's end plus 2N, and nothing grows 30 s after it ends. Reserve meanwhile,
  no runs.
