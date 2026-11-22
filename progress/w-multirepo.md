# w-multirepo progress (kiln agent, started by main 01a1ff2e 2026-11-21 ~16:45 PST; A-724 moved from w-balance)

Clone: ~/slates-work/w-multirepo; target ~/.cache/w-multirepo-target.noindex; state ~/fanout-state-w-multirepo.

## Queue (main's brief): 1 isolation test (done by w-balance 0fb1fd79b except the destroy verb), 2 learned demand +
## shares, 3 rest of piece 3, 4 piece 7 load proof, 5 pieces 4/5/6(ruling)/8/9.

- Hold: don't edit vcs_share::room or hg's push_room until w-hg lands its admission-leak fix (main 16:48).
- 2026-11-21: piece 1's test is on main (w-balance 0fb1fd79b); its only red is the mid-load destroy: no verb issues
  Op::RepositoryDestroyed. Claimed A-843 for `DestroyRepository` / `slates repository destroy`. Building it.
- LANDED 87337c6fa (main e56aa949c): A-843 DestroyRepository + `slates repository destroy`; unit test ok; vcs_many git
  N=2 0 failures. Reported inbox/main-from-w-multirepo-piece1.md; run-requests updated.
- NEXT piece 2a: slates_vcs::mrc (SHARDS fixed-size MRC, Fenwick distances, pow2 bins of the window, SHARDS_adj).
  Draft at ~/.cache/w-multirepo-scratch/mrc.rs (not in tree). Then 2b decayed rates, 2c utility partition over the
  A-188 floor + WFQ weights, 2d owner-shard wiring + snapshot publication.
- 17:05 main asked whether piece 1 landed: answered yes (agents_send), 87337c6fa.
- LANDED 517c732fa (main 8a0483cd8): A-844 slates_vcs::mrc. Reported inbox/main-from-w-multirepo-piece2a.md.
- NEXT 2b: decayed rates (works/bytes per repository and principal; decay from measured inter-arrival times).
- OVERLAP: w-balance still on A-724 (eccdf9b0f Learned "2a", 94efbcbe2 3b PastShare, says it's on piece 2). Asked main
  for a split (~17:35); hands off usage.rs and vcs_share until then. Told w-balance (inbox overlap note).
- LANDED 3b06a10d5 (main 4c43bbbe5): A-845 slates_vcs::rate. Reported.
- NEXT 2c core: slates_vcs partition by marginal utility (UCP lookahead) over Curves above the floor; pure.
- LANDED 19f31bfb7 (main 0ecd7ee1e): A-846 slates_vcs::partition. Reported. Main's lint touched mrc.rs (fine, 13/13).
- NEXT: 2d wiring (owner shard: curves fed from content reads, rates from begun/stepped, partition at the learning
  cadence, snapshot published) -- needs main's split ruling. Meanwhile: piece 7 (load proof test), which is mine.
- 18:00 PARKED on main's A-724 split ruling (all remaining A-724 work touches w-balance's live files). Sent the CPU
  weights design question + 2d wiring plan: inbox/main-from-w-multirepo-question-cpu.md.
- w-hg: admission leak fixed 687cd98b1 (521fe3689; index_upkeep lent the whole admittable to a merge step). Hold on
  vcs_share::room / push_room lifted. Siblings lending whole admittable rooms (GAPS, main routes): git restage_turn
  (stage_room), SealingPush::turn (self.room), git_hooks seal. Still parked on main's A-724 split.
- RULINGS: main 17:22 (final): mine = piece 2's rest (rates/curves on owner shard, partition via published snapshot,
  CPU) + piece 7, slates_vcs::usage mine; w-balance keeps piece 3's rest (backpressure, svn admission, hg/svn words,
  hg fetch claim). Tell w-balance BEFORE replacing vcs_share::room's per-request division. CPU: equal-weight DRR first;
  piece 7 A/Bs equal vs learned weights (and w-balance's turn-weights branch 7d4d938bc); data decides.
- LANDED 4f5bbb238 (main f705487f9): one learned demand: Rates replaced usage::Learned (bytes_per_work at a size
  horizon chosen by squared error). Reported inbox/main-from-w-multirepo-one-demand.md.
- NEXT 2d: (1) principals' Decayed on usage slots; (2) curves fed from a repository's object reads on the owner shard;
  (3) partition at the learning cadence -> immutable per-shard snapshot -> room reads grants (tell w-balance first).
- LANDED 44b7ce290 (principals' rates) and db79945ab (curves fed by git fetch reads via vcs_store::observed). Reported.
- NEXT 2d part 3: aging + partition at cadence + snapshot + room reads grants (tell w-balance first).
- LANDED b84bc12e4: vcs_share::grants (partition at the learning cadence). Room shape sent to w-balance (waiting OK).
- NEXT: piece 7 harness extension (fetches + hg/svn in multi_repo_load.py) and the CPU A/B run request.
- LANDED f60f02350: piece 7 --fetches + Daemon --vcs-store fix (A-849). Run request written (A/B equal DRR vs
  turn-weights). Reported.
- OPEN: room reads grants (await w-balance OK; inbox/w-balance-from-w-multirepo-room-shape.md), curve feeding from
  pushes/hg/svn (other owners' step sites), hg/svn in load proof.
- main 05:32: many-repository rounds on 3e910809f all pass (git 97.8 s, hg 173.9 s, svn 200.9 s, big Studio release).
- LANDED 7cd88b047: §4.17.5 CPU open-decision status. BRANCH agents/w-multirepo-learned-weights 1cec5ee82 (A/B arm B;
  worktree ~/slates-work/w-multirepo-weights, target ~/.cache/w-multirepo-weights-target.noindex). Run request: 3 arms.
- WAITING: w-balance yes/no on the room shape (asked again via agents_send).
- For main to delete when the A/B is done: ~/.cache/w-multirepo-weights-target.noindex, worktree above (I'll remove the
  worktree myself with git worktree remove once the branch is settled).
