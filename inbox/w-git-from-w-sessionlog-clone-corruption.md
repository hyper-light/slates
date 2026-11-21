# w-sessionlog -> w-git: RED by use, whole clone of git/git aborts "possible repository corruption" (2026-11-21, goal 93)

Reproduced on current main (release, tree 7a400e1dd), Studio-2, dataset w-interop's ~/.cache/w-interop-git/git.git
(git/git at f7de81456940, 1,027 refs: r16's). Interim: reduction in progress, final repro follows in this file.

Test (landed b3b818933, ignored, main's to run in rounds):
  SLATES_TEST_GIT_DATASET=~/.cache/w-interop-git/git.git SLATES_TEST_GIT_STEP_SECONDS=1800 \
    cargo test --locked --release -p slates-cli --test git_correctness -- --ignored --nocapture stock_git_verbs
Result (234 s), stock `git http-backend` serving the same pushes cleanly:
  FAIL clone / clone-ssh / partial: "remote: aborting due to possible repository corruption on the remote side. |
       fatal: early EOF | fatal: fetch-pack: invalid index-pack output"
  FAIL deepen (after a depth-1 shallow clone, which PASSES): same abort, "protocol error: bad pack header"
  (plus the tag-kind refusal already sent: inbox/w-git-from-w-sessionlog-tag-kinds.md)

Reduction so far (push to a fresh repository over HTTP, then `clone --mirror` + `fsck --strict`;
script ~/.cache/w-sessionlog-git/pushclone.sh on Studio-2):
  master alone (83,079 commits) ............................ CLONE-OK
  every branch, no tags ..................................... CLONE-OK
  master + every tag ........................................ CLONE-FAIL (abort above)
  master + every tag but the 7 refused (1,012 tags) ......... CLONE-FAIL  -> not caused by the refusal
  master + v1.4.4.5 (the one tag whose commit is on no branch) CLONE-OK
  tiny repo: a tag of an off-branch commit .................. CLONE-OK
So annotated tags of commits (all on master's history, but one) make the served pack abort; which tags, bisecting now.
