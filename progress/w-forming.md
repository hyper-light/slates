# RESUME HERE (2026-11-19 ~22:00 local, relaunched on the Mac Studio; supersedes the checkpoint below)
- Clone ~/slates-work/w-forming-repro (main); worktree ~/slates-work/w-forming-fix = agents/w-forming-repro
  (cb97f4500 WIP aid) + local merge of origin/main a7051af96. Pushed over SSH (this Mac is adas-mac-studio-4; push url git@github.com; https has no credentials
  ).
- BLOCKED: disk here 7.4 GB free (others' use; guard 20 GB; nothing of mine to delete) -> no local build.
  kiln run r1 (build live_chaos + 12 runs of the fresh-member flow; needs macos/arm64 disk=60g; target
  ~/.cache/w-forming-target, logs ~/.cache/w-forming-runs/fm1 on whichever Mac takes it) is QUEUED, unplaced:
  4 runners link down, no other macOS runner with cargo. Left queued; it starts by itself.
- ~23:15: r1 (cpus=8,mem=24g) never placed in ~60 min; stopped. r2 (cpus=3,mem=8g,disk=35g) placed then failed to
  start ("no room here now"). r3 (same ask) QUEUED ~40 min, first in line, no machine with room. Disk here 6.5 GB.
- ~00:10: disk freed (100 GB); built locally (a6764af1e = branch + origin/main), loop fm1 running (12 runs).
  Runs 1-6 PASS. Run 7 FAIL, NOT a forming shape: fleet formed+settled, fresh member rejoined 2.3 s, then the owner's
  holder move holders [1150..,7405..] -> moving_to [1150..,10913..(fresh)] never completed; writes refused ~40 s
  ("covers candidates false", head 1410 durable 1409); panic live_chaos.rs:4879 "the owner's holders settled within
  the fleet wait". Log ~/.cache/w-forming-runs/fm1/run-7.log (this Mac). Out of my goal's scope -> reported to main.
- fm1 done: 11/12 PASS; formation reached every candidate in 12/12 (load ~3/14 cores, not one-CPU). Next fm2: 12 runs under taskpolicy -b.
- fm2 (taskpolicy -b) 12/12 PASS. Formation reached every candidate 24/24 on current main (a6764af1e); no forming shape reproduced, nothing to fix. 48-run sweep is main's. Only failure: fm1 run 7 holder-move stall (reported). Built binary kept: ~/.cache/w-forming-target/debug/deps/live_chaos-7bc0fbed7b2dcb5e; loop ~/.cache/w-forming-scripts/loop.sh.
- 2026-11-20 ~08:20Z main's two new items: (1) 48-run sweep under taskpolicy -b on current main (branch merged
  90d380aea), loop tag sw48, bg task output brsl8komz; (2) chase run 7 (holder move onto the fresh member stalls,
  "covers candidates false"). Added diagnosis(...) to settled_holders' assert (live_chaos.rs ~4881; repro aid, test-only,
  rebuilt 00:20 PST -- sweep runs after that use it, behaviour unchanged). Repro loop rp1 (40 runs, unthrottled) beside it.
  Lead: covers_candidates needs member_anchors (fleet.rs deliver loop, ~10760, anchor via learned_members) + an accepted
  pair to the fresh member at its anchor; diagnosis shows fleet.seal.pair_* counters.
- State branch now kept in ~/fanout-state-w-forming (main 4d39b11 rule); A-numbers claimed only by a pushed amendments.tsv line.
- Next: when r1 ends, read its TALLY; a failure -> match to g225 (a) stuck dial / (b) term-1 joint entry, failing unit
  test, fix, land (needs a machine that can push). No failure -> report count to slates-7a.

# RESUME HERE (checkpoint 2026-11-19 ~23:50Z, ended on main's order "move every agent into kiln")
- Role: implementer for goal 01a1af515 "A forming fleet reaches every candidate at start (live-fixes)"; supervisor
  slates-7a. Target (main): 48 of 48 forming runs reaching every candidate at one CPU, B10's harness = live_chaos's
  fresh-member flow `a_member_that_never_held_the_log_never_stands_in_for_its_holder` (crates/cli/tests/live_chaos.rs).
