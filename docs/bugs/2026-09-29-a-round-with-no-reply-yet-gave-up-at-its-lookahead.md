# A round with no reply yet gave up at its lookahead

Date: 2026-09-29. Scope: `DispatchWait::judge` (`crates/cluster/src/lib.rs`), which decides when every quorum
collection stops: the consensus fan-out `broadcast`, record commits, promise and ledger collections, and
content rounds. Found on the KIND lane's new succession measurement (`cargo xtask kind succession`;
`docs/wip/kind-lane.md`), after the lease fix of
`docs/bugs/2026-09-29-a-yielding-voter-refused-the-voter-it-yielded-to.md` had been measured in simulation.

## Symptom

Three pods, pod 0's egress delayed 80 ms and pod 1's 20 ms (each ± 5 ms), pod 2 unshaped. The council
leader's egress was cut. On the fixed daemon a successor took 13.4 s and 25.4 s in the first two trials,
and the outranked pod 0 won the first.

The new status counters (`fleet_council_pre_elections`, `_pre_votes_granted`, `_pre_votes_refused`, the
refusals by reason) showed the central survivor beginning 7 and 14 pre-elections that drew **no reply at
all**, granted or refused, while pod 0 refused nothing. A diagnostic build logging each pre-election
round showed why:

- pod 2, central, asking its one live voter, pod 0: the round's deadline was 100 ms, and it ended with no
  reply at 78, 80 and 83 ms. The reply takes pod 0's 80 ms egress plus the path.
- pod 0, outranked, asking pod 2: the deadline was 126 ms, and the reply came at 84 ms. It won.

## Root cause

A progress-extending budget considers an extension at its lookahead, three quarters of the deadline.
`DispatchWait::judge` stopped the collection whenever the extender answered `Expire`, and a witness that
has never advanced is not progressing. So a dispatch that had gathered nothing stopped at the lookahead.
That contradicts the documented contract in three places:

- `ExtensionOutcome::Expire`: "let the hard timeout end it";
- `CommitBudget::with_extension`: "a stalled dispatch is left to time out at the current deadline";
- `round_budget`: "a round is given the slowest voter's round-trip tail before it can be judged stalled
  with no reply at all".

A round trip in the last quarter of a round's base was always cut off whenever no other reply had come
first. That is the case of a candidate whose only live voter is the far one after a leader's loss. The
2026-09-14 WAN fix (`docs/bugs/2026-09-14-consensus-round-expires-inside-the-wan-rtt.md`) raised the base to
the measured tail, which moved the stop to three quarters of the tail. On a low-variance path near one
heartbeat, such as KIND's 80 ms under a 100 ms base, every reply still landed after the stop.

Record commits were spared by accident: the owner's own acknowledgement counts as the first progress.

## Impact

- Every consensus group whose candidate has one live voter at between three quarters and the whole of its
  round's base: after a leader loss, a central candidate failed every pre-election.
- A candidate with a slower path, and so a longer base, succeeded instead: priority was inverted a second
  way.
- On KIND, 13–25 s to a successor.

## Fix

A dispatch that has gathered nothing is given its whole current deadline for a first reply, and is never
extended. One that gathered and then stalled for a stall window stops where the extender judges it, as
before, so a round with a dead voter is not lengthened (the 2026-09-12 defect,
`docs/bugs/2026-09-12-broadcast-waits-out-dead-voter.md`). The content round's lookahead is its deadline
(1/1), so the seal hedge is unchanged, and its test still holds.

Two failing tests first, by use over the fabric with the real `broadcast` (`crates/cluster/tests/extend.rs`):

- `a_round_is_given_its_whole_base_deadline_for_a_first_reply`: one voter silent, one answering at 13 ms
  of a 15 ms base with a 3/4 lookahead. Before the fix the round stopped at 12 ms without the reply.
- `a_round_with_no_reply_at_all_ends_at_its_base_deadline`: the round is not extended. Before the fix it
  ended at 12 ms.

## Measured after (KIND, the same profile, a fresh fleet per trial, 2026-09-29)

| Build | Successor | Successor latency | Pre-elections by the successor |
|---|---|---|---|
| lease fix and round fix | the central survivor, 6 of 6 | 1.57–4.96 s, median 3.08 s | 1–3 |
| round fix, no lease fix (`462b63d` with the fix applied) | the outranked pod 0, 6 of 6 | 4.97–7.58 s, median 6.47 s | 1–2, after refusing the central one's (leased) |
| lease fix, no round fix (two trials, one fleet each) | the outranked pod 0 once, the central pod 1 once | 13.4 s (pod 0) and 25.4 s (pod 1) | 5 and 14 |

With both fixes, 4 of the successors' 10 pre-elections still drew no reply within the deadline. In the one
logged, the reply took over 127 ms against a 124 ms deadline, a tail event on the measured path. The design
drops a late pre-vote reply, so the candidate waits out a timeout for its next campaign (GAPS).

## A test's bound it exposed

The WAN election fabric's killed-leader test (`crates/cluster/tests/wan_election.rs`, seed 11) failed after
the fix. The successor was elected 6.9 s after the death, against a bound of twice base plus span (6.4 s).
Both survivors' first pre-elections now collected each other's grants, which arrive in the round's last
quarter, so both went to a real election and the vote split. The retry won. Before the fix, one side's
grant was dropped, which hid the split in this seed.

The bound was wall-clock and omitted each campaign's own rounds, about 0.65 s here, which run inside one
coordinator period. Under the old code other seeds exceeded it too. Over 40 seeds (`run_scenario` for seeds
0–39, measured by a temporary loop not kept in the tree):

| | Median | Maximum | Seeds over 6 s | Campaigns |
|---|---|---|---|---|
| with the fix | 3.33 s | 9.3 s | 8 | 88 |
| before it | 3.42 s | 11.9 s | 6 | 95 |

The test now asserts the budget as the timer counts it, which is its stated intent: the first campaign within
one derived timeout of the death (base + span + one period of the timer's phase), and the successor elected
by its second campaign, one retry.

## Siblings swept

- Every `DispatchWait` user was checked: `broadcast`, `collect_bound` (records and content), `collect_promises`
  and `collect_ledger_promises`.
- Only a dispatch with nothing gathered changes behaviour. Record commits and promise collections count the
  owner's or a holder's acknowledgement first. The content round's lookahead is its deadline.
- The daemon's in-process fleet suite passes 53 of 53 (210 s, against 204 s before).
