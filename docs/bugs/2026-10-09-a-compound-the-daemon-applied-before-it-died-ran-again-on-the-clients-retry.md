# A compound the daemon applied before it died ran again on the client's retry

**Found:** 2026-10-09, by the new mixed kill bench (`docs/wip/bench/kill_mixed.sh`: a Linux NFSv4.2 client makes 30,000
random creates, writes, truncates, appends, unlinks and renames while the daemon is SIGKILLed 30 times). **Status:
fixed** (same day, A-127).

## Description

Two seeds of six failed. In each failure the application was told an operation failed although the operation had
happened:

- seed 9: `rename("f23670", "r23676")` raised `ENOENT`, yet after the storm the tree held `r23676` and the model did
  not (`names differ: extra ['r23676']`);
- seed 8: `unlink("r1668")` raised `ENOENT`, and the file was gone afterwards;
- earlier, seed 101 (attempt 1): `unlink("f2189")` raised `ENOENT` for a file the model held.

Every acknowledged change was there after the storm: nothing was lost. What was wrong was the answer.

## Root cause

The daemon applied the compound and committed its effect at the barrier (§4.8), then died before the reply left. The
client never saw the reply, so it reconnected and retried, and the restarted daemon ran the compound a second time:

1. NFSv4.1 sessions, and with them the slot cache that answers a retry (RFC 8881 §2.10.6), lived only in the daemon's
   memory. A-37 kept the client records durable but said explicitly that "sessions are not durable, as RFC 8881
   §2.10.13 allows".
2. So the retry met `NFS4ERR_BADSESSION`. The Linux client made a new session (`CREATE_SESSION`) and sent the same
   operation again on it, as a new request.
3. The restarted daemon ran it against a tree that already held its effect: the REMOVE found no file
   (`NFS4ERR_NOENT`), the RENAME found no source.

The RFC permits this outcome ("the only acceptable outcomes are either the original cached reply or an indication that
the client ID or session has been lost", §2.10.6.5), and Linux knfsd behaves the same way after a reboot. slates
promises more: a daemon restart is meant to be invisible to a mounted client (A-37: no grace period; A-113: held
connections).

Reproduced deterministically before the fix by
`a_compound_retried_across_a_daemon_restart_is_answered_never_run_again` (`crates/server/tests/recovery.rs`). It
applies a REMOVE and a RENAME, restarts the daemon over the same anchor segment, and resends each compound with its
original sequence id. The test failed with `Err(10052)` (`NFS4ERR_BADSESSION`). With durable sessions alone it failed
with `Err(2)` (`NFS4ERR_NOENT`), the double run itself.

## Impact

- Applications on any NFSv4.1/4.2 mount: a daemon restart in the window between a commit and its reply turned a
  successful change into an error. This hits `unlink`, `rename`, `mkdir` (`EEXIST`), `link`, an `O_EXCL` create outside
  EXCLUSIVE4 (`EEXIST`), `close` and lock release (`NFS4ERR_BAD_STATEID` or `NFS4ERR_OLD_STATEID`, which Linux mostly
  recovers from). A git lockfile rename that ran twice fails the git command. Docker, Kubernetes and the Linux
  containers mount over NFSv4.2, so this touches conditions 2 and 11.
- How often: a few in 180 kills under the bench. The window is the time between a barrier and its reply's send.

## Exact edits

- **Durable sessions** (`crates/bridge-nfs/src/v4/session.rs`):
  - `SessionRecord`; `ClientChange::SessionSet` and `SessionCleared`;
  - journaled at CREATE_SESSION, at DESTROY_SESSION (home or departed), at `forget_away`, and when the callback minor
    version changes (`journal_session`);
  - a dropped client's sessions go with its record.
- **Restore:** `Sessions::restore(.., sessions_kept, ..)` brings each kept session back with:
  - its back channel down, so the client rebinds;
  - its slots restored (`Slot::restored`: the first request is taken as it comes);
  - the last CREATE_SESSION's grant rebuilt.
- **Database** (`crates/db`): `NfsSessionRecord` and `NfsChannelRecord`; `Op::NfsSessionSet` and `NfsSessionCleared`
  (appended); `PartitionSnapshot::nfs_sessions` (appended); `NfsClientCleared` drops the client's sessions.
- **Server, sessions** (`crates/server/src/nfs_state.rs`):
  - the session records are mapped to operations and restored at boot;
  - `with_v4_sessions` commits what the table journaled (`crates/server/src/nfs.rs`).
- **Call records** (`crates/bridge-nfs/src/v4/replay.rs`, new): `CallRecords`, `CallIdentity`, `CallKey`, `digest`,
  `is_bulk`. A kept compound's calls (its SEQUENCE asked `sa_cachethis`) are recorded at the shard that serves them,
  keyed by procedure, argument digest and occurrence.
- **Riding the commit:**
  - a call that changed a volume rides the barrier's publication: `ShardImage::calls` and `ShardDelta::calls` and
    `calls_removed` (`crates/vfs`, image version 18, delta version 3);
  - a call that changed file state rides that change's transaction: `Op::NfsCallSet` and `NfsCallsCleared`
    (appended), `record_files(s, reply)` (`crates/server/src/nfs_state.rs`).
- **Answer or refuse:**
  - a retried call is answered from its record before it is served (`serve_local` and `serve_file_state_here`,
    `crates/server/src/nfs_replay.rs`);
  - a call past the records' bound is refused `NFS4ERR_DELAY` before it runs. The bound is half the NFSv4 client share
    of the reserve: `nfs_v4_record_bytes`, with `nfs_v4_clients` derived from the other half (`crates/server/src/config.rs`).
- **Purges:**
  - a dropped client's records go with its state (`STATE_PURGE`);
  - a destroyed session's records go with the new `STATE_PURGE_SESSION`, sent to every owner (`purge_dropped`,
    `Purge`);
  - a purge is acknowledged only once a publication carries it.
