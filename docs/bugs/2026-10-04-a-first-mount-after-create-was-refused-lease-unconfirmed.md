# A first mount right after `volume create` was refused `LeaseUnconfirmed` under load (2026-10-04)

## Description

The three-process CLI deployment (`three_daemon_processes_deploy_a_fleet_from_one_manifest_and_survive_the_owners_death`)
creates a volume on its owner and mounts it at once. Under load it failed about one run in ten (load average about
60):

```
slates mount failed: slates: refused: LeaseUnconfirmed { version: 3 }
```

It failed both with the post-quantum handshake and without it (an A/B of six runs each, 2026-10-04), so the
failure predates that change.

## Root cause

The owner-lease gate (`verbs::dispatch`, §4.8 "Leases and reads") refused a latest-state verb at once whenever the
owner's lease was not confirmed at that instant. A freshly created volume's lease confirms once the holders'
next probe answers reach the owner shard, which on a loaded machine can trail the create. Neither the CLI nor the
client library retried this transient refusal, so the mount failed.

## Impact

Every client of a fleet node had to write its own retry for a confirmation that was merely in flight, and one that
did not, `slates mount`, failed spuriously. Agents provisioning and mounting in quick succession on a busy fleet
were the common victim.

## Exact edits

- `crates/server/src/lease_wait.rs` (new): a verb that meets an unconfirmed lease is **parked** with its completion
  key, principal, body and reply route:
  - it runs, is recorded as its completion and is delivered when the lease confirms;
  - it is refused `LeaseUnconfirmed` only if the lease bound (about 0.9 s) passes first;
  - a retry joins it;
  - the list is bounded by the shard's client credit, and a verb past the bound is refused at once, counted.

  This follows the Raft lease read (Ongaro, thesis §6.4) and etcd's `ReadIndex`, which block a read until its
  confirmation round completes.
- `crates/server/src/verbs.rs`: `lease_gate` parks instead of refusing; both retry paths call
  `lease_wait::join`; `dispatch` is `pub(crate)`.
- `crates/server/src/fleet.rs`: `fan_configs_to_shards` resolves parked verbs on the control shard after its
  lease fold, and on each owner shard after the fan installs. One deadline timer per shard resolves expiries.
- `crates/server/tests/fleet.rs`: `a_status_meeting_a_lapsed_lease_waits_and_is_served_once_the_lease_confirms`
  (3/3; it fails with parking disabled, at "the status was parked").
