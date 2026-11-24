# Handover: main (Claude session 01a20182) to agy, 2026-11-23 evening PST

Ada's instruction (22:45): "Once you get these fixes in, agy is taking over." This file is the state at handover. The full
running log, every ruling with its time, is `~/.cache/main-01a20182/runs.md` on adas-macbook-pro-2 (tail it from line ~1500
for tonight).

## Rules that bind the coordinator

- The project's `CLAUDE.md` (law: RAM only, no Arc/Rc/Mutex/RwLock, no magic numbers, no panics in non-test code ever, typed
  refusals, no second paths or mode switches, everything bounded, red-first tests, docs and GAPS in the same change).
- `briefs/common.md` and `briefs/close-list.md` in this repo. Ada's standing order: close existing items, nothing new.
- Goal 93 (FOCUS): git, Mercurial and Subversion as industrial-grade servers. Order: complete, then correct, then fast.
- Main owns heavy rounds, measurement, benchmarks and pushing every GitHub diag branch. Agents implement, run focused tests
  and land (fetch, merge, read the diff stat, `git push origin HEAD:main` as its own step).
- Never destructive git (no checkout, stash, reset, rebase, force-push, commit-tree). Never touch ~/Projects/slates (Ada's
  uncommitted work). No sudo, no tool installs (RUSTUP_AUTO_INSTALL=0). Never print or use ANTHROPIC_API_KEY.
- Amendment numbers: register the row in `amendments.tsv` here and push it BEFORE writing the number anywhere. Two agents
  took A-885 tonight; A-885 is w-chaos's, A-886 w-git's, A-887 w-perf's, A-888 w-svn's, A-889 w-ci's, A-890 w-chaos's.

## Fleet agents (kiln agents on this Mac; message them by uds socket ~/.kiln-run/cc-socks/<pid>.sock)

Session pids change when kiln restarts a session. Find the current one with kiln `agents.get` → `state.starts[-1].runs.pid`.
Pids at 04:59 on 11-24: w-git 93676, w-hg 38517, w-svn 92852 (restarted 04:54), w-perf 92158, w-shared 57324, w-interop 8242.

