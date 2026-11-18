# An adopted move's seal was taken again after its answer was lost (2026-11-18, A-488)

## Description

Found by g251 while building A-435 and recorded in GAPS as owed (bug 3 of goal 85). Once a planned move's new owner
serves the volume, its control shard asks the volume's owner shard each period for the volume's seal
(`moves::settle_adopted`). When that ask's answer was lost after the owner shard had taken the seal, the next period's
ask took a second seal, and the snapshot the move handed over was never dropped.

## Root cause

Only the control shard recorded that the seal was taken (`MoveAdoption::sealed`), and only from the owner shard's
answer. The owner shard took a seal at every ask that came with none recorded (`verbs::seal_adopted_head` snapshots the
volume each call). An answer is lost after the effect when the owner shard stops for a window:
`xshard::call_in_shard_time` then forgets the call, and the shard runs it once it resumes. A-435 made the ask wait in
the owner shard's own time, which closed the case of a shard that kept working, but not this one.

## Impact

The second seal superseded the first seal, not the snapshot the move handed over. The settle then dropped the first
seal once the second was placed, reported the move adopted, and the handed-over snapshot stayed with the volume for
good: memory and a snapshot the user never took, and a volume that a later move refuses while it holds a snapshot
other than its head (`MoveRefusal`'s snapshot rule).

## Edits

- `crates/server/src/state.rs`: `VolumeSlot::adopted_seal`, the seal the owner shard took for the volume's adoption
  (the head it superseded and the seal's head epoch); it lives and ends with the slot.
- `crates/server/src/verbs.rs`: `seal_adopted_head` answers the recorded seal when there is one, and records the seal it
  takes; the three slot constructors start with none.
- `crates/server/src/moves.rs`: the module, `settle_adopted` and `seal_adopted` docs say the seal is taken once.
- Test: `an_adopted_moves_seal_asked_again_after_its_answer_was_lost_is_taken_once` (`moves.rs`), red without the
  record (the second ask took a second seal).

## Siblings (found by reading the `xshard::call_in_shard_time` callers; not fixed here)

The same class: an ask whose effect runs on the target shard and whose only record of it is the answer, which a shard
that stops for a window loses after the effect.

- `shards_route.rs`, the route's `take` of an offer: the take retires the offer and hands its ends over once (A-440),
  so a take whose answer is lost leaves the client with neither the ends nor an offer to take again.
- `daemon.rs` `rebuild_remote_works` (recovery): it moves the shard's `work_home.unbuilt` out with `std::mem::take`, so
  a lost answer drops those works unbuilt. Its doc already accepts that outcome for a shard that does not answer.
- `direct_map.rs`, the delivery of a map's descriptor through the client's shard (`send_to_client`): a lost answer
  after the send leaves the caller taking a delivered map for undelivered.
