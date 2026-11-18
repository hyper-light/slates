# A restarted owner's live log shipped nothing once its holders came back (2026-11-18)

Contracts: §4.10 live shipping (A-200: a holder off the chain or behind the retained tail is installed; A-240: a log is
durable only once its last whole entry covers every candidate's key), §4.8 "Recovery" and A-344 (a live log survives
its owner's warm restart).

## Description

`a_write_answered_after_a_live_owners_warm_restart_survives_its_takeover` (`crates/server/tests/fleet.rs`) went red on
main: the write after the owner's warm restart was refused `DurabilityPending` and the seventh file was missing after
the takeover. Main bisected it to A-376 (e3103554a, a holder publishes what changed of its content hold): green at
3f2011cc2, red at 691596dd9, green with e3103554a reverted there. Every red run printed the same held-reply line, on
main (the bound forced to 20 s, ef0c40e29) and in g230's four runs on the Mac Studio (`~/.cache/g230-logs/main-a344-1.log`,
`main-a344-2.log`, `whole-4-...`, `whole-5-...`):

    target 7; head Some(7); durable None; whole Some(6); owes whole true;
    holders ["...@None", "...@None"]; lanes []; unreachable []; head placed true;
    active set recorded true (at Some(5)); covers candidates false; fenced None

## Root cause

Read from that state and the code (no counter printed more):

- The restarted owner's log is resumed past position 5 (A-344). Its first whole entry, 6, is made right after the
  restart, before the owner's pairs are delivered to the candidates again, so it carries no candidate's key
  (`covers candidates false`). The write appends 7.
- The holders come back (`reached_volume`). Each has reported nothing on the new chain, so it is owed an install
  (`@None`). The last whole state (6) is behind the head (7), and the rule then was that such a holder waits for an image
  made at the head (`owes_image`).
- Once the pairs are delivered the period owes a whole record that covers the keys (`owes whole true`).
  `refresh_image` makes no image while a whole record is owed, and the record waits for credit
  (`Shipper::admits_checkpoint`: entries 6 and 7 in flight pass the credit 6 set).
- The credit is freed only when the holders take 6 and 7, which needs the installs: `lanes []`, nothing in flight, for
  the whole bound.

A-376 did not change any of these rules. Its cheaper holder publications changed the order in which the restart's steps
meet: the holders came back after the write instead of before it.

## Impact

After a live owner's warm restart, no write to the volume became durable and every one was refused at the held-reply
bound; a takeover then lost them (NoLoss through the refusal: the client was told, never a false success).

## Exact edits (A-428)

- `crates/db/src/live.rs`: `Shipper::retains_after(position)`, whether the retained tail holds every entry after it.
- `crates/server/src/live_ship.rs`: `reached_volume` owes an image only for a whole state the tail no longer reaches
  past; a holder is otherwise sent the last whole state and takes the entries after it from the tail.
- Test: `live_ship::tests::a_returned_holder_owed_an_install_is_sent_the_last_whole_state_while_the_tail_reaches_past_it`
  (red on the old rule: no install sent).
- `docs/wip/SLATES_DESIGN.md` (§4.10 status, A-428) and `docs/wip/GAPS.md` (A-344's entry, with the sibling still open).

## Siblings, reported

- With the retained tail cut past the last whole state, a whole record owed (for the keys, or after a refused
  publication) and a full credit can still meet the same wait (GAPS, A-344's entry).

## Evidence

Kiln on the Mac Studio (d1323898), 2026-11-18, a debug build of f7e4c54d8 from a clone of the pushed branch: the
`live_ship` unit tests, 18 of 18 green, the new test among them; with the old rule (an image owed for any whole state
behind the head) restored in a copy, the new test failed as the fleet runs did: `each returned holder is sent an install:
[]` (0 installs, 2 expected). The fleet test itself, `a_write_answered_after_a_live_owners_warm_restart_survives_its_takeover`,
is main's to run (`run-requests.md`).
