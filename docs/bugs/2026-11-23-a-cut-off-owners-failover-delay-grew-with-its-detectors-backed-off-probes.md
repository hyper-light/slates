# A cut-off owner's failover delay grew with its detector's backed-off probes (2026-11-23, §4.8, A-363, A-354)

Found by use: `crates/cli/tests/live_chaos.rs`'s cut-off owner flow,
`an_owner_cut_off_while_running_is_superseded_and_serves_nothing_stale`. w-chaos saw it once in ten runs on adacachy2 at
load 69. It is a liveness defect. No register invariant was at risk: every invariant held in each failing run, and the
held write was refused, never acknowledged.

## Description

A reply held for its volume's durability is refused `DurabilityPending` at the failover delay from its hold, read live
from the node's own estimate (A-354, A-375). `live_ship::judge_held` compares the reply's age with
`ShardState::failover_ns` at each record period's visit to the volume. On an owner cut off from every peer, that
estimate grew faster than the reply aged. So the write was never given up: it was held until the relay healed, about
20.8 s, and then refused `StaleEpoch { current: 2 }` as the owner learned its successor's fence. The flow's bound is the
failover delay the fleet measured before the cut plus a liveness budget, 5.2 to 5.7 s.

## Measured

On adacachy2 (x86_64, 32 cores), the release build of main with log lines added (the owner's `judge_held` every second
once a reply had waited 3 s, every coordinator step of 500 ms or more, and every node's status sampled throughout):
- 40 runs of the cut-off flow, 20 confined to two CPUs and 20 to four (`taskset`), at load 22 to 67: 4 held a write past
  the bound (runs 6, 17, 21 and 29), each about 20.8 s, each refused `StaleEpoch { current: 2 }` at the heal. The other
  36 gave their first held write up at 2.5 to 3.3 s.
- The write did wait where `judge_held` judges it, and no coordinator step of the owner took 500 ms or more in any run.
  `judge_held` saw the held write every second from 3 s on, and found it under its bound each time.
- The bound grew in steps. In run 6 it was 14.2 s at 3 s held, 23.1 s at 5 s, 52.8 s at 10 s and 97.8 s at 20 s. In the
  other three it went from 10.0 to 89.9 s, from 14.9 to 58.0 s, and from 17.2 to 49.6 s.
- The owner's own status in run 6 names the term. Confirmation was 1.12 s and recovery 1 s throughout. Detection was
  2.4 s before the cut, then 6.1 s at 1.4 s, 11.9 s at 2.6 s, 21.0 s at 5.1 s, 50.6 s at 9.6 s and 95.6 s at 19.0 s: steps
  1.2, 2.5, 4.5 and 9.5 s apart.
- In 24 earlier runs at load 16 to 27, all passing, the cut-off owner's detection fell instead, to 0.02 s on thousands of
  samples: its unanswered periods were about a millisecond long.

## Root cause

The detection term is SWIM's count of periods times the running tail of the detector's period (A-363). The member plane
times that period from each period's probe to the next (`MemberPlane::period_probed`), and it took every period as a
sample however the period ended.

A period whose probe went unanswered ends at the probe's retransmission timer, and that timer doubles at each expiry
(hyper-swim's `Rto`, RFC 6298 §5.5). On a node cut off from every peer every probe expires, so each period was twice the
last, and each was folded into the estimate as the detector's pace. `failover::RunningDuration` states the rule ("only a
completed duration is a sample: one cut short is not (Karn's rule, the caller's)"), and the member plane did not apply
it. A period ended by a timer measures the timer's setting, not the detector. The passing runs show the same fault the
other way: short unanswered periods pulled the term down.

## Fix

`crates/cluster/src/member_plane.rs`: the plane keeps the period's own probe (its nonce and target) and whether an answer
to it came, directly (`Ack`) or through a relay (`IndirectAck`, which carries the probe's nonce). `period_probed` folds
the period only when it was answered (`note_period_answered`). So a member that no peer answers keeps the term it
measured while they did. Lifeguard has such a member condemn no one, so there is no detection of its own to measure.

Test, red first: `a_member_cut_off_keeps_the_detection_term_it_measured_before_the_cut`
(`crates/cluster/tests/member_plane.rs`). Three members run on the simulated network until member 1 has timed 32
periods. Then member 1 is cut off both ways while it runs, for 40 s. It sent 9 datagrams in that time, and its own
probes suspected both peers.
- On main: detection 14.66 ms on 32 samples before the cut, 124.24 s on 40 after.
- With the fix: 14.66 ms on 32 samples, unchanged.

Surface: the same 40 runs of the cut-off flow with the fix (20 on two CPUs, 20 on four, load 18 to 52) all passed. The
first held write was given up at 3.8 to 4.7 s, and the bound the owner judged it by peaked at 3.9 to 5.6 s. Main failed 4
of 40 there. The variation above the frozen detection term is the confirmation term's (the first sibling below).

## Impact

Every wait that reads the failover delay grew with the detector's backoff on a node whose probes go unanswered: cut
off, deafened, muted, or with every peer dead. That includes a held reply's bound (A-354), a forward's bound (A-352),
the location round's cap (A-317), a takeover's attempts (A-304), the telemetry horizon and a move's overdue bound
(A-525). A cut-off owner's held write waited until the heal instead of the failover delay. Its client was never told a
false success. Where the unanswered periods were short, the same waits shrank instead.

## Siblings

- The confirmation term counts the coordinator's periods (`Failover::period_began`), which stretch while a cut-off
  node's rounds wait out silent peers. That is A-363's intended starved-coordinator signal, and it is bounded by the
  rounds' budgets rather than doubling. The runs above saw it reach 4.6 s for about two seconds and fall back. Not
  changed.
- `a_retired_owner_whose_takeover_gave_up_is_readmitted_once_it_is_heard_again` (A-351) timed out its wait for the
  successor's give-up in 2 of 6 runs on main under four-CPU contention (load 34 to 87). With this fix it passed 6 of 6
  there, and failed once in 10 runs overall, in a loaded run of eight fleet tests. Its budget reads the largest failover
  delay across the nodes, and its own wait's doc records that "a successor whose estimate rose tenfold during the wait
  failed it every time". A survivor's probes of the cut-off owner expire too, so this fault is the likely cause of that
  rise. That is an inference, not traced. Not changed.
