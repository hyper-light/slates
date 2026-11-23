# A re-admitted creator refused its successor's log as not the acting owner (2026-11-23, §4.8 "Promotion and takeover", A-382)

Found by use: the nemesis of `crates/cli/tests/live_chaos.rs` on adacachy2. Between faults it waits for every copy to
reach the owner's durable head, and it panicked with "every copy caught up to chaos-nemesis-b's durable head 47538 at
fence 2, or the copies behind kept moving, within each fleet wait: … [("chaos-nemesis-c", None)]". The same panic
came with seed 14946995733817154046: "… a's durable head 1044 at fence 2 … [("chaos-nemesis-b", None)]". It is a
liveness fault, not a safety one: no invariant broke, and the fleet stayed one copy short.

## Description

A volume's owner (its creator) is cut off, its successor takes the volume over, and the creator, healed, is admitted
again under its id at the next host epoch. At that install (A-334, A-382) the creator yields the volume and lets go
the register copies that name itself. The successor's next catalog record then makes the creator a holder again, and
the successor's live log should follow, so the creator's copy catches up.

It never caught up. The creator refused every live entry the successor shipped, counted
`fleet.live.hold_refused.not_the_acting_owner`.

## Measured

- In the failing run's fleet diagnosis, the creator, at `fleet_host_epoch: 2`, still held its own epoch-1 log at head
  47200, with fence 1 and one register copy held. It had refused `not_the_acting_owner` 109 times and
  `fleet.record.refused.foreign_generation` 15 times. The successor shipped at epoch 2 to the creator and the third node,
  and the third node was caught up.
- Seed 14946995733817154046's failing run showed the same: the creator at host epoch 2, its epoch-1 log at head 143, and
  8,826 `not_the_acting_owner` refusals.
- A diagnostic build reproduced it in 1 of 3 replays of seed 14946995733817154046 and logged each step:
  - The creator's second re-admission: "install_placement shard 0 version 26 -> 34 host epoch 2 -> 3 … readmitted
    true", "let_go_own_registers shard 0 let go 2 of 2 held", and "yield_catalogued shard 1 newly 1".
  - Every refusal after it, on the creator: "hold refused not the acting owner #1 … #64: object 53b6bd33… peer
    <successor> local <creator> head copy owner None catalog copy owner Some(<successor>) creator <creator>".

## Root cause

`fleet::content_authority` takes the acting owner of an object from the holder's copy of that object's own register,
and with none, from the object's creator (§4.16: content is placed before its first record). After its re-admission
the creator held the successor's catalog copy but no copy of the head: the successor sends the catalog record to its
candidates when it makes them holders, and the head's record only when the head next changes. So the creator took
itself for the acting owner of the volume it had lost and refused the successor's log. The catalog copy named the
successor all along.

## Fix

`crates/server/src/fleet.rs` `content_authority`: a holder with no copy of the object's own register takes the owner its
catalog copy names (the same owner writes both). Only with neither copy is the creator the owner.

Test: `a_holder_with_only_a_volumes_catalog_copy_takes_the_owner_the_catalog_names`, beside AUD-29-45's two tests.
- Red on main: the successor named by the creator's catalog copy was refused placement on the creator.
- Green with the fix: it places; the creator no longer acts as owner there; and with no copy at all the creator still
  does.

Docs: §4.8 "Authority scope"'s A-382 status, A-382's amendment, and GAPS (A-382's entry).

## Impact

A volume's creator that was cut off, taken over and admitted again never became a holder of its successor's log. The
volume ran one copy short until the head's next record reached it (a seal, a later takeover), or for good on a volume
whose head did not change. That is a lost holder, and with `f = 1` the volume's next fault then cost its quorum. In the
nemesis the fleet did not recover whole within the fleet wait, and the flow failed. Each successor ship to the creator
was a refused message (8,826 in one run).

## Siblings

- A-382's own case, the superseded adopter, is unchanged: its copies are let go, and the successor's records make them
  afresh.
- Every caller of `content_authority`/`content_authorized` reads the same rule: a holder's live admission, a content
  offer or put, and a merge holder's input check.
- With no copy of either register, content placed before a volume's first record is still the creator's.
