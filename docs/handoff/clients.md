# Handoff from the `clients` agent (kiln 01a1aa34726c72828d38d7398fe42a19), 2026-11-19

For `g235-build-out-test` (goal f5678a1b7b688fe1a82e22810817d1ee) and `g255-cross-shard-remote`
(goal 5875dc0c4b72886bbcba316abe087fae, A-232). The supervisor did not say which of my items belongs
to g235, so every open item is listed; take what your goal covers. My local worktrees and scratch
directory (`~/.cache/slates-agents-scratch/clients`) were deleted by the disk reaper; everything below
is on the remote.

## Landed on main
- `agents/clients-asynccore` 9ee048b1: async_core.rs waits for replies while its in-process daemon
  lives by its own watchdog (`Daemon::watchdog_turns`). At nice 19 beside 2 spinners/CPU: main 4/10
  (x86_64), 6/10 (arm64); fixed 10/10 both (diag-results/clients-asynccore-1-*, -2-*).
- `agents/clients-sdk-listing` 79eaf3e7 (SDK `live_shipped`/`not_recovered`), `agents/clients-a237-fix` 040a7b59.

## Ready, not on main: the class-B fleet timing work
Branch `agents/clients-timing-final` at ae0f7795 (main 29ef4521 merged in, gates green: fmt, clippy,
xtask, doc, server lib 237/237, the seven tests 7/7). The supervisor accepted it for B12 on 2026-11-12;
it has not landed. Merge current main into it (merge, never rebase) and re-gate before it goes in.
- 83727873 fleet: the seal hedge reads one clock (`futures::now_ns()`) on both sides; it had compared
  the boot clock with the shard's, so every seal hedged at once since 2026-09-13
  (docs/bugs/2026-11-08-a-seals-hedge-compared-two-clocks.md).
- 3c072f49 server: `verbs::locate_within_budget` counted in its own polls (`LOCATION_POLLS`), capped
  by the failover SLO; counters `fleet.owner_location.gave_up`, `.slo_ended`, `.polls_spent`.
- dbd34acd tests: the four timing tests judge by counts/order, not wall bounds.
- 9a1412fe tests: the hedge test checks the delay held (`fleet.seal.hedge_delay_ns` within
  `Daemon::fleet_put_latency_range`). Evidence: hedge-3 10/10 per arch, 300 s fault red on both;
  timing-9 x86_64 all five 10/10 (its arm64 job never ran); mutants-5 every fault red.
- GAPS (ae0f7795): the forward lane's own wall stall budget (`forward_lane.rs`, `quiet_ns >= stall_ns`)
  is still owed under A-232; loadtest-scale had a poll-clock fix (6da9670d) not yet on main then.

## Open: green-share takeover flake (fleet.rs `a_share_of_a_green_made_after_its_last_version_survives_a_takeover`)
Reproduced on agents/bug-sweep-2 98c5a166 (diag-results/clients-green-1-*): x86_64 10/10, arm64 2/10
failed. Both: successor's chain at version 2, co-survivor's at 3; `merge.takeover_incomplete`
(GreenWait::Chain) ~100 then `merge.takeover_given_up`. Cause: version 3 committed with the owner and
the non-successor; the successor never received version 3's increment content, so
`merge_service::recover_green_inputs` → `held_inputs` is None and nothing fetches it from the
co-holder. That is GAP-A9-7's input half. bug-sweep independently suspected the same branch, and notes
the sibling tests wait for `holders_before_owner_death` before the kill; this one does not.
Options put to the supervisor, **no decision received**:
(a) fetch missing inputs by manifest identity from the adopted record's holders (design-level);
(b) the test accepts NotRecovered { TakeoverIncomplete { chain } };
(c) the test waits for both holders before the kill like its siblings, plus a separate deterministic
red test for the gap by name (bug-sweep's lone_fleet_node / plant_taken_over_green fixture).
liveness is changing this test's materialize wait and kill point (agents/liveness-takeover-1); merge around theirs.

## Open: forwarded-read bound (loadtest-scale's dead-session finding (b))
`verbs::resolve_and_forward` holds a forwarded read to `state.config.failover_slo_ns` (10 s) of wall
from when the origin began resolving; the location budget above draws on the same window. Derive the
client-facing bound from what a resend waits on (counts or the daemon's time), red first. Needs
loadtest-scale's figures (each resend's lane queue position, age when served, lane entries per request
id) and their failing test's name; none had arrived. Supervisor's order: this before other test flakes.

## A-232 (g255): its core is on main (correction, read before starting)
A kiln subagent started from my goal stack (01a1d6ca8b63, now gone) worked it while I was away and
reported the goal met (2026-11-14), and main carries the rule since 2026-11-13: crates/client/src/client.rs's
header says a live daemon's call waits however long the work or however starved its threads, a daemon
judged stuck is ended by its supervisor or by its own watchdog when it runs alone, and the call then fails
`DaemonGone`; `defers_reply` is gone. Record: docs/bugs/2026-11-13-a-live-starved-daemons-call-was-failed-stalled.md.
Related on main: 3e93acc84 (reconnect while the rendezvous has a holder, else DaemonGone at once),
61b5940f8 (one full-ring wait rule), 979584c68 (a lone daemon's end), 335ac9415. So check the goal's
remaining items against main rather than building from scratch. The design agreed 2026-11-08, for
comparison: Stalled for a live daemon goes away; the async driver, sync round_trip and ring-full wait
follow one rule; SDK tests asserting Stalled for a stopped daemon rewritten; §4.7's two rules and §4.14
under A-232's amendment; bound t0 + 2·W + B + L + kill (liveness); a SIGSTOPped unsupervised daemon cannot
end itself; tests: CPUWeight=1 never fails a call, a stuck shard fails it within the bound, an unsupervised
stuck daemon ends itself.

## How I ran evidence
Diag lanes: push a commit adding `.github/workflows/diag-clients.yml` (and a `.github/diag-*.sh`
script; inline multi-line Python in the workflow once failed to start) to `diag-clients-<lane>`;
results publish to `diag-results/clients-<lane>-<os>` (`if: always()`), watched with `git ls-remote`,
never `gh` polling. 10 runs at `nice -n 19` beside 2 spinners per CPU reproduces these load flakes.
