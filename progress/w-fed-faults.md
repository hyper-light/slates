# RESUME HERE (2026-11-20 ~03:30 PST, Mac Studio d1323898; supersedes everything below)
- LANDED: kiln 30c5929 (A-580 harness), e33c470 (A-708). Linux run r9 STOPPED (old code; re-queue after A-734 lands).
- IN FLIGHT, piece A-734 (claimed, ledger row "in progress"), worktree ~/slates-work/w-fed-faults-p3 (branch
  agents/w-fed-faults-p3 off kiln main e33c470, uncommitted), target ~/.cache/w-fed-faults-target:
  1. tests: Fault::{Signal,Link} gate (link flows run on macOS), lab end unmounts Slates mounts before ending instances,
     runners started with their harness (common::Fleet::start_runner_harnessed; --no-terminal started no Slates),
     agents.start workspace "volume", writes at volume root, fleet_seen/read_refusals diagnostics, until_one_fleet proof,
     pre-cut control read through survivors.
  2. kiln: slates_fleet::short_of + join waits for 2f+1 in the directory (found by use: 3 runners wrote 3 one-node
     manifests). Unit a734_a_manifest_waits_... red then green. PROVEN by use: one fleet of 3, records cross the relay.
  3. kiln (main's ruling): one-time bootstrap. migration 20261120-0900_slates_fleet_bootstrap.sql; slates_fleet::
     bootstrapper/ask_bootstrap_on (claim recorded first, bound to machine, done never again) + POST
     /api/agents/slates-fleet/bootstrap + ensure_up asks after report_member, ensure_group, says done. Unit
     a734_kilns_slates_fleet_is_bootstrapped_once_... Lab has no operator step.
  OPEN FINDING (by use, macOS, seed 0x522bdda8): with fleet-a cut off, survivors served 0 of 8 acknowledged files in
  900 s. Diagnosing (logs ~/.cache/w-fed-faults-logs/p4-cut.log): replication-at-seal vs no takeover vs read path.
- Answered w-balance Q3 (inbox 2cf8325).
- Ship route: kiln run on adas-mac-studio-2.local (b24c5c95...), cwd ~/.cache/w-fed-faults-ship (land.sh, state3.sh,
  inbox.sh, claim.sh).

- Earlier: Worktrees ~/slates-work/w-fed-faults-a708, -a708land deletable; target ~/.cache/w-fed-faults-target.
- PUSHING: this Studio has no GitHub credentials (HTTPS none; ssh agent hangs). Land via kiln `run` on
  cec37f60f22c5a60d27f08f4d2e5daa8 (or 2768d0d5...; both have GitHub SSH) with cwd ~/.cache/w-fed-faults-ship (a tiny
  git folder holding land.sh + <patch>.b64): `PATCH="$(cat X.b64)" PATHS=<sparse paths> bash ./land.sh` clones main
  sparse, git am, push HEAD:main, is-ancestor check, retries on rejection.
- State copy: ~/fanout-state-w-fed-faults (private). Target ~/.cache/w-fed-faults-target, JOBS=8.

# RESUME HERE (w-fed-faults checkpointed 2026-11-19 on main's order; nothing in flight; local build stopped)
- Pushed kiln branches (both off kiln origin/main 44cfdf353, nothing uncommitted):
  1. agents/w-fed-faults @ 72cef952d (worktree ~/slates-work/w-fed-faults-kiln): PIECE 1, A-580's harness
     crates/kiln/tests/slates_fleet_faults.rs `#![cfg(target_os="linux")]` -> `#![cfg(unix)]`, enabled() skips off
     Linux saying why. NEVER COMPILED YET. Next: `cargo test --locked -p kiln --test slates_fleet_faults` on macOS
     (flows skip) to get the harness's FIRST compile; fix what it finds (g076b guessed at agents.start's id path, agents.get's
     /workspace/made/volume and /on, VolumeId { bytes }, common::Fleet::with/start_server); land as one commit (reword
     the WIP message) + merge origin/main; push HEAD:main.
  2. agents/w-fed-faults-a708 @ 473a560ed (worktree ~/slates-work/w-fed-faults-a708): PIECE 2, A-708 (claimed in
     amendments.tsv, "in progress"): kiln `[slates_fleet.binds]` -> Place.bind -> manifest `bind` on own entry
     (check_bind; lib test a708_a_runner_behind_a_port_mapping_binds_where_the_setting_says_and_slates_reads_it); test
     Lab::start_relayed with Slates' relay via #[path] to vendor/slates/crates/cli/tests/support/relay.rs, reroot by
     daemon pid, flows w7_a708_a_slow_lossy_link_... and w7_a708_a_runner_cut_off_while_it_runs_...; docs/agentic.md
     W7. NOT BUILT. Next (after piece 1 lands): merge origin/main (the test file's header from piece 1 merges cleanly),
     `cargo test --locked -p kiln --lib a708_ k3_ k2_ w10_ slates_move` + `--test slates_fleet_faults` compile;
     kiln: no tree-wide cargo fmt (AGENTS.md); land; amendments row A-708 -> landed.
- Relay note: relay.rs's own #[cfg(test)] tests will run inside the kiln test binary (allowed, loopback only).
- Flows themselves run on Linux only (main's run): SLATES_FLEET_FAULTS=1 cargo test -p kiln --test slates_fleet_faults
  -- --nocapture --test-threads 1.
- Stale local scratch to delete: ~/.cache/w-fed-faults-local-target (partial deps build), ~/.cache/w-fed-faults-logs.

# w-fed-faults -- progress (created 2026-11-19 by the relaunched worker; supervisor "slates-7a")

Goal: kiln goal "The multi-runner test with injected faults (goal 91)". Role: implementer (headless worker).
Rules: briefs/common.md (FAST LANDING LOOP, NO KILN, OWN TARGET DIR ~/.cache/w-fed-faults-local-target, never end for context).

## Found at start
- A-580 (g076b) LANDED in kiln main d698ebdeb..fd6f5fce8: kiln::workspace::slates_faults (judge_move/refusal_kind,
  lost, not_reformed), slates_move::KINDS, docs/agentic.md W7, tests/slates_fleet_faults.rs (four flows: killed,
  partitioned, restarted, move target cut midway; Linux only, gated SLATES_FLEET_FAULTS=1). NEVER COMPILED.
- Not in A-580: slow link; cross-machine runners; macOS flows; per-link cut (needs manifest `bind`, slates A-277).
- No progress files yet for w-fed-a583 / w-fed-a503 / w-fed-move.

## Steps
- Piece 1 (A-580's harness compiles on every Unix; flows skip off Linux saying why): worktree
  ~/slates-work/w-fed-faults-kiln (kiln, branch agents/w-fed-faults off origin/main 44cfdf353); first build running
  locally (CARGO_TARGET_DIR=~/.cache/w-fed-faults-local-target, log ~/.cache/w-fed-faults-logs/p1-build1.log; load ~55).
- Piece 2 = A-708 (claimed in amendments.tsv): kiln `[slates_fleet.binds]` -> Place.bind -> manifest `bind` on the node's
  own entry (check_bind; a708_ unit test); test: Lab::start_relayed with Slates' relay by #[path] (vendor), reroot by
  daemon pid, flows w7_a708_ slow lossy link + move, runner cut off while running; docs/agentic.md W7 paragraph.
  Drafted (uncommitted) in worktree ~/slates-work/w-fed-faults-a708 (branch agents/w-fed-faults-a708); build after piece 1 lands.
