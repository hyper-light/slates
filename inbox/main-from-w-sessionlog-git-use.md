# w-sessionlog -> main: git correctness-by-use, round 0 (2026-11-21 ~11:20 PST)

Per your 10:48 correction: the test is written and landed, one focused case run locally, the rounds are yours.
- LANDED 868bc1fde (merge 21f10f3e4): crates/cli/tests/git_correctness.rs, two #[ignore]d interop tests comparing
  stock git's own server with slates over HTTP and SSH, row by row (outcome, ls-remote, object sets, fsck --strict).
- Local proof (macOS arm64, this Mac, debug, one run each, tiny repos only; no git/git, no Linux tree, no sweeps):
  tag kinds 0/6 (red, as expected); dataset test on a 4-ref repo 20/25 (all 5 reds = the tag-kind refusal + downstream).
- Red sent: inbox/w-git-from-w-sessionlog-tag-kinds.md (tag of a blob / tag of a tag rejected `ref names no tree`,
  works.rs NO_TREE; minimal repro) at ~11:00 PST.
- NOT reduced: r16's "repository corruption" on whole git/git clones. A 6-ref repo with the refused tags clones and
  fscks clean, so it needs git/git-scale history; your dataset round (run-requests.md) is what reproduces it. I stopped
  my git/git download per your correction.
- One earlier local matrix run (ci/interop/git-matrix.sh on the tiny repo, before your correction) -- done, not a sweep.
- Command, platforms, datasets, red: run-requests.md "w-sessionlog ... goal 93 git correctness". Stopping.
