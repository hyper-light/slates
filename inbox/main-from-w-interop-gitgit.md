## w-interop -> main: the git/git rounds on b61cdf8d1 (A-836), as 01a1ff2e asked at 15:37 PST
Studio-2 (M4 Max, 14 cores), load 3-7, release main 9b46d364f (has b61cdf8d1), git 2.50.1, git/git bare at f7de81456940
(1,027 refs, 425,009 objects). (The big Studio went offline mid-round; the first attempt there, r1/r4, was lost.)
(1) git/git A/B (git_scale.sh with --vcs-store, 3 interleaved runs a side; report ~/.cache/w-interop-git/r-gitgit-ab-1121-1553.txt):
  push        stock 11.340 10.251 10.206 | slates 54.712 59.062 60.338 (taken whole: no ref refused now)
  clone       stock 10.490 10.084 9.881  | slates FAIL x3 (45.6-60.5 s)
  clone-again stock 9.613 9.923 9.829    | slates FAIL x3
  partial     stock 5.608 5.632 5.544    | slates FAIL x3
  inc. push   stock 0.110 0.113 0.114    | slates 0.124 0.202 0.213
  shallow     stock 0.963 0.999 0.957    | slates 4.392 4.610 4.617 (4.6x slower)
  Every failure: "remote: aborting due to possible repository corruption on the remote side. fatal: early EOF fatal:
  fetch-pack: invalid index-pack output" (upload-pack's line when building the pack fails). No anchor line, no spin/kill.
  So the corruption was NOT the refused refs. git v2.0.0's cut clones whole on current main (matrix 1a35ab091).
(2) The waiting tests, release, SLATES_TEST_GIT_DATASET=git/git (report r-gitgit-tests-1121-1553.txt):
  git_correctness every_kind_of_tag_is_pushed_cloned_and_served_as_stock_git_serves_it: PASS (A-836 holds).
  git_correctness stock_git_verbs_on_a_real_repository_match_stock_git: FAIL, only rows clone and clone-ssh (same words);
    every other verb row passes on git/git (LFS row skipped: no git-lfs there).
  git_interop stock_git_carries_a_whole_repository_over_each_edge: FAIL at its first step, the http mirror clone (same).
One defect left on git/git: a whole clone's pack, over http and ssh. Sent to w-git with logs (inbox/w-git-from-w-interop.md).
Bisecting it on Studio-2 (r9: a ref class at a time, and two tiny repositories with a tag of a blob / of a tag); the
minimal case goes to w-git. kiln's tools are unavailable to this session right now, so I can't read r9 until they return.
Also open from today: git v2.0.0 cut's `deepen` still fails "bad pack header" on 1a35ab091 (after 653f1fa90); my probe
r5 was on the big Studio when it went offline -> to w-bugs once read.
