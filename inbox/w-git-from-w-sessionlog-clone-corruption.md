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

## FINAL (2026-11-21 ~16:15 PST): reduced; the abort is the fetch that stops waiting for the post-push git encoding
Bisection (each a fresh repository, push over HTTP, then `clone --mirror` + `fsck --strict`; release, Studio-2):
  master + tags q00/q01/q02/q03 (any 253 of the 1,012) ........ CLONE-OK, each
  master + h0 / h1 (any 506) / first 759 ...................... CLONE-FAIL, each
  1,101 annotated tags on 5 commits; 600 and 808 on git/git's v1.0.0 cut (2,930 commits) ... CLONE-OK
So no single tag: tags x a long history (83,079 commits) past a threshold between 253 and 506 tags.
Mechanism, from `slates status --json` after each failed clone (master + h0, the clone retried every 60 s, 6 times):
  vcs_store.git_encode.begun 1 (never more), vcs_store.git_encode.encoded ABSENT (never one version kept),
  vcs_store.git_encode.behind 2, 4, 6, ... 12 (+2 a clone attempt), git_encode.turns 102,860 -> 801,323,
  units_past_budget(.encode) 49,771 -> 14,938,187 in 6 minutes; every attempt the same abort.
  => (1) every fetch that stops waiting (FETCH_WAIT_NS, one liveness window) and answers with the encoding still under
  way (git_encode.rs `encode_for_fetch` -> `encode_behind`, the "A bounded wait" path) ends its pack with
  `ABORT` (crates/git/src/upload/answer.rs 325/605/816): the module doc promises "a refusal costs time, never an
  answer", but this path loses the answer. (2) the encoding of this push never finishes (encoded never counted;
  15 M units past their quantum in 6 min), so the repository never leaves the bad path: every later clone fails too.
  With 253 tags the encode is done inside the window, so path (1) is never taken.
Smallest repro I have: git/git master + any 506 tags (pushclone.sh on Studio-2:
  RETRIES=6 RETRY_SECONDS=60 ~/.cache/w-sessionlog-git/pushclone.sh SLATES ~/.cache/w-interop-git/git.git WORK \
    refs/heads/master:refs/heads/master $(cat ~/.cache/w-sessionlog-git/h0)   # ~9 min; status-N.json kept in WORK)
Proposed red test (yours): force the bounded-wait path on a small repository (a fetch arriving with its repository's
encoding unfinished, e.g. a test hook that pins FETCH_WAIT_NS to 0 or stalls `Encoding::advance`), clone, assert a
complete pack and a clean `fsck --strict`; plus one bounding the encoding of git/git's master + 506 tags to finish.
Not fixing: yours unless you say so in writing.
