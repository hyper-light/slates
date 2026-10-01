# The configuration fan copied everything to every shard every period

**Date:** 2026-10-01. **Area:** `slates-server` (`fleet.rs` `fan_configs_to_shards`, `state.rs`).
**Audit:** AUD-29-29 (P2). **Design:** §4.8 "Lookup", D-7, D-14, R8.

## Description

Each coordinator period, the control shard cloned the committed placement, the full member list, the root
configuration and the owner lease once. It then cloned all of them again for every other shard and sent one
cross-shard message each. The receiver checked configuration versions only after the copies had been made.
Pruning the lease ledgers copied the membership twice more.

A quiet fleet therefore paid O(S × (N + H)) copies per host per period: S shards, N members, H root-home
entries. If every host carries the full roster, the aggregate membership copying reaches O(N²S). The
re-fan existed so that a message a full channel dropped would heal the next period.

## Root cause

The sender kept no memory of what each shard already held. Version-gating sat only on the receiving side,
after the cost was paid.

## Fix

- **What the control shard remembers** (`ShardState::fanned`, a `fleet::Fanned`):
  - per shard, what it last received: placement version, root version, readiness, and lease generation;
  - the lease as last fanned, with a generation that advances whenever the lease changes.
- **What a period sends.** `Fanned::owed` builds a shard's fan from only the parts newer than what it holds.
  Each part is cloned inside the branch that sends it, so a shard that holds everything costs no copy and no
  message (`fleet.fan.unchanged`).
- **When a delivery counts.** It is recorded only once the shard's bounded channel accepted the fan
  (`fleet.fan.sent`). A refused fan (`fleet.fan.refused`) is not recorded, so the next period owes it again:
  the self-healing the every-period re-fan provided is kept.
- **The receiver** (`Fan::install`) keeps every install version-gated as before.
- **Lease evidence** keeps its own cadence: it is fanned when it changes (a probe confirmation arrives), and
  its absolute send times still make a missed fan shorten a lease, never lengthen it.
- **The lease ledgers** are pruned against the borrowed membership, with no copy.

## Tests

- `a_shard_is_fanned_only_what_changed_since_it_last_received_it`, on the control shard:
  - a first fan carries everything the receiver can install (a root configuration only above version zero,
    which `adopt` would refuse anyway);
  - after a recorded delivery nothing is owed;
  - a lease change owes the lease alone;
  - an unrecorded (refused) fan is owed again;
  - a newer placement owes the placement alone;
  - installing a fan leaves the receiver with the control shard's placement version and lease.
- The fleet and recovery suites, as the check that routing and fencing are unchanged.

## Siblings reported

- AUD-29-28: the coordinator's consensus drive waits behind the record, takeover and content work, and
  sessions are lent exclusively. It awaits Ada: the fix needs the shared transport's per-peer multiplexing
  owner and the record-link task (see GAPS).
