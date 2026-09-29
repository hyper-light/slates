# A late empty fetch reply was counted as a refused join

Date: 2026-09-29. Scope: folding a voter's reply to a member's configuration fetch (`adopt_fetch`,
`crates/server/src/consensus.rs`; its callers in `crates/server/src/fleet.rs`). §4.8 learner fetch, D-14.
Found by a Linux io_uring loop of the three-process CLI test.

## Symptom

The loop ran `three_daemon_processes_deploy_a_fleet_from_one_manifest_and_survive_the_owners_death` 150 times
in `rust:1.98.0` with `--security-opt seccomp=unconfined`. Two runs, 125 and 147, failed formation at
`assert_formed`: "only superseded link work may end during formation". In each, one node's refusal counts
carried two names the allow-list does not hold:

```
shard 0 refused consensus.join.undecodable: 1; shard 0 refused consensus_join_refused: 1
```

An earlier loop of the same test, before the typed join counters existed, failed formation the same way
three times in 150 (only `consensus_join_refused` then).

## Root cause

A voter answers a member's fetch with no bytes when it has nothing newer for it: the member is caught up, or
the voter will not serve it (`serve_fetch(...).unwrap_or_default()`). A fetch round's replies are folded on
two paths.
- The on-time path, in `drive_learner_fetch` and its root counterpart, skipped empty replies.
- The late path, `fold_late_replies` for a reply that arrives after the round's budget, passed every reply
  to `adopt_fetch`.

Given an empty reply, `adopt_fetch` failed to decode it as a `Fetched`, returned `false`, and the caller
counted `consensus_join_refused`. The typed counter added for this diagnosis named the rule:
`consensus.join.undecodable`. Under load a caught-up member's reply easily misses its round, so formation
occasionally carried a refusal that was no refusal at all.

## Impact

- The status counts reported refused joins that never happened, which fails the formation check of the
  three-process test.
- No state was affected: nothing was adopted, and nothing that should have been adopted was dropped.

## Fix

`adopt_fetch` is now the one place a reply is judged, and says what folding it did (`FetchOutcome`):
- `Current` for an empty reply (the voter had nothing newer);
- `Adopted`;
- `Refused`, counted inside `adopt_fetch` under its reason and under `consensus_join_refused`.

Both paths call it the same way, and the on-time path's own empty-reply filter is gone. The undecodable reason
is split in two: `reply_undecodable` (non-empty bytes that are no reply) and `configuration_undecodable`. A
torn reply, if one ever arrives, is then told apart from an empty one.

## Test

`an_empty_fetch_reply_is_neither_adopted_nor_a_refused_join` (server lib) folds an empty reply for both groups.
Before the fix: 2 join refusals counted. After: none, nothing adopted, both outcomes `Current`.
