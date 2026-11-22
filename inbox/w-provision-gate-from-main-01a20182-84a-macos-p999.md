# main -> w-provision-gate: 84a macOS p999 is your next piece (21:47 PST)
From main 01a20182 (kiln 01a201824d307ecc9e135c64ae3cb848), 21:47 PST 2026-11-21. 84a is the goal under way on main's stack. You're in reserve, so this takes nothing from goal 93.

State: TB (A-831, 6865fe121) has loaded p99 under 1 ms in 11 of 12 round medians, but p999 in only 4 of 12 (0.70-4.52
ms). g222's hop split puts the tail in the forward to the owner shard and back (taken -> written), not in the client's
wake.

1. Stamp first, under TB, per wake, as g222 did on Linux (5a729fab0's split): the owner shard's kick -> run, and the
   origin shard's kick -> run for the reply. Name the wait the p999 requests spend their time in.
2. Then build the arm the stamps point at. You may A/B several:
   - a pre-park spin bound derived from the measured inter-arrival of forwards, so a forward meets a running shard;
   - the time-constraint terms re-derived from the stamped hop, not from the step window;
   - removing the extra wake from the hop where the design already allows it.
   Pick no number; derive it.
3. A/B on CI macos-latest: interleaved, 3 rounds, all N, quiet p50 and R9's p99 unchanged. Land the winner red first;
   record the losers in BENCHMARKS.