- Branch agents/w-forming-repro, pushed at cb97f4500 (origin/main 3bcaccf91 + one WIP commit: live_chaos `diagnosis`
  also prints fleet.learner* / fleet_learner lines, a repro aid; land it only inside a fix that needs it, with a real
  message). Worktree ~/slates-work/w-forming-repro (clean).
- Done: the brief's "zero-contact stall" is already on main as A-331 ddcf00768 (told main). Nothing landed by me.
- In flight: NOTHING. The first local build was stopped part-way (no run started). Target dir
  ~/.cache/w-forming-local-target (~partial, incremental; reuse it). No kiln run, no Studio clone.
- Next steps:
  1. `cd ~/slates-work/w-forming-repro && git fetch -q origin && git merge origin/main` (if behind), `df -h ~` (>=20 GB).
  2. Build: `CARGO_TARGET_DIR=~/.cache/w-forming-local-target CARGO_BUILD_JOBS=6 cargo test --locked -p slates-cli
     --test live_chaos --no-run` (cold build under load ~53 takes 30+ min). Note the test binary path it prints.
  3. Repro: `~/.cache/w-forming-scripts/loop.sh <live_chaos binary> a_member_that_never_held_the_log_never_stands_in_for_its_holder 12 fm1`
     (sets SLATES_TEST_CLI=1, TMPDIR=~/.cache/w-forming-tmp; logs ~/.cache/w-forming-runs/fm1/run-N.log; prints
     PASS/FAIL per run). The Mac's own load (~53 on 18 cores) stands in for one CPU; add `taskpolicy -b` if none fail.
  4. A failure: read its panic + diagnosis (which wait: wait_formed / wait_rejoined / wait_neighbourhood_settled),
     match it to g225's open shapes ((a) stuck record dial, (b) term-1 joint entry the leader lacks; (c) is Windows,
     main's), write the failing unit test first, fix, FAST LANDING LOOP (briefs/common.md), 3-line report to slates-7a.
  5. No failure in 12: report the count to slates-7a; the 48-run sweep is main's.

# w-forming — progress (started 2026-11-19 ~23:10Z; implementer, supervisor "slates-7a")

Goal: kiln goal 01a1af515aa6705aac9a155dd6fd0e36 "A forming fleet reaches every candidate at start (live-fixes)" (UI 81).
Predecessor notes: progress/g225-forming-fleet-reaches.md ("Goal 225 -- where it stands"), GAPS on main (docs/wip/GAPS.md
~1915-1965). Goal text: not reachable from here (no kiln MCP; local kiln.db has no goal_items) -- working from the brief.

## Step log
- 2026-11-19 ~23:15Z: brief's open item "zero-contact stall (fresh member joins against a stale configuration, holds no
  session)" is A-331, already on main ddcf00768 (DirectContact::Unadmitted fleet.rs ~7309; test
  consensus::a_fresh_member_joined_before_its_admission_keeps_contact_with_every_live_member). Told main; skipped.
- Remaining open shapes (GAPS "Initial membership did not commit"): (a) stuck record dial 10.8-15.8 s; (b) term-1 joint
  entry the leader lacks; (c) Windows two-region root (main's platform). None reproduced on current main yet.
- Next: reproduce by use on current main, locally (this Mac, own target ~/.cache/w-forming-local-target, JOBS=6):
  live_chaos fresh-member flow + the formation-wait fleet tests, a handful of runs each. A failure -> failing test first.
- ~23:25Z main (slates-7a): goal target = 48 of 48 forming runs reaching every candidate at one CPU (B10's harness,
  live_chaos fresh-member flow); take open items from g225's progress + what the failures show.
- Worktree ~/slates-work/w-forming-repro (branch agents/w-forming-repro off origin/main 3bcaccf91). Local-only change:
  live_chaos `diagnosis` also prints fleet.learner*/fleet_learner lines (repro aid; land only if a fix needs it).
- Build: `cargo test --locked -p slates-cli --test live_chaos --no-run`, target ~/.cache/w-forming-local-target, JOBS=6,
  log ~/.cache/w-forming-build.log. Loop script ~/.cache/w-forming-scripts/loop.sh <bin> <test> <n> <tag>; logs in
  ~/.cache/w-forming-runs/<tag>/run-N.log. Mac load avg ~53 on 18 cores (other agents) = the contention.
