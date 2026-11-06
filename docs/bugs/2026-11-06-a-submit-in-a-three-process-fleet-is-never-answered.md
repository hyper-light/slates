# A submit in a three-process fleet is never answered (open)

Found 2026-11-06 by goal 4's Linux rungs on adacachy, using loadtest-scale's `crates/cli/tests/load_scale.rs` at
`712dd67b`. The fleet was one primary and two replica server processes at `f = 1`, plus a client node, over `kernel/`
of Linux v7.4-rc1. §4.16 "Commit" (AUD-11): a submit at `f > 0` commits its effects at once, but is answered only when
its version's merge record commits at the quorum (`merge_service::resolve_accepted`). In these fleets that commit
never happens, so no submit is ever answered.

## Failing test

`a_submit_in_a_three_process_fleet_is_answered_once_its_record_commits` (`crates/cli/tests/cli.rs`, gated on
`SLATES_TEST_CLI`). It uses three daemon processes from one manifest at `f = 1`, formed and bootstrapped as the
deployment flow is. On one node it creates a green, a work and one edit, then runs `submit` with a deadline of the
fleet wait (40 s). On main `d61c3be2` it fails 4 of 4 runs: the submit is not answered in 40 s. Its failure prints
each node's merge, content and seal counters and fleet view.

In-process, the same rule passes in every shape tried:
- two nodes, one shard (`a_submit_is_answered_only_once_its_record_commits_at_the_quorum`);
- two nodes, two shards, with the green on the non-control shard (the new
  `a_submit_to_a_green_on_a_non_control_shard_is_answered_once_its_record_commits`);
- three nodes, local;
- three nodes plus a client node in another region, forwarded, on one shard and on seven;
- a green based on a snapshot.

Each of these resolved its acceptance after two periods of `merge.inputs_unplaced`. So the failure belongs to the
deployed fleet: separate processes under anchors, formed from a manifest.

## What the counters show

There are two stalled shapes, both in the failing test:
- **The green on the owner's control shard** (runs 3 and 4 of the test, and every load rung): the owner counts
  `merge.inputs_unplaced` every period (388 in 40 s), so it tries to put the version's inputs to the candidates. The
  candidates refuse some of those puts, `fleet.content.unauthorized` 3 each, and the owner counts
  `fleet.seal.pair_recipient_unknown` 8. `merge.acceptance_resolved` is never counted.
- **The green on another shard** (one run): the owner counts `merge.acceptance_deferred` 1 and nothing more. No
  `inputs_unplaced` and no content put, so the record plane never works the version.

The load rungs show the first shape at every size:
- 1 client: `inputs_unplaced` 454, with the tree on an overlay;
- 1 client: 607, with the tree written into a plain volume, so the overlay and the pin are not the cause;
- 30 clients: 591;
- 300 clients: 212 deferred, 0 resolved.

A CLI `submit` on the owner itself, before any client ran, had not returned after 600 s, so the client node and
the forward path are not the cause either.

Ruled out, each by a run:
- the tree's size (45 files fail as the whole of `kernel/` does);
- the overlay and its pin;
- forwarding and the client node;
- the shard count in-process;
- load (1 client fails);
- the access rule judging the creator (the green's creator equals the owner's current member id).

Instrumenting `content_authority` at the holder's serve path printed nothing in a run with no `unauthorized` count.
The puts that were refused in other runs were not captured with their reason.

## Consequences seen in the load rungs

The client never hears its submit. Its retry with the same request id waits on the same commit. A harness that
gives up on the retryable refusals (`Overloaded` after the 1 s liveness budget, `verbs.rs` `await_deferred_completion`)
never learns that the versions were accepted. Its oracle then reports later conflicts against those versions as
false. That was 787 and 720 "oracle failures" at 1,000 clients and 766 at 300.

That is a second defect, in the harness: `load_bench.rs` `Agent::refused` gives up on refusals the protocol calls
"retryable with the same request id". loadtest-scale owns that file and has been told. A local patch that retries
under the same word confirmed that the submits stay unanswered: 300 of 300 at 300 clients, each after one retry.

Under the anchor's fixed 1,000 ms heartbeat budget, the loaded client node was killed 3 times, once, and 47 times in
different rungs. That is the open defect `00e5269c` records, and it is not this one.

## Owner and next step

This is the merge record plane (`merge_service.rs` `next_merge_work` and `run_merge_period`, driven from
`fleet.rs` `run_record_period`) together with content authority (`fleet.rs` `content_authority`) and pair delivery
(`deliver_pairs`). It goes through the supervisor to its owner. Owed: name which stall each shape is, from each
holder's refusal reason and the owner's record period, then fix it with this test red first.
