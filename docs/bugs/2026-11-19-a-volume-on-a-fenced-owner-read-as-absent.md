# A volume on a fenced owner read as absent (2026-11-19, A-564, §4.8 "Lookup")

## Description

A verb for a volume runs on the volume's owner shard. When the owner did not run it, the verb was answered `NotFound`,
which says the volume does not exist. That happened on two paths:
- **From another node** (`serve_forward`): the call to the owner (`xshard::call_in_shard_time`) returns `None` for
  every way it can fail. It fails when the call was not sent, when the owner was given up in its own time, or when the
  owner's state refused the borrow. `named_if_unshared` turned that `None` into `NotFound`. A forwarded `PromoteRegion`
  and the partition lookup did the same when the serving control shard's own state refused them. So did a deferred
  write's completion poll (`await_deferred_completion`, whose `None` came back as no reply).
- **On the same node** (`send_forward`): the task on the owner shard turned a refused borrow into `NotFound`.

An owner's state is refused when it is fenced: a failed consensus publication fences the control shard, which serves
nothing from then on, and the control shard owns a partition of the volumes. So every verb for a volume on a fenced
owner read as absent, though the volume exists.

Found while fixing A-557, and reported in its record.

## Measurement

`a_verb_for_a_volume_whose_owner_is_fenced_is_refused_by_the_fence_never_not_found` (`crates/server/src/verbs.rs`, a
`--lib` test). The daemon has two shards. A volume is made on the second shard, and that shard is then fenced as a
failed publication fences one. The volume's status is asked by a client seated on the control shard (the same-node
forward), and as a verb forwarded from another node (`serve_forward`, run on the control shard as its fleet transport
runs it).

Before the fix the same-node ask was answered `NotFound`, and the test failed there in 0.75 s (Mac Studio, macOS arm64,
2026-11-19, the test's commit b1e7ca76c with `verbs.rs` taken from it). The assertion stopped at that first ask, so
the forward from another node was not read in that run. Its old path turned the same refusal into `NotFound` too
(`named_if_unshared`'s `None`). After the fix both asks are refused `ConsensusRecoveryUnavailable`, and the test passes
in 0.77 s. Seventeen nearby tests still pass: the forward lane's, A-394's forwarded read of a volume not yet made,
the slow-path forward, and the A-557, A-563 and A-566 tests.

## Root cause

An owner's refusal and an owner with no such volume were one answer. `xshard::call_in_shard_time` cannot tell its
caller why it has no answer, and `with_state` cannot tell why the state refused.

## Impact

- A client's read or write of a volume on a fenced owner was told the volume does not exist. A client that acts on
  that answer, by creating the volume again or by forgetting it, acts on something false.
- A forwarded write's `NotFound` from another node was the answer its origin delivered, though the owner never ran
  it.

## Edits

- `crates/server/src/verbs.rs`:
  - `refusal_of_access` answers a refused borrow by its kind. A fenced owner, or a retention check that fences it,
    answers `ConsensusRecoveryUnavailable`, the refusal a failed publication answers (`consensus_recovery::recover`).
    No state, or a borrow already held, answers `Overloaded`, not started.
  - `call_owner` runs the work on the owner with `try_with_state` and waits in the owner's own time
    (`within_shard_time`). It answers the work's result; or `Overloaded` for a call not sent or given up; or the
    refused borrow's kind.
  - `serve_forward` uses them for the read, the write, the `PromoteRegion` and the partition lookup.
    `answer_forwarded_write`, `await_deferred_completion` and `named_if_unshared` take and give a reply, never `None`.
  - `send_forward` answers a refused owner borrow by `refusal_of_access`.
- The test above. The design (§4.8 "Lookup" status, amendment A-564), GAPS (a new entry after A-394's) and this
  record.

## Siblings found

- **`remote_attach::serve_unforwardable` (not changed).** A peer's reap of its dead client answers `Overloaded` for any
  call that has no answer, a fenced owner included: retryable, and never `NotFound`.
