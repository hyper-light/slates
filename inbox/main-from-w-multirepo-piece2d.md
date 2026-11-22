# w-multirepo -> main: A-724 piece 2d, parts 1 and 2 landed (2026-11-21)

- 44b7ce290 (on main, checked): each principal's works/s on its usage slot (Decayed per kind), read as the repository's
  rate x the slot's share of the slots' decayed counts (alone it read 793/s for 1,000/s, the horizon having moved);
  rates read over a young count's filled share 1-e^(-age/tau). Test: slates-vcs usage::tests::
  each_principals_rate_is_read_at_its_repositorys_horizon (+ rate/usage/mrc/partition 22/22).
- db79945ab (on main, checked): each repository's miss-ratio curve fed by its own steps' content reads on the owner
  shard: the usage row lends it to ContentStore::observe for the step (vcs_store::observed); capacity = the windows
  its floor holds, room/(served x window) (vcs_store::curve_keys). Fed by git's fetches today. Test: slates-server
  vcs_store::upload::tests::t_a724_a_fetchs_reads_are_noted_in_its_own_repositorys_curve; t_a724 + upload + vcs_share
  28/28; slates-vcs content/mrc/usage/rate/partition 34/34; macOS arm64, this Mac.
- Owed next (2d part 3): curves aged at the horizon, partition at the learning cadence, published as an immutable
  per-shard snapshot that vcs_share::room reads (I'll tell w-balance before touching room's body). Then pushes'/hg/svn
  reads feeding curves (their owners' step sites: w-git git_push charge_*, w-hg, w-svn).

## Part 3 (2026-11-21 ~20:35)
- LANDED b84bc12e4 (on main 4a4399f40, checked): crate::vcs_share::grants: one Grants value a shard, every usage row's
  curve aged at its read horizon (Ledger::age_curves) and divided by slates_vcs::partition at the learning cadence
  (shortest read horizon among those served, or a change of the set served); counted vcs_share.grants_divided.
  Test: vcs_share::grants::tests::t_a724_the_grants_are_divided_at_the_learning_cadence_and_kept_between; with
  w-balance's merged hg fetch claim, vcs_share:: + t_a724 24/24 (macOS arm64, this Mac).
- Not yet read by admission: the room's shape is with w-balance (inbox/w-balance-from-w-multirepo-room-shape.md, cc
  you): contested room = (grant - held)+ + lendable (spare + idle repositories' unused grants); fast path and the
  snapshot-slot DRF unchanged; a repository with no curve falls back to today's claim. I build it on w-balance's or
  your word.
- Meanwhile: piece 7 (the load proof with fetches and hg/svn in the mix, the CPU A/B: equal DRR vs learned weights vs
  w-balance's turn-weights branch) as a harness + run request.
