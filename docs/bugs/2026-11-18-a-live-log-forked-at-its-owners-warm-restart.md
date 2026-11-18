# A live log forked at its owner's warm restart (2026-11-18)

Contracts: §4.10 live shipping (A-200: one chain an owner and an epoch, rank by epoch then position; A-240's recorded
holder set; A-329's Continuity), §4.8 "Recovery" (a warm restart keeps every promise the node made) and the register
invariants the db model checks: Agreement (TotalOrder), NoLoss, StaleNeverCommits, Convergence.

## Description

A live-shipped volume's log did not survive its owner's warm restart (the daemon killed, its anchor kept). Found by
the probes of A-343's test, on a Mac Studio (kiln r1978, 2026-11-18), three nodes at `f = 1` over anchors:

- **An owner restarted warm.** Before the restart its log's head was position 5, durable at 5, epoch 1; one holder held
  5, the other nothing. One write after the restart was answered (`FsDone`, 410 ms) on a new chain from position 0
  under the same epoch: the holder that held nothing took it (head 2, durable 2), and the holder at 5 refused it 893
  times `behind`. Two chains at one epoch. Phase one adopts the greatest rank, (1, 5), so a takeover then adopts the
  old chain and loses the write answered after the restart.
- **A successor restarted warm after its live takeover.** Its shipper ran at epoch 1, its host's, under the fence 2 its
  takeover had raised at the holders (`refused.no_credit`, nothing shipped), and its write was not answered within the
  client's 30 s deadline.
- **The successor's own copy of the log**, fenced at 2 by its takeover's phase one, came back from the restart at fence
  1 (or not at all, when it had held nothing).

## Root cause

- The owner's log, its shipper, lived in the owner shard's memory alone (`LiveState::shipping`). A restarted owner made
  a new one at its first publication, from nothing (`Shipper::new` with no start), so its first entry took position 0
  again under the same epoch, a second value at positions the first chain had used.
- The epoch came from `max(placed_heads' epoch, host epoch)`, and `placed_heads` is memory too, so a successor's
  promotion epoch fell back to its host's. The same rule wrote the volume's records (`fleet::owed_record`), so the
  restarted successor's catalog record, which its shipper must place before it ships (A-240), was refused by every
  holder.
- The successor raised its own copy's fence in memory and counted its own report without publishing it
  (`live_ship::fetch`), where every remote holder publishes the fence it raises before it answers (`serve_report`).
- An owner's fence, learned from a holder, lived in the shipper too, so a superseded owner restarted warm took the
  volume for its own again and answered reads from its own copy (found writing the fix's tests).

## Impact

Data loss and a forked log: a write acknowledged after a live owner's warm restart was lost when the owner was later
taken over, whenever a holder still held the old chain (Agreement and NoLoss, by the model: 790 of 800 seeds broken
with the old rule, a share of them Agreement or NoLoss). A taken-over volume could not take a write after its
successor's warm restart. A restarted superseded owner served stale reads (ReadSafety). A resumed old owner could have
had its entries taken by the successor's own copy at fence 1.

## Exact edits (A-344)

- `crates/db/src/live.rs`: `Shipper::resumed` (a restarted owner's first entry takes the position past every one it may
  have shipped, under the same epoch; nothing before it settled or durable), `Shipper::next_position`; `transferred`
  counts a resumed shipper's first entry as the floor a transition target must hold.
- `crates/server/src/live_ship.rs`: `OwnedLive`, the owner's kept piece of each live log it ships (epoch, the next
  position, the fence), carried by every publication that captures the volume (at the position its entry takes) or
  changes it (`owned_changes`), committed as the journal holds it (`owned_committed`), restored at recovery
  (`restore_owned`); `published` makes a shipper by one rule (`planned`: adopted head, else past the kept position,
  else nothing); `fenced` reads a kept fence until the shipper is made again; a newly learned fence is published at
  once (`fold_into`); the successor's own raised fence is published before its report counts (`fetch`).
- `crates/server/src/content_holder.rs`: the held image's owned kind (`KIND_LIVE_OWNED`).
- `crates/server/src/fleet.rs`: `promotion_epoch` (the volume record's takeover epoch, A-325), which `owed_record`
  writes the volume's records at as well.
- `crates/server/src/verbs.rs`: both publication paths commit the owned pieces they carried.
- Tests: the db model's restart nemesis (`a_live_log_resumed_past_every_position_it_may_have_shipped_holds_the_invariants`,
  and two mutations that must break it), `a_resumed_owners_first_entry_is_past_every_position_it_may_have_shipped`;
  in `crates/server/tests/fleet.rs`, `a_write_answered_after_a_live_owners_warm_restart_survives_its_takeover`,
  `a_successor_restarted_warm_takes_a_write_to_the_volume_it_took_over`,
  `a_successors_own_copy_keeps_its_raised_fence_across_its_warm_restart`,
  `a_superseded_owner_restarted_warm_answers_reads_with_its_successors_writes`.
- `docs/wip/SLATES_DESIGN.md` (§4.10 status, §4.8 status, the A-344 amendment) and `docs/wip/GAPS.md`.

## Evidence

On a Mac Studio, 2026-11-18 (kiln r2011, r2014):

- The db model's restart nemesis, 800 runs (`f = 1` and `f = 2`, 200 seeds each, the candidates fixed and following a
  joint transition): resumed past every position it may have shipped, none broke (7,151 restarts, 3,520 of them between
  an image and its append, 13,107 installs, 496 takeovers, 60,915 durable positions); begun again at genesis as
  before, 790 broke, 138 on Agreement and 16 on NoLoss; resumed past the durable position alone, 788 broke, 77 on
  Agreement and 7 on NoLoss (the rest of each, Convergence). The resumed shipper's first position, its in-flight count
  and its transition floor each fail a unit test when removed.
- The fleet tests, three nodes at `f = 1` over anchors, two shards each: green, the write after the owner's restart
  answered and all seven files read through the successor (4.4 s); the successor's write after its restart answered and
  all seven read (4.1 s); the successor's own copy at fence 2 before and after its restart; the restarted superseded
  owner's read answered with the successor's bytes (16.4 and 16.5 s). With every source change undone: the two writes
  were not answered within the client's 30 s deadline (both holders held the old chain at the owner's restart, so the
  new chain could not become durable; in the probe one holder held nothing, took the new chain, and a takeover would
  have lost the write), the copy came back without its fence, and the read answered `NotFound`. Each guard removed
  alone turned its own test red: the resume past the kept position, the promotion epoch in the records, the published
  own fence, the kept fence read before the shipper is made, and the learned fence published at once.
