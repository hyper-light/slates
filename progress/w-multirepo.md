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
