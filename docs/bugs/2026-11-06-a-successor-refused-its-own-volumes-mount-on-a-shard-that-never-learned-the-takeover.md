# A successor refused its own volume's mount on a shard that never learned the takeover (2026-11-06)

Contracts: §4.8 "Lookup" (ids route to owners; a takeover moves an object to its successor), A-222 (another host's
volume is attached in the record form only; every other form is refused by its kind, `OwnedByAnotherHost`).

## Description

Main's `three_daemon_processes_deploy_a_fleet_from_one_manifest_and_survive_the_owners_death` failed on macOS CI under
load (scheduled run 42206043007, job 127852812782): after `wait_successor` chose the survivor that listed the volume as
its own, that survivor refused the volume's kernel mount `AttachmentUnsupported { NfsLoopback, OwnedByAnotherHost }`.

## Root cause (from counters, not reading)

A-222's early refusal (`remote_attach::foreign_attach`) runs on the shard that serves the client, and decides from that
shard's own routing (`FleetNode::object_owner`). A takeover records the new owner on the control shard only
(`takeover::record_adoption`, `resolve_held_objects`; the mirror's promotion likewise). A CLI client lands on any shard,
so a mount served on another shard met no owner for a volume another host created and was refused.

Evidence: the refusal is now counted by what the refusing shard's routing said. Under two spinners a logical CPU on
macos-latest (diag run 42332031913), 5 of 9 runs failed at `cli.rs:989`, each with `shard 1 refused
attach.foreign_refused.owner_unknown: 1` on the successor, whose shard 0 had adopted the volume
(`fleet.takeover.adopted: 2`, `volumes=1`). It was neither a second adoption nor `volume list` showing another node's
volume: the successor owned it, and only its other shard did not know.

## Impact

A successor's own volume could not be mounted through it whenever the client was served on a shard other than the
control shard, until some lookup taught that shard the owner. A-222's other early decisions on such a shard (the
remembered remote volumes a reap sends to owners) read the same stale routing.

## Exact edits

- `crates/server/src/takeover.rs`: `track_owner_on_every_shard` records an owner in this shard's routing and sends the
  same entry to every other shard (`xshard::run_on_counted`, counted `xshard.run_refused` when a shard's admission
  refuses it). `record_adoption` and `resolve_held_objects` use it; so does the mirror's promotion
  (`crates/server/src/mirror.rs`).
- `crates/server/src/remote_attach.rs`: a foreign-form refusal is counted by its cause,
  `attach.foreign_refused.owner_elsewhere` or `attach.foreign_refused.owner_unknown`.
- `crates/cli/tests/cli.rs`: the deploy test also asks the survivor that does not own the volume to mount it and
  requires the refusal by kind (`mount_refused_by_kind`), so it asserts both answers: the owner serves, the bystander
  refuses `OwnedByAnotherHost`.
