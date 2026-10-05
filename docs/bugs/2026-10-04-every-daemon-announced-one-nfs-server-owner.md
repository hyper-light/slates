# Every daemon announced one NFS server owner

Date: 2026-10-04. Scope: the NFSv4.1 front end's `EXCHANGE_ID` (§4.6 A-35, A-37), against the Linux client
(Docker Desktop's 6.12 kernel).

## Symptom

A Docker A/B of the Rust workload (`e2e-docker-rust-slates.sh`, scratch) hung for 40 minutes on its third run.
`docker ps -a` showed the run's container stuck in **Created**, never started, and `docker rm -f` hung as well.

## How it was found

The Docker VM was inspected from a privileged container with `nsenter -t 1`:

- One of dockerd's threads was mounting the run's NFS volume, blocked in
  `nfs4_get_rootfh → nfs41_find_root_sec → _nfs41_proc_secinfo_no_name` (`rpc_wait_bit_killable`).
- The run's daemon was alive and answered `slates status`, but counted exactly one NFSv4 compound
  (`nfs4.compounds: 1`): the kernel had sent `EXCHANGE_ID` and nothing after it.
- Two NFS state-manager threads (`192.168.65.254-manager`) were blocked in `nfs4_proc_sequence`, renewing leases with
  daemons that no longer existed. One was from a run five hours earlier, whose `cp` was still in D state.

## Root cause

After `EXCHANGE_ID` the Linux client looks for an existing client of the same server and reuses it
(`fs/nfs/nfs4client.c`: `nfs41_walk_client_list` takes the existing `nfs_client` when `nfs4_match_client`, which
matches the minor version, the server-assigned client id and the client's owner string, and
`nfs4_check_serverowner_major_id` both agree). In slates both values came from the partition's NFSv4 instance:

- the `server_owner4` major id was the instance itself;
- client ids were `instance << 32 | n`;

and a fresh partition's first instance was always 1. So every fresh slates daemon, on any port and any host, answered
major id `1` and gave its first client the id `1:1`. The kernel took the new daemon for a dead one, attached the mount
to that daemon's `nfs_client`, and waited on its state manager, which retried a vanished server forever under a
`hard` mount.

The same collision broke RFC 8881 §2.4 inside one host: a daemon that came back with a fresh partition (whole-anchor
RAM loss) on the same port minted the same client ids as its predecessor, so a client presenting an id from the dead
life could be taken for a different, live client instead of being told `NFS4ERR_STALE_CLIENTID`.

## Impact

- Any Linux client that had ever mounted a slates daemon could not mount another fresh one while the old client
  lingered (a dead daemon under `hard`, which is the default): the mount hung.
- Two live slates daemons mounted by one Linux client would have shared one client between them.
- After whole-anchor loss, stale client ids could be misattributed.

## Fix

- Failing test first: `two_fresh_daemons_name_different_servers_and_mint_different_client_ids`
  (`crates/server/tests/nfs_mount.rs`) starts two fresh daemons and exchanges ids with each as the same client. It
  failed on the owner (`[7, 84, 84, 95, 77, 179, 98, 76]` twice, from a first attempt that used the per-host anchor id
  alone) and, with only the owner fixed, on the client id (`4294967297` twice).
- The major id is a keyed hash (`blake3` derive-key `slates/nfs-server-owner/v1`) of the host's anchor identity and
  the instance name (`nfs_state::server_owner`): stable across the instance's restarts, as RFC 8881 §2.10.5 and Linux
  nfsd keep it, and distinct per instance and per host.
- A fresh partition's first instance is 31 random bits of the boot nonce (`nfs_state::fresh_instance`), not 1; a
  kept partition still advances by one. Client ids, session ids and state ids carry it, so none repeats across lives
  (Linux nfsd puts its boot time in the same place, `cl_boot`).

## Siblings checked

- The NFSv3 write verifier is the boot clock in nanoseconds (`ShardState::write_verifier`): it changes every life.
- State ids carry `owner_tag(partition, instance)` and inherit the fix.
- The server scope stays `"slates"`; the major id now separates servers.

## Recovering a wedged Docker VM

Writing `1` to `/sys/fs/nfs/*/shutdown` in the VM (Linux 6.2 and later) fails every RPC of those clients; dockerd's
blocked mount and `docker rm` then complete. The e2e scripts must remove their Docker volume before stopping the
daemon.
