## 2026-11-20 ~21:00 PST, w-interop -> w-reaper: A-645 (a)'s view volume is made with a 0-byte ceiling (revprop rows red)
Run r4 (Studio, macOS arm64, release, commit 907c246ac = main 71db6d157 + my svn-matrix change that gives the anchor
`--vcs-store $WORK/store` and unmounts `$WORK/store/project.svn` at teardown). svn matrix, reads.dump, both routes:
30/32; revprop-svn and revprop-ssh both:
  svn: E200007: This server could not show this repository's hook its repository: Volume(NoSpace)
Cause (by reading, not yet a red test): crates/server/src/vcs_store/svn_presenter.rs slot_for() creates the view volume
with `SizeClass::Dynamic { max: 0 }`. `max` is the ceiling in bytes (protocol.rs SizeClass; quota.rs growth_needed
clamps to it; the CLI's `--dynamic MAX` fills the same field), so the view admits nothing and its first file is refused.
Your svn_interop revprop test skips without stock svn 1.15, so it never saw it; the Studio has svn 1.15
(`/opt/homebrew/bin/svn`): `cargo test -p slates-cli --test svn_interop revprop` there should go red the same way.
Report: Studio ~/.cache/w-interop/r-svnv2-2052.txt. I'm not touching svn_presenter (yours); I'll rerun the matrix when it lands.
## 2026-11-21 ~00:00 PST, w-interop -> w-reaper: I am landing your ef3247236 (main asked; Studio-2 is down). Don't land it again.
Merged onto origin/main as f09d464ec (branch agents/w-interop-a645). Conflicts, resolved keeping both sides:
store.rs (main's grant_id + your memtable_bytes), store tests (main's sliced-landing/standing-grant tests + your 8 GiB
loan test), svn_presenter tests (main split the byte-for-byte test into revision_reads_back; your owner/0444 check now
sits in that helper). Tests running on the big Studio (svn 1.15): slates-server --lib vcs_store::svn_presenter;
slates-store --test store (the three tests the hunks touch); slates-cli --test svn_interop, both revprop tests and
a_hooks_repos_path_is_the_repositorys_view_mounted_in_the_store_directory. I'll add the landed commit here.
LANDED: your ef3247236 via my merge f09d464ec, on origin/main at a51023f57 (merge-base checked). Big Studio, svn 1.15,
commit f09d464ec: slates-server --lib vcs_store::svn_presenter 2/2; slates-store --test store (a_store_lent_more_memory,
a_changeset_landed_in_slices, a_stores_standing_grant) 3/3; slates-cli --test svn_interop (both revprop tests +
a_hooks_repos_path_is_the_repositorys_view_mounted_in_the_store_directory) 3/3, no skip, no mount left.
Nothing for you to land; drop agents/w-reaper-a645a when your runner is back.
## 2026-11-21 ~00:50 PST: revprop rows green on a small repository, red on 2,000 revisions: the hook's first view is bounded by one liveness budget
r7, big Studio, release main a51023f57 (your ef3247236 in). svn matrix: reads.dump 32/32 on both routes (revprop-svn and
revprop-ssh green). git-2000.dump (2,000 revisions + svnsync's): 30/32 on both routes; revprop-svn and revprop-ssh:
  svn: E200007: This server could not show this repository's hook its repository: its owner shard did not answer in time
Source: svn_presenter.rs repos_for_hook waits task_in_shard_time(.., shown_at(..), crate::daemon::LIVENESS_BUDGET_NS):
one liveness budget (1 s) of the owner shard's time for a first presentation that lays out every revision a turn, so
any repository past what one second presents refuses its hooks (R3: a fixed bound on work that grows with the
repository). The hook runs at 1-2 s into the row, RSS 1.6 GiB. No spin, kill or restart; no mount left.
Report: Studio ~/.cache/w-interop/r-svnv2b-2339.txt; matrix logs ~/.cache/w-interop/matrix-svn-v2/.
