# The failover delay left out a detection its detector's timer judged (2026-11-23, §4.8, A-363, A-882)

Found with A-351's readmission flake
(`docs/bugs/2026-11-23-the-readmission-tests-wait-missed-a-detection-judged-by-the-timer.md`), where it was recorded
as open. It is a liveness defect of an estimate. No register invariant was at risk: every reader of the delay waits at
most the delay or holds a term of it, so a figure too small gave waits up early and terms short, never an answer wrong.

## Description

Each node's failover delay, `F = D + E + R` (A-363), is the bound every wait that bounds a failover reads, and the term
of the leases that take it (A-375, A-414). Its detection term `D` counts SWIM's `2(2m − 1) + 1` periods at the running
tail of the detector's answered periods.

Until a member's pool has a verdict, hyper-swim judges each pair it probes by the pair's own RFC 6298 retransmission
timer. That timer is floored at a second (RFC 6298 §2.4) until the pool has measured the host's stalls, and doubles at
each expiry. A peer that crashes then is condemned only once its unanswered probes have run the timer's span and the
relay stage, seconds each. `D` counted none of that, so in a member's first seconds the delay every reader used was
smaller than the detection it stands for.

## Measured

- adacachy2, A-351's readmission test logged once a second over 20 runs (2026-11-23): the survivors' delay at the cut
  was 2.2 to 5.1 s. Where both judged the cut-off leader by its timer, they condemned it 7.16 to 7.23 s after its last
  answer.
- The simulated network (`crates/cluster/tests/member_plane.rs`): a young member's `D` was 19.3 to 20.9 ms. Its
  detector stated 21.0 s for the deadlines it judged by, and the killed peer was condemned 6.0 s after its last answer.
- With the first form of the fix, 20 more readmission runs logged each survivor's `D` at the cut. In the four where
  both survivors judged the leader by its timer, both read 1.00 s, the anchor's supervision (A-574). Neither had yet
  ended one period: their first probes had gone unanswered, so neither the count nor the detector had a figure. That
  form took the bound only once a period was answered, and the final form below takes it from the detector's first
  period on.

## Root cause

- `D` described an estimator's detection only. A timer-judged pair's unanswered probes are the long periods of its
  detection. `D` times answered periods alone (Karn's rule, the fix of
  `docs/bugs/2026-11-23-a-cut-off-owners-failover-delay-grew-with-its-detectors-backed-off-probes.md`), so it never
  saw them.
- Before any period was answered, `D` was absent and the estimate fell to the anchor's supervision, the watchdog's
  window. A-574 meant that for a node no peer's detector judges, not for a fleet member whose own probes are still
  unanswered.
- A-363 set hyper-swim's own `detection_bound` aside as a ratchet, after it read 7.0 and 14.0 s before any loss. Those
  are SWIM's seven periods at the timer's 1 s and 2 s spans, so that bound was reading this case.

## Fix (A-882)

- `crates/cluster/src/failover.rs` (`detection`): the term is the larger of the period count and a stated bound. With
  no period timed, the stated bound alone is the term, with zero samples as a prior.
- `crates/cluster/src/member_plane.rs`:
  - `timer_bound` gives the detector's `least_detection_bound` while any pair it probes is judged by its own timer, and
    nothing otherwise.
  - `detection(now_ns)` reads that bound live while the latest period was answered. Once a period ended unanswered it
    takes the one stated as the latest answered period ended. A member never answered keeps the first bound its
    detector stated. So no timer's doubling is taken for a detection, as item 1 rules for the period's length.
- `crates/server/src/fleet.rs` (`refresh_failover`): reads the term on the clock the plane is stepped by.
- `least_detection_bound`, not `detection_bound`, which also counts each period at the longest any ever ran. On the
  simulated network, after one detection, `detection_bound` read 28.0 s once both survivors were judged by an
  estimator again, where `least_detection_bound` read 75 to 80 ms. A later fall back to a timer would have read the old
  detection's periods.
