# An agent's own channel could approve its own landing (2026-11-18)

## Description

A-285 makes write permission follow the mode the user gave the agent's harness. Under ask, a landing answers an
approval request, and only the launcher (kiln) or the harness approves it, by a grant bound to the presented manifest.
An agent's session talks to slates over a channel bound to its enrolled consumer, and that channel must never raise its
own permission: a policy setting from it is refused `Forbidden` whatever its proof (`set_policy_verb`), and so is a
merge's approval (`submit_approved`).

The landing's grant (`Grant`, `landing::grant_verb`) verified the proof and the manifest but never looked at the
channel. A consumer-bound channel holding a genuine launcher-class proof was granted, and the presenting client could
then land under the grant.

## Root cause

The grant verb predates A-285 (§4.13 "Grants", AUD-29). A-285 added the consumer refusal to the two verbs it built, the
policy setting and the merge approval, and left the existing landing grant verifying its proof alone.

## Impact

An agent cannot make a launcher-class proof without the launcher's secret, which the launcher keeps, so no path was seen
in use. But the rule "an agent never raises its own permission, even holding a genuine proof" did not hold for
landings: a leaked secret, or a proof a confused harness handed to the agent's own channel, let an agent approve its own
landing under an ask policy.

## Found by

The fs-server plan's step 1 (`crates/cli/tests/fs_server.rs`, `no_agent_can_set_or_approve`): an agent's channel on
the server node, enrolled by the server's launcher, issued the grant of exactly the presented manifest under the
server launcher's genuine proof, expecting `Forbidden`.

## Edits

- `crates/server/src/landing.rs`, `grant_verb`: a `Principal::Consumer` is refused `Forbidden` (`grant`), counted
  `grant_by_consumer`, before any lookup or proof check.
- Failing test first: `crates/server/tests/write_policy.rs`,
  `an_agent_cannot_approve_its_own_landing` (a consumer-bound channel lands its own volume under ask, then grants the
  presentation under a genuine proof; expected `Forbidden` and nothing on disk; without the guard the grant is issued).
- `crates/cli/tests/fs_server.rs`: the server-node agent's grant is refused `Forbidden`; the client node's attempts are
  refused `NotFound`, since it holds neither the volume nor its presentations.
- Docs: A-285's status (design) and the GAPS entry.

## Siblings (reported, open)

`Enroll` (`verbs::enroll`) and `Revoke` (`verbs::revoke_on_channel`) also verify the proof alone. A consumer-bound
channel holding a genuine proof could enroll a fresh consumer and bind a new channel to it, escaping its own consumer
policy, or revoke another consumer. They need the same guard.
