# A taken-over green lost its access list (2026-11-07, §4.16, §4.13, AUD-29-17)

## Description

A successor rebuilds a green whose owner died from the green's newest adopted merge record (AUD-14,
`crates/server/src/fleet.rs` `materialize_pending_greens`). The merge record carries the green's name, evidence policy
and owner, and nothing else from its catalog. `verbs::materialize_taken_over_green` recorded the rebuilt green with
`access: Vec::new()` and `catalog_version: 0`. So every share of a green was gone after a takeover: the successor
admitted the green's owner and nobody else.

A volume does not lose its shares: since AUD-29-17 its catalog register (`CatalogValue`, which carries the access list)
ships at every catalog change, and its head waits until that register is committed. A green has no volume slot
(`state.volumes`), and `unplaced_heads`, which ships catalog registers, walks only the slots, so a green's register
never shipped.

## How it was hidden

An owner is a bare uid, and the fleet tests run every daemon in one process. So the successor's own client was the
owner's uid and counted as the owner, and the lost access list never showed. It showed once principals carried their
host (the host-scoped principals work, `agents/clients-host`): the successor's client became another host's account,
admitted only by a share, and the successor refused it `Forbidden`.

## Measurement

On that branch, `crates/server/tests/fleet.rs`
`a_taken_over_green_serves_every_version_and_accepts_new_work_on_the_successor` failed on CI's Linux lane (diag
`clients-host-3`, 2026-11-07) and alone on macOS arm64 (47 s). The successor's own client was refused `Forbidden` for
`versions` and `read` of a green shared with its account at creation.

## Root cause

A green's catalog register was never shipped, so its successor had no access list to rebuild.

## Edits

- `crates/server/src/fleet.rs`: `owed_catalog` (the catalog register a volume or a green owes, and whether its current
  version is committed) is used by `unplaced_heads` for both. `unplaced_heads` now ships every owned green's register
  too. `green_catalog_committed` gates the merge plane; a green with no record here owes no catalog, since a destroyed
  green's tombstone stands for both registers. `materialize_pending_greens` waits for the adopted catalog and its
  placement (counted `merge.takeover_waits_catalog`), hands both to the owner shard, and forgets the pending catalog
  once the green is served.
- `crates/server/src/merge_service.rs`: `next_merge_work` ships no merge record of a green before its catalog register's
  current version is committed, so an adopted merge record always meets an adopted catalog, as a volume's head does.
  `GreenRecovery` carries the adopted access list and the register's sequence.
- `crates/server/src/verbs.rs`: `materialize_taken_over_green` records the adopted access list and continues the
  register's sequence.

## Tests

`crates/server/tests/fleet.rs` `a_share_of_a_green_made_after_its_last_version_survives_a_takeover` (new): a green
advanced three versions on A, shared read-only with account 7777 after its last version, then A killed. The share's
catalog register must reach both survivors, and the successor must rebuild the green with the share. On the host-scoped branch it was red before the fix (the register never reached the survivors,
447 s) and green after (34 s), macOS arm64. On main with this fix it passes beside
`a_taken_over_green_serves_every_version_and_accepts_new_work_on_the_successor` (89 s for both, macOS arm64,
2026-11-07).

## Siblings

None found: a plain volume ships its register (AUD-29-17), and a tombstone replaces both.
