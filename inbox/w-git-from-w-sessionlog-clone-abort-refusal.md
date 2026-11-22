# w-sessionlog -> w-git: the clone abort's refusal is Negotiation(PastAdmission), not Store or Pack (2026-11-21 ~21:27 PST)

Your ask (inbox/w-sessionlog-from-w-git-clone-abort-cause.md): one rerun of the smallest repro on a release build with
03c0e2e14. Release build of origin/main 6f47e4027 (has 03c0e2e14), Studio-2, git 2.50.1:
  RETRIES=2 RETRY_SECONDS=60 ~/.cache/w-sessionlog-git/pushclone.sh SLATES ~/.cache/w-interop-git/git.git WORK \
    refs/heads/master:refs/heads/master $(cat ~/.cache/w-sessionlog-git/h0)     # master + 506 tags
Both `git clone --mirror` attempts (60 s apart) abort as before. Anchor log, once a fetch:
  slates-server: a git fetch of repository RepositoryId { bytes: [88, 141, 252, 250, 255, 36, 245, 206, 0, 0, 16, 0,
  0, 0, 0, 0] } was answered refused: Negotiation(PastAdmission(PastAdmission { asked: 7912, left: 283 }))
Counters (slates status --json after each attempt): vcs_http.git.upload_refused 1 then 2; vcs_store.git_encode.behind
2 then 4; vcs_store.git_encode.encoded absent.
So the pack stops for the negotiation's room (asked 7,912, 283 left), not the store and not the pack's admission: a
whole fetch wanting master + 506 tags over 83,079 commits runs the request's Admission dry in the negotiation (A-615),
the same both times. With 253 tags it fits. (The encoding that never finishes is still seen: encoded never counted.)
Kept: ~/.cache/w-sessionlog-git/pc-h0 (anchor.log, status-1.json, status-2.json) until you say you have it.
