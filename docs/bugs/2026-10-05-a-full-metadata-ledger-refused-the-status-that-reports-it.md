# A full metadata ledger refused the status that would report it (2026-10-05)

## Description

CI's Linux test lane and TSan lane failed `a_metadata_class_bounds_the_volume_records_a_shard_admits` on every push
since at least `f706876`: the test fills a shard's metadata ledger with volume records, then asks for the daemon's
status, and the status call answered `Refused { BudgetExceeded { available: 1516 } }` (reproduced in a Linux
container; the panic's message had dropped the reply, and now prints it). On macOS the same test passed.

## Root cause

A status report too large for one reply slot is held as pages, and the held bytes are charged to the shard's metadata
ledger (`status_pages.rs`, 2026-09-20). The capture took that charge from the same admittable room every volume's
records take, so a full ledger refused the report that explains it. macOS's 16 KiB pages left more slack after the
last admitted volume than Linux's 4 KiB pages, so only Linux was red.

## Impact

An operator, a test or a monitoring agent could not read a shard's status exactly when its metadata ledger was full,
the condition the status exists to report.

## Exact edits

- `crates/mem/src/budget.rs`: `MetadataBudget::set_observation_room` keeps one capture's room as the ledger's
  headroom, which no volume reservation may take; `reserve_observation` may take into it; `observation_admittable`.
- `crates/ipc/src/status.rs`: `snapshot_capacity_of(bulk_bytes)` and `SNAPSHOT_BULK_DIVISOR`, the one rule for a
  capture's bound, which `snapshot_capacity` and the daemon both use.
- `crates/server/src/daemon.rs`: the observation room is a client ring's status snapshot capacity (128 KiB per shard
  under the derived ring geometry, 0.01% of a 1.37 GB metadata class).
- `crates/server/src/status_pages.rs`: a capture is charged as observation.
- Tests: the failing test now passes on Linux (it was red first) and macOS, its class sized with the room; a unit
  test holds the room's rule (records refused at its edge, one capture inside it, the room back on release).
