# A yielding voter refused the voter it yielded to

Date: 2026-09-29. Scope: the election timer (`ElectionTimer::follower_period`, `crates/cluster/src/timing.rs`)
and the pre-vote's lease (`RaftNode::on_pre_vote`, `crates/cluster/src/raft.rs`) as every drive combined them:
the daemon's council and root group (`crates/server/src/fleet.rs`) and the simulations that mirror them.
Found while measuring MLRaft (`docs/wip/research/consensus-enhancements.md` §3.6): the leader of log 1 crashed
and its successor took 8.7 s to elect, nearly two elections.

## Symptom

A trace of that run (log 1, seed 0, the crash at 20.0 s) showed the most central survivor campaigning first
and being refused by every voter, twice:

- 23,332 ms: HostId(3), rank 0, campaigns. HostId(4), HostId(1) and HostId(5) refuse its pre-vote. Each of
  them yields its own timeout to HostId(3) in the next second.
- 25,832 ms: HostId(3) campaigns again. All three refuse again.
- 26,001 ms: HostId(4) campaigns at its second timeout. Only HostId(3) grants it.
- 28,199 ms: HostId(5) campaigns at its second timeout. It is elected at 28,659 ms, the least central of the
  three that campaigned.

The failing test, `the_most_central_survivor_wins_its_first_campaign` (`crates/cluster/tests/prevote.rs`),
reduces it to three voters over the real nodes and the real timer, ticking in one instant. The leader stops.
The central survivor campaigns at period 10 and is refused, and again at 26. The survivor it outranks
campaigns at 32 and leads.

## Root cause

A voter grants a pre-vote only once it no longer believes in a leader (`has_leader`, thesis §9.6). That
belief was cleared in one place for a follower: `on_election_timeout`, its own campaign. Priority elections
(§3.4) added the yield: at its timeout a voter outranked by `r` live voters sits out, `r` timeouts in all, and
never calls `on_election_timeout`. So a yielding voter kept its lost leader's lease until its own campaign,
and refused the pre-vote of the voter it was yielding to. Before priorities the same rule held the lease for
a voter's own jittered timeout rather than the minimum.

Thesis §4.2.3 sets the rule: a server refuses a vote only "within the minimum election timeout of hearing from
a current leader" (etcd's `inLease` compares against the base timeout, not the randomized one).

## Impact

Every group that elects with priorities (the council and the root group across regions), whenever a leader
is lost. Measured on the timed simulation over Microsoft's published round trips, the leader cut off at 20 s,
200 seeds, before (`HEAD` `462b63d` in a scratch export) and after:

| group | successor p50 / p90 / p99 / max, ms | campaigns | successor |
|---|---|---|---|
| 3 regions, before | 6,766 / 6,989 / 12,109 / 12,143 | 562 | the outranked Japan East 195, West Europe 5 |
| 3 regions, after | 3,322 / 3,549 / 3,680 / 6,063 | 201 | West Europe, the most central survivor, 200 |
| 5 regions, before | 4,430 / 4,777 / 4,828 / 8,616 | 602 | Southeast Asia 176, Japan East 24 |
| 5 regions, after | 4,158 / 4,773 / 6,472 / 7,103 | 507 | Japan East 111, Southeast Asia 68, West Europe 21 |

In a three-region group the priority election did the opposite of its purpose: the outranked region won
97.5 % of losses, after two timeouts, and a transfer then moved leadership again.

No safety impact: a pre-vote is non-binding, and the lease only guards against disruption.

## Fix

- The timer counts silence since leader contact apart from its own age, and its period returns a step
  (`FollowerStep`, `#[must_use]`): `Follow`, `LeaderLapsed` once silent for the minimum election timeout (the
  base), or `Campaign`.
- On `LeaderLapsed` the drive calls `forget_leader` (`RaftNode`, `ConfigGroup`, `RootGroup`), which clears a
  follower's belief without campaigning. A leader's own period restarts the silence.
- A lease the group's leader still holds is untouched: its followers hear it every period and never reach the
  base.

The failing test passes. A timer test pins the rule: nothing before the base, the lapse at it, and the lapse
on every period of a yield. A gate on the timed simulation (`a_lost_leader_passes_to_the_most_central_survivor`,
`crates/cluster/tests/priority.rs`) holds that the most central survivor of three regions succeeds on every
seed at its first campaign. Run against the unfixed tree, it fails at seed 0: Japan East is elected after
6,174 ms and two campaigns.

## What the fix exposed, and three mitigations measured and rejected

At five regions the p99 rose from 4,828 to 6,472 ms: 11 seeds of 200 take about 6.4 s. Traced (seeds 11 and
25), each is a split vote among survivors whose priorities tie within their spread (Japan East 162 ms, West
Europe and Southeast Asia 169 ms). Two or three of them time out within a vote's round trip, win their
pre-votes, and split the real vote. The bug had serialized them: pre-votes were refused until each had
campaigned. The unprioritized control shows the same splits before and after (p99 8,452 ms in both).

Three mitigations were measured on the same 200 seeds and rejected:

- **A span of the base**, Raft's `[T, 2T]` in place of the design's span from variance. At three regions p50
  and p99 rise to 4,009 and 4,611 ms; at five regions p90, p99 and max rise to 5,931, 7,824 and 15,316 ms. It
  helps only the unprioritized control.
- **A strict candidacy order among tied voters** (point estimate, then id): exactly one live voter has rank 0.
  At five regions p90 rises from 4,773 to 7,295 ms: when the first in order cannot win (its log trails the
  lost leader's last entries), the next waits a whole yield. Raft's authors abandoned a ranking scheme for
  this reason (Ongaro & Ousterhout 2014 §5.2: "a lower-ranked server might need to time out and become a
  candidate again if a higher-ranked server fails").
- **Deferring a campaign after granting a pre-vote.** p99 is unchanged (6,529 ms), p50 rises by 250 ms, and
  successors skew away from the most central.

So a tie among equally central voters is left to Raft's randomized retry, as the paper chose (§5.2, §9.3).

## Siblings swept

Every caller of `follower_period` now handles the step:
- the council and the root group in `crates/server/src/fleet.rs`;
- the timed simulation (`tests/support/timed.rs`), the multi-log simulation and the WAN election fabric;
- the timer's own tests.

No other path clears `has_leader` without a campaign, besides `suspend`, removal from the configuration, and
a leader's CheckQuorum, each of which is a role change.

The daemon's in-process fleet suite passes 53 of 53. The WAN election fabric's killed-leader case now elects
the first candidate, at 3.05 s after the death, where the unfixed tree refused it and elected the second at
3.25 s.
