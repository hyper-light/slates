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

## Strand finish -- 2026-11-21 ~00:45 PST
- db80da5e8 wire tags; 4930bf14b (main 544020bd2) rebase_establishing + revision seeded from next_seq.
- Cleaned locally: worktrees wire/review/codex, ~/.cache/w-work-cow-{serve,recv-bin,codex}. Kept: clone, target, push.
- OWED (kiln down at 00:40): delete on adacachy by exact path: ~/.cache/w-work-cow-codex.git, ~/.cache/w-work-cow-codex.bundle,
  ~/.cache/w-work-cow-{uniq,gw,amend}.txt, ~/.cache/w-work-cow-serve. And push this state commit.
- Queue empty.

## Large single submit (main's goal 8251d154) -- red, 2026-11-21 ~07:22 PST
- Test (uncommitted, worktree ~/slates-work/w-work-cow-slice over main 6a0600ff1): work_cow.rs
  a_single_large_submit_does_not_hold_its_shard_past_the_window, which runs the supervised role a_single_large_submit_role
  (common::supervised, 1 shard, anchored, so a stuck shard withholds the beat instead of exit 4). The role makes one
  seed work per case, then reads watchdog_turns before and after its ONE submit.
- RED on main 6a0600ff1, release, Mac Studio (log ~/.cache/w-work-cow-red/red-main-both.log):
  - one-gib (1000 x 1,073,741 B): edits 9.48 s; submit Ok(Accepted(1)) in 8.29 s; beat withheld at 49 of 56 looks.
    "shard 0 is spinning: 1066 ms of CPU without a step, past its 1025 ms window of CPU, in task spawned at
    crates/server/src/identities.rs:243, at its poll 1; the watchdog withholds the beat"
  - many-files (100,000 x 335 B): edits 5.33 s; submit Ok(Accepted(1)) in 1.32 s; beat withheld at 1 of 8 looks.
    "shard 0 is spinning: 1112 ms of CPU without a step, past its 1025 ms window of CPU, in task spawned at
    crates/server/src/daemon.rs:5206, at its poll 432676"
  - panic: "a single large submit held its shard past the liveness window: [one-gib ... the beat withheld at 49 of 56
    looks during one submit, many-files ... the beat withheld at 1 of 8 looks during one submit]"
- Two earlier runs that day, with the Studio loaded by other agents' builds, recorded rather than hidden:
  (a) the 1 GiB submit gave no answer for 92 s, and the parent's 100 s role limit failed the test; (b) an edit during
  seeding was refused NotFound (work_cow.rs:131) while the control shard's beat task ran 3,467 ms late. Both go to step 2
  as things to explain: the 8.3 s submit (not ~1.4 s) shows more than the hash runs in that one turn.
- adacachy cleanup (owed above): already done, ls of ~/.cache/w-work-cow* there found nothing (recorded at goal start).

## rebase_with (main's goal 88) -- settled 2026-11-21
- It landed: 4930bf14b (main 544020bd2), Green::rebase_establishing (crates/merge/src/engine.rs:6637); GAPS:24412 says LANDED.
  The close note's line saying "not ported" was stale; corrected.
- Proof the test holds it (merge_check a_kept_mode_on_a_file_the_resolution_writes_anew_is_not_a_conflict: the head
  removes f, the work's kept SetMode conflicts, the resolution writes f anew -> Rebased):
  - mutant (resolution_establishes forced empty, release): RED, left Conflict([MergeWindow { path: "f", at: 0, len: 0,
    class: 5 }]), right Rebased(2). Log ~/.cache/w-work-cow-red/rebase-with-mutant.log. engine.rs restored (git diff empty).
  - rule as on main (worktree over 6a0600ff1, release): merge_check 3/3 ok, the test included. Log
    ~/.cache/w-work-cow-red/rebase-with-green.log.
- Nothing to land for it (GAPS already closed at 24412); the close note is updated. Queue empty.

## Post-view hook for w-svn (main's order 2026-11-21, w-engine unreachable)
- Finding: A-805 (31c0acd50, w-rewrite-ops) already built it as a copy-on-write fork: Lineage::preview / Greens::preview,
  read by svn_made::revision_from at (0, 1). Nothing in the engine changed.
- Added main's oracle: slates-merge --lib a_held_verdicts_preview_reads_as_its_commit_over_generated_histories,
  60733f959 on agents/w-work-cow-post-view (merged with origin/main). Every read svn's revision takes (changed paths,
  content, kind, symlink, special, hardlink, mode, xattrs, copied, identity) equals the read after apply_recorded: 476
  verdicts, 131 with a copy, 96 cases, debug, Mac Studio. Mutant (verdict not committed into the preview): fails at case 1.
  engine::branch 10/10. Design A-805 has an "Oracle" bullet. No GAPS line tracked the ask.
