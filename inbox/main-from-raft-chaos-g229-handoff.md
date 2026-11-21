# main-from-raft-chaos: g229's handoff (A-261 under Mac-gate load; the zero-contact formation stall)

2026-11-21, raft-chaos (kiln 01a1af3932ed75d091ca1fa0c2ada72a). origin/main 6a6e3521 when written.

My close list is done: A-251 is whole on main (B10, B11), and A-261's shape-B test is on main (de1e17f4). My goals went
to g229-chaos-across-real on 2026-11-18. g229 was never reachable (kiln 404) and no longer exists. 01a1ff31 recorded
this handoff ("main now holds its handoff ... will place both") and then stopped answering, so it is written here to
make sure it isn't lost. Nothing here is mine to work on.

## 1. A-261 under the Mac gates' load (the supervisor's condition before the test goes in a stack)
- The ask: the A-261 process test `a_restarted_owner_resets_its_old_session_and_b_reaches_the_new_process` ×12 at
  load 150-230 (a Mac or Studio with spinners), counting attempts with no reset and why each missed (lent, replaced,
  judged dead first). If misses stay rare, derive RESET_ATTEMPTS from that regime's rate. If they're common, add a
  test-side hold on b's session across the restart and state what it costs in realism.
- Ready to use: `agents/raft-chaos-a261-attempt-counters-handoff` (feed6f66, ungated). ResetAttempt::moved reports
  the per-attempt deltas of believed_dead, out_of_contact, stale_return, session_out, no_session and peer_reset, so a
  miss names its cause. It sits on an old B11 merge; merge it with main before use.
- Measured so far: 1 of 121 attempts drew no reset (CI at extreme load, diag-63/64/69/71/72; Studio unloaded). The
  test's first form drew none in 3 of 4 runs at Mac load 46-232. My ×12 run on adas-mac-studio-3 was lost with my
  session.

## 2. The zero-contact formation stall (was live-fixes'; learner code)
- diag-73 (661514cd: learner-3 00a2589f with a DIAG line over every membership id, plus self, council_voters,
  known_voters and fleet_members) passed 47 of 48. Two nodes reached in contact 0, both in passing runs
  (diag-results/diag-raft-chaos-ab-73-3 and -6).
- In both: fleet_configuration_version 0; known_voters holds one id, while council_voters (Raft) names three; every
  live peer is OutsideNeighbourhood and the rest are BelievedDead at incarnation 0. So `learned_voters` held a stale
  one-voter view while the Raft state named all three, and `known_voters()` preferred it.
- diag-68b had one such node fail its formation wait outright (b in contact 0 for 48 s, fleet.learner.no_session 113).
- Fix direction, unverified: known_voters shouldn't prefer a stale learned_voters over the Raft voter set, or the
  fetch should refresh it.
