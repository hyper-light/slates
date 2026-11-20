# w-engine: one copy of a file's bytes inside a green (step S2 of the content-once plan) (main, 2026-11-20 ~03:30 PST)

## Why
Ada (2026-11-20): one copy of every byte, and "significantly higher scaled throughput paired with significantly lower
resource usage". This is §4.17 D-28 ("one copy of every byte ... by identity", §4.11) as written. The approved
content-once plan (w-perf, A-723's amendment) has five steps. S2 is the merge engine's step, and it is yours.

## What w-perf found (2026-11-20, read in crates/merge)
- `Green::set_file` writes the new bytes into `content: Files`, a `dims::Blocks` arena shared by forked greens and lent
  between them by swap_arena/lend.
- It also records `content_value` in the green's history (`history::Chunks`, kept per green), spliced from the
  previous latest value.
- So the bytes a change ADDS exist twice. A whole-file replace duplicates the whole file, and every git or hg push
  and every svn import is one. A small edit duplicates only its bytes.
- Measured: svn 1,000 revisions held 234.8 MiB of history chunks beside about 585 MB of blocks.
- The fix crosses arena lifetimes: Files' blocks follow the fork/lend arena, while Chunks follow the green.
- `content(path) -> &[u8]` must stay contiguous for the VFS read path, so a head made of several spliced pieces
  keeps its own block.

## Your scope
1. DESIGN FIRST: a short entry in §4.16 (merge engine), with an amendment number claimed by pushing the ledger.
   - Unify blocks and history so that history's latest value IS the file's block, while older values keep chunk
     pieces.
   - Say how the two lifetimes meet: fork, lend, swap_arena, folding and checkpoint.
   - Say what recovery and replication rebuild, and what stays contiguous for reads.
   - Keep held.rs (A-716 1a, the identity -> block index) working, and remove nothing else it needs.
   Put the outline in your kiln summary. Main answers within minutes. Never wait idle.
2. IMPLEMENT in slices, each landing on its own, with the engine's model and proptest suites green
   (crates/merge's model tests, crates/vfs/tests/model.rs where it reads greens). Each slice carries a non-vacuity
   counter for the shared path: a block named by history instead of copied.
3. MEASURE: rerun w-perf's svn 1,000-revision profile (crates/cli/bench/svn_scale.sh) before and after, and record the
   history bytes saved in BENCHMARKS.md.

## Rules
- Follow ~/fanout-state-w-engine/briefs/common.md in full: the fast landing loop; claiming A-numbers by pushing the
  ledger; your own state folder; push routes; every piece ends on origin/main, confirmed with merge-base. Unsigned
  commits are fine.
- Follow CLAUDE.md: no Arc/Rc/Mutex, no panics, bounded work, typed refusals, derived tunables, module docs citing
  the design.
- Never force. Never checkout, switch, stash, reset or commit-tree. Never print, log or copy any API key or credential.
- Report in your kiln summary, since main can't take SendMessage across machines. Keep a progress file at
  ~/fanout-state-w-engine/progress/w-engine.md.
