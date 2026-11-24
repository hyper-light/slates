# The speed harnesses ran slates' git and svn legs without a store, and the round ended as if it had a record (2026-11-23)

## Description
Main's r592, the first `interop_speed.sh` round (the big Studio, main 79d4e0814, 20:28-20:51, five runs interleaved):
every slates git step failed in 0.02-0.04 s with exit 128 (push, clone, clone-again, the incremental push and fetch,
shallow and partial), and every slates svn step failed in 0.013-0.040 s with exit 1, the import first. Stock
`git daemon`, `git http-backend` and `svnserve` ran, and hg's slates leg worked. The round's table showed the slates
rows with a best of "-". hg's `push-chunked` and `import` rows were empty for both servers.

## Root cause
- `git_scale.sh` and `svn_scale.sh` started slates' anchor without `--vcs-store`. Since C2 a git or Subversion
  repository's history lives on the node's changeset store, so `slates repository create` was refused (reproduced on
  the MacBook: "a git repository's history is kept on the node's changeset store: start the daemon with --vcs-store
  DIR"), and every step after it met no repository. `hg_scale.sh` passed `--vcs-store`, and made the directory first,
  which the daemon needs.
- The harnesses ignored the refused `repository create`, printed a row with a best of "-" for a step every run of
  which failed, and exited 0, so the wrapper had no failure to report.
- hg's `push-chunked` and `import` steps run only under `PUSH_CHUNK` and `SLATES_FILL`. The table printed every step it
  knows, whether any server ran it or not.

## Impact
The first order-of-magnitude round measured no slates git or svn operation, and said so only as "-". Nothing in slates
was wrong.

## Edits
- `crates/cli/bench/git_scale.sh`, `svn_scale.sh`: the anchor gets `--vcs-store WORK/store-N`, its directory made
  first, as `hg_scale.sh` does.
- All three harnesses:
  - A refused `repository create` ends that run, named.
  - A step no server ran is no row; a step every run of which failed prints `FAILED`, not a best of "-".
  - Such a step, or a server that timed no step at all, makes the harness exit 1, naming them.
- `interop_speed.sh`: each harness's exit status is recorded, and any failure fails the round, loudly.

## Evidence
On the MacBook (debug slates, a 3-commit repository, load ~45):
- Before: `git_scale.sh` with `SERVERS=slates` printed every slates step `failed(128)` and exited 0.
- With a store removed from the fixed script: exit 1, "FAILED: every run failed for slates:every-step" for slates
  alone, and every slates step named beside stock.
- Fixed, `SERVERS="stock stock-http slates"`: exit 0, 24 steps ok. slates' clone was identical to the source in refs
  and objects, and its incremental fetch held the new commit.
- Not run here: `svn_scale.sh` and `hg_scale.sh`, which need stock svn and hg. Their edits are the same lines, checked
  with `bash -n`.
