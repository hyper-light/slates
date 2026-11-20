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
  so a take whose answer is lost leaves the client with neither the ends nor an offer to take again. **Fixed by A-512
  (2026-11-19)**, below.
- `daemon.rs` `rebuild_remote_works` (recovery): it moves the shard's `work_home.unbuilt` out with `std::mem::take`, so
  a lost answer drops those works unbuilt. Its doc already accepts that outcome for a shard that does not answer.
- `direct_map.rs`, the delivery of a map's descriptor through the client's shard (`send_to_client`): a lost answer
  after the send leaves the caller taking a delivered map for undelivered. **Fixed by A-505 (2026-11-19)**, below.

## A-505: a direct map's send to its client (Linux)

- **Description.** The copy's shard asks the client's shard to send the copy's descriptor on the client's control
  socket (A-188). When that ask's answer was lost after the send ran, the copy's shard replied `MapUnavailable` and gave
  the copy back, so the client read by copy while the descriptor it was sent sat in its control socket, holding the
  copy's pages outside every budget, until a later receive happened to drain it. An ask made again would have sent the
  same copy twice.
- **Root cause.** As A-488's: the send's only record was its answer.
- **Edits.** `crates/server/src/state.rs`: `ClientSlot::maps_sent`, the maps the client's shard sent the client whose
  replies have not reached it. `crates/server/src/direct_map.rs`: `Delivered` lifted out of the Linux code with
  `send_once` (a map recorded as sent is answered `Sent`, not sent again), `kept_as_handed` (an answered send, or no
  answer at all, keeps the copy as handed) and `forget_replied` (a `Mapped` reply ends its map's record as
  `deliver_to` delivers it on the client's shard); `send_to_client` sends through the record; `hand_off` asks once more
  when the first answer is lost; `handed` follows `kept_as_handed`, counting `dax.local_map_handed_unanswered`.
  `crates/server/src/daemon.rs`: the slot starts with an empty record.
- **Test.** `a_maps_send_asked_again_after_its_answer_was_lost_is_sent_once_and_kept_as_handed` (`direct_map.rs`).

## A-512: the Shards route's take of an offer (macOS)

- **Description.** An exchange on the socket route asks the offer's owner shard to check the claim and hand over the
  ends (`shards_route::take`). The take retired the offer and moved its ends into the answer. When that answer was
  lost (an owner shard that stopped for a window runs the take once it resumes, after the exchange forgot it), the
  dropped answer closed the ends: a stream's other side read its end and ended the attachment, a map's copy reached its
  client never, and a claim made again found no offer.
- **Root cause.** As A-488's: the take's only record was its answer, and here the answer carried the ends themselves.
- **Edits.** `crates/server/src/shards_route.rs`: `Offer::claimed_by`, the exchange (its daemon nonce) whose claim took
  the offer; `take` claims the offer for the exchange and answers copies of its ends, refusing every other exchange
  `NoOffer`; `settle` retires the offer once the exchange handed the ends (`Handed`) or frees it for a new claim
  (`NotHanded`); the exchange (`claimed`) settles after every take it asked, in the owner shard's own time;
  `take_from_owner` asks once more when the first answer is lost. The A-440 and A-492 tests read the claim, which the
  settle now retires, where they read the retirement.
- **Tests.** `a_take_whose_answer_was_lost_is_answered_again_with_the_same_ends_and_never_handed_twice` (red on the code
  before, the stream's end closed with the lost answer) and `a_claim_settled_not_handed_frees_its_offer_and_one_handed_retires_it`.
