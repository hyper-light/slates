# The readmission test's wait missed a detection judged by the detector's timer (2026-11-23, §4.8, A-351, A-363)

Found by use: `crates/server/tests/fleet.rs`'s
`a_retired_owner_whose_takeover_gave_up_is_readmitted_once_it_is_heard_again` (A-351) failed 2 of 6 runs on main under
four-CPU contention, and once in a loaded run of eight fleet tests with 0a79cc183's fix. It is a defect of the test's
wait, not of the fleet. No register invariant was at risk: in every failing run the survivors condemned the cut-off
owner within the bound their detectors stated, the successor's takeover round was still waiting its promises out, and
the test's later steps held (the fleet settled on one configuration once the owner was healed).

## Description

The test cuts a, the council's leader, off on the probe plane, its record plane whole. It waits for the volume's
successor to give the volume up (A-304's promise wait), heals a, and expects one configuration of all three. The wait
for the give-up had one budget from the cut: three times the largest failover delay any node reads, in coordinator
periods (`promise_wait_periods`), read again at each look. The three came with the test (A-351, 2026-11-18) and has no
derivation at its site. In the failing runs that budget ran out at 145 to 147 periods, 14.7 to 14.8 s, with the
successor's round still waiting.

## Measured

On adacachy2 (x86_64, 32 cores, shared with other work), the release build, the test alone, confined by `taskset` to
CPUs 0-1 (runs 1 to 10 of each round) and 0-3 (runs 11 to 20). Round 2 ran a diagnostic build that logged, once a
second from the cut, each node's leadership, configuration versions, failover delay and its detector's report on every
member. The diagnostic was not landed.

- Round 1: 4 of 20 failed, two at each CPU count. Round 2: 3 of 20 failed, one at two CPUs and two at four.
- The cut comes 0.9 s into the test, 0.2 s after the probe mesh formed. In all 20 logged runs a led the council at the
  cut.
- What the two survivors judged a's probes by at the cut decided each run:

  | Survivors' verdict on a at the cut | Runs | a condemned | New leader | Give-up after the cut | Result |
  |---|---|---|---|---|---|
  | an estimator's (own or pooled), both | 9 | at about 1 s | 3 to 4 s | 65 to 72 periods | passed |
  | the pair's timer at one survivor | 8 | 3 to 5 s | 5 to 7 s | 85 to 120 periods | passed |
  | the pair's timer at both | 3 | 8 s | 10 s | not by 145 to 147 periods | failed |

- A timer-judged pair's margin was 1 s at the cut, 2 s from about 4 s and 4 s from about 8 s. The survivor that
  condemned a by its own probes did so 7.16 to 7.23 s after a's last answer in the three failing runs, and 0.29 to
  0.77 s after it where an estimator judged at both.
- In all 20 runs one survivor condemned a by its own probes and the other held it dead by gossip, and every such
  condemnation came within the bound its detector stated: the closest 0.54 s within 1.03 s, the failing runs' 7.2 s
  within 56.0 s.
- The failover delay at the cut: 2.2 to 5.1 s on the survivors, 4.6 to 5.1 s on a. It rose to 6.1 to 7.9 s on the
  survivors at the condemnation and was back at about 4.6 to 5.0 s by 14 s. So the budget at its end was 3 × 49
  periods.
- At the budget's end the successor's round had restarted four times, once at each configuration version from 7 to 11,
  the last at about 10 s. In round 1's failures its current wait stood at about 38 attempts of a bound of about 46.

## Root cause

What a member's detector condemns a peer by depends on what judges that pair's probes (hyper-swim's `judging`). The
pair's own estimator judges once it configures. Before that, the member's pooled verdict judges, where its span covers
the pair. Otherwise the pair's own RFC 6298 retransmission timer judges, which "promises no bound": its timeout is
floored at RFC 6298's second until the member's pool has a verdict (`timer_floors`, `MINIMUM_RTO_NS`), and doubles at
each expiry. A peer judged by its timer is condemned only once two of its probes have run that span and their relay
stage unanswered, which measured 7.2 s in a three-node fleet.

The failover delay's detection term counts SWIM's `2(2m − 1) + 1` periods at the running tail of the answered ones
(A-363), about 0.7 s here. That describes a detection an estimator judges, not one the timer judges. The test's budget
was three failover delays as read at each look, so the detection the cut met was in none of its terms. By the time the
budget ran out the pools had formed and the delay had fallen back.

Why the survivors had no pool at the cut: a member's pool takes round trips only once its wakes and periods are
measured, and the cut came 0.9 s in. After the cut, every other period of a survivor probed a, and each of those ran a
timer's span and its relay stage, 2 to 4 s. So the pool took samples only from the other survivor's answers, about one
every three seconds, and formed about 8 s after the cut in the failing runs.

## Fix

`crates/server/tests/fleet.rs`: the wait is judged in two parts, each by the fleet's own measure.

- The detection. The test waits until both survivors hold a dead, with the file's retirement wait. "Dead" is the
  fleet's belief, which the council's death watch reads, or a already retired. It then asserts that a survivor's own
  probes condemned a, and that each that did so condemned it within the bound its detector stated
  (`assert_condemned_within_stated_bounds`). The record is new: `Daemon::fleet_condemnation` over
  `MemberPlane::condemnation`, which is hyper-swim's `condemned_after` and `condemned_within`.
- The give-up, from the death held, within `given_up_once_held_dead_periods`. That is the departure confirmed
  (`departure_confirmed_periods`: the death window, the election a departed leader leaves owed, six commit rounds and a
  period). It is split from `departure_learned_periods`, whose sum is unchanged. Then comes the successor's promise wait
  whole from the round's last restart (`promise_wait_periods`), and the period the give-up is recorded in.

Red: the test as it was failed 7 of 40 runs (above). Every logged failure came where both survivors judged a by its
timer at the cut, and no logged pass did.

Green: 40 of 40 with the fix under the same contention (20 runs on two CPUs, 20 on four, load 11 to 19, 2026-11-23
21:34 to 21:52 UTC). The detection, from the cut to both survivors holding a dead, took 0.48 to 15.2 s (4 to 152
periods). In 14 runs it met the failing condition (a condemned 7.20 to 7.27 s after its last answer), and in 2 a slower
one (15.27 s, the timer backed off once more). Each condemnation came within the bound its detector stated (49 to 99 s
in those runs). The give-up then came 53 to 89 periods after the death was held, whatever the detection had been. Its
budget was about 144 to 177 periods: with the council at its floor timing, a death window of 10 periods, an election of
20, six commit rounds of 11 and a period, then a promise wait of 46 to 79 periods and the period the give-up is recorded
in.

Red-checks, each mutation alone on a copy of the tree, two runs each on four CPUs (2026-11-23 21:54 to 22:08 UTC):

- With a successor that never gives up (`unpromised_past_the_bound`'s bound raised to `u32::MAX`), 2 of 2 failed at "a
  was cut off and the volume's successor gave it up". The wait spent its derived budget at 144 periods, 14.4 s, not
  the file's 4,000-period retirement budget.
- With a detector that states a zero bound for its condemnation (hyper-swim's `condemned_within`), 2 of 2 failed at
  "each survivor that condemned the cut-off member did so within the bound its detector stated" (2.65 s and 2.39 s
  after a's last answer, within 0 ns).

## Impact

The test failed about one run in six under CPU contention while the fleet did what it is built to do. No product code
changed behaviour: the new accessor only reads the detector's report.

## Siblings, reported and not changed

- **The failover delay omits a detection the timer judges (A-363).** While a pair a member probes is judged by its
  timer, a crash of that peer takes about 7 s to detect in a three-node fleet, and the member's failover delay says 2.2
  to 5.1 s. Every wait that reads the delay (a held reply's bound, a forward's bound, the location round's cap, a
  takeover's attempts, A-414's lease terms) reads the smaller figure for those seconds: a member's first seconds, until
  its pool has a verdict, and a pair the pool stops covering after a late answer. A-363 set hyper-swim's own
  `detection_bound` aside as a ratchet because "a quiet three-node loopback fleet read 7.0 and 14.0 s before any loss".
  Those are seven periods at the timer's 1 s and 2 s, the regime measured here. A non-ratcheting fix is in reach: count
  a crashed peer's two unanswered probes at the run the detector bounds them by now (its `longest_unanswered`: the span
  of the verdict judging the pair and the relay stage), which falls back once the pool has a verdict. That is a design
  amendment of A-363 and changes every consumer of the delay, so it is not made here.
- **The membership horizon's doc claims a bound the timer does not keep.** `lease::horizon_ns` (0.9 s) is documented as
  "the longest a peer can go unanswered before this node's detector declares it dead"; a timer-judged detection took
  7.2 s. The lease's two sides are judged from answers given and taken, not from the detector's verdict, so its safety
  does not rest on the sentence. The sentence is still untrue in that regime.
- **`departure_learned_periods` counts a detection as one membership horizon** (9 periods). Its callers would meet the
  same shortfall when a voter stops before the survivors' pools have verdicts. Not measured.
