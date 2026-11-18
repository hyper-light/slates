# A retired owner heard again was never re-admitted, and the fleet stayed split (2026-11-18, A-351)

## Description

The chaos nemesis (`a_seeded_nemesis_of_single_faults_keeps_every_register_invariant` in
`crates/cli/tests/live_chaos.rs`) ended every replay of two seeds with the fleet split for good (CI diag 44311638654,
main c1b33245a): seed 6876545201569229162 on both Linux arches, `[("3", "3", 2), ("2", "2", 2), ("2", "2", 2)]`, and
seed 7236833171758868842 on x86_64, `[("2", "2", 2), ("3", "3", 2), ("2", "2", 2)]` (council voters, configuration
members, record sessions per node, at `live_chaos.rs:716`). The two survivors held a configuration of two members; the
owner they had retired held its old one of three, alive and refuted, and was never admitted again.

## What the logs show

All three runs share one shape:

1. The first fault cuts the volume's creator off (`Partitioned(Owner, Isolated)`); a survivor takes the volume over;
   the creator is re-admitted under its id and the fleet settles.
2. A later fault cuts the adopter off (`Superseded(Deafened)` in seed 6876545201569229162, `Superseded(Stopped)` in
   7236833171758868842).
3. The adopter's takeover gives the adopted volume and its catalog register up: "the takeover of `<adopter>`'s objects
   gave up 2 of them ... after up to 100 periods" (A-304's bound).
4. The adopter is heard again and refutes its death ("holds `<adopter>` alive: Dead, incarnation 2 -> Alive,
   incarnation 3"), and the fleet never settles.

In-process (`a_fleet_settles_on_one_configuration_after_the_owner_a_takeover_made_is_retired_in_turn`, three nodes,
the adopter made deaf on the probe plane, Mac Studio, kiln r2219, 2026-11-18): run 1 of 3 reproduced it. The holder
counted `fleet.promotion.deferred` 103, the successor `fleet.takeover.page_unbound` 108 and `fleet.takeover.given_up` 2;
after the heal the survivors stood at configuration version 16 with two members and the adopter at 12 with three, for
the whole re-admission wait. Runs 2 and 3 completed the takeover while the adopter was deaf and settled.

## Root cause

Three rules met in a cycle with no way out:

- **The holder's gate counted answers that confirm nothing.** A holder defers a promotion of a departed owner's objects
  while an answer it gave the owner may still feed the owner's lease (AUD-08, `takeover::gate_open`). The membership
  plane's answer recorded `answered_alive(prober)` for every probe it answered (`member_task`'s `Fleet::answer`), though
  an answer to a host the council's configuration retired names no standing, and an owner's lease is confirmed only by
  a standing it recognizes (`OwnerLease::answered`; a `None` standing supersedes it). A deaf member hears no answer, so
  it condemns nobody (Lifeguard's local health) and probes every suspect every period. So every holder answered the
  deaf adopter each period and its gate shut again each time. The gate opened only when a page arrived between the
  holder's per-period prune of non-members' answers (`fan_configs_to_shards`, `AnswersGiven::retain_members`) and the
  adopter's next probe, a matter of phase: 2 of 3 runs opened it, 1 did not.
- **The same prune opened the gate early.** It dropped a former member's last answer the period the member left the
  membership, so a holder could promise while a lease built on its last answer to the member could still hold, for up
  to a horizon.
- **Nothing started a given-up round over but a configuration change.** A round that gave an object up keeps its share
  unconfirmed (A-304) and starts over only on a new configuration or above a fence (`takeover::begin_round`). The
  retirement it owes stays kept, and a kept retirement refuses its host's re-admission
  (`RegionalConfiguration::admits`). So the leader's admission of the refuted adopter changed nothing every period, no
  configuration came, and the round never started over.

A-349 (g250, landed) removed the other cause of the give-up the CI logs show: the re-admitted creator's own fenced
shipper took its round's record sessions every period.

## Impact

A member retired on a false death whose takeover gave anything up stayed outside the configuration for as long as the
fleet ran: alive, refuted, serving nothing, holding its old configuration, its seat lost. The fleet ran at one member
fewer than it had, every later departure counted against that, and the objects given up answered `NotRecovered` for
good though every holder of them was alive.

## Edits

- `crates/server/src/lease.rs`: `AnswersGiven::answered(peer, now, standing)` records an answer toward the gate only
  when it names a standing; `retain_members(members, now)` keeps a former member's last counted answer, and its
  announcement, until its horizon passes.
- `crates/server/src/member_task.rs`: `Fleet::answer` records through `answered` with the standing it names;
  `answer_probe`, a test seam to it.
- `crates/server/src/fleet.rs`: the per-period prune passes the clock.
- `crates/server/src/takeover.rs`: `HostTakeover` keeps the incarnation it was judged under; `begin_round` starts a round
  that gave objects up over once its retired host is held alive at a newer incarnation (`heard_since`), counted
  `fleet.takeover.judged_afresh`.
- Tests: `a_retired_owner_whose_takeover_gave_up_is_readmitted_once_it_is_heard_again` and
  `a_fleet_settles_on_one_configuration_after_the_owner_a_takeover_made_is_retired_in_turn` (`crates/server/tests/fleet.rs`);
  `a_holder_answering_a_retired_owners_probes_still_promises_its_objects` (`crates/server/src/daemon.rs`);
  `an_answer_that_names_no_standing_never_holds_a_promotion_back` and
  `a_former_members_last_answer_holds_its_promotion_until_its_horizon` (`crates/server/src/lease.rs`);
  `a_round_that_gave_objects_up_starts_over_once_its_retired_host_is_heard_alive_again` (`crates/server/src/takeover.rs`).

## Siblings found, reported

- Seeds 2120743995065985386 and 17036665960917068132 also ended unsettled, with status `?` on two or three nodes: the
  A-232 watchdog ended those daemons ("shard 1 is spinning, 1000 ms of CPU without a step"; g251's item), and under
  `Supervision::Bare` an ended daemon stays ended. The first failed after nine recovered faults, not at formation.
- The nemesis's `Superseded(Deafened)` holds its fault until a takeover is counted. Before this change a deaf owner's
  probes could hold every holder's gate shut for the whole fault, so the fault's own wait could never be met; now the
  gate opens a horizon after the retirement.
