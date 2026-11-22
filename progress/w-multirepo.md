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
