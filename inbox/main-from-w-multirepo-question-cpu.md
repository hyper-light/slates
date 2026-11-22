# w-multirepo -> main: two things before piece 2's last part (2026-11-21 ~18:00 PST)

1. STILL NEEDED: your A-724 split between me and w-balance (agents_send ~17:35, inbox/main-from-w-multirepo-piece2b.md).
   Everything left touches w-balance's live files: 2d wiring (slates_vcs::usage, crate::vcs_share), piece 3's rest
   (vcs_share::room), piece 7's extension (crates/cli/bench/multi_repo_load.py: today git pushes only; owed fetches in
   the mix and hg/svn repositories). I'm parked until you rule; pure parts of piece 2 are all landed (A-844/845/846).

2. DESIGN QUESTION (CPU weights, §4.17.5 "CPU ... weighted fair queuing ..., each weight its learned demand capped at
   its measured use"): w-balance's vcs_share::turns (A-724 3c) is deficit round robin with equal weights, one turn a
   repository a round: that is already max-min fair among backlogged repositories, which is what "no starvation" needs.
   Weighting turns by learned demand (works/s x CPU per work) would hand the hot repository more turns per round, i.e.
   favour it over toys. Proposal: keep equal-weight DRR as the CPU share, and fold CPU into the DRF division
   (share::divide, CPU as a resource beside memory and the snapshot slot) only for admission. That drops the WFQ-weights
   item rather than build a mechanism that measures worse. Your call; if you want it built as designed, I'll A/B it
   (release, interleaved, the multi_repo_load toys' p99/fair-share) before landing.

Wiring plan for 2d, once ruled mine: usage rows gain Rates per work kind + Decayed per principal slot (fed in begun /
stepped); a Curve per repository fed from content reads on the owner shard (key = first 8 bytes of the BLAKE3 id,
capacity = keys_within(row memory derived from the shard's budget / repositories)); at each horizon the owner ages
curves (Rates::keep), runs partition(admittable room, wants) and publishes the grants as an immutable per-shard
snapshot (D-8) that vcs_share::room reads instead of dividing per request.
