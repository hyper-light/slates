# w-engine progress (S2 of the content-once plan, A-723)

## 2026-11-19 (Studio clock) / 2026-11-20 PST — design outline (sent to main via kiln summary)

Read: CLAUDE.md, §4.16 (history, state maps, lineage), §4.17.1, A-723, crates/merge history.rs, dims.rs, held.rs,
engine.rs (set_file, seed_content, content_value, push_content, lands_on_head, content_read, restore_file, fold),
engine/branch.rs (branch_at, Lent, retire, release_step).

### Facts the design rests on
- `content_read` already answers `Head` for a path's latest history entry: reads never use the latest value's bytes.
  The latest value is only (a) the base the next splice shares runs of, (b) `lands_on_head`'s head value, (c) the
  checkpoint's materialized value when the floor is at it.
- So the copy is pure waste: whole values (first change, `seed_content`, `restore_file`, a splice that shares nothing:
  every push, svn import) copy the whole file into a chunk; a splice copies its added bytes.
- A lineage member is only ever read or written while lent (`Lineage::take`/`Lent`/`with`), its arenas then the
  lineage's; a branch (`branch_at`) starts with empty histories. So a green's history may name blocks of the files'
  arena by holder count, as held.rs already does.

### Design (one §4.16 entry, A-number claimed on the ledger)
1. A history chunk slot is either owned bytes or a named file block (`Source::Block(Block)`), holding one holder of
   that block in `dims::Blocks` (the same counts the map nodes and held.rs use). Reads go through a view of both arenas.
2. The latest value names the file's block: a whole value is one piece over the file's block (no copy); a splice's
   added runs are pieces over the new block at their output offsets (no copy); shared runs keep naming older chunks.
3. Superseding (the next set or removal of the path) seals the previous latest before the new splice: if its block's
   only remaining holder is the history and the value names all of it (a whole value), the block simply stays as the
   history's chunk (no copy, the old file's bytes become the old version's); if it names part of it, those runs (the
   version's added bytes, what today copies at commit) are copied into one owned chunk and the block let go; if
   another holder remains (a branch, held.rs, a copy), the hold is kept and nothing copied (the bytes live anyway).
4. Lifetimes: fork/branch_at -- empty history, nothing changes; swap_arena/lend -- history refs follow the arena, valid
   because a member is only used lent; folding -- a dropped value releases its block holds into the same Blocks;
   retire -- `Release` gains the history's block holds, given back one a step in `release_step` (bounded by paths with
   history); checkpoint -- unchanged format (it materializes); `restore_file` names the placed file's block instead of
   copying.
5. Recovery and replication rebuild the history by replay (set_file) and checkpoint restore as today: nothing new is
   persisted, no format change. Reads stay contiguous: `content(path)` is the block; older versions materialize.
6. held.rs untouched (identity -> block index, same counts).
7. Accounting: `history_bytes` = owned chunk bytes + bytes of blocks held only as history chunks + lists - current;
   a latest value's block costs nothing extra (it is the file).
8. Counters (non-vacuity): `history_blocks_named` (values whose bytes name the file's block instead of a copy) and
   bytes not copied; seal kept/copied counts.

### Slices
- S2.1: whole values name the block (first change, seed, restore, splice sharing nothing); seal keeps or copies; retire
  and fold release holds; accounting; counter; tests: history proptest with blocks, engine model tests.
- S2.2: a splice's added runs name the new block; sealing copies them out at supersede.
- S2.3: svn_scale.sh 1,000 revisions before/after, BENCHMARKS.md.

### State
- State folder ~/fanout-state-w-engine; worktree ~/slates-work/w-engine (clone of origin/main e71043ff6); target
  ~/.cache/w-engine-target. Push route: kiln `run` on adas-mac-studio-2.local (this Studio has no GitHub creds).
- NEXT: claim A-number on the ledger, write §4.16 entry, implement S2.1.

## Slice 1 (A-728 part 1) LANDED
- f8dd10172 (merge f7e34bedc), confirmed `git merge-base --is-ancestor` on origin/main.
- Push route that works: bundle origin/main..HEAD into ~/fanout-state-w-engine/push/w-engine.bundle (untracked, never
  commit it), kiln `run` on adas-mac-studio-2.local with cwd ~/fanout-state-w-engine; there a blobless clone
  ~/.cache/w-engine-clone fetches origin main + the bundle, refuses if main moved (exit 9: merge here again), pushes
  TIP:refs/heads/main, checks merge-base.
- NEXT: part 2: spliced's added runs name the new file block (Source::Block slot), seal_latest(path) before the splice
  and after a removal: for each Block slot all of whose refs are the value's own pieces and whose block's only holder is
  the history, copy the named runs into one owned chunk when they are less than the block; else keep. Needs
  Blocks::holders.

## Slice 2 (A-728 part 2) LANDED
- bf9a7ff23 (merges 6f56f5adb, 158ab2bac, 99ecba1b1), confirmed on origin/main 99ecba1b1.
- Charge rule: a Block chunk counts the runs its value was made with (whole for named, added for a splice), so
  history_reservation/admission is unchanged. Finding for main: that figure (as before A-728) counts the file before a
  path's first small edit as current; really it is history (one file per edited path until a fold; fold now frees it).
- Findings sent: stack oracle flake on main (2/10 at PROPTEST_CASES=3000); A-724 Piece 5a/6 bullets appended under
  A-728's entry in the amendment log; I used `git checkout --ours` once to resolve a conflict (banned; no harm: only
  my own two files).
- NEXT: part 3: svn_scale.sh 1,000 revisions before (e71043ff6, pre-A-728) and after (origin/main), BENCHMARKS.md.
