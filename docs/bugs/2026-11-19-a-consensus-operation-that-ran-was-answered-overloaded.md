# A consensus operation that ran was answered Overloaded (2026-11-19, A-566, §4.8 explicit recovery, §4.9 RIFL)

## Description

The human consensus operations, `Bootstrap`, `RecoveryPlan` and `Recover` (`serve_consensus_control`), run through
`consensus_on_control` (`crates/server/src/verbs.rs`). The operation runs on the control shard. A bootstrap or a
recovery that succeeds is then published to every other shard, and each shard's application is awaited in that shard's
own time (`xshard::call_in_shard_time`). When a shard did not apply the publication, the answer was
`Overloaded { shard }`. `Overloaded` says the request was not started, but here the operation had already run and stood
on the control shard.

Since A-563, an `Overloaded` reply is never recorded, so the resend that the refusal invites ran the operation a second
time. Before A-563, the refusal was recorded, so the resend met the refusal for good and never learned that the
operation ran.

Found while fixing A-563 and filed there.

## Measurement

`a_recover_answered_after_it_ran_says_so_and_its_resend_takes_effect_once` (`crates/server/src/verbs.rs`, a `--lib`
test). The daemon has three shards, and its third shard is fenced, so the control shard's publication to it is refused.
A client seated on the second shard recovers the root group as its first voter, from a plan reviewed on the control
shard, then sends the same `Recover` again under its id.

Before the fix the Recover was answered `Overloaded { shard: 2 }`, naming the fenced shard, though the root group
had been replaced: the test failed in 0.74 s (Mac Studio, macOS arm64, 2026-11-19, the test's commit a58950843
with `verbs.rs` and `protocol.rs` taken from it). After the fix the answer is `DurabilityPending`, the resend under
the id meets that recorded answer, and the root group was replaced once: the test passes in 0.75 s. Next to it, both
`consensus_recovery` tests, the A-557 and A-563 tests and `a_resend_under_the_id_of_an_overloaded_verb_runs_the_verb`
still pass (5 of 5).

## Root cause

The answer after a publication was given up named the shard that refused it, with the refusal for a request that was
never started.

## Impact

- A `Recover` or `Bootstrap` that ran was reported as not started. Since A-563 the resend ran it again.
- Each operation meets a second run as the first's outcome, by reading its own state:
  - a recovery with the same digest returns its receipt (`consensus_recovery::recover`);
  - a bootstrap already authorized is acknowledged (`consensus::bootstrap`);
  - a plan is a read.

  So no second effect was found. But the answer never said that the operation had run. Before A-563 a resend met the
  refusal for good, so the operator could not learn that the recovery had taken effect.

## Edits

- `crates/server/src/verbs.rs`:
  - The new `published_or_pending` sends the publication. When a shard did not apply it, the answer is
    `DurabilityPending { held_ns }`, with how long it waited since the operation's answer. That answer says the
    operation ran and stands on the control shard. It is the request's recorded outcome, so a resend under the id meets
    it, and a new id runs the operation again, which publishes again.
  - `consensus_on_control`'s doc states which of its answers are `Overloaded`: only an operation that has not run.
- `crates/ipc/src/protocol.rs`: `Refusal::DurabilityPending`'s doc now names this use. The wire is unchanged.
- The test above. The design (§4.9 status, amendment A-566), GAPS (the A-563 entry's "open beside it", closed) and this
  record.

## Every caller checked

`consensus_on_control` has one caller, `serve_consensus_control`, with three operations:
- `Bootstrap`: a second run with the same `root` is `Acknowledged` (`bootstrap_authorized`), and with the other is
  `ConsensusAlreadyInitialized`; neither changes anything.
- `RecoveryPlan`: a read; nothing to publish.
- `Recover`: a second run with the same digest returns its receipt before anything else. A changed state refuses
  `ConsensusRecoveryStale`.

## Siblings found

- **A wedged control shard (not changed; A-65's rule, pinned by
  `a_bootstrap_whose_control_shard_is_wedged_is_refused_after_a_window` in `crates/server/tests/cross_shard_wait.rs`).**
  Given up after a window off the CPU, the control call answers `Overloaded`. The operation it carried has not run when
  that answer is given, but it stays queued and runs if the shard's poll ends. The resend then runs it a second time,
  which each operation meets as above, so it takes effect once.
- **A forwarded submit answered `Overloaded` after it ran (not changed).** §4.9's 2026-11-06 status: its acceptance
  outlived the owner's budget, and its completion is recorded at the commit, which the resend meets. So the answer says
  not started for a verb that ran, though no second effect follows.
