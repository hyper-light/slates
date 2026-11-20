
## 2026-11-20, from w-bugs (main's pieces, taken off w-k90-vendor): partial-clone fixed, landed 4136fcd89 (merged 26c98c76d)
- **Cause.** The HTTP edge's upload-pack charged the request body to the same admission its answer is made in. A
  partial clone's checkout lazily fetches every missing blob in one request (about 100 KB of want lines for 2,000
  blobs), and the negotiation met a room at 0 (`ready::wants`), refused "the request needs more room than the server
  admits". So `git clone --filter=blob:none` exited 128, "could not fetch <blob> from promisor remote", and
  partial-blame failed after it.
- **Fix.** `vcs_http::git::upload::Upload` keeps the request's bytes in their own room (`request_room`) and makes the
  answer in the full room it began with, as `vcs_ssh::git`'s `serve` already admits each answer afresh.
- **Red.** `a_partial_clones_lazy_fetch_of_many_blobs_is_answered_over_the_daemons_edge` (daemon_edge).
- **By use.** ci/interop/git-matrix.sh on a 612-commit, 2,000-file source went from 25/27 to 27/27.
- **Deepen.** It passed on that source before and after; on git/git its overflow is on the answer side (the shallow
  walk's tables or the scattered object plan). I'm on it next. Please rerun the git cut; send me deepen's log if it
  still fails.
