# A forwarded write held for its live volume's durability was answered Overloaded at once (2026-11-23, §4.8 "Lookup", §4.9, AUD-11, A-200)

Date: 2026-11-23
Area:
- `crates/server/src/verbs.rs`: `await_deferred_completion`, `attach_deferred_forward`, `deferred_in_doubt`.
- `crates/server/src/live_ship.rs`: `Awaiting::forwarded`, `await_forwarded`.
- `crates/server/src/work_home.rs`: `answer_forwarded`.
Regressed from: 1fc1da1fb (2026-11-20, "AUD-11 amended", A-697's release bench).
Found by: the fleet's A-338 and A-349 tests, `a_superseded_owners_clients_reach_its_successor` and
`a_superseded_owner_ships_nothing_more_of_the_volume_it_lost`. Both failed 2 of 2 on adacachy2 at 715bfdbe4,
1c860b667 and b9849cb39, with item 1's change reverted too.

## Description

A client of a node that does not own a live-shipped volume writes through that node:

1. The node forwards the write to the owner.
2. The owner runs it and holds its reply until the volume's log is durable (`live_ship::defer`).
3. `run_forwarded` therefore answers nothing, and the forward's exchange asks the owner shard where the deferred reply
   stands (`answer_forwarded_write`, `await_deferred_completion`).

Since 1fc1da1fb that question went only to merge_service's deferred acceptances (`await_forwarded`). A reply held by
the live log is not one of them, so the answer was `Gone`, and `Gone` was answered `Overloaded` at once, every time.
The write had run and became durable a moment later.

`Overloaded` tells a client its verb was not applied. A client that resends under a fresh id applies the write again.
The fleet tests' client does exactly that, and never saw a success.

## Measured

Two diagnostic builds ran on a scratch copy of b9849cb39 on adacachy2, never landed:

- **v1** put `#[track_caller]` on `verbs::refused`. Every `Overloaded` these tests met came from one site,
  `await_deferred_completion`'s `ForwardedAcceptance::Gone` arm: 27 and 52 logged lines, rate-limited.
- **v2** logged both ends of every forward.
  - The successor's `serve_forward` answered every forwarded `FsWrite` `Overloaded { shard }`, about 11,000 times in
    each test. Some came from the superseded owner's client, some from the third node's.
  - The forwarded `Attach` and `DetachOf` were answered.
- The successor's counters in an earlier failing run show the writes ran: `fleet.live.appended` 13,897,
  `fleet.live.durable` 13,862.

## Root cause

1fc1da1fb made the forward's exchange event-driven. It attaches the exchange to a deferred acceptance, which its
commit answers. It did not attach it to anything else that defers a forwarded verb's reply. A write or destroy held by
the live log for its durability or its tombstone's placement (A-200, AUD-29-43) met `Gone`, and so did every other
deferral the exchange could not find.

The arm answered `Overloaded`. Before 1fc1da1fb the exchange polled the owner's completion window each heartbeat, and
so met the completion once the write was durable.

The same `Overloaded` closed A-794's path: a verb parked at its work's gate that resumed and recorded nothing.

## Fix

- **live_ship.** A held entry keeps its forward's exchange (`Awaiting::forwarded`, attached by `await_forwarded`, for
  writes and for destroys). The exchange is answered wherever the hold ends:
  - in `answer`: the verb's own reply once durable, or the fence's, the yield's or the forget's refusal;
  - in `tell_given_up`: `DurabilityPending` at the failover SLO.
  Counters: `fleet.live.forward_attached` and `fleet.live.forward_answered`.
- **verbs.** `attach_deferred_forward` looks in order at the completion, then the deferred acceptance, the work's gate
  and the live hold. `await_deferred_completion` answers:
  - the recorded outcome when there is one (counted `fleet.forward.deferred_completed`);
  - the hold's own answer while it waits;
  - `deferred_in_doubt`, which is `DurabilityPending` recorded nowhere, where the verb ran but its outcome is not known
    here: `Gone`, past the liveness budget, or the owner shard not answerable.

  Main's ruling of 2026-11-23: `Overloaded` only where the request provably was never admitted. Here it always was.
- **work_home.** `answer_forwarded` uses the same helper and the same in-doubt answer.
- **Test support.** `Daemon::inject_forward_answer_delay` makes an owner's exchange ask late, so a reply recorded
  before it asks can be tested.

## Tests (adacachy2)

- **Red on main**, with the three fixed files restored from b9849cb39:
  - the two superseded-owner tests: `StaleEpoch`, then `Overloaded` to the end of the wait;
  - `a_write_through_a_node_that_does_not_own_its_live_volume_is_answered_at_durability`: 4,878 `LeaseHeld`, then
    11,007 `Overloaded`, never `FsDone`;
  - `a_forwarded_write_recorded_before_its_exchange_asks_is_answered_its_outcome`: 7,741 `LeaseHeld`, then 11,345
    `Overloaded`. On main the hook it uses has no effect. With the fix, the exchange asks after the write is
    recorded, and the count shows that case happened.
- **Green with the fix:** all four in 27.5 s.
- Beside them, eleven forwarded-submit and takeover fleet tests pass, as does `works_off_owner` (19 of 19).
- The whole fleet suite file and every takeover, forward and submit test were run before landing (the commit names
  the result).

## Impact

- Every write sent through a node that does not own its live-shipped volume, and every destroy held for its
  tombstone, was refused `Overloaded`, though it ran and became durable.
- A client that resends under a fresh id applied such a write twice.
- `slates_client` resends under the same id, so it met the completion in the end, after its backoff. That is a lead
  for w-fleet's never-answered nemesis write and its 20 s hold.
- A superseded owner's clients could not write at all.

## Siblings

- `work_home::answer_forwarded`: the same `Overloaded` for a resumed parked verb. Fixed here.
- The exchange's other non-answers (past the liveness budget, owner shard not answerable) were `Overloaded` too. They
  now answer in doubt. The tests that had expected `Overloaded` for a forwarded submit's acceptance past the budget
  were run against the rule, and none depended on it.
- docs/bugs/2026-11-23-a-takeover-resend-met-the-unheld-forward-guard-before-its-completion.md: a regression of the
  forward's own guard, found by this fix's sweep and fixed in the same change.
