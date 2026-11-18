# A superseded owner's shipper starved its own takeover round (2026-11-18, A-349)

## Description

The chaos nemesis (`a_seeded_nemesis_of_single_faults_keeps_every_register_invariant` in
`crates/cli/tests/live_chaos.rs`, seed 6876545201569229162) failed on both Linux arches (CI diag 44311638654, main
c1b33245a): a takeover gave up 2 objects (`NotRecovered`) with only one member away, and the fleet never settled. That
is within tolerance (three nodes at `f = 1`, one departed), so giving an object up there is a NoLoss defect.

## What the logs show

Replayed with a status dump and three diagnostic log lines (diag-g250-nemesis-2, CI run 44320668620, tree af97b3354,
both arches, three rounds each; 5 of the 6 rounds failed this way, the sixth later on another check). Every failing
round reads the same:

1. Fault 1 (`Partitioned(Owner, Isolated)`, 3.3 s) cuts the volume's owner X1 off. The council retires it
   (configuration v5), X2 takes the volume over at epoch 2, both survivors confirm (v8), the takeover completes (v9) and
   X1 is admitted again under host epoch 2 (v10).
2. X1 still ships its epoch-1 log. A holder answered it with the fence, so X1 knows it is superseded
   (`fleet.live.fenced` 954 to 1,013: every write refused `StaleEpoch`), yet its record period built an install for
   each holder every period (`fleet.live.install_built` 2,227 to 2,245, and 923 to 945 of them refused
   `fleet.live.hold_refused.stale_epoch` by the holder that counted them).
3. Fault 2 (`Superseded(Deafened)`, 1.1 s) retires X2 (v13); X2 refutes later. X2's objects (the volume and its catalog
   register) fall to X1, the volume's home.
4. X1's round asks X3, the other survivor, for its page every period and never sends it: the diagnostic line
   `the takeover of X2's round sent no page ... wanted links: X3: endpoint false lent_to Some("fleet.lent_to.live_ship")`
   fired at unsent 1, 2, 4, ... 512 (`fleet.takeover.pages_unsent` 213 to 928). After 100 periods (the failover SLO)
   A-304's bound gave both objects up, `reported by [X1]` alone (`fleet.takeover.unpromised.past_bound` 2).

So no survivor withheld a promise. X1's round never asked X3, because X1's record session to X3 was lent to X1's own
superseded shipper at the start of every period.

## Root cause

- `live_ship::pending_sends` never read the volume's fence (`LiveVolume::fenced`). `learn_superseded` says a superseded
  owner "ships no more of it", and `Next::Fenced` says it "must stop shipping and serving". `published`, `lane_ended`
  and `reached_volume` honoured that; the record period's visit (`period_volume`, then `pending_sends`) did not. So every
  period it sent each holder the entries (or an install) the holder lacked, and each holder refused them `StaleEpoch`.
  The period also followed the volume's candidates (which can record a holder set in a catalog that is no longer the
  node's) and made whole records and images for it.
- Each send is a lane (`ship_to_holder`) that borrows the holder's record session (`borrow_session`,
  `fleet.lent_to.live_ship`) for an exchange bounded by the liveness budget plus the transfer. The coordinator starts the
  period's lanes in `run_record_period` and then drives the takeover rounds (`adopt_and_serve`) in the same period. A
  round's page question takes the session only if it is in its link at that instant (`takeover::ask_pages`, counted
  `fleet.takeover.pages_unsent` otherwise, "asked again next period"). With a lane started just before it in every
  period, the round never found the session.

## Impact

A node that was superseded on a live volume and then admitted again under its id is a survivor of the next departure
like any member. Its takeover round of that departure could not ask a survivor it shipped the lost volume to: for an
object that falls to it, A-304's bound gave the object up after the failover SLO (NotRecovered, the volume unavailable,
within tolerance); for one that falls to another survivor, its share never completed, the departed member's retirement
stayed kept and the fleet never settled. Each period also carried a stale install (the whole volume, 340 KB here) to
each holder, which refused it: about 760 MB over the run.

## Edits

- `crates/server/src/live_ship.rs`: `pending_sends` sends nothing for a fenced volume, and `period_volume` does nothing
  for one (no candidates followed, no whole record, no image). Every other path that ships was already gated.
- `crates/server/tests/fleet.rs`: `a_superseded_owner_ships_nothing_more_of_the_volume_it_lost` (the owner cut off on
  the probe plane until its volume is taken over, healed, one write through it to meet the fence, then 20 record
  periods watched): red before (each holder refused the owner's fenced entry again every period), green after.
- Design (§4.10 chaos flows' status, A-349) and GAPS.

## Siblings found, not changed here

- **Re-adoption (owed).** With the round fixed, X1 adopts X2's registers and materializes the volume; but
  `verbs::materialize_taken_over` returns at once when the node already holds the volume (`state.by_id`), so X1 would
  keep its stale epoch-1 copy (missing X2's acknowledged writes) and its fenced shipper, which `published` never lets
  ship again. A takeover that names a node holding a superseded copy of the volume must replace that copy, its shipper
  and its kept fence with the adopted state (A-334's owed "re-adoption", g229 told).
- **A refused lane is sent again at once.** `lane_ended` sends a holder what it lacks the moment its lane ends with any
  answer, a refusal included. X2, retired but not yet aware of it, shipped its epoch-2 log to the two members, which
  refused it `not_a_member` 85,877 and 86,502 times in the run: its two sessions were out on that loop for good.
  `fold_into` already follows only an answer that moved the holder on the chain, for the same reason (982 and 5,501
  refusals in one run, 2026-11-05).
- **A round's question is starved by any per-period borrower.** `ask_pages` (and the takeover's fetch, `adopt`) take a
  session only if it is in its link at that instant. A healthy shipper re-sends only to a holder that lacks something,
  so it leaves the session free once the holder catches up; a borrower that never stops would starve the round the same
  way.
