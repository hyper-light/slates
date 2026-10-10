# A node alone answered an id it never created `OwnerUnreachable`

**Found:** 2026-10-10, running the CLI's anchor flow
(`the_anchor_supervises_a_daemon_the_verbs_answer_and_the_daemon_leaves_with_the_anchor`, gated by `SLATES_TEST_CLI=1`)
while checking the daemon's new stop watch. **Status: fixed** (same day).

## Description

On a single anchored daemon, `slates volume stat 00000000000000000000000000000000` exited 1 with
`refused: OwnerUnreachable { region: 0 }`. The flow expects `NotFound`, as before 2026-10-07. The failure reproduces at
the commit before the day's changes (`8d2343a6`), in a scratch worktree.

`a_single_node_answers_an_id_it_never_created_not_found` (`crates/server/tests/daemon.rs`) reproduces it without the
CLI. It failed first with exactly that refusal.

## Root cause

`358aa47d` (2026-10-07) made a lookup by id reach the volume's creator from any node of its own region
(`verbs::owned_by_another_host`). A forwardable read or write whose volume the owner shard's catalog lacks, and whose id
names another creator, goes to owner location and a forward.

On a node alone, every id it did not create names "another creator". The forward found no owner, because there is no
other host, and the client was answered `OwnerUnreachable` instead of the verb's own `NotFound`.

## Impact

- A laptop answered every lookup of a volume id it did not hold with `OwnerUnreachable` instead of `NotFound`. That
  includes a destroyed volume's stale id, a typo, or an id from another machine. A client reading the refusal would
  think a remote owner was down, not that the volume does not exist.
- The CLI's gated anchor flow failed from 2026-10-07 on. It runs only with `SLATES_TEST_CLI=1`, and this machine had
  not run it since.

## Exact edits

- `crates/server/src/verbs.rs` `owned_by_another_host`: no forward unless the fleet has a member besides this node.
- `crates/server/tests/daemon.rs`: the regression above. It also requires the answer well inside the liveness budget,
  so no location round is waited out.
- `docs/wip/SLATES_DESIGN.md` §4.8 "Lookup": amended.

## Siblings

- The cross-region redirect (`home_redirect`) already short-circuits a fleet of one region.
- Two more gated CLI checks still encoded the pre-2026-10-07 rule, in
  `three_daemon_processes_deploy_a_fleet_from_one_manifest_and_survive_the_owners_death`:
  - a holder's `volume stat` was expected to be `NotFound`;
  - the successor was picked by the first `stat` that answered, which a forwarding non-owner now does.
  Both now ask each node's own listing, the invariant `358aa47d` moved the server's fleet tests to.
- Open, not changed: in a fleet, an id whose creator is no member, live or retired, still waits out a location round
  before `OwnerUnreachable`. `NotFound` may be decidable there from the retirement records. Recorded in GAPS.
