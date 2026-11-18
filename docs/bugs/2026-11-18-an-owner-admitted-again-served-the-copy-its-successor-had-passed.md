# An owner admitted again served the copy its successor had passed (2026-11-18)

## Description

The first gated run of `crates/cli/tests/live_chaos.rs` (the integrator, main bfc34cace, macOS arm64; the same on
Linux x86_64 and arm64 at 69cd38b22) failed ReadSafety in `an_owner_partitioned_mid_stream_is_fenced_and_loses_nothing`.
The flow stops the owner of a live volume (SIGSTOP), waits for a survivor to take the volume over, writes through the
successor, resumes the old owner and reads the successor's last five acknowledged files through it. All five answered
`NotFound`: the first read sent 2 ms after the resume, answered about 0.7 s later, the rest at 3.0 s, 70 to 780 ms
after their acknowledgements. A read through any node must answer an acknowledged write's bytes or a refusal.

## Root cause

The flow now notes the old owner's state when a read through it answers missing (`note_missing`). Its first run with
the note (Mac Studio, 2026-11-18, release) showed:

- `fleet_host_epoch: 2` with the old owner among the members: the council had retired it while it was stopped, and it
  refuted its death on resuming and was admitted again under its own member id, at a newer host epoch.
- Its volume list still held the volume, and its live line was its own epoch-1 shipper at head 47: the copy from before
  the takeover.
- On the volume's shard: `lease.refused.superseded` 1, `lease.wait.parked` 1, `lease.wait.served` 1. The first read
  was parked while the lease was superseded, then served once the holders confirmed the re-admitted host.

A re-admission yields a host's objects only in its routing table: `FleetNode::install_configuration` forgets the
host's own routing entries when its epoch advances, and its documentation says later lookups forward to the successors.
Nothing told the shards. Their catalogs kept the copies; `verbs::owned_by_another_host` runs any verb on a volume this
node created or catalogues here; and the owner lease is the host's, not the object's, so it confirmed again under the
new standing. The read ran against the copy, which lacked every write the successor had made.

## Impact

ReadSafety: a client reading through a host that had been falsely judged dead (a stopped or starved process, a
partition that healed) saw an older state of a volume than writes it had been told were durable, including files
missing. Writes were never at risk: the copy's shipper was fenced, so a write through it was refused `StaleEpoch`
(`fleet.live.fenced` 20 in the run). The same copy fed a mount's live tree on that host.

## Exact edits (A-334)

- `crates/server/src/readmission.rs` (new): `install_placement` installs a committed configuration on a shard and, when
  it carries a newer epoch for the same host, yields every volume the shard catalogues (`ShardState::yielded`, counted
  `fleet.readmission.yielded`). Both run-time installs use it: the control shard's (`fleet::sync_config_from_council`)
  and every other shard's fanned one (`fleet`'s `Fan::install`).
- `crates/server/src/verbs.rs`: `owned_by_another_host` forwards a verb on a yielded volume, whoever created it and
  whatever the catalog holds; `lease_verdict` refuses a yielded object's latest state (`LeaseVerdict::Yielded`,
  counted `lease.refused.yielded`), so a mount or a peer's forward still naming this node never reads the copy.
- `crates/server/src/owner_location.rs`: `Query::known_owner` never names this node, so a yielded volume's forward
  takes the location round, where the successor claims it.
- `crates/server/src/lease.rs`: `LeaseVerdict::Yielded`, `LEASE_YIELDED`.
- `crates/server/tests/fleet.rs`: `an_owner_admitted_again_forwards_the_volume_its_successor_took_over_and_never_serves_its_own_copy`
  (both survivors judge a live volume's owner dead though it lives; a survivor takes the volume over and writes a second
  file; the owner refutes and is admitted again; every read of the second file through it answers the bytes or a
  refusal, one answers the bytes, and the volume was yielded).
- §4.8 "Leases and reads" status, A-334, GAPS.

## Sibling sweep

The same rule, "a volume this node created is its own", decides two more things, both on the client's shard, which need
not be the shard holding the yielded set:

- `remote_attach::elsewhere`: an attach in a form other than the record form (a mount, a device, a stream) of a yielded
  volume is not refused by its kind, so it attaches the copy here. Its latest state is now refused there (the live
  tree is fenced), so nothing stale is served, but the attachment is useless. Owed: refuse it as another host's.
- `remote_attach::remember` and `reap_at_owners`: a record-form attach of a yielded volume through this node is
  forwarded to the successor but not remembered for the client's reap, so a dead client's attachment there ends at its
  lease term rather than at the reap. Owed: remember it.

Also owed: a later takeover that names this node a yielded volume's successor again keeps the copy
(`verbs::materialize_taken_over` returns early for an id it has), so the volume stays yielded, refused and forwarded
here, until a rebuild over the adopted head replaces it; and the copy's memory is released only by the ledger transfer
GAP-A9-7 owes.
