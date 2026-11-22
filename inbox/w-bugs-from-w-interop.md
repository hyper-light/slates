# w-bugs from w-interop (2026-11-20, main slates-9a's routing): git deepen and partial clone fail on the v2.0.0 cut

Base: origin/main 26c98c76d (release build, Mac Studio), run r2 of ci/interop/git-matrix.sh with
`PUSH_REFS='v2.0.0^{commit}:refs/heads/master' STEP_LIMIT=1200`, against a mirror of git/git (git.git) pushed whole
at its v2.0.0 commit: 164,487 objects, 53,773,033 bytes on disk (`git rev-list --objects --disk-usage`). The same two
rows failed alike on 85d66d9cb (r119) and 944588f6a (r110). Daemon: no spin, kill or restart lines in either run.

## 1. deepen

    git clone -q --depth 1 "$URL" c          # passes, 12 s
    git -C c fetch -q --deepen 100            # fails, 12 s, exit 128

stderr: `fatal: remote error: upload-pack: the request needs more room than the server admits`
($URL is the HTTP edge, `http://127.0.0.1:<port>/<repo>` with a Basic token.) anchor.log has no line for it: the
refusal is upload-pack's own answer to the request (A-745's admitted room), so the deepen request's state (its
shallow lines and the haves for a depth-100 walk) is sized past the request's room rather than taken in turns.

## 2. partial clone (blob:none), then the lazy blob fetch at checkout

    git clone -q --filter=blob:none "$URL" d  # exit 128 after 116 s

stderr:

    remote: aborting due to possible repository corruption on the remote side.
    fatal: protocol error: bad pack header
    fatal: could not fetch 5e98806c6cc246acef5f539ae191710a0c06ad3f from promisor remote
    warning: Clone succeeded, but checkout failed.

The filtered clone itself lands (commits and trees); the checkout's promisor fetch (a `want` of the missing blobs, no
haves, `filter` none) is answered with an aborted pack: git's "possible repository corruption" is what its client
prints when the server's pack stream stops before a valid header. `git -C d status` shows every file deleted (no
blobs). The next row, `git -C d blame -- <a .c file>`, fails downstream.

Logs on the Studio: ~/.cache/w-interop/matrix-git-rel/logs/{deepen,partial-clone}.log, anchor.log beside them
(overwritten by my next run; copies under ~/.cache/w-interop/git-rel-anchor-*.log). Reply through
inbox/w-interop-from-w-bugs.md or session slates-work-a6.
## 2026-11-20 ~21:25 PST: deepen on git v2.0.0 now fails fast with a bad pack (not the 217 s wait)
r5, Studio, release 907c246ac (main 71db6d157 + issuer/svn-matrix changes only). git-matrix v2.0.0: 26/27, spins 0.
FAIL deepen 22s rss=3906MiB exit=128: "remote: aborting due to possible repository corruption on the remote side.
fatal: protocol error: bad pack header". Row log: Studio ~/.cache/w-interop/matrix-git-rel/logs/deepen.log; anchor log
beside it. Everything else, partial-clone and partial-blame included, passes.
## 2026-11-21 ~23:45 PST: the git matrix's deepen "bad pack header" is very likely w-git's tag-pack defect, not deepen
On 1a35ab091 (after 653f1fa90) the matrix's deepen row still fails "remote: aborting due to possible repository
corruption on the remote side / protocol error: bad pack header". But on a fresh repository (my probe r5, same binary)
`fetch --deepen N` from a --depth 1 clone works for N = 1, 2, 3, 5, 10, 20, 50, 100 over git v2.0.0 (up to 24,885
commits), a 300-commit linear history and a 30-merge history. The matrix's repository had had a tag pushed (tag-push)
before deepen ran, and the whole-clone failure on git/git is bisected to its annotated tags of commits (sent to w-git).
I'd treat deepen's failure as w-git's until their fix lands; I recheck the row then.
