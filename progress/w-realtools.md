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
- 15:35 PST: FINDING (recorded): one dead hard NFS mount stalls every NFS mount on the macOS client (READDIR/CREATE never
  sent). Main unmounted w-forming's; root-mounted kiln-01a1f9e974cb... remains (Ada). Real-tool runs moved to another Mac
  via kiln run (needs macos). Debug prints reverted. Experiment script: worktree .realtools/exp.sh (untracked).
- 15:45 PST: LANDED A-781, 5d639b68e (merge d5a465908): without a birth time, an unchanged base file keeps its inode
  number across a takeover (it was renumbered in lookup order, which breaks `#pragma once`). macOS arm64, Studio-4,
  local: vfs base 52/52, base_identity 8/8. No inode eviction path exists in vfs (inodes are never unloaded), so
  "eviction" has no renumbering source. Run request filed (Linux/Windows sim tests).
- Next: the kernel-mount acceptance test (crates/cli/tests/real_tools.rs, SLATES_TEST_CLI=1): a clang/gcc `#pragma once`
  build held mid-compile by a FIFO include across a daemon SIGKILL; then make (mtime/ctime), cargo incremental and
  git status. The kiln run r3 (.realtools/exp.sh, needs macos) is queued; Studio-4's NFS client is stalled by a
  root-mounted dead mount (Ada's).
- 16:05 PST: wrote crates/cli/tests/real_tools.rs (branch agents/w-realtools, not on main: unrun). One macOS kernel-mount
  flow over an overlay of a git checkout: a cc compile held by a FIFO include across `volume snapshot` and a daemon
  SIGKILL must see `#pragma once` header as one file; `make -q` and `git status --porcelain` clean after each event.
  Compiles locally (`--list` shows it). Run: kiln r7 (needs macos), queued -- Studio-4's NFS client is stalled; the
  balancer's macbook-pro-2 failed the repo sync 3x; adas-mac-studio.local and adas-macbook-pro.local went offline.
  Land it once its red/green is read; if green on APFS (birth times), the red case is a no-birth-time base (A-781 shape).
- 17:05 PST: C1 CLOSED (316a046f3). C2-C4 CLOSED on macOS: 617c88102, 249f5177d, 7f53665a1; 4/4 green on
  adas-macbook-pro-2 in 30.97 s. Linux FUSE and WinFsp runs are filed in run-requests.md. Runs go through my clone
  ~/.cache/w-realtools-clone on adas-macbook-pro-2 (sync:false; kiln's sync to that runner failed 3 times).
  Open in GAPS: the WinFsp port of the flows; memory pressure from a host actually short of memory.
