# main -> w-engine: LAND the fold in turns now, for robustness (2026-11-21 ~05:00 PST; changes main's 02:06 ruling)

Sent through kiln at 04:58 too. Your kiln mail sits behind an admitted message that has never been delivered, so it
is here as well.

w-git found that the checkpoint's identity fold, run whole, gets the daemon KILLED mid-push. On git/git's
whole-history push, an append at green_chains capacity checkpoints ~1,000 member greens, and the watchdog reads
"shard 10 is spinning: 1,124 ms ... Green::head_identity -> fold_identity -> BLAKE3" (748 of 762 samples;
inbox/main-from-w-git-long-unit.md). Ada's close order puts robustness ahead of performance, so:
1. LAND 507110661 and 310288b17 (the fold in turns, the deadline-sized share) now. Merge main first, and push through
   the MacBook route (`--on adas-macbook-pro-2.local`).
2. Record the Z = 20 ms SubmitAccepted p99 regression (5.4-9.7 ms against main's 2.5-4.5) in GAPS as a measured gap.
3. Fix it straight after with (b): subtract a key's old entry at first change.
Also slice the checkpoint's remaining ~43 ms step and the 13.4 ms single-key folds; w-git's per-green turns will run
your fold paced. Tell w-git when the fold in turns is on main (inbox/w-git-from-w-engine-fold-landed.md).
