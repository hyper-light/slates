# A learner lost the only voters it knew and stopped learning configurations (2026-11-08)

Contracts: §4.8 and D-14 (a non-voting member learns the committed configuration by fetching it), and the rule that a
learner fetches on evidence that it is behind.

## Description

raft-chaos's five-node flow, with its settle wait holding every node to the fleet's newest configuration, caught this
with no pause or crash injected (CI diag-raft-chaos-mesh-52, runs 2 and 7 of 8). After a voter was replaced, both
non-voters stayed at configuration version 8 for a whole 40 s fleet wait while the three voters reached 11 and 12. A
stale learner also reaches an owner's crash unnoticed by a settle wait that compares each node with itself.

## Root cause

`drive_learner_fetch` asked only `council.voters()`. On a learner that list is the voter set of its join state: a
fetched configuration updates a read view and leaves the Raft state alone, so the list never changes after the join. A
learner that joined while the council was the bootstrapper alone knows that one voter for good. When that voter left, no
session matched, and the function returned without asking anyone and without counting anything.

The two failing runs had replaced the bootstrap voter, the lowest host id (b in run 2, c in run 7); run 3, which replaced
a voter that was not the bootstrapper, passed. `serve_fetch` has no voter check: any initialized member answers.

## Impact

Any non-voter whose join-time voters were all gone stopped learning configurations for good: it kept placing, reporting
settlements and answering under an old configuration, and could not be told so.

## Exact edits

- `crates/server/src/fleet.rs`: `drive_learner_fetch` asks every member of its region it holds a session to. What a
  member serves is a configuration it applied, so a committed one; the answer is empty unless it is newer than the
  asker's; and adoption only moves forward. A period with evidence and no session to ask counts
  `fleet.learner.no_session`, where before it left nothing.
- `crates/server/tests/fleet.rs`: `a_learner_learns_configurations_once_the_voter_it_joined_from_is_gone`.
- `docs/wip/SLATES_DESIGN.md`: §4.8's learner paragraph.

## Evidence

CI, one test binary per platform, five daemons:

| Arm | macOS | Linux |
|---|---|---|
| Without the fix (diag-live-fixes-45) | 4 of 4 failed at the final check, 408 s each | 4 of 4 failed at the final check, 408 to 412 s each |
| With the fix (diag-live-fixes-46) | 12 of 12 passed, 3.3 to 5.6 s | 12 of 12 passed, 3.4 to 6.4 s |

Each failure left one survivor at version 7 or 8 against 10 to 12 for the others.

## A second use of the same list: a learner's reports

`send_council_report`, which carries a learner's settlements and takeover confirmations, also went only to the join
state's voters, and counted itself sent before knowing anyone was there. In CI diag-raft-chaos-learner-56 run 1 a node's
neighbourhood stayed unsettled for a whole fleet wait with `fleet.report.sent` 662. A report now goes to every member of
the region this node holds a session to; the leader proposes it and any other member answers `NotLeader`.
`fleet.report.no_session` counts a report with no session to go on. Test
`a_learners_reports_reach_the_council_once_the_voter_it_joined_from_is_gone`: without the fix 5 of 8 failed (macOS 2 of
4, Linux 3 of 4, CI diag-live-fixes-51), with it 23 of 24 passed (diag-live-fixes-52). The whole fleet suite was not run
on this change alone.

## A third use of the same list: who the learner keeps contact with

With reports going to every session, 2 of 60 runs of the report test still left a node unsettled (CI diag-live-fixes-53).
In both, that node's own view of the voters was the stopped bootstrapper alone, and its contact reason for the real
leader was `OutsideNeighbourhood`: the contact rule read the same frozen list, so the learner dropped the session its
reports needed. A fetched configuration's reply now names the council's voters, and a learner keeps them
(`RegionalCouncil::learn_voters`, `known_voters`), which the contact rule reads. They move only with a newer view or a
first answer, so a late reply cannot put an older set back. Test
`a_learner_learns_who_votes_once_the_voter_it_joined_from_is_gone`: without it 8 of 8 failed on macOS and Linux (CI
diag-live-fixes-54), with it 32 of 32 passed (diag-live-fixes-55), and the report test passed 60 of 60 on Linux
(diag-live-fixes-56).

A limit: the voters arrive with a newer configuration, so a voter promoted after the last configuration commit is unknown
to a learner until the next one. Held in RAM: a restarted learner relearns them at its next fetch.

## Sibling, reported and not changed

`drive_root_learner_fetch` filters on the root group's voters the same way, and a root learner's contact rule reads them. It needs its own test.