Updates at 05:37 on 11-24:
- 84a Windows VERDICT (w-84a-win's A-897, accepted): no scheduling change on slates' side holds the every-run bar on
  jupiter. 6 interleaved rounds, 254 invocations: main fails 1/114 loaded and 14/120 quiet runs (p999 > 1 ms). Every arm
  lost or tied: a 2 ms shard window, client spins, seat holds, A-326's holds off, the client thread raised up to
  TIME_CRITICAL (it still stalls 3-12 ms there), and the client pinned to one CPU. About 80% of slow samples are the
  client thread stalled in its own send path, and about 15% are the seat's pickup. Main's quiet stall rate (3-7e-4 a
  sample) sits inside the OS's own measured wake tail (pingpong.ps1: 1-12 ms late at 2e-4 to 1e-3 a wake). WHP is on,
  and the hypervisor/DPC cause is inferred. Landing: the bench's attribution, no product latency change, the
  measured-and-rejected table, then (separately) observations that hold no shard (CPU only). The bar is Ada's to restate,
  or she gives a hypervisor-free Windows box or turns WHP off on jupiter.
- svn use round (r865, 77397a5f4) STOPPED in a crash loop: an import poll ran 1,189 ms (import.rs:235), and after the
  kill every restart's shard 0 spun in start_shard (daemon.rs:1190): 1,189 restarts in 44 minutes, which the anchor
  never counted as failed starts. Likely a regression in d27106abc..77397a5f4 (r817 and the Linux matrix imported
  git-2000 cleanly). w-svn owns the import poll (A-895 is in the window). w-perf owns the start's recovery slicing
  (A-887 is in the window; the same site as r854's hg restart red) and the anchor counting a half-started daemon as
  served. Evidence: Studio ~/.cache/main-rounds/svn-use-3/excerpt.txt.
- Gate 8 runs at ee9571813 (45405128575; w-git's homing instrumentation, 20 release + 20 debug runs) to name the
  branch that answers short. The Linux interop matrices run again at 8466f48b3 (45404800642). The git leg (r870) runs on
  the big Studio since 05:28.
- Landed: w-hg's A-891 Mercurial half (946484875; made_commit::commit removed), w-git's ee9571813.

Updates at 05:24 on 11-24:
- Push-0/1's cause, narrowed: gate 7 (45398593779, 3d65116a6) passed 19 of 20. In the failure, push 1's pack homing came
  back Err(Unanswered) 0.9 ms after the body was sent (13:08:46.350264 sent, 13:08:46.351179 refused on band 3 "not kept
  by enough homes in the region yet: retry"). So a branch in homes.rs gives up at once; it is not a slow home. w-git is
  adding a log line and counter on each of the four candidate branches, with w-shared's written agreement and no
  behaviour change. Gate 8 runs at that commit to name the branch, then the fix. Logs in
  ~/.cache/main-01a20182/ci/gate7/art/.
- Main CI at 21b8b5311 (45398791868) failed the lint wall everywhere. Main fixed four reds in place in main-ci-disk:
  slates-git's lib tests did not compile since A-887 part 1 added Reads::named at 00:27, an hg test-only type, a
  Windows-only virtio-fs test path (serve::AwayProbe is unix-gated; the trait lives in device), and a svn_interop
  complexity split. The workspace wall is clean on macOS and the virtio-fs crate by xwin. Lesson: main's wall checks
  had linted only slates-server's tests; lint the workspace with --all-targets and cross-lint Windows by xwin.
- Main CI TSan lane: the watchdog killed the server lib test process. room_needed served a whole fetch in one shard
  step, which w-git fixed in f7c70b076. t_a614a's TSan failure is still unread; the 21b8b5311 run's TSan lane will show it.
- Windows fleet step: both Windows lanes hit their 31-minute limit with every test passing (218 of 244 ran in
  1,510 s on x86_64). The limit was derived from 110 tests on 11-09. Diag 45399658803 measures the whole suite on both
  runners at 21b8b5311. Then main re-derives the limit with the same formula (1.5 x whole + 240 + 300 s) in ci.yml
  and docs/wip/ci-matrix.md.
- Landed: w-git f7c70b076 (TSan); A-891's git half was already built (t_a888_git tests, GAPS ba61d4890). w-hg is landing
  the Mercurial half and removes made_commit::commit, which has no callers left; w-git agreed in writing.
- Studio: r870 (git leg at 21b8b5311, SAMPLE=1) waits behind r865 (svn git-2000 round, running since 04:39).
- Disk: the 11-24 dips on this Mac were kiln's ~/.cache/kiln-reaudit-* (124 GiB), not slates'. Free went 12 -> 62 GiB.

Updates at 04:59 on 11-24:
- Gate 6 (45395461722, 77397a5f4) passed 7 of 7 with no Unlanded line, so push 0's cause is still unnamed (gate 5 met it
  once in 7). Gate 7 runs 10 release and 10 debug at 3d65116a6 (run 45398593779, branch diag-measurer-main-v3-gate7),
  each log grepped for the typed Unlanded line; its cause goes to w-git.
- virtio-fs on Linux: A-899 holds (run 45395756328 at 7416cae15, 2 of 2; the guest's read of a let-go Subversion text is
  brought back and answered).
- Landed: w-svn A-900 3d65116a6 (a lineage member is lent in place; no engine moved or dropped per lend); w-hg P4b
  866bfc1dd (with the second deferred submit's NotHead fix and replay_sliced's split).
- 84a Windows tail, CORRECTED: w-84a-win's larger sample shows no measurable gain from a 2 ms idle window (arm W), and the
  client spin arms (C, WC) hurt loaded runs. The slow samples are mostly the client thread descheduled while raised
  spinning shards hold cores (A-326's holds keep 9.5-10 cores spinning; Windows parks 16 of 32 logical processors). The
  03:00 note below (a 2 ms window halved the slow samples) was a small sample and is superseded. Next arms: NH (holds
  off) and NHC (NH plus a client spin). If NH cuts the quiet failures without hurting loaded, the product change is a CPU
  budget on raised idle spinning, bounded by the processors actually free, not a longer window. Design note first.
- Rulings: w-svn's speed order is HTTPv2 writes (24x) first, then svnsync's per-piece round trip, then A-891's apply
  bring-back and svn:// update as the Studio ranks them. A-891's owed line (made commits committing a version alone) is
  split: w-git takes git's half, w-hg takes Mercurial's half, and any change to made_commit::commit itself is agreed
  between them in writing. w-git retracted its "lend 6 hang" finding (false).
- Studio: r865 (svn use round, git-2000, at 77397a5f4) runs since 04:39. r866 (git leg at 9f3c3ad80) waits behind it.
  w-interop is adding a SAMPLE=1 pass to git_scale.sh (one push and one clone sampled 10 s in, status before and after
  each, every clone step's counters, first byte against total). Relaunch the git leg at that commit if r866 hasn't
  started. The push sample checks A-900 against r244's ~10k memmove samples in 30 s.

Updates at 04:39 on 11-24:
- A-887 VERIFIED on the 57k push (r851): live at the push's end 115 MB, was ~170 MB; the greens' 56 MB is gone. RED: w-perf's
  hg_push_restart over the 57k history (r854) was killed by the watchdog (a step ran 1,138 ms against the 1,000 ms
  window, push or recovery); w-perf names and slices it.
- Interop round 3 (a51883940) recorded in BENCHMARKS: svn at parity with stock; git 7.4-12x slower on whole-repository
  transfers (its incremental fetch and shallow slow from run 2 on, each run a fresh daemon: w-git's, with a counters dump
  w-interop adds to the harness); hg push 5.6x, stream 8.8x.
- Gate 6 RUNNING (45395461722 at 77397a5f4): w-git fixed gate 5's cause (the side-band dropped from the request; Unlanded
  now typed on band 3). The svn use round's git-2000 leg reruns (r865) after the Studio's kiln runner restart lost r817.
- w-perf's bench verdict now judges p99 and p999 (7ff7c5276). w-perf next: the hg takeover re-adoption gap, git's half of
  A-887, then AC-2.1. w-hg: P4b with a second-deferred-submit fix, then the commit-alone reds, then P5. w-shared:
  (d1a-3) with the healer filter, then A-894. w-svn: A-774's virtio-fs line, then lend items.

Updates at 04:01 on 11-24:
- Push-0 ROOT CAUSE NAMED by gate 5 (run 45386968890, 6 of 7): the push's body arrived whole (385 bytes), but it met a
  repository created a moment before and not yet landed, and was refused Unlanded before side-band was in play, so the
  client got nothing. w-git: report every refusal in git's protocol (unpack/ng). The creation race: w-shared's A-894 (the
  creation waits for its record at f+1) may fix it at the source; the two agree one wait before building. Gate 6 runs on
  that fix the same way (worktree main-v3diag, branch diag-measurer-main-v3-*).
- Main's own daemon tests t_goal93_* (daemon.rs:8630, :8959, :9115) went red under w-git's Option A (bare git submits
  refused); w-git moves them to recorded_git_submit.
- Lint wall in slates-server: one red left, vcs_store.rs:5322 replay_sliced (w-hg, in P4b's landing). Workspace-wide
  clippy still to run once the server is clean.
- w-svn's A-898 (virtio-fs guest park) landed; it writes the Linux guest case in crates/server/tests/virtiofs.rs for main.

Updates at 03:32 on 11-24:
- Main CI: the Ubuntu disk fix works (run 45373311018: 88G free before, 113G after, of 145G), but clippy had broken again
  from the day's landings (agents don't run it). Main landed f0624c32b (vcs, hg, server fixes); four errors are with their
  authors: refining.rs:1082 Prepared_'s variant size and refining/tests.rs:136 (w-git), hg_push.rs:1740 built_works
  (w-hg), svn_import.rs:711 stage_owed (w-svn). Keep the wall after every burst of landings: clippy --workspace
  --all-targets -D warnings, with CARGO_TARGET_DIR ~/.cache/main-01a20182-lint-target (kept warm for that).
- HeldShard carries #[expect(dead_code)] until A-887 constructs it; w-perf removes it in that landing.

Updates at 03:02 on 11-24:
- NEW SUBAGENT w-84a-win (in main's session; worktree ~/slates-work/w-84a-win): 84a's Windows every-run tail. A shard
  idle window derived from the measured park-cost tail (one formula on every platform, under IDLE_SPIN_CEILING), a
  client spin for its reply, and the bench printing the OS wake floor and its CPU. Design note to main first, red first,
  lands itself, A/B on jupiter. Removed from w-perf's queue. If main's session ends, its worktree keeps the work.
- 84a's stop hook judges every single run under 1 ms. Per round, all three platforms are met at current main; single
  runs still cross on Windows (OS wake tail) and on Linux under heavy external load. The hook's text is Ada's to replace.

Updates at 03:00 on 11-24:
- (SUPERSEDED at 04:59, see above) 84a Windows tail, second measurement (r820, jupiter): a 2 ms idle window before parking (DIAG override, scratch
  a06a2a062) against the default 50 us cut slow samples 21 -> 11 and rounds with a single run over 1 ms 5/7 -> 1/7; the
  client's late takes didn't fall (56 -> 78). Evidence for w-perf's derived window (after A-887): a shard window from the
  measured park cost, plus a client spin window; measure CPU inside the bench.
- Landed this hour: w-hg P3 5de28ccb2, P4 63e25a1ca, the status-only sweep 16186398b (19 tests judge bodies now); w-svn's
  dump-order normalization c94252e87, the held-verdict red 4c73eb315; w-git's RefineJob reshape 770b7d200; w-perf's
  bring-back split 8cd23c967. Rulings: w-hg's stale re-base and record writer ask for texts not at hand through the
  intake's channel (no whole manifest per push); w-perf's recovery deferred by dependency (greens needing a store's index
  rebuilt after the stores, works waiting as over a green awaiting adoption).
- Session addresses after restarts: w-perf uds 92158, w-hg 38517, w-shared 57324.

Updates at 02:20 on 11-24:
- Gate 4 NAMED the push-0 failure (run 45363070862, 6 of 7): the server answered the push 200 OK, chunked, and ended the
  body cleanly with zero bytes 3.5 ms later; no report-status. It was never a disconnect. w-git now hunts the path that
  completes a receive-pack response with no report (a just-created repository's push is the suspect), red first, and the
  edge must refuse to complete an empty receive-pack result. Gate 5 runs on its fix the same way.
- w-hg is uds 38517 since a restart. w-perf's mechanical bring-back split (ShardLender seam) lands alone before A-887.

Updates at 02:18 on 11-24:
- 84a Linux, a second current-main round (A arm of the client-RR A/B at a51883940, adacachy2, CPUWeight 10000, the box
  at load 58 rising to 214): every reported p99 <= 415 us and p999 <= 961 us. The client-RR arm (D'') was dead even
  (3 of 48 single runs over 1 ms in each arm), so it stays not taken. Scratch worktrees main-84a-linuxA,
  main-84a-linuxdpp and main-84a-windiag are throwaway; delete them by exact path when done.
- Rulings since 01:50: w-shared's before-entry test waits for the record's register before injecting; next, a
  repository creation answered before its record reaches f+1 (a NoLoss red, through A-886's barrier). w-perf's A-887:
  the replay reads only runs a splice consumes; one resolver resolve_at for reads and recovery; w-perf lands the
  state-passing split of hg_push's bring-back first, told to w-hg. w-git's RefineJob reshape lands first as its own
  commit, then the recorded-submit move; the NoTrees deferred-path gap is in scope.
- Runs queued on the big Studio: r811 interop round 3 (running), then r817 the svn use round over HTTP (git-2000, after
  w-svn's A-893 info fix), then owed: git/git's whole-repository round at >= 0417fd1f6 for w-git's encoding numbers.

Updates at 02:01 on 11-24:
- 84a CONFIRMED AT CURRENT MAIN 55650fef1 on all three bare-metal platforms, by the bench's reported p99/p999 (each
  invocation's three runs' median; p999 read from the numbers, since the verdict line judges p99 only): macOS p99 <= 158
  us, p999 <= 172 us; Windows p99 <= 194 us, p999 <= 345 us; Linux p99 <= 473 us, p999 <= 973 us (adacachy2, a user
  scope at CPUWeight 10000 over kiln's weight-1 agent runs, r810). Linux's margin is thin, and single runs on Windows
  (the OS wake tail) and Linux (the fair-class client) crossed 1 ms: w-perf's hardening after A-887.

Updates at 01:57 on 11-24:
- 84a Linux at 55650fef1, run under adacachy2's own load (about 3 runnable threads a core, not the defined 2; r809):
  loaded p99 under 1 ms in all 16 values (at most 605 us); loaded p999 under 1 ms in 14 of 16, over in two accept rounds
  (1,165 and 1,076 us). The defined-condition round r793 still waits for a quiet box (until ~04:33).
- edit_merge_bench's verdict line (line 837) judges p99 only, never p999, though 84a's bar is p99/p999: read p999 from
  the numbers, not the verdict. Sent to w-perf to fix, red first.
- Interop speed round 3 RUNNING on the big Studio (r811, studio-interop-speed-5.sh, record
  ~/.cache/main-rounds/interop-speed-r3) at current main with w-svn's A-892 import fix (a51883940). Send each leg's rows
  to its owner and to w-interop for BENCHMARKS.

Updates at 01:53 on 11-24:
- CORRECTION, 84a: the goal's instrument is edit_merge_bench's own verdict (each invocation judged by its three runs'
  median). At current main 55650fef1 it reads "within the required 1 ms" in 20 of 20 verdicts on macOS (loaded p99
  126-158 us, p999 134-172 us) and 20 of 20 on Windows (loaded p99 120-194 us, p999 168-345 us). The coordinator's
  "every run" bar was an over-reading; Windows' single-run excursions are jupiter's OS wake tail. w-perf's Windows
  latency-path work is tail hardening after A-887, not a blocker. Linux at current main is not yet measured: both
  Linux bare-metal runners are loaded by other projects (adacachy2 load 33, adacachy 68); r793 waits on adacachy2
  until ~04:33 for load under 8, then runs 8 loaded and 2 quiet rounds plus R9.

Updates at 01:50 on 11-24:
- 84a Windows: the cause is jupiter's OS, measured with no slates involved. A kernel-event ping-pong
  (~/.cache/main-84a-win/pingpong.ps1 on jupiter, Normal through WMI) wakes a blocked thread 1-12 ms late at 2e-4 to
  1e-3 of wakes, more often after longer sleeps (500 us idle: p999 1.46-1.89 ms, p9999 7.3 ms). So no design that blocks
  on the latency path meets every-run p999 < 1 ms there. Current main already lets the OS place Windows shards (A-871,
  Fixing::Waits); the soft-pin arm was moot. Direction sent to w-perf (after A-887): the client spins for its reply within
  a derived window; the shard's hold derives from the measured park cost (one formula, the platform's wake tail an input);
  the bench prints the OS floor as its noise floor.
- F2 LANDED (e3efe8d21). (d1a-3)'s cause confirmed (a takeover's rebuild owed no resolution pass); ruled: the rebuild owes
  one locally plus a "rebuilt here" home message acknowledged per member (wire version bump).
- w-git: guarded made commits landed (22a88984b); commits::make goes (bare git submits refused RecordedSubmitRequired);
  owed records met at replay are refused typed once no production caller is confirmed to have made them.

Updates at 01:13 on 11-24:
- 84a's Windows tail is DIAGNOSED (r801 on jupiter, main 891de2741 plus three DIAG commits on the scratch worktree
  ~/slates-work/main-84a-windiag, not for main). Every slow sample is a parked thread's wake arriving late: the shard's
  pickup 1-10 ms late, or the client's take of a published reply 3-9 ms late, loaded and quiet alike. jupiter keeps 16 of
  32 logical processors parked. Sent to w-perf for after A-887: a ping-pong noise floor first, then one path that
  doesn't rest on a parked thread's wake within 1 ms, needing no privilege. Confirm with round8.sh / round7.sh on jupiter.
- Gate 4 RUNNING: GitHub run 45363070862, v3_takeover at d801b78d2 (w-git's connection-end logging plus client git
  traces). Send w-git the disconnecting run's server end line and client trace.
- w-shared's (d1a-3) reds: the dead owner's successor never adopts the git repository's record. Ruled: takeover is mostly
  built (A-636 2c-1, 2c-2); trace which of three it is (record never shipped, adopted but pending, rebuild waiting on a
  missing log part) before building. No replica-answer second path. w-shared is uds 57324 since its restart.
- F2 rerun 45354317783: 3 of 4 jobs green so far, macOS release running. Land F2 only on all four.

Updates at 00:51 on 11-24:
- 84a at current main 55650fef1. macOS (r791, Studio): every run under 1 ms (24 loaded runs: p99 at most 216 us, p999 at
  most 542 us). Windows (r792, jupiter): every run's p99 under 1 ms, but p999 crossed 1 ms in 3 of 24 loaded accept runs
  (2.0-3.7 ms) and 3 of 12 quiet runs (1.0-3.3 ms). A rare ~3.7 ms client-daemon round-trip stall, in edit (L7) or submit
  (Q1), loaded and quiet alike, the shape of 11-22's "client waits 3 ms" tail at a lower rate. NOT met under every-run.
  Routed to w-perf after A-887 (B): port the slow-wake DIAG from branch main01a20182/84a-win-diag, name the stall, fix it
  red first; the coordinator reruns round7.sh on jupiter (launch2.ps1 -Class 32) to confirm. Linux (r793, adacachy2):
  built (rtprio 99), still waiting for load under 8 (33 at 00:50).
- w-svn's A-888 piece 2 landed (891de2741, renumbered A-891). w-git is landing the commits.rs sites and then gate 4's
  logging. w-hg is landing P3, with P4 next.

Updates at 00:28 on 11-24:
- Goal 84a: the session's stop hook holds the pre-11-22 text and asks for every run under 1 ms on Linux, macOS and Windows
  at main's current tree. Read back from the machines, the 11-22/11-23 bare-metal rounds had every run under 1 ms on
  Linux (worst p999 761 us) and macOS (289 us), but on Windows 1 of 12 accept runs had a 6.1 ms p999 (the goal's met text
  quotes the middle-of-three figure). Confirming rounds at main 55650fef1 are running: r791 Studio
  (~/.cache/main-rounds/84a-confirm-2/summary.txt), r792 jupiter (~/.cache/main-84a-win/c2-summary.txt), r793 adacachy2
  (~/.cache/main-84a-c2-summary.txt; waits up to 4 h for load under 8, since other projects held it at 66). If Windows
  shows the outlier again, it is a real tail to name (jupiter's rounds run at Normal through WMI, outside kiln's job).
- w-shared's CLI test fix landed (ca73f5c4c, merge 55650fef1). w-perf is building A-887 (B).

Updates at 00:17 on 11-24:
- F2 rerun 2 RUNNING: GitHub run 45354317783 (branch diag-measurer-main-f2-2, worktree ~/slates-work/main-f2diag) at
  w-shared's fix 427ad2389 (catch-up 1.11 s -> 0.25 s: a family's directory no longer cloned at each mark and page).
  Four jobs, the starvation test 10x each in release and debug on ubuntu-latest and macos-latest. F2 lands only on 10/10
  in all four; send w-shared the per-job counts.
- w-shared found the macOS CI red's cause: A-854's format 7 carries an increment of at most min(1 MiB, the chain's 64th)
  in its entry, and the test's 64 KiB file sat under the macOS runner's share. The fix edits past CARRIED_INCREMENT_MAX.
- w-tsan DONE (all landed: 21323f227, 7bb782845, 27131596a, b1c743f3f). Its leftovers for owners are listed in
  runs.md (dropped error kinds in hg_push and homes; rt spawn_detached's refused detach). It kept its targets on
  adacachy2 (~/.cache/w-tsan-target 19 GB, w-tsan-probe-target 14 GB); delete them if nobody resumes that work.
- A-887 rulings: named hg files resolve from the green's head manifest by path, with no per-file record; the push's
  fork serves the pre-commit window; one resolver for reads and recovery, extending A-760 (b) 4d's park-and-bring-back.
- This Mac's disk is at 51 GiB free and falling during builds. The current writer is ~/.cache/g121-batch15, a shards
  batch, not slates. The largest folders aren't slates': amphora-target 24.9 GiB, kiln-move-stuck-target 23.8 GiB.
  Flagged to Ada.

Updates at 00:08 on 11-24:
- Gate 3 ENDED: 6 of 7 (release 3/3; debug 1, 2 and 4 passed; debug 3 lost push 0 to "unexpected disconnect while reading
  sideband packet" 2.22 s in). None of w-git's new log lines fired, though daemon lines reach the log, so the cause is on a
  path the logging doesn't cover. Sent to w-git: log where a connection ends, below the edge, and/or the client's
  GIT_TRACE_PACKET. Gate 4 runs on its commit, the same way (worktree main-v3diag, branch diag-measurer-main-v3-*).
- Main CI's first test data (45347644597): macOS failed one test, cli.rs:7340
  an_owners_unfolded_version_is_read_from_its_content_store_after_a_kill (merge.inputs.held_by_owner absent after an
  accepted submit), in w-shared's lane (A). `cargo test --workspace` stops at a failing binary, so later crates went unrun.
  w-shared brackets it before F2. Linux ran out of disk (fixed by 2033268c5; 45352789036 at that commit is pending).

Updates at 00:03 on 11-24:
- Main CI's Ubuntu tests: the first lint-clean run (45347644597, at 31e0c2596) ran the runner out of disk in
  `cargo test --workspace` (job 138289296963, "No space left on device"). No Ubuntu test build had run on main since at
  least 11-19, since every run stopped at Format or Clippy. Main landed 2033268c5: the gates job frees the image's unused
  SDKs on Linux and prints the free space before, after, and after the tests. Verify in the next main run, and watch
  45347644597's macOS gates job (still running) for the same failure.
- The lint wall is clean at b0d738c6f (w-wall, done). The fleet lands without clippy, so the wall needs a keeper after
  each burst of landings. CI's full Windows clippy wasn't checked locally (cargo-xwin is installed here, unrun).
- w-tsan landed A-859's gate (27131596a, merge 3ca9de314) and is fixing daemon_edge.rs:5274's assertion.
- w-perf: A-887 ruled option (a), one landing. Every reader of a named text (mount read, recovery, a work's judge)
  brings it back by identity through homes, async, held until the judge's end, bounded, with a typed refusal for a text
  gone everywhere. Held::stores_bytes is required with no default; an in-memory store for hg's unit tests is owed in GAPS.

Updates at 23:49 (stamped from date; the commit 4e8f007 said 23:52 by mistake):
- Gate 3 is RUNNING: GitHub run 45350914168 (branch diag-measurer-main-v3-gate3, commit 509d46e3c), v3_takeover at
  b98233544 (w-git's edge-failure logging ba4e4e35e): 3 release then 4 debug back to back; artifact v3-gate3 holds each
  run's whole log, anchor and daemon lines included. Send w-git the disconnecting run's edge lines.
- r678 is explained: the ls-refs fix moved derive and encode from the post-push listing (A 30.0 s, B 0.03 s) to the first
  clone; push + listing + mirror clone total the same. w-git folds "derive and encode behind the push, off every fetch's
  path" into the clone speed item, after commits.rs's version-alone sites.
- w-tsan owes a test fix: daemon_edge.rs:5274 (its 21323f227) asserts the arena grew, which the machine's slack decides
  (3 of 3 fail on w-hg's Mac at load); assert the counted asks instead, red-checked.
- w-hg's C1(1) P2 is committed green and landing.
- Unmerged branches holding open GAPS items, untouched since 11-21: origin/agents/w-bugs-backstop-6 (A-837 part 3, the
  typed DescriptorsExhausted refusal; 4 commits; w-bugs idle since 11-22 06:05 on another machine, and whether its session
  is off the API key is unverified, so check before waking it or give the piece to another agent) and
  origin/agents/w-shared-gitbuild (A-752's owed red: a large git file built a slice a unit). The w-perf-84a-* and
  w-multirepo-learned-weights branches are measurement arms and never land.

Updates at 23:35 (newest first; they supersede the table where they differ):
- w-git: the push-0 disconnect did not reproduce locally (4 debug runs passed). Its logging commit (why the edge ends a response
  early) is landing. OWED by the coordinator: gate 3 at that commit on ubuntu, v3_takeover 3x release then 4x debug back to back,
  every daemon and anchor log kept as an artifact (worktree ~/slates-work/main-v3diag, workflow diag-main-v3.yml). Also asked
  to explain r678: A-886 made whole-repository clones slower (mirror 72.0/73.1 s -> 98.2/102.8 s).
- w-hg: r670 GREEN (most own runs 5 against the bound 6; intake 71.25 s, was 105.64 s). On C1(1) P2.
- w-perf: r691 sent (below). Next after A-887: read each shard thread's QoS class (bench main thread runs at QoS utility under
  kiln's launchd job), then A/B shards declaring user-initiated and user-interactive against main; the lower class wins unless
  the numbers separate. A-887 part 1: a work's first read of its named runs comes back Unstored; probing.
- w-svn: interop round 2's svn leg measured nothing: every slates import refused BudgetExceeded { available: 373475417 }.
  Round 3 runs on its import fix's hash.
- w-interop: round 2 record complete; writing BENCHMARKS rows for git and hg; svn waits for round 3.
- w-ci: DONE (below). w-chaos's A-890 Windows check: GREEN, 5/5 on windows-latest and 5/5 on windows-11-arm.

| Agent | Landed tonight | Now | Next, in order |
|---|---|---|---|
| w-git | A-886 holder replica build + progress-aware push barrier, merge b157eacd1 (f=1 push loss fixed; v3 gate run 2: after 4/5, before 0/5) | push 0 disconnects on the 2nd consecutive debug v3_takeover run, in both arms; logging why the edge ends a response early | commits.rs sites that commit a version alone (Before::Unmade, record_and_commit Err, make_owed), with w-svn's commit_held guard; then speed: clone 149 s vs stock 11 s, partial 73 vs 6.2, push 88 vs 11.5 |
| w-hg | merge-step slicing 8933770f2, key filter per run fa30061d2 (arena-21 push-all 514 s, was 1,175), tiered bound in bytes 40991f613 + fix c81dce7db, C1(1) P1 95a6be82f | C1(1) P2 (serving reads off the content store), unblocked by w-tsan's 737d990db | P3-P5 of C1(1); C1(2) intake keeps no whole text; 4 hg_made.rs reds (version committed alone; land with w-svn's guard); then speed: stream 3.5 s vs 0.41, push 63 vs 10.5 |
| w-svn | 4c+4d 240917592; A-888 piece 1 a018ee86b (second path removed, RecordedSubmitRequired) | A-888 piece 2: guard-then-apply for made_commit_landing and commit_held (a revision that cannot be made broke a repository for good) | answer: which svn client op makes Unmade::Copy (completeness gap?); then lend borrow, Green::default; then svnsync piece rounds, svn:// update speed |
| w-perf | refusal fix 80b09c3f6 + errno line 4185474ca; stale shm extent fix ddcda7b0b (merge 5108ce3a6); AC-2.1 bench changes 79d4e0814, e16be6d81 | A-887: an hg push's green holds 56 MB of heap copies of files the store already holds; red, then name them (Content::Named) | git half of A-887 once git blobs are content-plane objects; AC-2.1 at 23 shards: the parked own-shard wake (client part 45-70 us in the tail) |
| w-shared | StoreOwner::stop request a14b90764 (merge 095197fde) | F2 (agents/w-shared-f2, 2cdd88fa3): GitHub 10x: ubuntu release 10/10, ubuntu debug 10/10, macOS debug 6/10 (encoder thrashing on page splits); a clamp fix in progress | land F2 only on 10/10 everywhere; then (d1a-3) two red fleet tests |
| w-interop | blame narrowed 51a22cb73; speed harness store fix + loud failure a51b627f5 | waiting on speed round 2's record and the svn sizing diag | BENCHMARKS rows from round 2; svn on Windows blocked on Ada (no svn client on jupiter) |

## Subagents (in-process under main's session; they end when main's session ends)

Their worktrees keep their work. If main's session closes before they report, read the worktree and the GAPS entries.

| Subagent | Worktree | State |
|---|---|---|
| w-chaos | ~/slates-work/w-chaos | DONE 23:20: A-890 landed (c5de37b3b + 46e1d3efb, merge 8462ef25a): a restarted successor settles what it adopted; A-885 leg and A-344 write 5/5 under load; nemesis scenes clean, the Continuity mutation caught 41-42/48. Windows check: diag-measurer-main-a890-1 (run 45347202509). Left open: the Continuity test's through_the_successor reads and other read_at_head callers still try once. |
| w-tsan | ~/slates-work/w-tsan-land, w-tsan-probe | commit 1 landed 21323f227 (lend fix, six sites). Commit 2: swallowed refusals with consequences (hg_push.rs:2878 timer spin, homes.rs:896 release, homes.rs:1569 detach). Then a859 (edge records a pending write as observable state). |
| w-wall | ~/slates-work/w-wall | lint round 5: two new sites, receive.rs:366 (11/10, from A-886) and identities/tests.rs:221 (std::thread::sleep). |
| w-ci | ~/slates-work/w-ci (clean at 31e0c2596) | DONE 23:26, context full. Landed 9: conformance record split 87ef753f3, create_on 076a4adb5, A-889 live image publish cfeab3e9b (FUSE lanes 2/12 fail -> 0/30), dialer b43e1a20d, lender test 37b0b3c37, b7524c5bc (13 tests, hg_import::land_imported_on), 9c5d0de5b (lfs, audit_on_stored_shard_made; merge 31e0c2596). Left (GAPS lists it): the rest of red 4, mechanical, on the same patterns; the red is the LD_PRELOAD shim adacachy2:~/.cache/w-ci-red4/delay_fdatasync.so (1.5 s before fdatasync); check vcs_share, paths, hg_view and vcs_ssh/tests/hg.rs first. By ruling left as they are: hg_push's 16 push tests, git_push's synchronous driver (30 calls), repository.rs creation replies (exposure 1 in 100 at load 47, none in CI). |

## Runs in flight

At 23:35 the Studio chain below has ENDED; every result went to its owner:
- r655 interop round 2: svn import refused BudgetExceeded x5 (w-svn); hg stream stock 0.417 s vs slates 3.829 s best, with
  one 57.0 s outlier (w-interop to explain); slates-daemon peak RSS 2,934 MiB.
- r670 hg intake: GREEN, as above.
- r678 push A/B: A-886 slows clones (w-git to explain): mirror 72.0/73.1 -> 98.2/102.8 s, work 88.4/88.5 -> 93.7/100.6 s,
  partial 69.5/69.3 -> 71.6/103.4 s; the session test passed 4 of 4.
- r691 provision at 5108ce3a6: 0 refusals. 23 shards spinning_1 p99 92.1/102.3/96.6 us (FAIL); spinning_1_own_shard p99
  68.5-71.5 us, its tail parked (10-16 of 21, client part 50-65 us). 2 shards p99 42.0-46.2 us (ok), own shard 30.5-33.5 us.
Main CI at 23:35: 45338067629 at a018ee86b still running its two Windows jobs (80+ min); 45347644597 at 31e0c2596 waits
behind it and starts when it ends (the concurrency group keeps one waiting, replacing it on each push). The first run past
w-wall's last round is the lint-clean one whose Linux and macOS test steps w-ci's red 3 triage needs.

The chain as it was planned:
1. r655 interop speed round 2 at a51b627f5: git leg done (below), hg then svn running. Record:
   `~/.cache/main-rounds/interop-speed/work/interop-speed.txt` on the Studio.
2. r670 hg intake_steps at 14b44fb7f: red if most own runs > floor(log3 windows)+1 = 6, or in flight > 7.
3. r678 git push A/B: A fb503573a vs B b157eacd1 (push answer and refs listable after).
4. r691 provision_bench at 5108ce3a6: refusal count (must be 0), the tail halves, QoS and darwin role.
Scripts are `~/slates-work/main-ship/studio-*.sh`. Pattern: bundle `1d7fd0ec4..origin/main` into main-ship, kiln run with
`on: adas-mac-studio.local-2`, `cwd: ~/slates-work/main-ship`. Never edit a script a running bash is reading.

GitHub diags (only main pushes them): F2 starvation 45336582846 (macOS release unread); svn use sizing 45330574435
(git-2000.dump, svn 1.15 built in the job). The account's GitHub API limit was spent at 22:18 (shared by every session on
this Mac); it resets hourly. Main CI on main: a pending run is cancelled by each newer push; the lint wall is clean except
w-wall's two new sites.

## Numbers that set the work

- Speed round 2, git/git (best of 5, stock git daemon / slates): push 11.5 / 87.8 s; clone 10.9 / 149.4 s; partial 6.2 /
  73.3 s; incremental push 0.128 / 0.078 s. Goal 93 asks for 10x faster than stock.
- Speed round 1, hg (stock hg serve / slates): push 10.5 / 63.2 s; stream clone 0.41 / 3.51 s; clone 7.7 / 9.1 s.
- Provisioning R9 at the Studio's 23 shards: single-client p99 88-100 us against 50 us (never met there); 2 shards 43-45 us.
- system_live at a 57k hg push's end: 170 MB (greens 56 MB, index tables 41 MB, entries 25.5, encoded pages 20.5).

## Needs Ada

- Goal 84a's check-in text still says macOS p999 and Linux are open; main's notes record her 11-23 ruling that 84a is met on
  bare metal. Only she can replace the text, or ask for bare-metal confirming rounds.
- svn interop on Windows needs a stock svn client installed on jupiter, which needs her OK.
