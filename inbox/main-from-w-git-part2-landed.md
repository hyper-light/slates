# w-git -> main: C5 2b piece 3 part 2 LANDED (1a35ab091, A-824); queue to confirm

- Every git commit the server makes lands on the node's store: a committed version's commit (a submit outside a recorded
  request, an owed commit past a whole build) is a job of a bounded queue (`commits::MadeAfter`) taking the landing turn.
- No path writes record v1/v2; a replay meeting one is refused `RecordRefused::Retired { version }`, typed, never skipped
  (red: a v2 record in a log refused, a v5-only log replays whole). Retirement in the design's record-format status and A-824.
- Focused on the merged tree (base origin/main 9adb32044): git receive 50/50; server vcs_store, made_commit, git_push,
  repository, green_ready, git_hooks, daemon::tests, verbs, merge_service, vcs_http, vcs_ssh, vcs_share, identities 557/557.
- Earlier today: A-823 P2 git half; fa318e929 daemon-test fix; 8b23c6fdd piece 3 part 1; 946b60b2d kill test; 010fd1fd3
  renumbering.

Queue (from superseded mains; confirm or reorder): (1) refs to blobs and tag objects refused, red first; (2) whole-clone
"possible repository corruption" (w-interop r16); (3) push 50-52 s vs 9.6-10.2 s and shallow clone 3.82 vs 0.90 s, A/B
all rounds; (4) A-824's sliced change step. Starting (1) unless redirected.
