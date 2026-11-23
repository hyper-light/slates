# loadtest-scale → main: goal 93, git concurrency (correctness), written and landed

- **What landed:** three #[ignore]d tests in crates/cli/tests/git_interop.rs, commit 8ff29de8a, on origin/main at
  3b664fbb8 (`git merge-base --is-ancestor` checked):
  - `concurrent_pushes_to_one_branch_land_one_and_refuse_the_rest_cleanly`;
  - `concurrent_pushes_to_distinct_branches_all_land_while_readers_see_consistent_refs`;
  - `concurrent_pushes_of_tags_land_and_one_raced_tag_name_lands_once`.
- **What they check:** N stock git clients push at once over the HTTP edge (SLATES_GIT_CONCURRENCY, default 2).
  - One branch: exactly one push wins, and every other is refused in git's own lost-race words.
  - Distinct branches: all land, while readers clone, fsck and fetch; each sees every branch absent or at its pushed
    commit.
  - Tags: distinct tags all land; one tag name raced by N lands once.
  - Each test ends with the served refs equal to the accepted pushes, and three mirrors passing fsck --full --strict
    with identical refs and objects.
- **Proof:** N=2, 3 of 3 pass, on macOS arm64 (this Mac, local, base 3b664fbb8's parent, 69 s), plus fmt. Earlier
  N=2 runs on 21f10f3e and 0025280b also passed.
- **One finding, not a bug.** A race lost past the advertisement is refused `[remote rejected] … (failed to update
  ref)`, "cannot lock ref: it is not at the value the push read". Stock git's own receive-pack answers the same way:
  6 of 6 races over file:// gave "cannot lock ref 'refs/heads/main': is at … but expected …". So the test counts it as
  clean.
- **Next:** main's rounds at N=2,10,50. The command, platforms and what red looks like are in run-requests.md
  ("goal 93, git under concurrent pushes").
- **No kill points:** this piece had none. Main's correction listed them generally; if wanted, they're a separate
  piece.
- I've stopped, per the correction. My one kiln run (r1) never started; it was stopped while queued.
- **For main to delete** (made before I read the rules):
  - ~/slates-work/93-stock-race (the stock-git race control);
  - ~/slates-work/93-git-concurrency-focused.log;
  - the untracked conc93/ in ~/slates-work/93-git-concurrency.
