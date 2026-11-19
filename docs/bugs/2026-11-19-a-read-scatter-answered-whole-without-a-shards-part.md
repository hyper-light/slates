# A read scatter answered whole without a shard's part (2026-11-19, A-557, §4.8 "Lookup", §4.9, §4.14)

## Description

Three verbs answer by gathering a part from every shard of the daemon (`crates/server/src/verbs.rs`): a listing
(`scatter_list`), the caller's grants (`scatter_grants`) and the daemon's status (`scatter_status`). Each other shard's
part runs as a task on that shard, which borrows the shard's state. When that borrow was refused, the task still sent a
part home:
- a listing's or the grants' part as an empty one (`unwrap_or_default()`);
- a status' report as `None`, which the gather then skipped (`extend`).

The origin folded the part in as if served, and answered as if whole.

A shard's borrow is refused in four cases:
- its state is fenced: a failed consensus publication fences the control shard, which serves nothing from then on;
- the retention check that closes every borrow refused it;
- the shard's start failed and installed no state (the daemon serves the rest, counted `INIT_FAILURES`);
- the borrow is nested.

Reported as a sibling in `docs/bugs/2026-11-05-a-scatter-whose-part-could-not-be-sent-never-answered.md`.

## Measurement

`a_read_scatter_whose_part_its_shard_refused_is_refused_naming_that_shard` (`crates/server/src/verbs.rs`, a `--lib`
test). The daemon has two shards, and its control shard is fenced (`Daemon::inject_consensus_failure`, the fence a
failed publication sets). A client seated on the other shard lists the volumes, asks for its grants and asks for the
daemon's status. The client reads each answer from its own ring.

Before the fix the listing came back `Listed { volumes: [], next: None }`: complete, empty, and with no word that a
shard was missing. The test failed in 0.75 s (Mac Studio, macOS arm64, 2026-11-19, the test's commit 9892495fd over
the fix's tree with the old `verbs.rs`). After the fix all three are answered `Overloaded { shard: 0 }`, and the
asking shard counts three parts unserved. The test passes in 0.73 s.

## Root cause

The part tasks took a refused borrow, which means "no part", to mean "an empty part".

## Impact

- A listing left out every volume the refusing shard owns, and its client could take the list as whole.
- The grants left out that shard's grants.
- A status left out that shard's report, though that shard's health was the thing worth asking about. An operator saw
  a daemon with fewer shards, all of them healthy.
- In practice the refusing shard is a fenced control shard or a shard whose start failed. A refused retention check
  fences the control shard as well.

## Edits

- `crates/server/src/verbs.rs`:
  - The three part tasks now send home `Option<part>`, which is what the shard served or nothing.
  - The gathers (`gather`, `gather_grants`, `gather_status`) take the part's shard. They settle a missing part with
    the new `settle_unserved`, the way a part that could not be sent is settled (`settle_refused`): the scatter answers
    `Overloaded { shard }` for the first such shard once its last part is in. The settlement is counted in
    `scatter.part_unserved` (`SCATTER_PART_UNSERVED`), apart from a full control channel's refusal, which answers the
    same.
  - A status' report is now pushed, never extended from an `Option`.
  - Docs updated: `send_part`'s siblings `settle_refused` and `part_unsent`, and the three scatters.
- `crates/ipc/src/protocol.rs`: the doc of `Refusal::Overloaded` says that a gathered answer uses it for a shard
  whose part it lacks. The wire is unchanged.
- `crates/server/src/state.rs`: the doc of `ShardState::scatter_refusals` now covers unserved parts.
- The design (§4.9 status, amendment A-557), GAPS (the 2026-11-05 entry's reported sibling, closed) and the
  2026-11-05 record.

## Siblings found

- **A scatter's refusal is recorded as its completion (not changed).** A listing's or the grants' `Overloaded`
  reaches its client through `deliver(…, false)`. `retry_deferred` then records it (`record_completion`), so a resend
  under its id meets the refusal and never runs the scatter again. The protocol calls `Overloaded` not started and
  retryable under its id. `run_recorded` leaves such a refusal unrecorded (2026-11-17), and `xshard::AnswerUnstarted`
  delivers its own with `true`. The status scatter delivers with `true` already. This is true for a part that could
  not be sent since 2026-11-05, and A-557's unserved part takes the same path: `settle_refused`'s `paged`, and the
  refusal branch of `gather` and `gather_grants`. Reported, not changed.
- **Forwarded verbs (not changed).** A verb forwarded to an owner shard whose state refuses the borrow (fenced, or
  never started) is answered `NotFound`. `xshard::call_in_shard_time` returns the target's `with_state` result, so a
  refused borrow arrives as `None`, the same value as an owner given up, and `named_if_unshared` turns no reply into
  `NotFound`. A volume that exists on a fenced owner is reported absent, silently, the same class of fault. Reported,
  not changed.
- **Orphaned refusal marks (not changed).** `settle_refused` marks `scatter_refusals` before it checks that its scatter
  is still tracked. A part settled after its scatter was settled gone, which happens only while the daemon stops,
  leaves a mark that nothing removes. Its number is bounded by the requests in flight at that stop, and the state goes
  with the daemon. This is older than A-557: the send's wait for room reaches it too. Reported, not changed.
- **Checked, already right:**
  - The NFS host root's listing refuses when a shard's borrow is refused (`nfs::gather_entries` probes
    `state::with_state(|_| ())?` first).
  - The acknowledgement's scatter counts a refused release in `ack.release_refused`, by design (A-216).
