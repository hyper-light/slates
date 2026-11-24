# A successor restarted warm gave up the volume it had adopted (2026-11-23, §4.8 "Promotion and takeover", A-251, A-883, A-885)

Date: 2026-11-23
Area:
- `crates/db/src/register.rs`: `Acceptor::adopt`.
- `crates/server/src/takeover.rs`: `adopted_acceptor`, `commit_adopted`, `promise_local_copies`.
Found by: GAPS' load flake `a_successor_restarted_warm_takes_a_write_to_the_volume_it_took_over`, its cause read in a
diagnostic build of A-883 on adacachy2.
Regressed from: A-251 (2026-11-09), which recorded the owner a held copy's authority moved from at an install or a
promise, and nothing at the adoption itself.

## Description

A takeover's successor adopts each object of the departed owner that ranks it first, then confirms its share once the
adoptions are placed. A round lives in the successor's memory (`ShardState::host_takeovers`). A successor restarted
warm before it confirmed its share therefore runs the round again from nothing, and it must reach the end the first
run reached.

It did not. The rerun learned the adopted volume from the third node's copy, which still falls in that round by its
move from the departed owner. It then asked itself by name, as every round does (`answer_own_copies_by_name`), and
refused: its own copy is the adopted one, owner itself, and recorded no move (A-251: "the asker's own objects never
fall here"). The cohort's quorum needs both survivors, so the volume waited for that promise. It was given up at the
failover SLO, and every write was held there.

## Measured

- In a diagnostic build on adacachy2, the scene failed 2 of 8 runs at load 50. Each failure logged:
  - `named-refused-self ... copies [Some((successor, None)), Some((successor, None))]` for the volume's two registers;
  - "the takeover of the creator's objects gave up 2 of them";
  - three writes held to the SLO.
- CI's run at ae43b42ea logged the same give-up on Windows x64, before A-883.
- It needs the restart to come before the confirmation, so it failed only under load, where the restart is slower
  relative to the round.

## Root cause

The adoption (`commit_adopted_at`) made the adopted acceptor the successor's own with `install_authority`. That kept
whatever move the copy carried: none for a copy made at the adoption, or for one whose move a later install
overwrote. So nothing recorded that this node's copy is the departed owner's object, moved to it at that retirement.
The first run never needed the record, since it kept the object in its `adopted` set. A rerun has only the copy.

## Fix (main's ruling of 2026-11-23, its (b): a restarted round counts its own adoption)

- **The adoption records its move.** A takeover's adoption makes the copy the successor's with `Acceptor::adopt`, which
  records `MovedFrom { departed, retirement version }` whatever the copy served before (`adopted_acceptor`).
  - The copy then falls in that retirement's round for its adopter (`copy_falls_to`), and the record is persisted, so
    a rerun after a warm restart lists the adoption as the successor's own promise and answers it by name.
  - The rerun was to reach the end of the first run, the object adopted by the successor, but at the rerun's epoch.
    A-890 corrects that: the end of the first run is the adoption at its own epoch (below).
  - A moved volume's adoption (A-291) records none. A later retirement of the same host never counts it (A-883).
- **The second adoption was not harmless (corrected by A-890, 2026-11-23).** Main's ruling had the rerun re-adopt the
  object one epoch above the first run's, a fence the warm restart would make invisible. It was not invisible. The
  successor's live log resumes at the first run's epoch after its restart (A-344), and a holder the rerun had fenced one
  epoch up refused it as stale. The successor then marked the volume superseded by its own new epoch, and its owner
  shard forwarded every verb to the control shard. There its routing named it the owner, so each verb was refused
  `Overloaded` while no materialization was pending. Evidence: a diagnostic build on adacachy2 (kiln r649, load 38 to
  57), 4 of 6 runs. A holder counted `fleet.live.hold_refused.stale_epoch` 1, the successor counted
  `fleet.owner_location.owned_unmaterialized` 4,165 to 4,774 and `fleet.live.taken_over` 1 since its restart, and
  every read was refused for its whole 60 s window while both coordinators ran about 585 periods a minute. The 2 runs
  that passed took nothing over again. A-890 has the rerun settle its adoption, by the record the adoption now persists
  with its copy (`Acceptor::adopted`), at the adoption's epoch.
- **A round never promises again what it adopted.** `promise_local_copies` leaves out the objects in `adopted`. That
  kept the round's own promises off its adoption, but a holder's page promises every copy falling to the asker, so a
  round started over above its epoch still fenced the adoption through its holders. Since A-890 a round above its
  settled epoch asks by name only.

## Tests (adacachy2)

- `a_successor_restarted_warm_promises_what_it_adopted_when_its_round_runs_again` (`crates/server/src/daemon.rs`,
  deterministic). It makes this node the successor of a held object, adopts it as the round does (`adopted_acceptor`),
  writes and restores the held image (the warm restart), and asks the rerun's question by name of itself.
  - Red, with the move not recorded in a scratch copy: refused 1, answered nothing.
  - Green: answered the adopted record, refused 0.
- `a_successor_restarted_warm_owing_its_share_promises_the_volume_it_adopted` (`crates/server/tests/fleet.rs`, three
  anchored daemons). The successor withholds its confirmation until its restart
  (`Daemon::inject_confirmation_withheld`), so it restarts owing the share.
  - Green: 2 of 2 runs whose setup completed. The write was answered at once, all seven files read back, the rerun
    re-adopted the volume's two registers and listed its own adoption by its move (`fleet.takeover.adopted` 2 and
    `fleet.takeover.listed_by_moved_from` 6 and 10 since the restart), and nothing was given up.
  - With the move not recorded it is no deterministic red. It fails only where the successor held no copy of the
    volume's registers before adopting them, and that depends on the volume's placement. The unit test above is the
    deterministic red.
  - Under load 55 to 73 on adacachy2, runs of either kind met `Overloaded { shard: 1 }` after the successor's restart,
    at the write's attach or the reads after it. This was reported as the harness's, since both helpers tried once.
    It was the state above: no refusal ever cleared (A-890).
- A diagnostic build of this change logged the rerun's re-adoption: the volume's registers adopted at epoch 2 by the
  first run and at epoch 3 by the rerun.

## Impact

- A successor restarted warm before it confirmed a takeover share gave up the volumes it had adopted. Every verb on
  them answered `NotRecovered`, every write was held to the failover SLO, and the share was never confirmed, since a
  given-up object holds the confirmation (A-272).
- The volume's data stayed on its holders, but nothing served it until a later round.