- **Tests:**
  - `a_compound_retried_across_a_daemon_restart_is_answered_never_run_again`: REMOVE and RENAME;
  - `an_open_and_a_close_retried_across_a_daemon_restart_are_answered_never_run_again`: the file-state leg — the OPEN
    keeps its state id and its seqid, the CLOSE is not `BAD_STATEID`;
  - `a_destroyed_sessions_records_are_purged_and_stay_purged_across_a_restart`;
  - all three in `crates/server/tests/recovery.rs`;
  - twelve unit tests in `replay.rs` (T-127.1 to T-127.12, including hostile record decoding);
  - the streamed checkpoint and delta tests now carry call records.

## Found on the way

- The second test's first run failed: the OPEN came back with seqid 2. STATE_OPEN's arguments end with the client's
  delegation wish (which a restart's down back channel changes) and the caller's clock, so its digest differed on the
  retry and the open ran again. The digest now leaves out those twelve bytes, as it leaves out a WRITE's data. A
  retried call that meets its procedure and occurrence under another digest is counted (`nfs4.replay.diverged`), so a
  regression like this one cannot pass silently.

- The bench's first run with the fix counted `nfs4.replay.diverged` 2–14 times in each final daemon life, every one a
  false positive. A call's occurrence counts calls with the same procedure *and digest*. So two calls of one compound
  with one procedure and different arguments are both occurrence zero, and a record of the same life showed the second
  as "diverged". Divergence is now asked only of restored records (test T-127.10), and the next runs counted none.
- The first cut cost about 12% of a single writer's create (p50 192 → 216 µs, five alternated rounds). Three causes:
  - a durable record was re-carried by every publication after each later read;
  - file-state records rode publications as well as their transactions;
  - the digest was BLAKE3, on its portable path on aarch64.
  The final cut costs 2–5% (BENCHMARKS): a record rides a publication only for a volume change, the digest is FxHash's
  word mix, and answering skips its lookup while no restored record is held.

## Measured

- The mixed kill bench, before: 3 of 13 seed runs failed. After, on three builds: 450 kills over 15 seeds, every tree
  identical and no operation failed. The final build's lives restored 29–49 records per seed, and one retry was answered
  from its record (BENCHMARKS).
- The cost: +4 to +11 µs on a single writer's create p50 near 195 µs; +1 to +5 µs per OPEN+CLOSE pair in one daemon.

## Siblings (reported, not changed here)

- **macOS NFSv3 over a held connection (A-113)** re-runs the one request in flight at a death: a successor reads it
  again from the held socket. A REMOVE that was applied but unanswered is therefore answered `NFS3ERR_NOENT`. The
  design accepted this ("what the client's own retransmission after a reconnect did before"). The A-61 remedy applies
  unchanged: publish a reply keyed by the held connection and the xid, and answer a re-read request with it. Owed.
- **Pending purges are not durable.** `pending_purges` lives only in the home's memory, so a purge queued when a
  daemon dies is forgotten: the client's file state (pre-existing) and now its call records stay at an owner the purge
  never reached. Within one daemon every owner is a shard of the same process and is reachable, so the gap needs a
  failed cross-shard call followed by a restart.
