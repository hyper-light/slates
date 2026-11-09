# An NFSv4 file removed while open was reclaimed at once, and the client renamed it aside (2026-11-08, A-260)

## Description

Through the daemon's NFSv4.1/4.2 server, a program that removes a file it holds open (git, editors and sqlite all do)
lost the file, or left a `.nfs*` name in the volume:

- Over the daemon's own transport, a REMOVE of an open file reclaimed it at once. Its handle then answered
  `NFS4ERR_STALE` at the first GETATTR, and its READ under the open failed with it (the three red tests below).
- The Linux client keeps POSIX's unlink-while-open by renaming the file aside, unless the server's OPEN answered
  `OPEN4_RESULT_PRESERVE_UNLINKED` (RFC 8881 §18.16.4; the client honours it since kernel commit 43245eca, 2022). This
  server never did, so every such file became `.nfs<fileid><counter>` until its close: with the promise turned off, the
  kernel's client made `.nfs000100000000000b00000009`. pjdfstest listed it on the NFSv4 transport as `unlink/14.t:4`
  (NFS-OPEN), and a landing taken while the name existed would have carried it to disk.

## Root cause

An NFSv4 open lived only in the owner shard's file state (`crates/bridge-nfs/src/v4/files.rs`, A-36) and its journal
(A-37). It took no reference in the volume, whose unlink keeps a file only while a transport references it
(`Volume::drop_link`: an orphan while referenced, reclaimed otherwise). FUSE takes those references at lookup; NFS took
none, since NFSv3 has no open.

## Impact

Every NFSv4 client: a removed open file vanished under its holder (reads failed with ESTALE), or, through the Linux
client, the volume carried a `.nfs*` name until the close. NFSv3 is unchanged: it has no open, so its clients' silly
renames stay (NFS-OPEN on the NFSv3 lists).

## The fix (A-260)

- `crates/vfs/src/ids.rs`: `RefOwner::NfsState(attachment)`, a third owner kind, kept out of the recovery image and
  settled by its attachment's survival (`crates/vfs/src/volume.rs`).
- `crates/bridge-nfs/src/v4/files.rs`, `delegation.rs`: a `Holding` queue (`Took` at a new open or grant, `Gave` at each
  end, however it ended), kept only by the durable state a restore builds.
- `crates/server/src/nfs_state.rs`: `record_files` applies the holdings after the call's record commits (`hold`); a
  recovery re-takes every journaled open's and delegation's hold before it settles references (`hold_recovered`,
  called from `crates/server/src/verbs.rs`); an attachment's end releases its holds (`release_attachment`).
- `crates/bridge-nfs/src/v4/compound.rs`: OPEN answers `OPEN4_RESULT_PRESERVE_UNLINKED` when the backend preserves
  (`Backend::preserves_unlinked`: the daemon's does, `crates/server/src/nfs.rs`; the standalone server's does not).

## Tests, each red before the fix

- `crates/server/tests/recovery.rs`: `a_file_removed_while_open_over_nfsv4_serves_until_its_last_close`,
  `..._serves_across_a_daemon_restart` (red again with the recovery's re-take removed) and
  `..._is_freed_when_its_clients_lease_lapses` (red again with a purge's give-backs removed: its 65,536 bytes kept).
- `crates/bridge-nfs/tests/v4_delegation.rs`: `every_open_and_delegation_takes_one_hold_and_gives_it_back_at_its_end`
  (red with the delegation give-back removed).
- `crates/server/tests/nfs_v4_kernel.rs` and `crates/cli/tests/nfs_v4_restart.rs`, through the Linux kernel's client
  (red with the promise off: `.nfs000100000000000b00000009`).
