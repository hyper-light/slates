# A member that missed its promotion refused every election

Date: 2026-09-29. Scope: the Raft core's vote handling (`RaftNode::pre_vote_refusal`, `on_request_vote`,
`on_vote_reply`; `crates/cluster/src/raft.rs`), and the council's and the root group's learner period
(`crates/server/src/fleet.rs`). Found by CI run 36576662318 on `38c987e` (Ubuntu): the three-process CLI fleet
test's takeover wait ran out.

## Symptom

`three_daemon_processes_deploy_a_fleet_from_one_manifest_and_survive_the_owners_death` kills the owner of a
sealed volume. It then waits for a survivor to take the volume over. On CI neither survivor served it within
the fleet wait. It reproduced in Linux Docker with io_uring in 2 of 81 runs, and never in 8 runs on macOS.

The wait now dumps each survivor's status when it runs out. The second failure (run 31 of a loop) showed:

| | survivor A | survivor C |
|---|---|---|
| council leads | no | no |
| term | 1 | 1 |
| pre-elections begun | 26 | 0 |
| pre-votes granted to it / refused | 0 / 26 | — |
| pre-votes it refused, by role | — | 26 |
| leader lease held | no | **yes** |
| path samples to the other voters | 499 | **0** |

So the council had no leader, the dead owner could not be retired, and nothing was taken over. A campaigned
every timeout; C refused every one of its pre-votes "by role"; C itself never campaigned and still held a
lease of the dead leader.

## Root cause

Two rules combined.

1. **The core refused a vote from a receiver that is not a voter in its own configuration**
   (`pre_vote_refusal`: `!self.is_voter(self.id)` → `Role`; `on_request_vote`: `granted = self.is_voter(self.id)
   && …`). C had been caught up and promoted to voter. The leader (the owner, killed next) committed the
   joint and final configuration entries with A's acknowledgement alone, a majority of both sets, before
   they reached C. So A held voters {A, B, C} and needed C's vote, while C, still holding {A, B}, refused.
   Raft's thesis §4.1 is explicit: "servers process incoming RPC requests without consulting their current
   configurations". The candidate's configuration decides whether a vote counts.
2. **A learner's lease never lapsed.** In both groups' drive, a node that is not a voter in its own view takes
   the learner branch: it fetches the configuration when it diverges, and returns before the election
   timer. The lease of a lost leader is forgotten only on the timer's `LeaderLapsed` step (thesis §4.2.3),
   so a staged member that had heard the leader's catch-up appends kept that lease for good. With rule 1
   alone removed, C would still have refused, as "leased".

## Why this is safe

AUD-07's rule is that new members cannot vote before **initialization**. It is enforced where it holds:
`RegionalCouncil::answer` (and the root group's) answers nothing until the node has the group's base and
prefix. It does not depend on the core's self-check. A fresh boot has a fresh member id, and a warm restart
retains term and vote, so no identity can vote twice in a term.

Raft's election safety rests on three things, and none of them asks whether the voter knows it is a voter:
- one vote per term per server;
- the up-to-date check;
- majorities intersecting in the candidate's configuration, which joint consensus guarantees across a
  change.

One place did depend on it. A candidate recorded every granted vote, and the recovery a new leader runs counts
the granters it heard (`heard = votes.len()`) and reads their window reports (Fast Paxos's threshold). A
grant from outside the candidate's configuration could have entered that count. A demoted voter that had not
learned its demotion could already send one. A candidate now records only votes from its own configuration's
voters, the only ones that can count toward its majority.

## Fix

- `pre_vote_refusal` refuses by role only when this node leads.
- `on_request_vote` no longer asks whether this node is a voter in its own configuration.
- `on_vote_reply` records a grant only from a voter of the candidate's effective configuration (either set of
  a joint one), so the recovery's count and reports come from its own voters.
- A member outside its own configuration still never campaigns (`on_election_timeout`, `start_election`).
- Both groups' learner branches now run the election timer for its lapse: a learner forgets a leader it has
  not heard for the minimum election timeout, and where a voter would campaign it re-baselines instead
  (`lapse_learner_lease`).

## Tests

- **Failing first (core):** `a_member_that_missed_its_promotion_still_votes_for_a_candidate_that_has_it`.
  B leads {A, B} and moves the group to {A, B, C} through the joint and final entries, each committed with A
  alone. B is lost, and A campaigns. Unfixed, C refused A's pre-vote; fixed, C grants the pre-vote and the
  vote, and A leads.
- **Reworked:** `a_learner_neither_grants_votes_nor_campaigns_before_admission` asserted that a member
  outside its configuration refuses a vote request. That was the rule that deadlocked, stricter than AUD-07.
  It is now `a_member_outside_its_configuration_never_campaigns_and_its_vote_counts_nowhere_else`: the member
  never campaigns, and a vote it grants does not count toward a candidate whose configuration does not name
  it.
- **Added:** `an_uninitialized_member_answers_no_vote` (the council wrapper). This is AUD-07's rule where it
  is enforced: no answer to a pre-vote or a vote, and no campaign, before initialization.
- The Raft explorer at full scale finds no safety violation: 3 and 5 voters × 400 seeds × 4,000 steps, with
  1,443 and 1,203 membership changes begun, 773 and 654 members caught up, and 21,900 and 22,090
  crash-restarts. So does the multi-log explorer at full scale.
- The cluster suite passes 282 tests. The in-process fleet suite passes 57 of 57 (237 s).
- **By use:** the three-process CLI fleet test in Linux Docker with io_uring. Before the fix it failed 2 of 81
  runs: the one dumped was this deadlock, and the other's log was not kept. After it, on a frozen copy of the
  tree, 150 runs had 2 failures, neither this deadlock: in both the council had elected a leader. Those are
  a second, independent defect in the takeover path
  (`docs/bugs/2026-09-29-a-takeover-stalled-when-a-survivor-never-received-the-head.md`).

## Siblings

- The root group has the same learner branch, fixed the same way.
- A permanent learner (a member beyond the council's `2f + 1` floor) fetches the configuration instead of
  receiving appends, so it never holds a lease. Its lapse is a no-op.
- A demoted voter that has not learned its demotion could grant a vote before this change, and its grant now
  counts nowhere outside its configuration.
