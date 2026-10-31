# A connection's end went to every shard (2026-10-31)

**Status:** fixed before it shipped, failing test first, in A-192's change.

## Description

A-192 keeps byte states per NFS connection on the shards that serve its READs and WRITEs, and drops them when the
connection ends. Arm B dropped them by sending the end to every shard of the daemon, whatever the connection had read:
a `Control::Spawn` on each other shard's control channel per connection that ended. Found reading the code after arm B
failed the hostile suite's `a_mount_survives_connections_broken_and_tampered_mid_call_under_load` once on fedora-2: two
honest clients' calls were answered `SYSTEM_ERR` (`RPC accepted`, left 5).

## Root cause

`nfs_writeback::closed` broadcast `ended` to every shard. Each shard's control channel and task arena are sized for the
clients' forwarded calls: one task per forwarded request at its owner and one for its reply (`config.rs`,
`tasks_per_shard = clients_per_shard × TASKS_PER_CLIENT + LOOP_TASKS_PER_SHARD`). Connection ends are not bounded by
the clients' seats: a client that opens and drops connections that read nothing put a task on every other shard per
connection, from the same channel forwarded calls are admitted through, and a daemon of S shards paid S − 1 messages
for each.

## Impact

None shipped: found before A-192 landed. It is not shown to have caused that `SYSTEM_ERR`, and something else causes
it too: interleaved with arm A on the same box (load 24–63, `CPUWeight=1`, two CI jobs), arm A, which sends nothing at
a connection's end, failed the same test the same way in one of five runs. A forwarded call's
deadline is the liveness budget, 1 s (`serve_remote`), and the box starved that run's shard for up to 1.6 s (the
daemon's own heartbeat line: `a heartbeat came 1626 ms after the last`).

## Reproduction

`crates/server/tests/nfs_writeback.rs` `a_connections_end_reaches_only_the_shards_its_reads_and_writes_did`: on a
two-shard daemon, 32 connections each read the file of one volume, and one reads a file on each shard. Before the fix
every end went to the other shard (macOS, 1 run of 1):

```
NFS-AB ends: 33 sent to another shard for 33 connections; live tasks 10 before, 10 after
```

## Exact edits

- `nfs_writeback::Footprint`: the shards a connection's READs and WRITEs were served on, each noted once, owned by the
  connection and moved with it between shards. Dropped however the connection ends (its end, a refused move, a
  cancelled task), it drops the states there: inline on its own shard, by a message to each other one, counted
  `nfs.writeback_ends_sent`.
- `nfs.rs`: `Connection` carries its footprint; a loopback READ or WRITE notes the shard serving it (the connection has
  moved to its volume's owner by then); the network export's entry (`reply_to_message`) notes the owner it routes to;
  `migrate` names the footprint, so it moves with the connection rather than drop with the husk.
- `xshard::run_on_waiting`: a message whose loss would leave state behind waits out a full channel as a reply does
  (`send_back`'s wait, shared as `given_room`), counted `nfs.writeback_close_refused` past that.
- `nfs_tls.rs`: a session's footprint replaces the explicit end, so a cancelled session leaves nothing either.

After the fix the test above reads 1 of 33, live tasks 10 before and 10 after, on macOS and on fedora-2 (9 and 9).

## Siblings swept

Every loop over all of a daemon's shards in `crates/server/src` (`grep -n "for shard in\|shards.clone()"`): at boot
(`daemon.rs` `restore_client_ids`), after a control verb or a revocation (`verbs.rs`, the control channel's rare
verbs), and in periodic loops (`reap.rs` `sweep`, `fleet.rs`, `member_task.rs`). One more is per request and is
reported, not changed here: a listing of the host root (`nfs.rs` `gather_all_entries`) asks every shard for its entries
on each READDIR of the root, though the capability it presents authorizes one volume, whose owner its id names
(D-14). A client holding a capability can ask it as often as it likes.
