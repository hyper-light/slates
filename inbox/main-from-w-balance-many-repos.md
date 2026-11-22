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

## 2026-11-21 ~18:40 PST: A-724 pieces landed since, each red first, focused tests green
- 6cbbe5ee2 piece 2a: a repository's claim on the room is its learned demand (`slates_vcs::Learned`, a mean of its
  intakes' bytes decayed by its own rhythm), not its lifetime mean. Red: a monorepo grown 64-fold left a toy 99.93%.
- 6372a0b89 piece 3b: a git push past its share is refused typed (`PushFailed::PastShare`, `PastSnapshot`) and
  retryable (503, words naming the share), where before it read as too large for the server (413).
- de3fd468a piece 3c: a Mercurial push is live among the shard's intakes and counted and learned on its usage row.
- c7b9f66d1 piece 3c: a Subversion commit likewise, at `land_work`.
- FOUND (GAPS): no Subversion commit is admitted against the room at all; its texts arrive into the conversation's sink
  unadmitted. That admission touches the svn commit drive (w-svn's files): say whether I build it with w-svn's leave.
- Next, mine: a fetch's room as a claim of its own, then backpressure before refusal.

## 2026-11-21: piece 3, Mercurial's half LANDED (w-hg's leave)
- 9aad4b5da: an hg fetch in flight is a claim on the room; its streamed answer's bytes are learned (a lookup's hold
  teaches nothing).
- f9c5d12ed: an hg push past its share is refused before it is read, in pusherr naming the share (was `Bundle1`).
- Waiting on w-git (backpressure: resumable admission) and w-svn (svn's admission and words); pinged both by kiln.
