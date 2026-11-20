# RESUME (checkpoint 2026-11-19, for main's kiln move): w-unresolved's QUEUE IS DONE. Nothing to resume.
- Landed on main: A-707 (05e3a2ddc), the lineage model's mirror of A-508 and A-556; then the goal 84 audit
  (b2a544735), the GAPS table.
- Queue item 2 (the Lineage.tla text) was already on main as A-518.
- No work is uncommitted or unpushed, and no run is in flight. The worktree ~/slates-work/w-unresolved-check and
  ~/.cache/w-unresolved-local-target are deleted. The branch agents/w-unresolved-check is local only, its commits all
  on main.
- Left for goal 84, all main's runs: the store-destroyed live_chaos flow on merged main (Linux, both architectures);
  MC_Lineage_two.cfg run to its end; lineage_model past its default cases (run-requests.md).
- Ada decides whether a volume recovered past tolerance refuses writes until an operator acknowledges it.

# w-unresolved progress (kiln goal 84 d9f7a795, "An unresolved object is recovered", A-272 (ii))

Role: implementer, headless worker; supervisor "slates-7a". Rules: briefs/common.md FAST LANDING LOOP + OWN TARGET DIR
(~/.cache/w-unresolved-local-target, JOBS=6, local on this Mac, 20 GB disk guard).

## 2026-11-19 start: state found
- Predecessor g250 landed on main: A-309 (records, current-members round, live past tolerance, Lineage.tla, Rust model),
  A-332, A-335, A-340, A-341, A-349, A-362, A-364, A-373, A-394, A-412, A-484, A-508, A-518 (Lineage.tla text for
  A-341/A-484/A-508 -> queue item 2 ALREADY ON MAIN), A-529, A-538, A-546, A-556; A-577 (g267).
- Owed (GAPS A-508 line): the Rust lineage model's mirror of A-508 (settle away) and its oracle. -> queue item 1 piece.
- Then queue item 3: goal 84 clause-by-clause check against main.
- Worktree: ~/slates-work/w-unresolved-check (branch agents/w-unresolved-check, from origin/main 3bcaccf91).
- Old kiln agent ~/slates-agents/live-durability has uncommitted changes (cluster lib.rs, forward_lane.rs,
  live_ship.rs) on agents/live-durability-1: not this goal (live ship perf); left untouched.

## A-707 LANDED main 05e3a2ddc (piece 4f2d92af2), 2026-11-19
- crates/db/tests/lineage_model.rs mirrors A-508 (settle_away on Confirmed::Not; oracle SettleKeepsFPlusOne: f+1 other
  members hold a record at least as new) and A-556 (round on readiness_cohorts/readiness_current_cohorts). New verdicts
  reclaimed_settled_away 72, kept_outside 2526 over 400 seeds; no violation. Local (this Mac): lineage_model 4/4 pre- and
  post-merge (merge changed register.rs); fmt clean. Design A-707 + A-508 owed status; GAPS A-508 line. Run request filed.
- Queue item 2 (Lineage.tla text for A-341/A-484) was already on main as A-518 (b69463eec) -- nothing to build.
- NEXT: queue item 3, goal 84 clause-by-clause check (GAPS entry with a table, as g262's library audit at GAPS ~3025).
  Same worktree ~/slates-work/w-unresolved-check (branch agents/w-unresolved-check).
  Note for the check: GAPS "Open (A-309): a holder the round never asked keeps its copy" (~line 4502) reads stale
  after A-341/A-484/A-508 -- check and mark in the audit.

## Goal 84 audit LANDED main b2a544735 (piece ab5e36ea1), 2026-11-19
- GAPS: "Goal 84 audited clause by clause" table placed before A-309's "Fixed (A-272 ii)" entry: 5 clauses, each
  built and tested. Model table row MC_Lineage.cfg filled from main's TLC r140 (focus-84-90.md: no error, 30.6M
  distinct, depth 18, 22 min). A-309's "Open: a holder the round never asked" line got a status note (both owed parts built).
- Still owed (runs, main's): store-destroyed live_chaos flow on merged main (Linux x2) -- no recorded run since A-309;
  MC_Lineage_two.cfg to its end; lineage_model beyond default cases. Ada's: past-tolerance write refusal (Decisions).
- QUEUE DONE. Worktree w-unresolved-check removed; ~/.cache/w-unresolved-local-target deleted.
