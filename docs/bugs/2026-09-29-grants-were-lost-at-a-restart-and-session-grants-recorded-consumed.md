# Grants were lost at a restart, and session grants were recorded consumed (2026-09-29, AUD-29-06)

## Description

Three linked defects in how grants outlived their first use and a daemon restart:

- **Session grants were recorded consumed.** Finishing any landing appended
  `GrantStateChanged::Consumed` for the grant it presented, while the engine consumes only a single-use
  grant. A session grant was therefore recorded spent at its first landing, and runtime and durable state
  disagreed at once. A single-use grant whose landing aborted, leaving it usable for the resume, was also
  recorded spent.
- **A restart lost every grant.** A restarted shard rebuilt none of its runtime grants from the durable
  records, so every issued grant was unusable afterwards: a landing under it was asked for a grant again,
  while the catalog still listed it as issued.
- **The first grant after a restart failed.** Grant ids restarted at 1, so the first grant issued after a
  restart re-minted a recorded id and was refused `AlreadyExists`. Grants could not be issued until the
  counter passed the old ids.

## Root cause

- `landing::finish` pushed `GrantStateChanged::Consumed` for `Some(grant)` without asking what the engine
  did.
- The runtime `Grants` table lived only in memory; shard initialisation recovered the landing counter
  (`next_landing_counter`, since 2026-09-19) but not the grants.
- The durable grant record did not carry the target directory's identity. Even a rebuild could not have
  restored the binding AUD-29-01 made the check depend on.

## Impact

- A human's session approval covered one landing and was then recorded spent.
- A daemon restart silently revoked every grant.
- The first approvals after a restart failed with a database refusal.

## Exact edits

- `crates/db/src/catalog.rs`: `GrantRecord` gains `target_device` and `target_inode`, appended. With the
  principal (the consumer, since AUD-29-01), volume, snapshot and target, the record now holds the
  grant's whole binding.
- `crates/db/src/partition.rs`: `Partition::grants`.
- `crates/land/src/grant.rs`: `Grants::restore` keeps a record's id and state, a duplicate id does not
  replace it, and the next issued id passes it.
- `crates/server/src/landing.rs`: `issue_grant` records the target identity. `finish` records `Consumed`
  only when the runtime grant was consumed. `restore_grants` rebuilds the runtime grants from the durable
  ones, each in its recorded state and bound as approved.
- `crates/server/src/daemon.rs`: shard initialisation calls `restore_grants` beside the landing counter's
  recovery.

## Evidence

- Failing test first, shown red by disabling each part in turn:
  `crates/server/tests/recovery.rs` `a_session_grant_outlives_a_restart_and_a_single_use_grant_stays_spent`.
  - Without the rebuild, the first session landing after the restart answered `GrantRequired`.
  - With every grant recorded consumed, the listing showed the session grant consumed (`{1: "consumed",
    2: "consumed"}`).
  - With both fixes, both session landings after the restart land. The single-use grant is listed
    consumed and the session grant issued, and a grant approved after the restart takes a new id.
- `crates/land/src/grant.rs` `restored_grants_keep_their_state_and_new_ids_pass_them`.
