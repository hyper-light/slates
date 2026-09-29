# The lease gate refused volumes the node did not hold

Date: 2026-09-29. Scope: the owner-lease gate (§4.8 "Leases and reads"; AUD-08) at its two sites: the verb
gate in `dispatch` (`crates/server/src/verbs.rs`) and the mount gate in the NFS set's `serve`
(`crates/server/src/nfs.rs`). Found by CI run 36560510107 on `5836a8c`, Ubuntu lane.

## Symptom

`three_daemon_processes_deploy_a_fleet_from_one_manifest_and_survive_the_owners_death`
(`crates/cli/tests/cli.rs`) failed at `assert_not_served`. Before the owner dies, the test asks each
survivor for `volume stat` of the owner's volume and expects `NotFound`, because survivors keep the
volume's records but not the volume. One survivor answered:

```
slates: refused: LeaseUnconfirmed { version: 0 }
```

The whole binary ran in 4.84 s. The same test passes locally (9.8 s) and passed on CI's other lanes.

## Root cause

The gate asked whether this node's lease was confirmed for *any* volume a latest-state verb named. It never
asked whether the node held that volume. Its own comment scopes it to "a read of an owned object's latest
state".

In the failing run, the survivor had no configuration installed on the shard that served the stat
(`version: 0`). The CLI test's formation wait counts SWIM's alive set (the status's `members` is
`membership().alive()`), not the council's committed configuration. So a node can look formed before a
configuration reaches it: the council's commit reaches its control shard, then the per-period fan
(`fan_configs_to_shards`) reaches its other shards. With nothing installed, no confirmation carries the
shard's version and no install has started the startup allowance, so the lease cannot hold. The gate then
refused a volume the node never held. Which of the two steps lagged on the runner is not known; either one
produces this answer.

The mount gate had the same defect, and a second one: it ran before the authorization check.

## Impact

- **Verb path.** A node whose lease is unconfirmed refused `LeaseUnconfirmed` for a volume it does not hold,
  where the answer is `NotFound`. That covers a peer that has not yet installed a configuration, a cut-off
  or paused node, and a superseded one. A client that treats `LeaseUnconfirmed` as "retry here" retried a
  node that can never serve that volume.
- **Mount path.** During a lapse, every request answered `NFS3ERR_JUKEBOX` (retry later), including two
  that have final answers:
  - a handle to a volume no longer in the node's set, such as a destroyed volume, whose answer is
    `NFS3ERR_STALE`;
  - a request whose capability does not authorize the volume, whose answer is `NFS3ERR_ACCES`.

  A kernel client retries a `JUKEBOX` without end on a hard mount. So a mount of a destroyed volume hung
  until the lease came back, instead of failing.

No stale read was served. Both defects refused too much; neither refused too little.

## Fix

- **Verb gate.** It applies only when the partition's catalog holds the volume
  (`state.db.partition().volume(..)`). Every latest-state verb answers from that record (`find`,
  `find_record`, `require_green`), so without one the answer is `NotFound` whatever the lease says, and there
  is no latest state here to serve stale.
- **Mount gate.** Authorization now comes first, and the lease gate applies only to a volume in the shard's
  set (`by_id`, the only volumes `with_export` serves). An unauthorized caller gets `NFS3ERR_ACCES` whatever
  the lease's state. Merely narrowing the gate would have let a lapse tell such a caller which volumes the
  node holds (`JUKEBOX` for a held one, `ACCES` otherwise). The design already refuses to disclose that
  (AUD-01: "a volume cannot be discovered by an unbound or unrelated caller").

## Tests

**Failing first:** `a_lapsed_lease_refuses_only_what_the_node_holds_and_authorizes`
(`crates/server/tests/fleet.rs`). A three-node `f = 1` fleet:
- A holds a volume mounted over NFS under two capabilities, and ends one of them;
- A mounts, writes and destroys a second volume;
- B creates a volume of its own.

A is isolated on the probe plane until its lease lapses. The test observes each answer before and after:

| | before the lapse | after, unfixed | after, fixed |
|---|---|---|---|
| status of the held volume | served | `LeaseUnconfirmed` | `LeaseUnconfirmed` |
| read through the live capability | `NFS3_OK` | `JUKEBOX` | `JUKEBOX` |
| read through the ended capability | `ACCES` | `JUKEBOX` | `ACCES` |
| read through the destroyed volume's handle | `STALE` | `JUKEBOX` | `STALE` |
| status of B's volume on A | `NotFound` | `LeaseUnconfirmed` | `NotFound` |

The first two rows show the gate is live on both paths (non-vacuous).

After the fix:
- the CLI test passes locally;
- the in-process fleet suite passes 55 of 55 (338 s, with the KIND cluster still running on the machine);
- `nfs_mount` passes 12 of 12, and the server's unit tests 110 of 110.

## Siblings

- **`Daemon::fleet_lease_holds`**, the test observation, still reads the lease for any object, held or not.
  It is a question about the lease, not a served read, so it is unchanged.
- **Open, recorded in GAPS: the synthetic root has no lease gate.** Its `LOOKUP` of a volume's name and its
  `READDIRPLUS` entries return the volume root's attributes (`root_object`), which are part of its latest
  state. Only a handle-based procedure passes the gate. `root_object`'s seam returns attributes as
  mandatory. RFC 1813 makes both replies' object attributes optional (`post_op_attr`). During a lapse the
  root could return the handle without attributes, so the client's next `GETATTR` meets the gate.
- **Observed, not changed: destroy treats attachments differently by volume kind.** A plain volume's
  destroy leaves its attachment records (`destroy`, `step_destroys`). A merge volume's destroy removes them
  (`destroy_merge_volume`). So a destroyed plain volume's mount handle answers `STALE` (its capability still
  validates, and the volume is gone), while a destroyed green's pin answers `ACCES`. Each attachment is
  still ended by its consumer's detach, unmount or reap. Recorded for Ada's decision.
