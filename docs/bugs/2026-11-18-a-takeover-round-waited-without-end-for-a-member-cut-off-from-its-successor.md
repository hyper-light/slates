# A takeover round waited without end for a member cut off from its successor (2026-11-18, §4.8 phase one, A-304)

## Description

A successor adopts a learned object of a retired host once each of its recovery cohorts has given its recovery quorum
of promises (`cohort − f`; §4.8 "Promotion and takeover", A-251's counting rule). A dead member's silence ends when the
council retires it, which changes the configuration and starts the round over. A member that never answers the
successor while every other member hears it (a link cut between those two alone, a session lent away for good) is never
retired, so an object that needed its promise waited every period for as long as the daemon ran
(`fleet.takeover.unpromised` each period), and every verb on it answered a retryable `Overloaded`: an unbounded retry,
which CLAUDE.md §2 item 8 forbids whatever its cause, and which the materialization's bounds of 2026-11-09 and
2026-11-18 had already closed for every later branch of a takeover.

g242 reported it unverified on 2026-11-18 ("a takeover round's learned objects wait for their cohorts' promises each
period with no bound of their own").

## How it showed

`a_taken_over_object_a_survivor_never_promises_is_given_up_by_name` (`crates/server/tests/fleet.rs`): three nodes at
f = 1, the head held by both survivors, each survivor leaving the other's takeover pages and questions unanswered
(`Daemon::inject_takeover_silence`, new test support), the owner stopped. With the bound multiplied by 1000 the
successor answered `Overloaded` until the 60 s serve deadline (452 s of the daemons' periods at load 41 to 52).

## Root cause

`takeover::ready_objects` judged each learned object every period and counted a waiting one, with nothing that ever
ended the wait but a configuration change.

## Siblings found on the way

1. **A given-up volume was answered `Overloaded` on any fleet of more than one node.** `give_up_taken_over` (the
   materialization's bound) records the volume unrecovered on its owner shard and claims it to lookups, but a
   forwardable verb on its owner shard went through `verbs::owned_by_another_host`, which never looked at
   `unrecovered`: the verb went to the location round, met this node's own routing and answered `Overloaded` for good.
   The bound's tests all ran on a lone node, which never forwards. With the phase-one bound in place and the check
   removed, the three-node test answered `Overloaded` until its deadline (467 s of periods).
2. **A lost object is never answered either (open; g250's A-309 takes it).** An object whose cohort has fewer members
   left than its recovery quorum is counted `fleet.takeover.lost` and kept from confirmation (A-272), but is never
   recorded unrecovered: every verb on it answers `Overloaded` for as long as the daemon runs. A record naming the
   quorum was built and proven the same day (`a_taken_over_object_whose_cohort_lost_its_quorum_is_refused_by_name`,
   five nodes, the owner and one of its two other holders stopped together; red with the record removed: `Overloaded`
   to the 60 s deadline) and withdrawn before it landed, at g250's request: A-309 recovers such an object instead, so
   the refusal would only be undone. The test belongs to A-309, inverted: the object is recovered.

## Fix (exact edits)

- `crates/server/src/takeover.rs`: `PromiseWait` (per learned object: the periods it waited with no member promising it
  anew and no page of the round advancing); `ready_objects` counts them, `unpromised_past_the_bound` lists those past
  `fleet::takeover_attempts(failover_slo_ns)`, `give_up_unpromised` gives them up (one call per
  owner shard; `TakeoverIncomplete { waited_on: "promises" }`; claimed to lookups); `HostTakeover::given_up`
  keeps them out of the round and keeps the share unconfirmed (`share_done`); a restart judges them afresh. Counters
  `fleet.takeover.given_up`, `fleet.takeover.unpromised.past_bound`. `Daemon::inject_takeover_silence`.
- `crates/server/src/verbs.rs`: `owned_by_another_host` answers a volume its shard records unrecovered; a taken-over
  materialization (plain and green) clears a stale record. `crates/server/src/fleet.rs`: `takeover_attempts` shared;
  an adopted tombstone clears a stale record.
- `crates/ipc/src/protocol.rs`: `TakeoverIncomplete`'s names listed whole.

## Tests

`a_taken_over_object_a_survivor_never_promises_is_given_up_by_name` (red twice: the bound cut out, and the forwarding
check removed), `a_promise_wait_counts_only_periods_with_no_new_promise_and_no_page_advance`,
`a_round_that_lost_an_object_does_not_confirm_its_share` (extended to a given-up object).
