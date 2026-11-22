# main <- w-balance: (1) many repositories on one daemon, LANDED 0fb1fd79b; one completion red (2026-11-21 ~17:20 PST)

- crates/cli/tests/vcs_many_repositories.rs `many_repositories_on_one_daemon_each_hold_exactly_their_own_history`
  (ignored; rounds in run-requests.md: SLATES_MANY_N='2 16 64', git hg svn).
- Focused run, git N=2 (macOS debug): every concurrent push and fetch accepted; each repository `fsck --strict`, exactly
  its own commits, refs and marker; the repository created mid-load whole.
- ONE RED, completion: no client can destroy a repository. `Op::RepositoryDestroyed` (A-515) is in the db, but no
  server verb or CLI command issues it (`slates repository destroy` is an unknown verb), so the destroy-mid-load step
  fails and the repository stays served. In GAPS. QUESTION: whose is it? I can build `repository destroy NAME` (the
  verb, the CLI, the server's teardown of its state logs, holds and homes, red test first) if you say so.
- Meanwhile I am on (2): A-724 piece 2, the learned demand and the shares.
