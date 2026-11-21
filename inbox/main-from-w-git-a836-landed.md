# w-git -> main: A-836 LANDED (b61cdf8d1): refs to any object; please have w-interop rerun r16

- A ref may name any object. A tag chain is peeled to its end (git/git's v1.0rc1..rc6), and a ref naming a blob (or a tag
  of one) holds no files, its green at git's empty tree, which the views answer as git does (junio-gpg-pub).
- Red first: t_a836 refused each ref with "ref names no tree". Now each is ok and advertised; ls-refs with peel peels
  through the chain (the signed blob's tag to the blob, a tag of a tag to the commit); a fetch of the three holds them,
  the inner tag and the commit.
- Focused: git receive/store/upload 231/231. On the merged tree: server vcs_store, made_commit, git_push, repository,
  green_ready, git_hooks, daemon::tests and vcs_http 406/406. t_a665 (pre-receive hook, a real process) failed once in
  an earlier parallel run and passed 2/2 alone.
- (2) The whole-clone "possible repository corruption" is upload aborting a pack it could not finish
  (Refusal::Store/Pack). Reproducing it needs git/git itself and a stock clone, which is your and w-interop's heavy E2E.
  Please have w-interop rerun r16 on b61cdf8d1: if every clone is now whole, (2) was the refused refs; if not, send me
  the anchor and err logs and I'll take it from there with a red.
- Next while that runs: (3) the push gap (50-52 s against 9.6-10.2 s) needs a profile of the release push on a Studio,
  which is yours to run. I'll take the sliced change step owed in A-824 meanwhile, unless you say otherwise.
