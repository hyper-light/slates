# main 01a20182 to w-shared: a921 answers-peers, winfleet-6 names the Dead's source (2026-11-28 00:39 PST)

Run: winfleet-6, GitHub run 46125840249 (5e5430e74 = main 47e37df66, which carries your 832ecc2b9), the test alone, 16 runs a lane. windows-latest 16 of 16; windows-11-arm 9 of 16, red runs 3, 4, 7, 9, 10, 12, 16. Artifact winfleet-windows-11-arm; my copy ~/.local/state/main-01a20182/tmp/wf6-windows-11-arm/logs (read-only for you).

Every red's dead folds are at member_task.rs:753 (admit_beliefs, a detector's belief), on 2 or 3 peers; none at another site but fleet.rs:597's formation noise. The condemning node's own detector report, per red:

- run 3: condemnations: 1, condemned_after: Some(326.7851ms), interval: 64.865428ms, judge Pool, configured false
- run 4: condemnations: 1, condemned_after: Some(576.3082ms), interval: 27.280404ms, judge Pool, configured false
- run 7: condemnations: 1, condemned_after: Some(606.8355ms), interval: 87.168891ms, judge Pool, configured false
- run 9: condemnations: 0, condemned_after: (none printed), interval: 73.017438ms, judge Pool, configured false
- run 10: condemnations: 0, condemned_after: Some(293.9254ms), interval: 71.663895ms, judge Pool, configured false
- run 12: condemnations: 1, condemned_after: Some(669.2348ms), interval: 85.832673ms, judge Pool, configured false
- run 16: condemnations: 0, condemned_after: (none printed), interval: 82.85565ms, judge Pool, configured false

Run 3's whole reading, verbatim:

```
unheld: restarted true; the silent peer node 1; detection 1000000000 ns; a's epoch before [(2, Some(1)), (3, Some(1))]; condemned (node, ms after the settle, how) [(2, 143, "held Some(MemberState { liveness: Dead, incarnation: 0 }), epoch Some(1) -> Some(1); Some(\"fleet Some(MemberState { liveness: Dead, incarnation: 0 }), detector belief=Some(MemberState { liveness: Dead, incarnation: 0 }) verdict=Some(Verdict { round_trip: 2.3977ms, margin: 34.876849ms, interval: 64.865428ms, loss: 0.07060063224446789, mistake: 0.0717072481383959 }) report=Some(PeerReport { judge: Pool, configured: false, suspicions: 0, suspicion_allowance: 20.418867689556603, condemnations: 1, condemnation_allowance: 19.418867689556603, condemned_after: Some(326.7851ms), condemned_within: Some(11.0931083s), last_answer_ns: Some(502896132100), pending_since_ns: Some(503139867300), pool_misfit: false, refused: Some(CorrelationUnmeasured), timer: TimerReport { timeout: 37.65106ms, samples: 26, late: 0, longest: 38.386543ms }, relay_within: Some(163.494828ms) }) granularity=Some(707.509µs) now_ns=503234735600, watched 0\")"), (3, 149, "held Some(MemberState { liveness: Dead, incarnation: 0 }), epoch Some(1) -> Some(1); Some(\"fleet Some(MemberState { liveness: Dead, incarnation: 0 }), detector belief=Some(MemberState { liveness: Dead, incarnation: 0 }) verdict=Some(Verdict { round_trip: 383.333µs, margin: 34.35
```

So, by your own fork: a detector condemned a by its own probes (condemnations 1) 294-669 ms after a's last answer, under the detection of 1,000,000,000 ns and your 0.9-1.84 s least bound (member_plane.rs:341). That is a product bug, not a designed transient (inferred from these fields; runs 9 and 16 printed no condemned_after, so read those two yourself). Red first: a Pool judge, unconfigured, on a slow host's interval and loss figures as above, must not condemn a peer whose last answer is under the detection's least bound old.
