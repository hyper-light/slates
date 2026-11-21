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

## Piece 2: chunk-grain first edit (A-790) -- in flight, 2026-11-20 ~16:45 PST
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
