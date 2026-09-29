# A takeover stalled when a survivor never received the head (open)

Date: 2026-09-29. Scope: the takeover's phase one (§4.8 "Promotion and takeover"): `drive_takeover`,
`takeovers` and `serve_held_promotion` in `crates/server/src/fleet.rs`. Status: **open**, reproduced
deterministically, fix designed below and not yet built.

## Symptom

After the election fix (`docs/bugs/2026-09-29-a-member-that-missed-its-promotion-refused-every-election.md`),
the three-process CLI fleet test still failed 2 of 150 runs in Linux Docker with io_uring. In both, the
council had a leader, so the dead owner could be retired, yet neither survivor ever served its volume
(`NotFound` on both, no volume in either catalog).

## Reproduction (deterministic, in-process)

`a_takeover_completes_when_one_survivor_never_received_the_head`, three daemons at `f = 1`, fails in 61.8 s:
1. The owner's record session to the third node is held out (`Daemon::hold_record_session`) from before the
   volume exists.
2. The volume is created and snapshotted; its head places at `f + 1` with the second node.
3. The third node holds no record of the object at all.
4. The owner dies. No survivor serves the volume within the 60 s serve deadline.

The test is kept out of the suite until the fix lands (CI would fail on it).

## Root cause

A takeover runs phase one per object, driven by the successor: the survivor that rendezvous ranks first
among the object's remembered candidates. Two things make it impossible when one surviving candidate never
received any record of the object:

1. **A holder with nothing does not promise.** `serve_held_promotion` answers an empty reply when it holds no
   acceptor for the object, and the successor counts no promise. With `2f + 1 = 3` candidates and one dead,
   the `f + 1 = 2` promise quorum needs both survivors, so a successor that holds the head gathers one
   promise forever.
2. **A successor with nothing never learns the object.** Objects are known only to their holders (there is
   no global catalog, by design). A survivor that never received the object has no routing entry, so a
   takeover never reassigns it there and it never drives one. If rendezvous names it the successor, nobody
   drives the takeover.

The design already describes the round the implementation lacks: "each new owner runs phase one in one
batched round per register class across the neighbourhood: every holder raises its fence **for that host**
to the new epoch and reports the highest record it holds **for each object**". Its safety argument is that
"f+1 acknowledgements and f+1 replies intersect". A reply that reports nothing for an object is still a
reply, so a holder with nothing belongs in the quorum.

## Fix (designed, owed)

Bring phase one to the design's per-host batched round:
- A survivor that is a candidate for a retired host's objects asks the neighbourhood, in one bounded round
  per host, for the highest record each holder holds of that host's objects, each with the object's
  remembered cohort.
- Every holder raises its fence for that host and replies, a holder with nothing included.
- For each object reported where the survivor ranks first among the surviving cohort, it adopts the newest
  record across `f + 1` replies and re-commits it under the new epoch.

So a successor learns the objects it holds nothing of, and an empty reply counts toward the quorum. Owed with
it:
- the model tests of the register (`crates/db/src/register.rs`, `crates/cluster/tests/promote.rs`) extended
  with an empty holder;
- bounds on the round's reply (the host's objects in the neighbourhood);
- the A-9 revalidation the design already notes.
