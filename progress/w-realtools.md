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
