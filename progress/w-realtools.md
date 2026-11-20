# w-realtools progress (real tools define filesystem correctness)
Brief: briefs/w-realtools.md. Clone: ~/slates-work/w-realtools (object store), worktree ~/slates-work/w-realtools-red
(branch w-realtools, from origin/main 2e4415441). Target: ~/.cache/w-realtools-target.noindex.

## Queue
1. RED: `#pragma once` across a snapshot change on a real macOS NFS mount (inode number stable) -- in progress
2. RED: make rebuilds nothing untouched (mtime/ctime stable) across snapshot change, eviction, daemon restart/takeover
3. RED: cargo build incremental no-op; 4. git status clean over the mount
4. Fix at the root per platform; cross-platform runs in run-requests.md

## Log
- 2026-11-20 13:55 PST: cloned, briefs read, building the CLI to establish the facts by use.
- 2026-11-20 ~14:30 PST: BLOCKED on this Mac's kernel NFS client, not on slates. On every slates mount here (mine and
  the live kiln workspaces), `ls` and `touch` hang inside the kernel. GETATTR, LOOKUP and ACCESS are answered, but the
  client never sends READDIR or CREATE. Debug prints in nfs.rs serve_connection/serve_v3 (uncommitted, in the
  w-realtools-red worktree) show every call answered and sent, no retransmits, and empty socket queues. Two dead hard
  mounts are present, with no server listening:
  kiln-01a1f9e974cb724bb826f0199619ef25 (port 57217, "not responding") and
  ~/.cache/w-forming-land.noindex/tmp/slates-live-fleet-live-mounted-19318/mnt (port 49260).
  Likely cause: a dead hard mount stalls the shared macOS NFS client (unproven). Asked main whether `umount -f` on
  those two is OK; they are not mine.
- Facts read so far: the NFS fileid is the volume's inode number (bridge-nfs procedures.rs fattr3). Overlay base
  entries get inode numbers lazily in materialize() (vfs base.rs:1826, "nothing is journaled"), which is the EdenFS
  shape and the first red candidate for daemon restart and eviction.
  Snapshot change on a mount = `slates advance ATTACHMENT [SNAPSHOT]`.
## For main to delete
- ~/slates-work/w-realtools-scratch (my experiment tree; nothing mounted)
