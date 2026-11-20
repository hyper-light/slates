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
- ~08:40Z: rp1 20/20 PASS (run-7 shape not seen again; run 21 killed). sw48 run 16 FAIL at start: daemon b still in
  measure(--quick) (pre-banner profile measurement, role/daemon.rs start()) past START_WAIT 30 s; control-shard beats
  of a gapped 3-5 s = host starvation while rp1 ran beside the throttled sweep. Stopped rp1. sw48 runs 1-21 overlapped
  rp1 (contaminated); plan: sweep finishes, then 21 clean runs (tag sw48b) -> 48 clean. Then resume the run-7 chase.
- ~09:00Z sw48 run 25 HUNG 30+ min: fresh member a's `volume list` client stuck in rendezvous Claim::wait; the live
  bootstrap object had all 64 slots free (read via shm at the hang) -> claim stranded in the predecessor's unlinked
  object while the successor held the owner name. Evidence ~/.cache/w-forming-runs/sw48-25-evidence/ (samples).
  Also in run 25: two daemons watchdog-killed (exit 4: shard 0 spinning 5.7 s CPU without a step) under the throttle --
  reported, not chased. Killed run 25 (counted FAIL), sweep continues on pre-fix main.
- LANDED A-735 727ebe674 on origin/main (verified merge-base): ipc Claim::look made_again() reads its own slot (pid+start)
  in the object under the name; red 2.000 s -> 178 us; ipc --lib rendezvous:: 16/16, --test rendezvous 14/14, fmt.
  Landed from ~/slates-work/w-forming-repro (main ff'd), not the wip branch. Ledger row A-735 pushed first.
- ~09:40Z sw48 run 26 hung the same way (fresh b's `status` client in Claim::wait, pid absent from the live object):
  A-735 again on the pre-fix build. Stopped sw48 at 24/26 PASS (16 = start starvation beside rp1; 25, 26 = A-735).
  Started sw48c: clean 48 on exact origin/main 69d1f2ec3 (has A-735), built from ~/slates-work/w-forming-repro,
  taskpolicy -b, nothing of mine beside it (another agent's g227 suite loop does run on this Mac). Bg task bff43ui5h.
- sw48c run 21 FAIL after forming (formed, fresh b rejoined 37 s): new owner d served the volume after 19.7 s of takeover (6254 writes acked), then its shard 0 spun 5628 ms CPU without a step -> watchdog exit 4 -> a write unanswered 40 s (live_chaos.rs:1779). Same spin as run 25: on a successor owner right after a takeover. Log ~/.cache/w-forming-runs/sw48c/run-21.log (diagnosis printed).
- DONE sw48c (09:41Z end): formed+settled 48/48, fresh member rejoined 48/48, test 47/48 (run 21 = takeover spin).
  GAPS line landed 2add29641 (verified on origin/main). Reported to main for the forming goal.
- ~10:50Z rp2 started: 40 runs throttled on agents/w-forming-repro (c685be2eb = origin/main + settled_holders diagnosis aid), spin-sampler on, A-740 (main) names a spinning task in the watchdog line. bg b420ae1gd, logs ~/.cache/w-forming-runs/rp2.
- main: forming goal met at 143/143 formed-and-settled on main with A-735 (1-in-48 stall shows with 95% odds). Count: sw48c 48 + rp2 (up to 40) + rp3 (55 more, same flow, throttled, sampler on) -> then add the combined count to the GAPS sweep entry.
- rp2 lead (sampler, run 23, near-miss polls 1.0-1.5 s, no spin verdict): member e shard 0 ~770/1000 samples in reap::remove_on_owners -> xshard::call_in_shard_time -> observe::ShardTime::new -> rt registry::shard_cpu -> thread_clock::read -> thread_info (mach_msg trap); likely kernel wait under the throttle, not CPU. Samples ~/.cache/w-forming-runs/rp2/spin-run-23-*. Waiting for an A-740-labelled spin verdict.
- Queue after sw48c: run-7 stall chase; then run-25 spin (main 2026-11-20: name the unsliced task, fix failing-test
  first or hand main its location). Tool: ~/.cache/w-forming-scripts/spin-sampler.sh <runs-dir> samples every chaos
  daemon on a >=300 ms poll line; use with repro loops only, never during evidence runs.
- (old) NEXT: when sw48 ends, rebuild on current main (with A-735) and run a clean 48 (tag sw48c, taskpolicy -b, nothing
  beside it) = the goal evidence; then GAPS line + report. Run-7 chase after that (diag aid on agents/w-forming-repro).
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
