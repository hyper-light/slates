# A leader held dead kept leading and was never retired

**Found:** 2026-10-06. CI's fleet lane failed an isolated-owner test intermittently. Reproduced locally 1 run in 9 and
1 in 28, with the fleet harness's wait trace and, at the failure, the council's state.

## Description

`an_isolated_owner_refuses_latest_state_reads_while_the_successor_advances_the_green` cuts the owner A off on the probe
plane, both ways, and expects the survivors to retire it and take its green over. In the failing runs, after 401 s:
- both survivors had retired A in their failure detector's view;
- A was still in the **committed** council membership on both;
- neither led, and the successor's council was a follower at **term 1**;
- no takeover was counted.

## Root cause

Only the council leader proposes retirements (`reconcile_council_membership`), and a leader never proposes its own.
When the owner A was also the council leader, its council traffic still travelled on the record plane, which the
probe-plane isolation leaves whole. Its heartbeats renewed both followers' lease, so neither campaigned. A follower
holding a lease refuses another candidate's pre-vote, so neither could have won anyway. The failure detector and the
consensus plane disagreed, and the consensus plane won by default. The test failed only when A happened to lead,
which made it intermittent.

## Fix (A-103)

`serve_council` answers nothing to a peer the node's failure detector has held dead for the confirmation window, the
same window a retirement waits out. The followers stop renewing the dead leader's lease, the leader loses CheckQuorum,
the followers elect one of themselves, and the new leader retires it.

## Tests

`a_council_leader_the_failure_detector_holds_dead_is_replaced_and_retired` (`crates/server/tests/fleet.rs`) isolates
whichever node leads the council. It failed before the fix (402 s, neither survivor leading, the leader still a
member) and passes after (4 runs, 7.7–146 s). The isolated-owner test now reports the membership and council views on
a failure, and passes 10 of 10. The fleet suite passes 71 of 71.

## Sibling sweep

- **The root group** (`serve_root`) has the same shape across regions. A region's representative is judged by region
  liveness, not the council's death watch, so it is left as is and recorded in GAPS.
- **Convergence time:** up to 146 s, from refutation over another plane. Open in GAPS.
