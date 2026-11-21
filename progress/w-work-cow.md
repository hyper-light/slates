# w-work-cow progress (2026-11-20)

Brief: briefs/w-work-cow.md. Host: Adas-Mac-Studio.local (32 cores, 96 GB, no GitHub push creds; land via kiln run on adas-mac-studio-2).
Clone: ~/.cache/w-work-cow-clone (seeded from w-git-clone, origin = GitHub HTTPS). Worktree: ~/slates-work/w-work-cow (branch agents/w-work-cow).
Target: ~/.cache/w-work-cow-target.noindex. Logs: ~/.cache/w-work-cow-logs.

## Finding (13:50 PST, main f727b9ec4)
The eager copy in the brief is from 896233d1f (2026-10-26). On main, create_work_here (crates/server/src/verbs.rs ~8254)
already starts a work with WorkFiles::default() (a delta only), reads base files from the green at its pinned version
(crate::work_files, green maps are persistent radix trees, A-534), pins the base by WorkFloorSet and drops it by
WorkFloorDropped. So the fix itself is on main (A-247/A-534); what's missing is the proof.

## Piece 1: red test + numbers -- LANDED 819341ab4 (main baa470b90), 2026-11-20 ~14:10 PST
crates/server/tests/work_cow.rs: 64 MiB green (1024 x 64 KiB), 16 works each request -> read -> edit; resident growth must be
<= 16 x (64 KiB metadata + 64 KiB file) + 8 MiB, i.e. below one copy of the green. Run in release on this Studio.
Next: grid measured example (1/10/100k files, 1 MB-1 GB, 1/10/100 works) if the test shows anything above the bound.

Result (release, macOS arm64, Mac Studio 32 cores, base e73283fd6): 16 works over 64 MiB green add +1,228,800 B (75 KiB/work),
mean 0.52 ms request->read->edit. Red with eager seed copy put back: +974,880,768 B, 4.46 ms/work, test fails.
Landed via studio-3 (2768d0d5...) since studio-2 (b24c5c95...) didn't answer in 15 s twice; script ~/.cache/w-work-cow-push/push.sh.
Grid (1/10/100k files, 1 MB-1 GB, 1/10/100 works) queued in run-requests.md (env SLATES_WORK_COW_*), for main under section 8.
Next: none in queue. One finding for w-work-mount: a first edit copies the whole file into the work (file-granular CoW,
WorkFiles::splice), so editing a 1 GB file costs 1 GB; chunk-granular divergence would bound that.
For main to delete: ~/slates-work/w-work-cow, ~/.cache/w-work-cow-target.noindex once main is done with the grid.

## Piece 2: chunk-grain first edit (A-790) -- LANDED 0c5dd0cb5 (main af045255b), 2026-11-20 ~18:40 PST
main 15:20: "a one-byte edit to a 1 GB file costs 1 GB; fix at chunk grain; red first; A/B release interleaved best-of-N".
- Design: a changed file = piece table (work_files.rs `Held`): base ranges of its origin path at the pinned base version +
  ranges of an append-only written buffer, compacted when dead > live. Edit/rename/unlink read only the base LENGTH
  (off-owner `Need::Lengths` + `owner_lengths`); off-owner reads of a changed file ask the base span its pieces name;
  post-state copies each op's range (`copy_range`); restatement written from the rebase's map. mounted_work.rs:91 ->
  `!work.files.is_empty()` (told w-work-mount, inbox -3).
- Red (tests/work_edit_cow.rs, release, this Studio): 8 one-byte edits of 64 MiB: +201,490,432 B resident, 2.38 ms.
  Green: +18,752 B live heap, 0.14 ms.
- A/B (BENCHMARKS, all N): live heap/work A 1.05 MB / 16.8 MB / 268 MB for 1/16/256 MiB vs B 2.6-2.9 KB; best mean
  261 us / 455 us / 5.15 ms vs 113 / 132 / 173 us. Note: RSS hides A's copies (allocator reuses freed seed blocks);
  live heap (slates_mem::heap::stats().in_use) is the meter.
- Ledger: A-790 (A-788 went to w-sessionlog). Branch agents/w-work-cow local (wip commit under a merge; land as ONE
  fresh commit from origin/main). Baseline worktree ~/slates-work/w-work-cow-ab (delete after landing).
