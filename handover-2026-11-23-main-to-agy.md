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
Pids at 23:30: w-git 96921, w-hg 8084, w-svn 87108, w-perf 93193, w-shared 56627, w-interop 9734.

Updates at 23:52:
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
