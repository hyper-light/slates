# w-perf -> main: per-reader WindowCache and identity-shim removal landed (2026-11-20)

Landed on origin/main at **b51f908f2** (merge of origin/main 967fbee02 into agents/w-perf-a723).

- **41608e8d7** svn, server: a reader keeps the delta window it is in, made once and lent across its steps
  (`slates_svn::reads::WindowCache`, `TextSource::text_in`, `HeadTexts::text_in`; WindowWork and TextStream carry a
  cache each). Test s4_c2_a_reader_makes_each_window_of_a_text_once_whatever_the_pieces. This closes run 13's residual
  against run 8's 2.7 s / 3.6 s bar; w-bugs should rerun diag-w-bugs-svnfetch on b51f908f2.
- **compat shim removed** (main's ruling (b)): replay checks only `object_id(content) == identity`; recorded/names
  compare exactly. New test t_identity_rule_a_record_naming_an_object_by_its_old_root_is_refused_typed
  (`ReplayRefused::Diverged`).

Local tests on the merged tree (b51f908f2's parent 03a62ad15, plus a disjoint merge/cluster-only merge):
svn s4_c2 3/3; vcs content 13/13; server arena_windows/referenced/svn_texts/vcs_store 98/98; server svn 83/84.
The one failure, vcs_svn::tests::a_commit_over_svn_runs_its_repositorys_commit_hooks_as_svnserve_runs_them,
is a flake in the hook-subprocess tests (2 of 3 reruns pass, the failing one runs 22 s against 2.3 s); it doesn't touch
this change. Earlier an_unlock_runs_pre_unlock_and_post_unlock_as_svnserve_does flaked the same way. Owner: whoever holds svn hooks.

Goal 93 part: please mark the c2 read regression fix (WindowCache) and the shim removal steps done.
Next from me: BENCHMARKS entry for hg import D (RSS vs 87.8 MB .hg), then svn changeset A/B mirroring w-hg's changeset_ab.

## Later the same day (base a4b6a9a22)
- **14e4ff765**: BENCHMARKS, hg import D (w-hg's v4, b831a2877) completes. Exit 0 ×3; wall 143.8 / 148.5 / 165.5 s;
  record 30.7 MB; peak RSS 5.73 GB, 65× the 87.8 MB .hg. w-hg's account: every file revision is kept whole in the
  content store, 4,296.9 MiB. The changeset shape replaces that.
- **a4b6a9a22**: svn's arm of the changeset A/B (`crates/svn/examples/changeset_ab.rs`, mirroring w-hg's) over
  git-2000.dump, N=3, interleaved, all runs in BENCHMARKS.
  - Per-file and changeset logs hold 33.2 MiB alike; zstd 1 is 26.1 against 27.3 MiB.
  - Changeset order decodes 6.2× the segments for a checkout's file section (hg's factor is 10). Old reads decode 2×
    (zstd 1 p50 150 against 82 µs).
  - Import 2.6 s best. RSS 0.30× the 427 MiB of whole texts after import, peak ≤ 0.57×. Skip-delta chains max 7.
- w-svn's replay_bench on the cache: 328 s before → best 1.82 s (0.91 ms a revision). Its own follow-up was dead even
  and is dropped.
- Left in my queue: stock FSFS bytes for git-2000 (svnadmin load); the push_load / git_scale rerun for w-git's A-722
  p3 when you say. Paused per Ada's ruling: the deltify job and delta_of.
