# A rolled-back clone kept its pin on its origin's snapshot (2026-11-18, §4.5, AC-2.3)

## Description

A clone pins its origin's snapshot when it is admitted (`verbs::admit_clone`), so the snapshot cannot be destroyed while
the clone shares its tree, and records its volume and lineage in the verb's transaction (`publish_clone`). When that
transaction's commit fails (the log full or unpublishable), `Db::commit` rolls the partition back and
`reconcile_unpublished_effects` tears the orphaned clone's slot down; nothing gave back its pin, so the origin's snapshot
stayed pinned and its destroy was refused `Pinned` until the next restart's recovery recounted the pins
(`reconcile_clone_pins`). Found by reading on 2026-10-24 (GAPS, an open sibling of the guest-image work).

## Proof

`a_clone_rolled_back_unpublished_gives_back_its_pin_on_the_origins_snapshot` (`crates/server/tests/recovery.rs`): a
volume and its snapshot; the next publication refused before its append (`PublicationFault::BeforeAppend`); the clone of
the snapshot refused `Unpublished` with one rollback counted; then the snapshot's destroy. Before the fix: refused
`Pinned`. After: destroyed.

## Fix (exact edits)

`crates/server/src/verbs.rs`: `reconcile_unpublished_effects` recounts the clone pins against the catalog's recorded
lineage (`reconcile_clone_pins`) whenever it tore an orphan down. The rolled-back record that named the origin is gone,
so the recount is what can tell; it runs only on that failure path.
