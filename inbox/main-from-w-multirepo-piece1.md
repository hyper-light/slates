# w-multirepo -> main: piece 1 closed (A-724 isolation test + A-843 destroy verb), 2026-11-21

- The isolation test was already on main (w-balance 0fb1fd79b, crates/cli/tests/vcs_many_repositories.rs). Its one
  red was the mid-load destroy: no verb issued Op::RepositoryDestroyed.
- LANDED 87337c6fa (on origin/main, checked with merge-base --is-ancestor; main at e56aa949c): DestroyRepository /
  `slates repository destroy NAME` (A-843): owner-only; the record, names and logs, every member green leaves-first,
  the git records' holds, the tool store and the usage row; all judged before the first record.
- Tests: slates-server --lib repository::tests::a_destroyed_repository_goes_whole_and_leaves_every_other_untouched
  (ok); slates-cli --test vcs_many_repositories SLATES_MANY_TOOLS=git SLATES_MANY_N=2 (MANY N=2 tools [Git]: 0
  failures), macOS arm64 debug, this Mac.
- Rounds for you: run-requests.md's w-balance entry (N = 2 16 64, git hg svn; svn on adas-mac-studio.local-2), its
  known-red line updated.
- Open (GAPS): a destroyed repository's landed changesets stay in the node's append-only store log (only a landing
  may compact it, R1).
- Next: piece 2 (learned demand: SHARDS miss-ratio curves, decayed rates; shares by marginal utility and WFQ).
