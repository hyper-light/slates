# A dropped NFSv4 client's purge was owed only in memory

**Found:** 2026-10-10, sweeping A-127's owed items ("the in-memory purge queue does not outlive a daemon").
**Status: fixed** (same day).

## Description

`a_dropped_clients_purge_owed_at_a_restart_is_sent_after_it` (`crates/server/tests/recovery.rs`) shows it:
1. client A opens a file denying writers, and client B's open of it for writing is refused `NFS4ERR_SHARE_DENIED`;
2. every owner refuses purges for a moment, and A destroys its session and its client id;
3. the daemon restarts over the same anchor.

B's open stayed denied after the restart, `NFS4ERR_SHARE_DENIED` for as long as the test waited (10 s, ten purge
rounds). The test fails that way with the fix's restore taken out.

## Root cause

The shard holding a client's table clears a dropped client durably at once (`Op::NfsClientCleared`). The purge it then
owes every owner shard (`STATE_PURGE`, or `STATE_PURGE_SESSION` for a destroyed session) lived only in memory:
- the session table's `dropped` and `destroyed` lists;
- `Server::pending_purges` for a purge an owner refused.

A death in between lost the obligation. After the restart no shard knew the client, so no lease of its could expire, and
its file state at the owners stayed.

## Impact

- **Locks held for good:** a dropped client's byte-range locks, share reservations and delegations survived at the owner
  shards across the restart. Other clients' conflicting opens and locks were refused indefinitely. Nothing but a manual
  purge would release them.
- **A destroyed session's call records** stayed too. That cost only room, and they went when the client was purged.
- **The window:**
  - short while every owner answers, the cross-shard purge calls;
  - unbounded while one does not, since the purge stayed queued in memory.
- **Reachable by the kill benches:** the A-127 kill bench kills daemons mid-workload.

## Exact edits

- `crates/bridge-nfs/src/v4/session.rs`:
  - `Purge` moves here from the compound module;
  - `ClientChange::PurgeOwed` and `PurgeSettled` are new journal entries;
  - the table's `owed` list replaces `dropped` and `destroyed`;
  - each drop journals its purge owed right after its own change (`owe`), so the two commit together;
  - `settle` journals a settlement, and `restore` takes the owed purges back.
- `crates/bridge-nfs/src/v4/compound.rs` `purge_owed` (was `purge_dropped`): sends every owed purge to every owner, and
  settles a purge only once every owner acknowledged it. Purges are idempotent, so a partly delivered one goes to all
  again. `Server::restore` takes the owed list.
- `crates/db` (`catalog.rs`, `op.rs`, `partition.rs`): `NfsPurgeRecord`, `Op::NfsPurgeOwed` and `Op::NfsPurgeSettled`
  (appended), the partition's owed set, and its snapshot field. The oracle's histories own and settle purges across
  crashes (`crates/db/tests/model.rs`).
- `crates/server/src/nfs_state.rs`: the journal entries become those ops, and a restored table takes the partition's
  owed purges.
- `crates/server/src/nfs.rs`:
  - `purge_owed_here`, each shard's round at the reap cadence, over a minimal `PurgeBackend`. It builds a not-yet-built
    table when its partition owes purges, as the first compound would.
  - A test hook, `Daemon::inject_nfs_purge_refusal`, with its non-vacuity counter `nfs4.purge.refused_injected`.
- Tests, all passing after the change:
  - `a_drop_owes_its_purge_in_the_same_journal_and_its_settlement_is_journaled` and
    `a_restored_table_owes_the_purges_its_records_owed` (`crates/bridge-nfs/tests/v4_session.rs`);
  - the end-to-end test above, which failed first with the restore of owed purges taken out.

## Siblings

- A departed session's guest drops (`guest_drops`, A-76) are also memory-only. A guest session left on another shard
  after its client was dropped is harmless: requests on it find no client. Recorded in GAPS, not changed here.
- The kill bench through Docker (`docs/wip/bench/kill_mixed.sh`) is owed a run against this change once the Docker VM
  is restarted.
