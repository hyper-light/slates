# A scatter whose part could not be sent never answered (2026-11-05, §4.9, §4.14)

## Description

Four verbs answer by scatter-gather over every shard: `Acknowledge` (`scatter_acknowledge`), `ListVolumes`
(`scatter_list`), the grants listing (`scatter_grants`) and `DaemonStatus` (`scatter_status`), all in
`crates/server/src/verbs.rs`. Each sends a task to every other shard's control channel, and when a send is refused
(the channel full) it settles that part at once by calling its gather (`gather_acknowledge`, `gather`,
`gather_grants`, `gather_status`). But it calls the gather from inside `serve`, which holds the shard's state, and
the gather borrows the state again through `state::with_state`, which refuses a nested borrow without running the
closure. So the part is never settled.
- **Every part refused:** the reply is never delivered, and the scatter's entry (`ack_scatters`, `scatters`,
  `grant_scatters`, `status_scatters`) is never removed.
- **Some parts refused:** the remaining parts can never bring the count to zero, with the same result.

## Measurement

`a_refused_release_is_counted_and_the_next_watermark_takes_it_over` (`crates/server/tests/ack_release.rs`, written
for A-216). With the other shard of a two-shard daemon held and its control channel filled, an `Acknowledge` was
**never answered**: the raw client's wait ended `deadline exceeded waiting for RequestId { client: 1, sequence: 2 }
Acknowledge { up_to: 1 }` after 5 s (macOS arm64, 2026-11-05, main 8d7c6a10).

## Root cause

The settle-on-refusal path went through a second borrow of state the caller already held.

## Impact

A full control channel on any shard is exactly the load the bench's 8- and 64-client forms make. There, the sync
client's periodic acknowledgement, and any `slates status`, `list` or grants call, could be left unanswered until the
client's reply deadline: `Stalled` for a live daemon. That fits the `Stalled` aborts in those forms at load 23–36 on
macOS, in every arm, main included (GAPS), though they are not yet shown to be this. Each lost reply also left its
scatter entry forever, unbounded growth under repeated refusals.

## Edits

- `scatter_acknowledge`: fixed by A-216. The reply no longer waits for the other shards. A part that cannot be sent is
  settled on the state the verb holds (`release_part_back`), counted `ack.release_refused`, and the tracked entries
  are bounded (`ack_scatter_bound`).
- All four scatters now send a part through `send_part`, which handles each refusal the channel can give.
  - **Full.** The part is handed to a task of the origin shard that waits for room for half the client's reply
    deadline (`PART_ROOM_BUDGET_NS`, `xshard::given_room_within`), so the answer reaches the client before it would
    give up. Past that wait the part is settled refused (`settle_refused`, in its own borrow, delivered after it). The
    listing, grants or status then answers `Overloaded { shard }`, counted by name (`part_unsent`), when its last part
    is in. The acknowledgement counts `ack.release_refused`.
  - **Shard gone.** That happens only while the daemon stops. The entry is removed and counted (`scatter.shard_gone`)
    and no reply is sent: the client's resend under the same id, to the next daemon, answers, as for every request in
    flight at a stop.
  - **Measured on the first form**, which answered at once on any refusal:
    `every_call_ends_and_the_loop_is_never_held_across_restart_and_death` (`crates/client/tests/driver.rs`) ended
    admitted `List` calls `Overloaded` across a restart, in the first run of each loop (1 of 8, 1 of 15, 1 of 20).
    Those were refusals from the stopping daemon's gone shards, where main had let the resend answer (0 of 10 on main).
    With both cases above it passes 15 of 15. Each was never answered before, with a 5 s deadline exceeded:
  - `a_listing_whose_part_cannot_be_sent_is_refused_overloaded`;
  - `a_grants_listing_whose_part_cannot_be_sent_is_refused_overloaded`;
  - `a_daemon_status_whose_part_cannot_be_sent_is_refused_overloaded`.

  All five `ack_release` tests pass (2.91 s, macOS arm64).

## Siblings found

- In the same three read scatters, a refused part is settled as an empty part. A listing, grants or status can then be
  answered as if complete without that shard's volumes, grants or report. Not changed here; reported.
