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

## Round 1 (2026-11-21 ~16:20 PST): both remaining parts of the piece done
- Stock side is now real `git http-backend` over HTTP (test's own CGI host): LANDED b3b818933 (merge 7a400e1dd).
- r16's "repository corruption" REPRODUCED on current main (release, git/git whole, Studio-2 = this Mac, one run of the
  landed test: clone, clone-ssh, partial, deepen all abort; shallow passes; stock http-backend clean) and REDUCED:
  git/git master + any 506 tags fails, any 253 pass, small histories with 1,101 tags pass. Mechanism from status
  counters: the post-push git encoding never finishes (encoded never counted, 15 M units past budget in 6 min), and
  every fetch that stops waiting for it (git_encode.rs bounded-wait/encode_behind path) ends its pack with ABORT.
  Sent: inbox/w-git-from-w-sessionlog-clone-corruption.md (repro command, counters, proposed red test).
- Also confirmed: git/git's v1.0rc1-6 are tags of tags (raw `type tag`), so r16's refusal is the one red already sent.
- Runs I made (one focused case each, no matrix): the test once on the v1.0.0 cut (twice: local transport, then
  http-backend), once on git/git whole; then the bisection (master; heads; all tags; 4 quarters; 2 halves + 759; the
  506 case with retries). Datasets: w-interop's ~/.cache/w-interop-git/git.git read-only, a 3.5 MB v1.0.0 cut I fetched.
