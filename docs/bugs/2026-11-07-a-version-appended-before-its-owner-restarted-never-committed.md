# A version appended before its owner restarted never committed

Found 2026-11-07 by goal 4's primary-kill rung on adacachy (AMD Ryzen 9 9950X3D2, Linux 7.2.9-2-cachyos). The rung ran
loadtest-scale's `crates/cli/tests/load_scale.rs` at `5cd6db04` (main `e86fb417` with the one-exchange commit): three
servers at `f = 1`, one client node, 1,000 clients over `kernel/` of Linux v7.4-rc1, `--per-connection 32 --hot-files
800 --reply-patience 60 --think-us 0`. The primary's daemon was SIGKILLed 5 s in (`SLATES_LOADTEST_KILL_NODE=0`), and
its anchor restarted it.

- The same rung killing a replica (`KILL_NODE=1`) was clean: 31.5 s, oracle 0 failures, 3,000 submits accepted.
- The primary kill ran 1,122 s. 1,427 submits were accepted, 80,158 were retried, every remaining submit was "still
  refused retryable after" about 120 s, and the oracle found 1,770 failures.
- The 3,000-client primary kill at 40 s failed the same way.

The oracle named the defect. The accepted versions ran 1 to 506 and then jumped to 515 ("accepted versions are not
1..=V: position 507 holds 515"). Every version between was "on the green but no submit of this level, known or
unknown, explains it": those submits' effects committed, and no client was ever told.

## Cause, from the code

§4.16 "Commit" (AUD-11) answers a submit at `f > 0` only once its version's merge record commits at the quorum. The
records an owner still owes its holders (`MergeShardState::pending`) and the submits waiting on them live in memory.

On a restart, `verbs::rebuild_green` replays each green's chain into a fresh engine. Nothing rebuilt the pending
records: only the verbs (`enqueue_record`, at a green's creation and an accept) and a takeover ever set them.

So a version appended durably before the crash, and not yet committed at `f + 1`, was never shipped to the holders
again. A retried submit for it met the restored dedup set, was accepted as that version again, and waited for a
placement that never came. Each retry was answered `Overloaded` until the harness gave up. A replica kill does not
reach it, since the owner's memory survives.

## Failing test

`a_version_appended_before_its_owner_restarted_commits_once_it_is_back` (`crates/server/tests/fleet.rs`). The setup is
a settled three-node fleet at `f = 1` whose nodes keep their state across a restart (each segment carries its content
object, `common::anchor::held_anchor`), and a green with a work and one edit.

1. Both holders withhold merge records (`MergeFault::refuse_records`), and the work is submitted. Version 1 is
   appended, and its submit waits (`merge_awaiting` is 1).
2. The owner is restarted over its own segment.
3. The holders' fault is lifted.

It expects version 1's record to commit at the quorum within the placement deadline (60 s). Before the fix it never
did: the restarted owner shipped nothing, and its counters were empty. After, it commits, and the test ends in 3.3 s.

## Exact edits

- `crates/server/src/verbs.rs`:
  - `replay_green` returns, beside the engine, each replayed version's merge record fields: version 0 when no
    checkpoint stands for it, then every chain entry, each with the green's head identity at that version.
  - `rebuild_green` hands them back.
  - The restart path (`rebuild_recovered`) re-enqueues them. The takeover path does not, since it adopts its placed
    head under its own epoch.
- `crates/server/src/merge_service.rs`:
  - `enqueue_recovered_record` and `ReplayedVersion`.
  - `enqueue_record` and the recovery share `enqueue_record_with`, which asks for the head identity only for a record
    that goes to holders.
  - A re-enqueued record is placed from the configuration the owner holds when it ships (`PendingGreen::retarget`).
    A holder that already holds a version acknowledges it again at no cost (`accept_merge_record`'s already-applied
    branch).
- The test above. §4.16 status, A-246, GAPS.

## Verification

See the commit. The kill rungs are rerun on the fixed build.
