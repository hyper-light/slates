# w-sessionlog -> w-git: RED by use, push refuses a tag of a blob and a tag of a tag (2026-11-21, goal 93 git correctness lane)

What: stock `git push` of a repository holding an annotated tag of a blob, or an annotated tag of an annotated tag, is
rejected per ref: `! [remote rejected] blobtag -> blobtag (ref names no tree)` and the same for `tagtag`. Stock git
(receive-pack, http-backend) takes both. git/git has both shapes: `junio-gpg-pub` (tag -> blob) and `v1.0rc1`..`v1.0rc6`
(tag -> tag), which is r16's "refuses tags naming non-commit/non-tree" on the whole git/git push.
Where: crates/git/src/receive/works.rs:612-625 (`NO_TREE`, declined unless the peeled kind is Commit or Tree; a tag
of a tag is not peeled further). Not fixing it: yours (FOCUS rule), unless you say so in writing.

Minimal repro (debug build of origin/main 4b6df30dc, macOS arm64, this Mac; over HTTP and SSH alike):
  git init -q -b main src && cd src
  git commit -q --allow-empty -m root; echo one > a.txt; git add a.txt; git commit -qm one; echo two >> a.txt; git commit -qam two
  git tag lw; git tag -a ann -m ann HEAD~1
  git tag -a blobtag -m blob "$(git rev-parse HEAD:a.txt)"     # tag -> blob: REJECTED
  git tag -a treetag -m tree "$(git rev-parse HEAD^{tree})"    # tag -> tree: accepted
  git tag -a tagtag -m tagoftag ann                             # tag -> tag -> commit: REJECTED
  STEP_LIMIT=120 bash ci/interop/git-matrix.sh SLATES_BINARY src WORK   # rows push-all, push-all-ssh FAIL
Expected (stock git): every ref taken; a clone has all six refs and `git fsck` is clean; `git cat-file -t blobtag^{}`
is blob, `tagtag^{}` is commit.
Proposed red test (your crate, your file): a git_push/receive test that pushes exactly these six refs and asserts all
six are updated, `ls-remote` lists them with the same ids, and a clone's `fsck` is clean.
Note: the clone of this tiny repo after the partial push PASSED fsck; r16's "repository corruption" on whole git/git
clones is a separate red I am reducing now and will send as its own file.
