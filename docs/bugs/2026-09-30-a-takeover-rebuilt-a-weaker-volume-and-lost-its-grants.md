# A takeover rebuilt a weaker volume and lost its grants

**Date:** 2026-09-30. **Area:** `slates-server` (fleet record plane, takeover, verbs), `slates-db` (object
ids, the volume record). **Audit:** AUD-29-17.

## Description

A successor rebuilt a taken-over volume from its head register's value. That value carried the name, size
class, name policy and owner, but not the locked-RAM policy or the access list. The rebuilt record set
`require_locked: false` and `access: []`.

- A volume that had to live in locked RAM came back swappable.
- Every consumer the owner had granted lost access.

The head value ships only when a seal places. So a policy change made after the last seal would have been
lost even if the head had carried it.

Red test: `crates/server/tests/fleet.rs`, `a_grant_made_after_the_last_seal_survives_a_takeover`. It seals,
grants uid 7777 read access after the head placed, kills the owner, and expects the grant on the successor.

## Fix: the design's catalog register class

§4.8 lists catalog entries as registers of their own, and the head value's catalog fields were recorded as
"the owed split".

- **Its object.** `ObjectId::catalog()` is the volume's id with a register-class bit set: the top bit of the
  48-bit per-creator counter. `placement_key()` clears it, and `rendezvous_weight` hashes the placement key.
  A catalog therefore has exactly its volume's holders, and exactly its successor at a takeover.
  - `fresh_volume_id` now refuses (typed `Unsupported`) once the counter would reach that bit
    (`MAX_OBJECT_COUNTER`). Until now it truncated the counter silently.
- **Its sequence.** `VolumeRecord.catalog_version` is raised in `apply` by `AccessChanged`, `VolumeResized`
  and `VolumeRebased`, so replay reproduces it.
- **Its value.** `CatalogValue` holds the name, size, name policy, owner, `require_locked` and the access
  list, and is strictly decoded. `HeadValue` now holds content only (manifest and holders).
- **Class bytes.** Every register value leads with its class byte (`HEAD_CLASS`, `MERGE_CLASS`,
  `CATALOG_CLASS`). Takeover reads a record's class from the object and the tag, never by trying decoders
  in turn.
- **Shipping.** `unplaced_heads` ships each volume's catalog at its current version before the head. A head
  ships only once its catalog's current version is committed or owed to no one, so an adopted head always
  meets an adopted catalog. The neighbourhood-change check also counts catalogs.
- **Takeover.**
  - Adoption files a catalog under its volume (`pending_catalogs`), and materialization waits for it.
  - `materialize_taken_over` rebuilds the record from the catalog: `require_locked`, locking the arena as a
    strict create does, or refusing `BudgetExceeded` with nothing published; the access list;
    `catalog_version` from the adopted sequence.
  - The successor keeps the catalog's placement, so it continues the register at the promotion epoch.
- **What a successor does not inherit, and why.**
  - **The base.** Stays `Scratch`: sealed content is always whole in RAM, because an overlay's base-backed
    bodies are never sealed.
  - **A lease.** Belonged to a client of the dead owner; clients take new ones.

## Tests

- **Fleet.** `a_grant_made_after_the_last_seal_survives_a_takeover`: the grant's catalog record reaches both
  survivors, and the successor serves the volume with it.
  - The head-takeover tests (`three_daemons_…`, `five_daemons_…`) now wait for and read the catalog register
    for the essentials the head used to carry. The first run read too early: takeover adopts object by
    object.
- **Unit.** `a_takeover_keeps_the_volumes_grants_and_its_locked_policy`.
- **Hostile input.** `catalog.rs` tests catalog round trips, truncation, padding and a foreign class byte.
- **Suites.** The fleet suite (60), the db suites and the server suites pass.
