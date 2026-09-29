# A takeover stalled when a survivor never received the head (open; fix being built)

Date: 2026-09-29. Scope: the takeover's phase one (§4.8 "Promotion and takeover"): `drive_takeover`,
`takeovers` and `serve_held_promotion` in `crates/server/src/fleet.rs`, the routing view
(`crates/cluster/src/routing.rs`), and the regional configuration (`crates/db/src/register.rs`). Status:
**open**, reproduced deterministically; the fix is designed below and being built in steps.

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
   promise forever. This is not a corner: a record commits at `f + 1` of `2f + 1`, so up to `f` candidates
   may hold nothing of it, and a successor must be able to count them.
2. **A successor with nothing never learns the object.** Objects are known only to their holders (there is
   no global catalog, by design). A survivor that never received the object has no routing entry, so a
   takeover never reassigns it there and it never drives one. If rendezvous names it the successor, nobody
   drives the takeover.

## Why a local fix is not safe

The obvious patch lets a holder with nothing promise, and lets holders tell an empty successor about the
object. That patch is safe only if every survivor agrees on each object's candidate set (its **cohort**) and
that cohort holds the object's newest committed record at `f + 1`. The implementation guarantees neither.

- **Cohorts disagree after a neighbourhood change.** A holder remembers the cohort computed at the generation
  of the last record it accepted. When a member joins or leaves, the owner's neighbourhood is refixed at once,
  and the owner ships its head only to candidates that have not acknowledged the head's sequence
  (`unplaced_heads`). So a holder in both the old and the new cohort that acknowledged under the old one keeps
  the old cohort, while a new candidate remembers the new one. When the owner dies they rank different
  successors. At `f = 1` in three nodes with one restart, this can leave both survivors stalled. Above that it
  can let two successors each gather `f + 1` promises.
- **A change in flight.** Even the owner's latest cohort is not enough. Suppose the neighbourhood changed at
  the last version before the owner died, and the owner had not re-placed its newest record. A quorum of the
  new cohort can then miss that record. At `f = 2`: the record is at `{H, B, C}` of the old cohort, the new
  cohort is `{H, A, D, E, F}`, and D alone received the re-ship. A quorum `{A, E, F}` of the new cohort, with
  E and F empty, adopts A's older value. Without empty promises this history stalls; with them it loses the
  record.
- **Repeated takeovers.** Suppose a successor S dies before re-committing an object it was taking over from H.
  Then the holders that saw S's adoption and those that did not follow different lineages, and can elect
  different successors.

The design already answers all three. §4.8 is Vertical Paxos II with the owner as leader-acceptor: "while a
change is in flight the owner writes to a quorum of the old candidates and a quorum of the new ones (joint
writes); the group retires the old set only after the owner has acknowledged the new configuration and the
newest committed record is held by f+1 of the new candidates", and the new owner "only then serves under
confirmed authority". The implementation had none of that bookkeeping: its per-object remembered cohort
(`docs/bugs/2026-09-17-takeover-ranks-an-empty-replacement.md`) stood in for it.

## Fix (designed; being built)

Everything a takeover decides comes from the committed configuration, so every survivor decides the same.

**1. Settled neighbourhoods (the old set, retired only on the owner's acknowledgement).**
- Each member's neighbourhood keeps the version at which its host set last changed.
- Each member also has a **settled** neighbourhood: the set its records are all placed on, with the failure
  domain of each host (so its cohorts can be recomputed after some of those hosts leave).
- A new member's settled neighbourhood is its first one, since it owns nothing yet.
- While a member's current neighbourhood differs from its settled one, the member commits every record at
  `f + 1` of the settled cohort **and** `f + 1` of the current one (joint writes). It also ships its existing
  heads to the new candidates.
- Once every object it owns is placed at `f + 1` of the new cohort, it reports the new neighbourhood's version.
  The council then commits `Settle`, and the new neighbourhood becomes settled.
- So every record a member ever committed is at `f + 1` of its settled cohort.

**2. Retirement records.**
- Retiring a member moves its settled neighbourhood into a retirement record, with the version that retired it.
- The record also holds the successors that have **confirmed** their share of its takeover, and the retired
  hosts whose takeover it had not itself confirmed when it died (its **unconfirmed** lineages).
- A record is dropped once every surviving host of its settled neighbourhood has confirmed or retired, and no
  other record lists it as unconfirmed.

**3. The recovery cohort, agreed.**
- The objects of a retired owner `D` are recovered through `D`'s settled cohort. If `D` had unconfirmed
  lineages, they are recovered jointly through those lineages' settled cohorts too, closed transitively.
- Recovery needs `f + 1` promises from the survivors of **each** of those cohorts.
- The successor is the survivor that rendezvous ranks first over the union of those cohorts.
- All of it is computed from the configuration, never from a holder's memory.

**4. Phase one, one batched round per departed host** (the design's shape).
- Every survivor in the union of `D`'s recovery neighbourhoods runs one round for `D`: it asks every other
  such survivor for the records it holds of `D`'s objects whose successor is the asker.
- The prepare names `D`, the asker's epoch and the generation. Each holder:
  - applies the lease gate for `D`;
  - raises the fence and installs the asker's authority on each object it lists;
  - replies with those objects' highest records, paged under a derived bound.
- A holder's complete reply is its promise for **every** object of `D` whose successor is the asker. An object
  it does not list is an empty promise, which the generation fence makes binding: `D` is no member, and only
  the agreed successor writes the object. A holder whose fence on a listed object is above the asker's epoch
  says so, and the asker retries above it.
- For each object it learns, the successor adopts the newest record once it holds complete replies from
  `f + 1` survivors of each recovery cohort. It then re-commits the object under its own placement and
  serves it.

**5. Confirmation.**
- A survivor confirms its share of `D` once it holds complete replies from every surviving host of `D`'s
  recovery neighbourhoods, and has adopted every object that ranks it first. An object with fewer than
  `f + 1` survivors in one of its cohorts cannot be recovered, and is counted.
- The council commits `Confirm`.
- A survivor that dies before confirming leaves its own objects to a joint recovery with `D`'s cohort, which
  covers an object it had not adopted yet.

**6. Holds reclaimed.** A holder drops a stale copy of a departed owner's object once the object's successor
has confirmed and the holder is not in its new placement. That bounds the old copies that today are never
reclaimed.

**7. The lease** counts confirmations over the owner's **settled** cohort, which is the set a successor's
promotion quorum is drawn from.

It replaces, not layers:
- the per-object `drive_takeover` and `serve_held_promotion`;
- the routing's remembered recovery cohort;
- the successor-only discovery that made an empty successor impossible.

## Steps

1. Configuration: neighbourhood change versions, settled neighbourhoods, retirement records, `Settle` and
   `Confirm`, their codec and bounds.
2. The owner's side: joint writes while unsettled, re-placement, the settlement report, and the council's
   `Settle`.
3. The takeover: the per-host round, the agreed successor, adoption, confirmation, reclaiming holds, and the
   lease over the settled cohort, replacing the per-object path.

Owed with it:
- the register and cluster model tests, extended with empty holders, neighbourhood changes and repeated
  takeovers;
- this test (both cases: a successor that holds the head, and one that holds nothing);
- a neighbourhood-change takeover and a repeated takeover in the fleet suite.

The A-9 revalidation the design already notes still stands.