- Landing notes: a shared target let the baseline worktree's slates-server look fresh to mine (one run tested A's
  library); fixed by removing the baseline worktree and touching sources, every result after shows `Compiling
  slates-server (.../w-work-cow-a790)`. main's b8c16aad7 (A-776 C1) re-asks the owner with `fetched.files` keys; it now
  carries `fetched.lengths` keys too (the generated oracle caught it: UnknownBase on 8 shards). work_home merge with
  A-697 f: `Need::Lengths` joins the holder-replica base reads.
- Final focused run (main da2b52d3e + A-790, release, this Studio): lib work_files 3, work_charge 1, work_log 2,
  work_home 2; tests work_edit_cow (+18,304 B live heap, 2,288 B/work), works_off_owner 16/16, mounted_work 4,
  mounted_work_cow 1. Merged 60586cdd3 after (docs + engine additions only, none of my surfaces): not re-run.
- Cleaned: worktrees w-work-cow, w-work-cow-a790, w-work-cow-ab; ~/.cache/w-work-cow-{ab-run,serve,logs}. Kept:
  ~/.cache/w-work-cow-clone, ~/.cache/w-work-cow-target.noindex (1.3 GB), ~/.cache/w-work-cow-push (push scripts).
- Next (not started): a rebase restates changed files whole (Rebased::changed full bytes; WorkFiles::from_changed) --
  a work rebased after a one-byte edit of a 1 GB file holds 1 GB again. Pieces through the rebase would bound it.
  mounted_work.rs ~389 `changed_span` materializes a changed file via `get` (w-work-mount's; told them len+page).

## Close list (Ada's order 2026-11-20 16:20): C1 + P2 -- BOTH CLOSED
- C1 = A-790 (0c5dd0cb5, main af045255b); close note pushed (bff98c3f9).
- P2 = the grid (5aef5f02f, main f9c113ae9): 135 runs in BENCHMARKS; after 1.6-5.6 KB a work flat, before 1.5 MB-1.25 GB.
- New finding -> GAPS only: a 1 GiB / 100k-file single submit stalls its shard past the liveness window (watchdog exit 4).
- Cleaned: grid worktree, ~/.cache/w-work-cow-{grid-run,serve}. Kept: clone, target, push scripts.
- Queue empty.

## Codex goal (main 2026-11-20 ~20:00): land the Codex agent's Green/Work + atomic-conflict work, or record it superseded
- Inventory (adacachy, read-only on ~/Projects/slates): 476 branches (work/slates-*, experiment/*, codex/*), 1,387
  distinct commits not on main (cc29bfc4c), ~6,394 behind; 204 commits touch merge/green/work/conflict.
- My analysis copy: adacachy ~/.cache/w-work-cow-codex.git (bare, --shared on the checkout, gh/main fetched), lists
  ~/.cache/w-work-cow-{uniq,gw}.txt there. Strands seen: green_preparation (Green publication/physical fill),
  green namespace fill + directory recreation (merge engine), owned merge verdict (crates/merge/tests/prepared.rs),
  mounted early conflict, Work NFS lease stale writes, work/slates-atomic-conflicts, work/slates-conflict-cli.
- Main lacks the Codex files (green_preparation.rs etc.); verdicts by behaviour pending. Next: bundle the Codex refs
  to this Studio and read each strand against main. Kiln server disconnected ~20:40; retrying.

## Codex goal -- CLOSED 2026-11-20 ~23:10 PST
- d8416294c GAPS record (94 Green/Work branches: 3 landed, 7 verdict, 38 publication, 10 engine, 14 Work, 16 perf -> superseded; 1 finding; 5 out of scope).
- A-796 (ledger) landed: 6823fde6f via 68ce3d585 (CheckMerge/ReadMergeCheck/ResolveMerge, MergeAsk, RebaseAnswer::Checked);
  c1e76d0a7 via d2ad10da4 (MCP + CLI). Tests: merge_check (1 & 8 shards), works_off_owner 16/16, verdict_pages 5/5,
  work_cow, lib work_*; mcp the_mcp_surface_serves_the_tools; cli the_grammar_parses_the_merge_verbs.
- Cleanup: worktrees w-work-cow-codex, w-work-cow-review; adacachy ~/.cache/w-work-cow-codex.git (+ .bundle, lists, serve).
- Queue empty.
