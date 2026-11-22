# w-sizing -- end gross over-allocation, slates-wide (brief: briefs/w-sizing.md; supervisor "main")
# Machine: Adas-Mac-Studio-4.local (14 cores, 36 GiB), local builds, target ~/.cache/w-sizing-local-target.noindex
# Clone ~/slates-work/w-sizing-main (no checkout); worktree ~/slates-work/w-sizing-seg (branch agents/w-sizing)
# Logs/patches ~/.cache/w-sizing-logs

## Piece 1: the segment's prefault reserve -- LANDED (A-765)
- LANDED main 6337049f6 (piece 6ea16f4c5), confirmed merge-base. Worktree ~/slates-work/w-sizing-seg.
- Cause (base 159f1c873): the idle reserve (A-214) = min(last burst, reserve_per_shard, arena free), no decay; the sweep
  settled with it held, so an idle shard held its largest burst forever. reserve_per_shard = memory / shards / 3.
- Fix: daemon::decayed_reserve (burst whole one sweep interval, then halved per interval); the sweep stays armed while a
  reserve is held; below one discard it is zero.
- Numbers (macOS arm64, Studio-4, local): 64 MiB burst, 20 s idle: reserve 67,108,864 B before -> 0 after.
  Tests: lib decayed_reserve 1/1; recovery an_idle_shards_reserve_decays + a_deleted_files_memory_goes_back 2/2
  (also on the merged tree).
