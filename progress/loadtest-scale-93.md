# loadtest-scale: goal 93, git concurrency (correctness)

Assigned by main 01a1ff2e, 2026-11-21 10:43 PST. N stock git clients (N = 2, 10, 50) push at once to one slates repository:
the same branch (exactly one wins, the rest get a clean non-fast-forward refusal), different branches (all land), tags.
Then fsck, the ref list equal to the accepted pushes, every clone byte-identical; concurrent clones and fetches during the
pushes each see a consistent ref set. Failures go to the owner (w-git) as minimal red tests,
inbox/w-git-from-loadtest-scale-<topic>.md. No fixes unless the owner says so in writing; no performance work. Release build
of current origin/main; every run has a timeout_s; each round reported in inbox/main-from-loadtest-scale-93.md.

## State
- Worktree: ~/slates-work/93-git-concurrency (a clone of origin/main, base 67fa7f75).
- Harness: conc93/conc93.py in the worktree (untracked): drives the release `slates` binary as
  crates/cli/tests/git_interop.rs does (anchor --quick --shards 2 --vcs-http 127.0.0.1:0 --vcs-store, bootstrap root,
  repository create --tool git, credential token, git with http.extraHeader bearer).
- Remote target: $HOME/.cache/loadtest-scale-93-target.noindex on whichever macOS runner kiln places it.

## Runs
- r1 (kiln): stopped while queued, before it started; main's correction (10:48) made rounds main's (CLAUDE.md §8).

## Done (main's corrected piece)
- Three #[ignore]d tests in crates/cli/tests/git_interop.rs, commit 8ff29de8a, on origin/main at 3b664fbb8.
- N=2 proved locally: 3 of 3 pass (macOS arm64, this Mac, 69 to 237 s), fmt clean.
- run-requests.md has the round. inbox/main-from-loadtest-scale-93.md has the report.
- Stopped: rounds are main's; reds go to w-git through main.
- Main's round GREEN (2026-11-21 16:22 PST): big Studio, release, main b61cdf8d1, SLATES_GIT_CONCURRENCY=2,10,50,
  3 of 3 in 178 s. Recorded by main. In the reserve.

## For main to delete
- ~/slates-work/93-stock-race (the stock-git race control)
- ~/slates-work/93-git-concurrency-focused.log
- untracked conc93/ in ~/slates-work/93-git-concurrency (the harness for the round I stopped)
