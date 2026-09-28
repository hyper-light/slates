# Formation dropped a dial to the peer it was reaching

Date: 2026-09-28. Design: §4.8 ("Deployment": seed ids route discovery; every daemon announces a fresh id,
task #22, AUD-07). Found by CI run 36408099369 (ubuntu-latest). In
`three_daemon_processes_deploy_a_fleet_from_one_manifest_and_survive_the_owners_death`, one node counted
`fleet.dial.stale_dropped: 3` while the fleet formed. That refusal is outside what the test allows during
formation.

## Root cause

A record link knows its peer first by the manifest's **seed** id, `member_id(anchor, 0)`, a routing
placeholder. When the peer's first authenticated contact announces its fresh id, the link follows it
(`refresh_record_identity`). Any change of id was treated as a restart: the dial still in its handshake was
dropped as pointing at "the old incarnation", counted as stale, and redialed a period later.

The seed was never an incarnation. The pending dial was reaching the same live process at the same address,
under the same pinned certificate, so dropping it threw away a handshake that was about to complete. The
refusal only showed when a handshake was mid-flight at the moment the id arrived, which is why it was
intermittent.

## Fix

- **The seed rule.** `refresh_record_identity_in` keeps the pending dial when the previous id is the seed,
  and still drops and counts it when a peer that had a live incarnation restarts.
- **Split for testing.** The decision is split from the state borrow, so it is tested on a real shard with a
  real pending `Endpoint`.
- **Nonce zero is reserved.** A boot nonce is now never zero (`boot_incarnation(..).max(1)`). Nonce zero
  names the placeholder, and the rule above relies on no live incarnation ever equalling it. Before this,
  the odds of a clash were 2⁻⁶⁴, but nothing enforced it.

## Test

`a_seed_replaced_by_the_first_fresh_id_keeps_the_dial_but_a_restart_drops_it` (`fleet.rs` unit test, on a
shard). When the seed is replaced by the first fresh id, the link moves, keeps its dial, and counts nothing.
When that incarnation is then replaced by a restart, the link moves, drops the dial, and counts one. With
the seed rule removed, the test fails with exactly the CI symptom (`(true, false, 1)` where `(true, true, 0)`
is expected).
