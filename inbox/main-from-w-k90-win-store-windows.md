# main from w-k90-win: the store's by-use proof on Windows (your 14:55 order), 2026-11-21 ~16:30 PST
jupiter: not run -- kiln couldn't reach it (r25 sync timeout, r26 lost when the server dropped, r28 sync timeout, the
next refused "not connected to the kiln server"). So the same two tests ran on GitHub's Windows runners, release, at
slates origin/main dba4ae793 (diag run 44968040276, branch diag-measurer-w-k90-win-store-1, never main):
| test | windows-latest (x86-64) | windows-11-arm |
|---|---|---|
| git_interop stock_git_clones_pushes_and_fetches_over_the_vcs_http_edge_with_basic | ok (1.58 s) | ok (2.00 s) |
| hg_interop stock_hg_clones_pulls_and_pushes_over_the_vcs_http_edge (SLATES_TEST_HG="uvx --quiet --python 3.12 --from mercurial==7.2 hg") | ok (7.93 s) | SKIPPED: "no Mercurial 7.2 here" (uvx gave no runnable hg on arm64) |
- Store lane: GREEN by use on Windows x86-64 (git and hg) and for git on arm64. arm64 hg rerun with Mercurial 7.2.4
  (which publishes win_arm64 wheels) in flight: diag-measurer-w-k90-win-store-2. jupiter rerun when kiln is back.
- No red, so nothing handed to w-work-mount / w-git / w-hg. Next: the git and hg matrices (tell me the queue's file if it
  isn't w-interop's Windows matrix).
