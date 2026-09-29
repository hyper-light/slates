# Holders promised a takeover without the lease gate

Date: 2026-09-29. Scope: the holder side of the takeover's phase one (`serve_held_promotion`, `sync_config_from_council`
and `accept_held_record` in `crates/server/src/fleet.rs`) and the owner lease (`crates/server/src/lease.rs`; §4.8
"Leases and reads", AUD-08). Found by reading the takeover path while fixing
`docs/bugs/2026-09-29-a-takeover-stalled-when-a-survivor-never-received-the-head.md`.

## Symptom

No test or run observed a stale read. The defect is a missing rule: the lease module states that "a holder answers
a promotion for a departed owner only once it has **not** answered that owner alive for the horizon", and the
state's documentation names `serve_held_promotion` as a reader of that evidence. The code did not match: only the
successor consulted it, in `takeovers`, about itself. Every other holder promised at once.

## Root cause

The owner lease holds while `others − f` of an object's other candidates have freshly confirmed the owner
(`confirmations_needed`). Any `f + 1` promotion quorum then contains a confirming holder, and that holder's refusal
is what keeps a successor from adopting while the lease holds. The argument needs **every** promising holder to
refuse while its own answers may still feed the lease.

A counterexample at `f = 1`: owner `H`, candidates `{H, S, T}`. `H` loses the council and `S` but still reaches `T`.
The council retires `H`, and `S` is the successor.
- `H`'s lease needs one confirmation. `T` keeps answering `H`'s probes, so the lease holds for up to the lease
  bound (about 0.9 s) after `T`'s last answer, and `H` keeps serving its latest state.
- `S` gated only itself (it had stopped hearing `H`), and `T` promised at once. `S` adopted with `{S, T}` and could
  advance the object while `H` still served its past.

## Fix

- `serve_held_promotion` refuses to promise while `AnswersGiven::promotion_open` is closed for the object's departed
  owner. That is, this holder answered the owner within the membership horizon and has not heard it acknowledge its
  retirement. Each refusal is counted `fleet.promotion.deferred`, and the successor retries next period.
- The configuration install records the departed owner for **every** held object it reassigns, at every holder,
  not only for the objects reassigned to this node (`departed_owners`).
- A holder drops the record when it accepts a record of the object from its new owner (`accept_held_record`), as
  the successor already did when it placed the adoption.

For a genuinely dead owner nothing is delayed. The council confirms a death over its death-confirmation window,
which already exceeds the horizon, so by the time a successor exists every holder's last answer is older than the
horizon.

## Tests

- **Failing first:** `a_holder_defers_a_promotion_while_its_answers_may_feed_the_departed_owners_lease`
  (`crates/server/src/daemon.rs`). A holder holds a departed owner's object and has just answered that owner. Before
  the fix it promised. After the fix it promises nothing, then promises once its last answer is older than the
  horizon.
- `a_takeover_prepare_binds_its_owner_to_the_authenticated_peer` still passes, so a forged prepare still promises
  nothing.

## Siblings

- The successor's own gate (`takeovers`) reads the same record, now set the same way at every holder.
- Nothing else answers a promotion: the ledger promotion stream (`LEDGER_PROMOTE_STREAM`) is served only by the
  cluster crate's test drivers, not by the daemon.
