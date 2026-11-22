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
