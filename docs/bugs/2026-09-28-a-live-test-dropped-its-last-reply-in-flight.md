# A live test dropped its last reply in flight

Date: 2026-09-28. Scope: `crates/cluster/tests/raft_live.rs`, a test harness. No production code is involved.
Found when the gate for the priority-elections change (`docs/wip/research/consensus-enhancements.md` §3.4)
hung.

## Symptom

`cargo test -p slates-cluster` stopped making progress in `raft_live`. The one test,
`a_candidate_wins_an_election_and_replicates_over_the_transport`, ran for 11 min 50 s at 95% CPU before the
gate was stopped. Run alone under a 90 s alarm, it hung again. `sample` showed the test thread in
`SimRuntime::run_until_idle`, expiring timers: the simulated clock was advancing through the candidate's
retransmission timers with no progress.

Stage markers (temporary, since removed) printed: candidate established, voter established, voter served
the vote, candidate leads, **voter served the append**, and nothing after. The candidate never received the
reply to its append. Removing the priority table from the *request* did not help; the reply was the cause.

## Root cause

`Endpoint::reply` returns while the reply is still in flight, and the transport documents that a server that
stops serving must call `Endpoint::settle` before it drops the session (`crates/transport/src/endpoint.rs`,
`settle`). The voter task dropped its endpoint straight after its last `serve_raft_once`.

That held only while each reply fit the first flight. At this test's frame cap (a 16-byte payload per
frame), a fresh session grants `REORDER_THRESHOLD + 1 = 4` packets of credit, 64 bytes. `AppendReply` was 57
bytes. The election priority it now carries (a quorum round trip and a spread, 16 bytes) made it 73. The
tail needed a window update that reached a dropped endpoint, so the candidate's request, which has no
deadline in this test, waited forever.

## Impact

A hung gate. No deployed path is affected: the daemon's serve loops keep their sessions for the session's
life.

## Fix

The voter settles before its session drops, as every sibling live test already did (`fleet_live.rs`,
`promote.rs`, `ledger_promote.rs`, `commit.rs`, `config_group_live.rs`, `root_group_live.rs`, `content.rs`,
`extend.rs`, `swim.rs`). The test passes in 0.05 s.

## Sibling sweep

Every test and example that serves (`serve_once`, `serve_raft_once`, `serve_once_async`) was checked:

- `wan_election.rs` and `class_latency_bench.rs` serve in loops for the scenario's life, so nothing is
  dropped with a reply in flight.
- `crates/transport/tests/admission.rs`'s `echo` serves once and then stops driving the server endpoint. It
  holds only because every echoed payload fits one 16-byte frame (all are 13 bytes or less). This is
  reported, not changed.