- `crates/server/src/lease.rs`: `horizon_ns`'s doc said the horizon was the longest a peer can go unanswered before the
  detector declares it dead. It now says what the horizon is: the lease's window, judged from answers, and the
  detector's budget. It also says what the detector keeps: the bound it states, at least its `least_detection_bound`
  while a pair is on its timer.
- `crates/server/tests/fleet.rs`: `departure_learned_periods` counted the detection of a stopped voter as one
  membership horizon, nine periods. Its three callers stop the bootstrap voter as the fleet forms. It now counts the
  survivors' detection term (`Daemon::failover_detection_ns`, new beside `Daemon::failover_ns`).

Red, on main's behaviour: `a_young_members_detection_term_covers_a_detection_its_timer_judges` (each term 19.3 to 20.9
ms against a stated 21.0 s, the peer condemned 6.0 s after its last answer); and
`a_member_never_answered_keeps_the_first_bound_its_detector_stated` (no term at all).

Green, with the fix:
- The young member's term was 21.0 s and the condemnation came within it. Each term fell back to 14.6 to 14.7 ms once
  no pair was on its timer.
- The never-answered member's term was 14.0 s with zero samples, still 14.0 s after 40 s cut off while its detector
  stated 112.0 s.

Red-checked, each mutation alone on a copy of the tree:
- Without the timer's bound in the term, both plane tests fail.
- Without the never-answered member's first statement, its test fails and the young member's passes.
- Without the prior before a period, the unit test `detection_is_at_least_the_bound_a_timer_judged_pair_is_stated_at`
  fails.

## The readers, checked against the corrected figure

- **Product.** Every reader waits at most the delay or holds a term of it. None decides by taking the delay back off a
  stored time. The two that subtract it are a landing's `lease_seen_ns`, which only feeds the `longest_lease_gap_ns`
  gauge, and a test's setup. In a member's first seconds each reader now waits or holds longer, while detection itself
  takes that long. The list is in A-882.
- **Tests, adacachy2, two and four CPUs, at load 20 to 100 (2026-11-23 23:12 to 00:17 UTC).** With the first form and
  then with the final one:
  - A-351's readmission test, 20 of 20 and 10 of 10. Its give-up came 64 to 97 periods after the death was held, and
    its settling after the heal took 3.4 to 11.8 s (3.0 to 10.2 s before the change).
  - The three departure tests that stop the bootstrap voter as the fleet forms, 4 of 4 and 2 of 2 each.
  - The held-verb SLO test, which reads the delay at formation and holds three verbs for it: 3 of 3 and 2 of 2, 11.0
    to 20.5 s a run.
  - A-363's own estimator test, 3 of 3 and 1 of 1.
  - The three dead-session and forward tests that size a reader's patience by the delay, 2 of 2 and 1 of 1 each.
  - `live_chaos`'s cut-off owner, whose held write is given up at the delay, 5 of 5 and 3 of 3.
  - The server's failover unit tests, 11 of 11 each time.

## Impact

In a member's first seconds every wait that bounds a failover gave up early, and every lease and attachment granted
then had a short term. A forward was refused, a held reply was given up as `DurabilityPending`, and a takeover round
gave its objects up, each before the failover could have completed.

## Siblings, reported and not changed

- **Before the detector's first period has ended it states no bound.** The delay then takes the anchor's supervision
  (1.0 s on these fleets) until the first period ends, about a second and its relay stage after the first probe where
  that probe goes unanswered. Both survivors read it at the cut in 7 of 30 readmission runs, four under the first form
  and three under the final one. Closing it needs a statement hyper-swim does not make before
  a period ("`None` before a period"), a change to the vendored detector.
- **A bound stated at a crash is outgrown by a detection that needs further expiries.** The bound read at a crash
  counts the timer's span then. The told probe runs it doubled, and relays not yet heard from add their stage. Two
  readmission runs took 15.3 s to condemn. The bound the detector states at condemnation (`condemned_within`) held in
  every run measured, 95 of 95. The term follows a detection as the member's answered periods restate it.
- **The published report does not tell the two apart.** `FailoverReport` gives the detection term's value and the
  periods behind it, not whether the timer's bound set it. Adding it would be an appended field and a contract bump.
