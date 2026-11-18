# A takeover successor's chain lacked the versions its checkpoint stood for (2026-11-18)

## Description

`a_successor_two_versions_behind_adopts_the_versions_between_and_serves_the_green` (`crates/server/tests/fleet.rs`)
failed intermittently at its chain assertion on Linux (adacachy runs r409, r343, r271): the successor's
`merge_chain_identities` was `[(2, …), (3, …), (4, …)]` where the dead owner's was `[(1, …), (2, …), (3, …), (4, …)]`.
The main session counted 2 failures in 4 runs at `2a5bf7264`, before A-273 step 3; 0 in 4 at main `9dec6fa`; and 2 in
3 at main with its lint batch. So it was not A-273's. In the test the successor holds version 2 when the owner dies,
its merge faults refusing content puts and records, and it adopts versions 3 and 4 from the other holder's ledger
(A-364, A-412's path) before it places the green at version 4.

## Root cause

A-404 lets a holder keep a durable checkpoint of its replica at the newest committed version its owner's record names,
and then let go of its ledger's positions after version 0 and below the checkpoint's own (`compact_ledger_between`):
the checkpoint stands for them. When the successor recomputed version 3, whose record named version 2 committed, it
kept a checkpoint at version 2 and dropped position 1. Whether that happened depended on timing: it needed a record the
successor recomputed to name version 2 committed, and the owner names only what it knows committed when it makes the
record.

An adopted green names the versions up to its adoption by this node's ledger: its partition records it by its adoption,
and its chain holds only the versions after it (A-333). `Daemon::merge_chain_identities` read those versions from the
ledger alone (`ledger_value`, one position per version). Position 1 was gone, so version 1 was left out. Its identity
was not lost: the held checkpoint keeps it in its record of what it accepted (`Checkpoint::accepted`), and the
successor's engine, rebuilt or advanced from that checkpoint, holds it too. The defect was that this read did not
consult the checkpoint, which since A-404 stands for the positions the ledger let go of.

## Impact

What the successor reported as its chain, and so the fleet test's assertion, whenever a checkpoint had let go of
adopted versions. The engine the successor serves from, its rebuild at a restart (`ReplicaSeed`, which starts from the
held checkpoint) and its durable state (the ledger's kept positions and the held checkpoint, carried by its image) were
right. By the code, a read of the green at version 1 is answered from that engine. The failing runs stopped at the
chain assertion, before the test's read of version 1, so no run shows that read.

## The fix

- `merge_origin::ledger_identities(state, object, through)`: each version from 1 to `through`, named by the ledger's
  record when it still holds the position, else by the held checkpoint's record of what it accepted.
- `Daemon::merge_chain_identities` names an adopted green's versions through its adoption by it.
- Test, failing first: `a_successors_chain_names_the_versions_its_checkpoint_stands_for` (`crates/server/src/merge_origin.rs`).
  It drives the same order as the fleet test: a holder at versions 0 and 1; version 2 naming version 1 committed;
  version 3 naming version 2 committed, so the checkpoint at version 2 drops position 1. It then reads the identities
  through version 3. With the old read, version 1 was left out.

## Siblings swept

Every other reader of a ledger position by version (`ledger_value`'s callers):
- Version 0 (`merge_origin.rs` origin, `fleet.rs` 5291) and the checkpoint's own version (the rebuild's check): never
  let go of.
- The rebuild's chain steps and `ledger_unheld`: only versions after the checkpoint and the replica's head.
- `green_adopt.rs`'s completion gathering: a version the ledger let go of carries no completion it can know, as its doc
  says, and a resend of that version's submit is answered by the engine's identity record (`Green::accepted`), which
  the checkpoint restores.

None reads a version the checkpoint let go of as present.
