# A takeover's resend met the unheld-forward guard before its completion (2026-11-23, §4.8 "Lookup", §4.9, A-255)

Date: 2026-11-23
Area: `crates/server/src/verbs.rs`, `forwarded_write` and `refusal_of_unheld_forward`.
Regressed from: 6b2952f4d and 0a4dcc51b (w-chaos, 2026-11-23 morning). Those changes made a forwarded verb at a node
that neither holds nor owns its volume refused, first `Overloaded`, then `NotOwnerHere`; see
docs/bugs/2026-11-23-a-forwarded-read-at-a-node-that-neither-held-nor-owned-the-volume-answered-not-found.md.
Found by: the sibling sweep of the forwarded-write fix
(docs/bugs/2026-11-23-a-forwarded-write-held-for-its-live-volumes-durability-was-answered-overloaded.md). These four
fleet tests failed with `NotOwnerHere { owner: None }`:

- `a_submit_resent_after_its_owner_was_taken_over_is_answered_its_version`
- `a_submit_resent_after_two_takeovers_in_sequence_is_answered_its_version`
- `a_request_id_copied_into_another_session_reaches_no_completion_through_a_takeover`
- `a_survivor_holding_no_copy_of_the_works_catalog_learns_its_green_from_a_location_round`

## Description

After a work's owner is taken over, the work has no owner. A resend of its submit goes to its green's owner (A-255),
which holds the submit's completion. That node holds no record of the work and routes it to no owner. So the guard
answered `NotOwnerHere`, retryable and recorded nowhere, where the node should have answered the recorded `Submitted`.
Every resend met the same answer for good.

A fresh submit of such a work, which the green's owner must refuse `NotFound` (the work is gone with its owner), met
`NotOwnerHere` as well.

## Root cause

Two faults in the guard:

1. **Its order.** `forwarded_write` ran the guard before `run_forwarded`. `run_forwarded` is where a request the
   partition knows is answered by what it recorded (`Completed`, `Acknowledged` or `Awaiting`). The guard it replaced
   refused only where the node's own routing named itself, so it never met a node holding another owner's completions.
2. **Its notion of authority.** The rule by authority (only the volume's owner may say it does not exist) left out the
   authority A-255 gives a work no member owns: its green's owner.

## Fix

- `forwarded_write`: the guard applies only to a request new on the partition (`Seen::New`). A known request takes
  `run_forwarded`'s path and is answered what it recorded.
- `refusal_of_unheld_forward`: where this node's copy of the work's catalog record names a green this node owns
  (`owner_location::held_work_of`), the verb is dispatched. That is the answer A-255 gives, the completion or
  `NotFound`.

## Tests (adacachy2)

- **Red** on b9849cb39: the four tests above, `NotOwnerHere { owner: None }`.
- **Green**, with the order fixed:
  - the first three tests pass, and so do 11 other forwarded-submit and takeover tests;
  - the fourth (`a_submit_resent_after_its_owner_was_taken_over…`) then failed only its fresh-submit step, `NotOwnerHere`
    for the `NotFound` it expects. With A-255's authority it passes too. All four pass in 16.9 s.
- The ReadSafety unit test,
  `a_node_neither_holding_nor_owning_a_volume_answers_not_owner_here_and_its_creator_not_found`, still passes.

## Impact

For half a day on main:

- A submit resent after its work's owner was taken over was never answered.
- A fresh submit of a work gone with its owner was never refused.

Clients retried `NotOwnerHere` until their own budgets ran out.

## Which suites a change to `forwarded_write` must run

The forwarded path is shared by every verb another node sends. A filter is how these four were missed: 6b2952f4d ran
the nemesis seed and the destroy flows, not these. A change to `forwarded_write`, `refusal_of_unheld_forward`,
`serve_forward`, `answer_forwarded_write` or `await_deferred_completion` runs:

- `cargo test -p slates-server --test fleet`, the whole file;
- the server tests that hold takeover, forward or submit tests: `works_off_owner`, `forward_retry`, `green_post_state`,
  `cross_shard_wait`, `mounted_work`, `daemon` and `fuse_mount`;
- the client's `edit_and_submit`;
- `SLATES_TEST_CLI=1` `slates-cli`'s `live_fleet` and `v3_takeover`, and `live_chaos`'s takeover, forward and submit
  flows.

The file to search for these is the test name: every test named for a takeover, a forward, a submit, a successor or a
superseded owner.