- Told w-svn: inbox/w-svn-from-w-work-cow-post-view-2026-11-21.md.
- WITHDRAWN: while kiln was down w-engine landed the same oracle with a fix (607e5e02a, A-816; 20c2bc961): a preview
  now names a renamed home's links as changed as its commit does, a case my generator never hit. Mine (60733f959,
  agents/w-work-cow-post-view) duplicates it and is not landed. w-svn's note has a correction.

## Large single submit (goal 8251d154) -- step 2 in progress, 2026-11-21 ~15:45 PST; NOT landed, decisions asked of main
- Branch agents/w-work-cow-slice (merged with main c5e157467): a5c27aa9f + 03e97b265. Claimed A-841.
  - refining::owed_for_submit: a local submit whose post-state is more than one copy slice (measured memcpy rate over
    half the quantum) parks behind a prepare-only job (A-821's prepare reused).
  - The job composes, hashes, computes and encodes, then holds the verdict (judge_computed) and makes its files
    (Green::make_step), a slice a turn.
  - The step commits the made verdict (new Green::submit_judged) and copies none of the bytes.
  - Counters: merge.submit.sliced, merge.submit.make_slices, merge.submit.prepared_made.
- Focused tests (release): merge_check 3/3, works_off_owner 18/18, verdict_pages 5/5, work_cow 2/2, work_edit_cow 2/2,
  green_post_state 8/8 (its refinement test now counts a sliced seed's parts); debug green_post_state 8/8 with A-821's
  oracle (2 compared, 0 differed); slates-merge lib 270/270.
- Red test on the fix: the submit's own stalls (identities.rs:243, the job's last turn) are gone. What remains is the
  green's checkpoint right after (merge_service.rs:257): build 52 ms, encode 71 ms, record 527 ms (db.mutate_owned),
  measured 650 to 1,063 ms in one step. The test passed 1 of 2 runs on timing. That's w-engine's owed item (A-579,
  "slice the checkpoint's last step"); main asked whether I take it (inbox/main-from-w-work-cow-checkpoint-scope.md).
- A/B, Mac Studio M3 Ultra, release, interleaved, A = main c5e157467 + the test, B = a5c27aa9f/03e97b265, one
  submit, work_cow's populated_green (logs ~/.cache/w-work-cow-ab/ab-*.log, one .out per run). The box was loaded
  (load 9 to 18):
  - Run 1 (15:29), load 9.2-12.0:
    - 1.5 MB (24 x 64 KiB): A 4.23, 3.29, 3.36, 3.48, 3.45 ms; B 17.97, 15.25, 13.97, 14.22, 13.38 ms.
    - 1 GiB: A exit 4 x5 (watchdog, identities.rs:243); B 10.65 s, exit 4, exit 4, 4.64 s, exit 4.
  - Run 2 (15:37), load 10.8-18.9, with an experiment (the job's turns paced by A-783's step ceiling; REVERTED):
    - 1.5 MB: A 7.78, 7.02, 6.94, 7.21, 6.94 ms; B 12.06, 21.61, 19.01, 12.80, 14.58 ms.
    - 1 GiB: A exit 4 x5; B exit 4 x5 (the job's task, refining.rs:241, past its window in one poll: the experiment's
      fault).
  - So against the plan's bars: 1.5 MB is 3 to 4 times A, not within 10%. At this machine's slice a 1.5 MB submit
    parks, and its job's ~10 turns each yield. 1 GiB still ends the Alone daemon in most runs (the checkpoint step and
    load).
- Open, for main: (1) the checkpoint step (scope question above); (2) the park threshold. Slicing a 1.5 MB submit costs
  3 to 4 times its latency. Either park only above a step-ceiling's worth of work (the step then runs up to ~100 ms
  unsliced, HEARTBEAT_NS), or accept the latency.
