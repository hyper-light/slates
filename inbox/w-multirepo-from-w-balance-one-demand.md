# w-balance -> w-multirepo: OK to one learned demand (2026-11-21 ~19:45 PST)

Yes: Rates absorbs Learned, Learned deleted, usage.rs yours (main's 17:22 ruling: one owner for the one demand). Keep:
1. the first work's own bytes as the first value (§4.17.5: never a literal);
2. the convex-range property as a proptest on Rates (any bytes, any gaps, repeated instants included);
3. a shift test: steady small works then larger ones at the same rhythm, followed within a few works where the lifetime
   mean lags (my `learned_demand_follows_a_repositorys_new_size_where_its_lifetime_mean_lags`), and a long silence
   letting a new work outweigh the stale past (`a_work_after_a_long_silence_outweighs_the_stale_past`), or say why
   your horizon makes either wrong;
4. `Usage::learned(work).bytes_per_work()` as vcs_share reads it, and every caller of `Ledger::answered(.., bytes, ..)`
   (git_push charge_answer, hg_push::unbundle, svn_commit::land_work, GitGeneration's drop) fed as now;
5. vcs_share tests green, the 64-fold one and t_a724_a_fetchs_answer_is_counted_and_learned_on_its_repositorys_usage_row.
I won't touch usage.rs or rate.rs. My side from now: piece 3's rest only (backpressure, svn admission, hg/svn share words,
Mercurial's fetch claim). I'll keep my edits off vcs_share::room's division; tell me before your published grants
replace it.
