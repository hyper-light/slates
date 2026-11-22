# w-svn → main 01a20182: finding (2), the attribution command and what it found so far (2026-11-21 ~23:20 PST)

## The command (git-2000, this MacBook, release, at origin/main 9296695b5 or later)

```
unset ANTHROPIC_API_KEY
SLATES_SVN_IMPORT_DUMP=<git-2000.dump> SLATES_SVN_IMPORT_HOLD=<a path of yours> \
  cargo test --release -p slates-cli --test svn_interop -- --ignored an_import_measured --nocapture
```

- 4bc541ca5 makes `an_import_measured` print `slates status --memory` (every kind, the heap, each shard's
  `arena.allocated`, the content store's purges) with the daemon's RSS four times: before the import, after it, and
  2 s and 30 s into the hold.
- Nothing suspends the daemon: no vmmap, heap or malloc_history.
- Delete the hold file after the "idle 30 s" lines.
- **What I need back:** the four `an_import_measured: memory ...` blocks.

## What it already shows

**1. The heap figure is mostly the daemon's own startup allocation.**
- In the smoke run (debug, 2 MB dump), the daemon has heap `resident_bytes=1,303,204,592` before any import, at an
  RSS of 84 MB.
- `heap.resident_bytes` is `in_use`: bytes allocated, touched or not.
- So r10's 1.49 GB of heap is ~1.30 GB of startup baseline plus at most ~0.19 GB from the import. The 1.96 GB is RSS,
  pages touched, and the heap figure can't say which.

**2. In process, the arena gives its bytes back but RSS does not drop.**
- I reproduced the pattern with a test (local, not landed): 400 rewrites of one 256 KiB file, a 105 MB dump, one batch.
- RSS: 179 MB before, 867 MB after (6.4x the dump).
- Live owners afterwards hold almost nothing:
  - the green: its 256 KiB head, no history;
  - the records: 24 KB;
  - the VCS content store: empty;
  - no volumes or views.
- The shard's chunk arena holds 317 MB after the import and gives it back within ~1 s of idle (to 0.5 MB). Yet RSS
  only falls to 789 MB.
- The heap's `reserved` rose 228 MB, freed memory the allocator kept, while `in_use` fell.
- So what stays is pages released but not returned: the arena's released extents and the malloc zones' freed
  regions. It is not a live holder.

## Next for me

Your four samples tell me whether git-2000's 1.96 GB is the same pattern at scale. If they are, the red test is RSS
(or physical footprint) after an import and an idle tick, bounded by a derived multiple of what the import still
holds. Then I look at how the arena gives pages back (`release_block`: MADV_FREE leaves pages counted until the
kernel takes them) and at the malloc zones' retention. If they are not, I look for the live holder.

## Update ~23:45 PST: run at 78e584f18 or later, not 9296695b5

c74f35cc1 (merged 78e584f18) adds the daemon's **physical footprint** to each sample. It reads `top`'s MEM column,
`proc_pid_rusage`'s `ri_phys_footprint`, without suspending the daemon. On macOS, RSS keeps pages the arena's idle
purge marked reusable (`MADV_FREE_REUSABLE`) until the kernel takes them, while the footprint drops at once. So the
footprint, not RSS, says whether the import's memory was given back. The command is otherwise the same. Smoke: 45 MB
of footprint before the import and 51 MB after, against 85 MB and 105 MB of RSS.
