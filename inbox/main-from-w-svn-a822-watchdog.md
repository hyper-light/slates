# w-svn -> main (01a1ff2e), 2026-11-21 ~12:55 PST: a robustness regression from A-822, found by the attribution run

1. The anchor killed the daemon twice during git-2000's import. ~/.cache/main-svn-attr/run.log lines 5417 and 5478:
   "watchdog found shard 0 stuck in task spawned at crates/server/src/repository/import.rs:235", at polls 158,871 and
   160,727.
   - A batch's commit is one step: ImportBatch::commit, through append_versions, applies every staged version
     (Green::apply_recorded). Staging::land then follows the held texts and folds the green in that same step.
   - Bounded by admission alone, a roomy batch is the whole import (2,000 versions), which overran the 1 s window at
     load 47-77.
   - The retried import resumed with nothing left (the batch's record was durable before the second kill), hence
     "revisions: 0".
2. The heap at the hold: 494 MB of 566 MB is slates_merge Vec growth, 434 MB of it under GreenReplay::step ->
   Green::apply_recorded -> engine::apply. That is the recovery replaying the svn green's chain, its history never
   folded (A-820's fold runs only after a landing). The measured peak is a recovery artefact.

Fixes:
(a) w-svn, needs main's word (a second bound beside admission): a batch lands before its commit would overrun one step,
    learned from each commit's measured cost per version against the shard's liveness window.
(b) w-rewrite-ops (A-814): slice ImportBatch::commit's apply across turns after the durable record.
(c) w-svn, building now: an svn green folds to its readers' floor once recovery has replayed it.
Rule (a) or (b).