- Finding (probe ~/.cache/w-sizing-probe/shm_probe.c, 64 MiB shm_open object): MADV_FREE_REUSABLE drops the footprint
  64 -> 0 MiB but RSS and mincore stay 64; MADV_FREE/DONTNEED do nothing; MADV_PAGEOUT refused. So on macOS RSS/vmmap
  "resident" overstates given-back shared memory; facts::resident_now (pressure hold's own growth) reads RSS.
- Finding: content.purged_bytes counts discarded ranges, not resident pages (10.3 GB "given back" for a 64 MiB burst).
- Finding: content_range.arena.resident read 0 with 64 MiB allocated in the red run (a stale sweep).

## Piece 1b (main's 4 items after A-765), in flight
- w-hg's split (release 1d4909821, after hg history push): RSS 8,520 MiB; prefault_reserve 4,931.4 MiB = whole burst;
  20 s idle -> 12,055 MiB (+3.5 GB refill). Content retained 4,495.5 MiB is w-hg's.
- Item 4 DONE (probes ~/.cache/w-sizing-probe): REUSABLE on shm: footprint 513->1 MiB, RSS 513 stays, NOT reclaimed in
  10 s under pressure (reusable_reclaimed +0); frees only when the object is destroyed. Purgeable Mach memory entry +
  VM_PURGABLE_EMPTY: RSS 513 -> 1 MiB at once with two mappings, reads zero after NONVOLATILE, whole-entry granularity.
  => true give-back on macOS needs arena extents as purgeable entries shared with the anchor by port (design piece).
- Item 1 A/B: kiln run r1 (queued): ab/run.sh in ~/slates-work/w-sizing-seg (untracked ab/): A = main 6337049f6, B =
  ab/no-prefault.patch (reserve 0); provision_bench + nfs_large_write, release, interleaved x3, /usr/bin/time -l.
- Sweep list LANDED main 79e0a4723 (GAPS, MEMORY SIZING entry: every site, today -> target).
- A/B r1 lost its placement before starting; resubmitted as r2, started on d1323898.
- LANDED main 2dc414a84: idle purge walk allocation-free (for_each_piece, no order clone; oracle test). mem arena:: 19/19.
- A/B: r2 (sync not answered by d1323898), r4 (d1323898 silent 15 s), r3 stopped (no room at mem=24g); r5 = no-sync
  remote clone of origin/main cbf7c84c3 + inline patch, needs cpus=4,mem=8g: QUEUED (macOS fleet full). Local Studio-4 is
  at load 26 (A/B here would be noise).
- Correction (probe): on macOS RSS is the honest holding; footprint UNDERstates (given-back shared pages stay physical,
  not reclaimed in 10 s under pressure). resident_now (RSS) stays; GAPS line "(open)" to be corrected.
- By reading: reserve_per_shard's tables (inodes, dirs, chunks, tags, clients) are segmented slabs grown on demand,
  bounds only; its RAM cost is the prefault cap + admission; the rest is address space.
- True macOS give-back design (to scope with main): pool extents as purgeable Mach memory entries (VM_PURGABLE_EMPTY frees
  while the anchor still maps it), handed to the anchor by port; replaces shm_open's one content object on macOS.
- No-prefault removal written, tested, NOT landed: branch agents/w-sizing-noprefault (1fe113d5b + dbf369922, base
  5518fb2bc): -907 lines (reserve, refill, burst fields, status row, arena prefault/upcoming/KeptRun, Buddy::upcoming);
  red/bound test a_burst_leaves_nothing_resident_behind_it_but_its_data (Linux). Local: mem 36/36, server idle_sweep 2/2,
  recovery deleted-file 1/1. To land: fresh worktree from origin/main, squash, docs (A-214 status, GAPS, BENCHMARKS).
- A/B now branch-based (A = branch~2, B = branch tip), no sync: r6 macOS, r7 Linux (both queued). r5 was stopped by me
  just as it started on d1323898 (mistake).
- A/B DONE (r6, big Studio, release, interleaved x3): provisioning 1-client p99 A 226.7/197.4/203.5 vs B
  232.5/200.6/182.3 us (both fail 50 us at load 9-31 alike); NFS best equal; kept rounds ~5% to A; NFS peak RSS A
  1,137-1,139 vs B 625-627 MiB. => removal.
- A-777 LANDED main 21a306a04 (reserve removed, -907 lines; BENCHMARKS entry with all N; design A-214 status + A-777
  section; GAPS). Worktree ~/slates-work/w-sizing-land. Branch agents/w-sizing-noprefault kept until r7 (Linux A/B) runs.
- r12 (kiln, d1323898, its hg source ~/.cache/w-hg-mem/hg 84 MB): hg history push before (47dc24b72) vs after
  (21a306a04), interleaved BAAB: rss, top MEM, system free/reusable, status content ranges after push and after 20 s.
- Proposal to main: inbox/main-from-w-sizing-content-extents.md (content as extents made on demand; needs ruling).
- r12 (big Studio, release, BAAB, hg history 84 MB): daemon RSS after push 6,192-6,197 MiB in BOTH arms (before
  47dc24b72 / after A-777 21a306a04); +1-3 MiB after 20 s idle in both. A-777 does not move this workload (A-765 had
  already ended the idle refill); the 6.2 GB is held data (w-hg's content).
- FOUND BY USE: system free fell 44,904 -> 58 MiB across r12's four runs: SIGKILLed anchors leave their shm segment +
  content in the kernel on macOS. Unlinked my 8 names: free 2.8 -> 23.7 GiB. inbox/main-from-w-sizing-shm-leak.md.
- Build break from A-777 (cli.rs imported CONTENT_PREFAULTED; my grep misread): fixed, LANDED main 42267c087.
- A-778 LANDED main 779840fba: daemon removes the anchor's segment/content names when the anchor is gone
  (slates_mem::unlink_handoff); red->green cli test on the big Studio; bug record docs/bugs/2026-11-20-a-killed-anchor-...
- Worktree now ~/slates-work/w-sizing-a778 (landed, removable). Branch agents/w-sizing-noprefault kept for r7 (Linux A/B).
- 15:20 ruling (main): content as extents made on demand, YES; macOS first behind the pool API; conditions (a)
  provisioning untouched (made ahead), (b) Linux/Windows measured first (g226 takes it), (c) bounded counts, (d)
  recovery across processes. Branch agents/w-sizing-noprefault deleted (landed as A-777; its A/B was r6). Local target
  deleted (2.5 GB). g226's status residency read 0 MiB with a 256 MiB burst on macOS (sweep defect; mincore on a fresh
  view does report the object, probed) -- tell g226.
- Extents piece 1 (macOS), branch agents/w-sizing-extents, worktree ~/slates-work/w-sizing-a778:
  red 961112d0e (recovery a_bursts_extents_go_back_to_the_system_once_its_volume_is_gone; measure = anchor's content
  object residency + process RSS). RED on adas-macbook-pro-2 (quiet): before 63 MiB, with burst 1,621, after destroy
  1,622 (content object 259 MiB kept, RSS 1,362 kept). Fix d0f3199f9: slates_mem::extent_handoff/make_extent; pool
  makes own slice's extents at start, maps by name, release unlinks + remakes empty; daemon unlinks every extent name
  when the anchor is gone. mem shared:: 6/6 local. Green run r22 queued.
- GREEN r22 (adas-macbook-pro-2, quiet): 256 MiB burst: pool 420 -> 1,648 -> 123 MiB after destroy (content object
  2 -> 0 MiB). (d) r24 (big Studio): cli killed-anchor (incl. extents), locked-volume SIGKILL-daemon restart byte for
  byte, anchor-supervises, recovery acknowledged-content restart: all ok; fmt clean. (a) provision A/B r25 (A=1b63ab896
  base, B=3f24e4fea) queued -- the last gate before landing A-789 (claimed). Docs drafted in the worktree.
- g226's Linux/Windows give-back numbers: inbox/w-sizing-from-g226-giveback.md. Linux: one memfd + punch (keep);
  Windows: section per extent (decommit refused, discard per process, commit only back at close). Asked g226 to land
  them in BENCHMARKS as "Content give-back per platform"; told g226 its status residency reads 0 on macOS.
- A-789 LANDED main 362bd8c39 (merge 4e461fd36): macOS extents as own objects. Provision A/B r26 (big Studio, load
  44-79): p50 equal, p99 not worse; quiet rerun in run-requests.md. Branch agents/w-sizing-extents deleted; worktree
  w-sizing-a778 removed; now ~/slates-work/w-sizing-a789 (landed, removable).
- Flaky, not mine: mem arena::tests::a_purged_block_leaves_the_footprint_and_is_cleared_before_reuse failed once in a
  parallel run on pressured Studio-4 (footprint 69,812,680 -> 69,632,408), passes alone.
- NEXT (extents): Windows section per extent (g226: only a closed section returns commit); the content object's arena
  address-space range (layout); the 5x process RSS over a burst (1,362 MiB for 256 MiB, r21) -- find what holds it.
- Then item 2 (if prefault pays: EWMA of pages turns touch) or remove; item 3 reserve_per_shard work-derived + pressure.
- Disk here 14 GiB free (others' caches; mine 2.5 GB).

## CLOSE LIST (Ada 16:05-16:15, binding; briefs/close-list.md): C1, R2, R3 only. New findings -> GAPS + here.
- C1 CLOSED: inbox/main-from-w-sizing-close.md (A-777 21a306a04; r6 A/B).
- R2 (7.1 GB resident after an 84 MB push; extents made on demand): macOS extents A-789 362bd8c39 + leak A-778
  779840fba + prefault A-777. Linux: no change needed (g226: one memfd + punch frees with peers mapped). Windows:
  section per extent needed (only close returns commit) -- needs anchor-held per-extent handles; ask main whether R2
  includes Windows. Measurement for the close: r30 (hg history push, before 47dc24b72 vs after 92f7a756b, RSS + status
  split + system free after the anchor's SIGKILL) queued.
- R2 measured by use, r35 (big Studio, release, hg history push; before 47dc24b72 / after 92f7a756b): RSS after push
  6,198-6,200 MiB both arms, unchanged at 20 s idle; after SIGKILL system free: after-arm 5,569 -> 11,951 MiB, before-arm
  stays 732 (leak; 2 names unlinked by the script). R2 status written to inbox/main-from-w-sizing-close.md (macOS +
  Linux closed; Windows section-per-extent: asked main whether it is inside R2).
- R3 (peak RSS 145x, with w-hg + w-store): split sent (inbox/w-hg-from-w-sizing-r3.md). Remaining 6.2 GB after an hg
  history push is held content (w-hg 4,495.5 MiB content store) + register + heap; status split came back empty in r35
  (g226 is fixing the status residency).
- 16:46: main asked to land agents/w-sizing-extents: already landed as A-789 362bd8c39 (squashed; branch deleted).
- Disk (Studio-4): deleted my targets incl. two whose names contained "CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=6" (an
  `env $E` quoting slip made cargo use the whole string as CARGO_TARGET_DIR; 15 GB + 4.8 GB) and w-sizing-ab-target.
  Studio-4 now 97 GiB free. Builds go through kiln with needs "macos,disk=60g" from now on.
- g226: status residency fix choice (a) filed with main (inbox/main-from-g226-close.md); g226 frozen by the close order.
- Worktree ~/slates-work/w-sizing-a789 (landed; removable). Scripts ~/.cache/w-sizing-logs (hgmem.sh, hgmem-cmd.txt).

## R2 Windows (main 18:40: Windows is inside R2; build it; Windows runs to run-requests, w-lint runs them, jupiter off)
Plan: a section per pool extent, closed when wholly free; the anchor holds each extent's handle across a daemon restart.
- anchor/src/handles.rs: new hold kind WHAT_CONTENT_EXTENT = 4 (1 handle, record = extent id), admits(), its own bound
  class in HoldBounds (partitions x POOL_EXTENTS_PER_PARTITION; condition (c)).
- server/src/anchor_hold_windows.rs: hold_extent/release_extent (as hold_section/release_section); adopt() collects
  HandedExtent {id, section, record} for the successor (condition (d)).
- server/src/content_pool.rs (cfg windows): own slice's extent sections made ahead of need on a background task at
  shard start and held with the anchor (condition (a): claim only maps from a ready handle); release: drop region,
  close own handle, release the anchor's hold (section closes -> commit returned, g226), make again in background.
- Red first, across processes: a cli/server test (Windows) that an extent's section survives a daemon SIGKILL (anchor
  holds it) and that a released extent's commit goes back. Builds/tests -> run-requests.md (w-lint, jupiter offline).
- LANDED 20bef898c (merge b30a0ab6d): Windows arm. Branches/worktrees removed (only ~/slates-work/w-sizing-main left).
- R2 macOS variance: NOT the pressure hold (injected readings, branch agents/w-sizing-r2hold ea004b541, both red:
  591 / 340 MiB kept; content object 0-2 MiB). Residual = process heap/buffers -> R3. GAPS corrected c131361d3.
  R2 CLOSED (close note). Branch agents/w-sizing-r2hold holds the two-test refactor (red; not landed).
- w-lint pieces: recovery.rs sleeps -- already gone (A-777 removed that test); slates-server unsafe budget -- LANDED
  d15a7fc45 (merge 39aa0995d): macOS open+retry one site, budget 10 (6 + macOS 1 + Windows 3). Replied
  inbox/w-lint-from-w-sizing.md.
- R3 NEXT: hg history push on the big Studio with `slates status --json` memory kinds (segment/content/arenas/regions/
  heap) after the push and after 20 s idle, rev 39aa0995d: script ~/.cache/w-sizing-logs/hgmem.sh (+ hgmem-cmd.txt).
  Blocked at 20:40: the big Studio had 5 GB free (needs ~6.2 GB at peak); it is the only runner with uvx and the
  dataset (~/.cache/w-hg-mem/hg). w-hg/w-store have not answered inbox/w-hg-from-w-sizing-r3.md yet.
- R3 measurement QUEUED as kiln r46 on d1323898 (main's order): `on` + mem refuses at once when memory is short, so the
  run asks no memory and waits on the machine itself until 8 GiB is free (<= 2 h, then NO-ROOM), then pushes hg's
  history at 39aa0995d and prints RSS + status memory kinds after the push and after 20 s idle. Kiln reports its end.
- r46 (big Studio, release 39aa0995d, hg history 84 MB): RSS 90 -> 6,191 MiB after the push (179 s), 6,191 after 20 s
  idle; after the anchor's SIGKILL system free came back (66 -> 5,173 MiB), no names left. Memory-kind split printed
  empty: the rows' fields are resident_bytes/reserved_bytes/modified_bytes/measured (parser fixed). Rerun r48 queued.
- r46's build showed `content_pool::parts_of` dead on macOS (since 20bef898c): gated to Windows, LANDED 6008a67d5
  (merge f68722759); xwin check of slates-server clean of it.
- R3 measured (r48): RSS 6,193 after the push = arenas 5,166 + segment 229 + content 2 + rest ~796. Idle heap check
  (1/4/23 shards): RSS 35/40/87 MiB vs status heap 920/925/961 -> heap row counts allocated bytes. GAPS c7e7f3b4c; split
  to w-hg/w-store; close note updated. R3 waits on w-hg (content holdings) and w-store (disk store).
- Branches: agents/w-sizing-r2win already deleted at landing; agents/w-sizing-r2hold (red experiment) deleted; none left.

## Queue
2a. (main's input, g226) address space: ~220 GB mapped on the 96 GB Studio. By reading: each shard's content slice =
   write log R/4 + images 2R + delta log R + arena R (R = reserve_per_shard = memory/shards/3) ~ 1.42 x memory, plus
   rings/snapshot slots. Size it from the work (the arena already grows from the pool's extents, A-98). Use g226's
   in-process region report in `slates status`, never vmmap on an anchored daemon.
2. Sweep of every sizing site -> docs/wip/GAPS.md entry; convert machine-fraction ones, largest first.
3. Device pressure seam (crate::pressure / slates_machine::memory_pressure exist: A-570) consulted by sizing sites.
4. Zero-alloc on data paths. Seen by reading: the idle purge allocates per pass (prefault_reserve -> Vec,
   arena.order.clone(), pieces_against -> Vec per run).

## For main to delete
- (none)

## 2026-11-21: STANDING BY
- New main 01a1ff31 recorded C1 closed (21a306a04) and R2 closed on all three platforms; R3 closes on w-hg/w-store numbers. I hold nothing else.
- 2026-11-21 10:25: main is now 01a1ff2e. w-store answered R3: content leaves RAM when w-hg lands through the store. R3 waits on w-hg; close note updated.
- 2026-11-21 ~11:00: w-lint found the A-789 Windows CLI tests compiled out (cli.rs is cfg(unix)). Moved to crates/cli/tests/content_extents_windows.rs, LANDED acff9a6e8 (merge d3f2810e0); xwin check clean; run-requests entry updated; --list on jupiter = kiln r1 (queued). Slip: r1 detached a fresh scratch clone on jupiter with git checkout (rule: worktree add); no other tree touched.
- 2026-11-21 11:09: main put me in goal 93 RESERVE for memory (svn import heap attribution, git client RSS red when main sends them). Start no runs meanwhile; r1 (--list on jupiter) finishes on its own.
- 11:21: r1 stopped (main). No runs of mine in flight.

## 2026-11-21: BRIEF AUDIT (the person: "Examine all of your assigned goal. If you have not completed it, you are not done.")
- 1 segment/prefault: DONE (A-777, A-778, A-789 macOS+Windows).
- 2 sweep: list DONE (GAPS MEMORY SIZING); CONVERSIONS NOT DONE. Largest first: (a) the content object's per-slice
  address space from reserve_per_shard (write log R/4, image slots 2R, delta log R, arena R; arena R is unused on
  macOS/Windows since A-789); (b) the segment's log ring (table_bytes) and snapshot slots (2 x table_bytes); (c) the
  tables (bounds only); (d) the client/fleet/NFS shares.
- 3 device-pressure seam: NOT DONE.
- 4 zero-alloc: idle purge DONE (2dc414a84); NFS reply path (results Vec + framed record Vec per request) NOT DONE.
- Resuming 2 -> 3 -> 4, each red first, landed within ~30 min a piece.
- A-839 LANDED b5249f883 (merge 6a6483574): macOS content slice without the arena range; one-shard content object 139,264 -> 106,496 MiB (big Studio, red then green). Slip: my A-number claim loop ran `git reset --hard` on my clean state clone (banned; nothing lost); removed from my scripts.
- NEXT in item 2: image slots (2R), delta log (R), write log (R/4) need growable objects (recovery format) -- design first; then item 3 (pressure seam), item 4 (NFSv3 in-place framing).

## 2026-11-21: brief audit
- Done: item 1 (segment and its prefault); item 2's GAPS list; A-839 (b5249f883), macOS content slices no longer reserve
  their arena (one-shard daemon, big Studio: reserved 139,264 -> 106,496 MiB).
- Open, with plans sent to main (inbox/main-from-w-sizing-brief-remaining.md): 2b, growable image slots, delta log and
  write log (changes the recovery format); 3, the device-pressure seam; 4, NFSv3 in-place reply framing.
- Holding for main's go/not now on each: the 11:09 reserve ruling says start no runs, and every red needs a kiln build.
  The NFS worktree is removed and its branch deleted (nothing was edited in it).
- 14:55 main: 2b and 3 deferred until after goal 93. Both plans are in GAPS under the MEMORY SIZING sweep (37592d80b,
  corrected 9ab8a0602). I stay in goal 93's reserve. Item 4 (NFSv3 in-place replies) is still unruled.
- Found: ~/slates-work/w-sizing-main's working tree has been empty since 10:24 (7,770 staged deletions, .git intact,
  HEAD 356b28d96). Not mine to repair under the no-reset rule; I work from fresh worktrees.
- 14:58 main: item 4 deferred too; its GAPS line landed as c829bcdde. All three plans wait for goal 93 to end.
- w-sizing-main cause: not a truncation. Its .git was made by a fresh `git clone` at 10:24:59-10:25:15 (the reflog's
  only entry; on Studio-4's hostname), with no checkout after it: no index, and an empty tree. This fits a clone whose
  checkout failed when this Mac's disk was full. It was the clone, not a worktree, so `git worktree remove` didn't
  apply. I removed its two landed worktrees (a839, gaps4), checked that agents/w-sizing-arena's change equals main's
  b5249f883, and deleted the folder by its exact path. The next piece starts from a fresh clone.
- 17:20 main 01a1ff2e: content pool growth by need (A-848, goal 93). Written in ~/slates-work/w-sizing-claim: slices cut
  into a run doubling from one unit, claim(want) best fit, LAYOUT_VERSION 16, tests for main's two cases. Red tree
  ~/slates-work/w-sizing-claim-red (origin/main + tests, ca8a04b96). Focused runs r8 (fix) and r9 (red) queued: the big
  Studio and main's MacBook did not answer on r3-r7, then every macOS 16-core runner's link went down. Share cap open.
